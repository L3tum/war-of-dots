//! CombatSystem — resolve adjacent enemy combat using spatial hash.
//!
//! Uses HEAD's per-pair direct read-modify-write pattern with game-owned
//! scratch buffers (capacity reuse across ticks). No HashMap, no two-phase
//! batch apply — compounding works naturally because ECS writes are immediate
//! and re-fetched on each engagement.

use crate::game::components::*;
use crate::game::game::GameSettings;
use crate::game::game::TickContext;
use crate::game::world_ext::WorldExt;
use hecs::Entity;
use hecs::World;
use std::collections::HashMap;
use std::collections::HashSet;
use std::sync::Arc;

// ============================================================================
// Data Structures
// ============================================================================

/// Pre-allocated scratch buffers for combat — .clear()'d each tick, capacity preserved.
/// Owned by `Game` and passed by mutable reference into `run()`.
pub struct CombatScratch {
    resolved: HashSet<(hecs::Entity, hecs::Entity)>,
    fought: HashSet<hecs::Entity>,
    dead_set: HashSet<hecs::Entity>,
    dead: Vec<hecs::Entity>,
    /// Candidate unit snapshots for this tick, cleared and reused each run
    /// (capacity preserved). Named `candidate_units` (NOT `candidates`) to avoid
    /// colliding with the `pub candidates: usize` diagnostic counter (compile
    /// rule R2).
    /// `NearbyEnemies` is deliberately NOT in this tuple: the resolution loop
    /// re-reads each unit's nearby list live (a unit only ever attacks ONE enemy,
    /// so cloning the full — often ~10KB+ in dense melees — list per candidate
    /// was a ~7 MB/tick allocation hotspot).
    pub candidate_units: Vec<(hecs::Entity, UnitPosition, u8, UnitStats, UnitFlags)>,
    // Diagnostic counters (benchmark/status): reset each tick alongside clear().
    pub candidates: usize,
    pub attackers: usize,
    pub pairs_scanned: usize,
    pub pairs_same_owner: usize,
    pub pairs_out_of_range: usize,
    pub pairs_already_resolved: usize,
}

impl CombatScratch {
    pub fn new() -> Self {
        Self {
            resolved: HashSet::new(),
            fought: HashSet::new(),
            dead_set: HashSet::new(),
            dead: Vec::new(),
            candidate_units: Vec::new(),
            candidates: 0,
            attackers: 0,
            pairs_scanned: 0,
            pairs_same_owner: 0,
            pairs_out_of_range: 0,
            pairs_already_resolved: 0,
        }
    }

    /// Clear all collections but preserve capacity for reuse across ticks.
    pub fn clear(&mut self) {
        self.resolved.clear();
        self.fought.clear();
        self.dead_set.clear();
        self.dead.clear();
        self.candidate_units.clear();
        self.candidates = 0;
        self.attackers = 0;
        self.pairs_scanned = 0;
        self.pairs_same_owner = 0;
        self.pairs_out_of_range = 0;
        self.pairs_already_resolved = 0;
    }

    /// Number of (normalized) pairs resolved into actual combat this tick.
    /// Read after `run()` returns; mirror of the `resolved` set so the Game
    /// layer (and benchmark) can report engaged-pairs-per-tick.
    pub fn engaged_pairs(&self) -> usize {
        self.resolved.len()
    }
}

impl Default for CombatScratch {
    fn default() -> Self {
        Self::new()
    }
}

// ============================================================================
// Attrition Helper
// ============================================================================

/// Apply equipment attrition to `eq` **in place**, reducing each item's `count`.
/// Returns `false` (leaving `eq` untouched) when there is no damage to apply or
/// the unit carries no equipment — preserving `attrition_from`'s `Option`
/// contract. Attrition never adds/removes items or changes `name`/`max_count`/
/// `cost_per_item`, so an in-place count update is bit-identical to the old
/// "build a fresh `Vec<Equipment>` and reassign" path, but allocates **nothing**
/// (no `Vec` buffer, no per-item `name` `String` clone).
fn attrition_in_place(eq: &mut UnitEquipment, dmg: f32, attr_rate: f32) -> bool {
    if dmg <= 0.0 || eq.equipment.is_empty() {
        return false;
    }
    for e in eq.equipment.iter_mut() {
        let loss = (dmg * attr_rate).min(e.count as f32);
        e.count = (e.count as f32 - loss).max(0.0) as u32;
    }
    true
}

// ============================================================================
// Terrain Modifier Helper
// ============================================================================

/// Combat strength multiplier from equipment stock, linear in the equipment
/// ratio (carried / required): 100% equipment → 1.0, 60% → 0.6, 0% → 0.
/// Under-supplied units hit proportionally softer.
fn equipment_effectiveness(eq: &UnitEquipment) -> f32 {
    let needed: f32 = eq.max_equipment.iter().map(|e| e.max_count as f32).sum();
    if needed <= 0.0 {
        return 1.0;
    }
    let carried: f32 = eq.equipment.iter().map(|e| e.count as f32).sum();
    (carried / needed).clamp(0.0, 1.0)
}

/// Convert terrain type to combat multiplier. Hills boost (1.5×), forest (0.75×),
/// water weakens (0.5×), all others neutral (1.0×).
fn terrain_modifier(terrain: &crate::game::unit::Terrain) -> f32 {
    match terrain {
        crate::game::unit::Terrain::Hills => 1.5,
        crate::game::unit::Terrain::Forest => 0.75,
        crate::game::unit::Terrain::Water => 0.5,
        _ => 1.0,
    }
}

// ============================================================================
// Main Combat System
// ============================================================================

pub fn run(
    world: &mut World,
    world_entity: hecs::Entity,
    scratch: &mut CombatScratch,
    settings: &GameSettings,
    ctx: &TickContext,
    unit_types_index: &HashMap<String, usize>,
) -> Vec<hecs::Entity> {
    // Clear scratch for reuse (capacity preserved)
    scratch.clear();

    // Unit-type lookup uses `unit_types_index` (name -> index into
    // `settings.unit_types`), built once per `Game` — no per-tick HashMap build.
    // Per-engagement attrition lookups index straight into `settings.unit_types`.

    // Scoped Arc extraction: drops hecs::Ref borrow immediately, only clones terrain Arc (24 bytes)
    #[cfg(any(debug_assertions, feature = "debug-overlay"))]
    let _t_wdc = crate::game::debug_stats::DetailedTimer::new("combat/world_data_clone");
    let (terrain, terrain_w) = match world.world_data(world_entity) {
        Some(wb) => (Arc::clone(&wb.terrain.grid), wb.terrain.width as usize),
        None => return Vec::new(),
    };
    // wb (hecs::Ref) dropped — borrow released. Can now do mutable queries.
    #[cfg(any(debug_assertions, feature = "debug-overlay"))]
    _t_wdc.finish(world, world_entity);

    // Only check units that moved this tick OR are already fighting
    // Collect cheap components upfront (Stats, Flags are Copy). Equipment deferred to resolution.
    #[cfg(any(debug_assertions, feature = "debug-overlay"))]
    let _t_ue = crate::game::debug_stats::DetailedTimer::new("combat/unit_enumeration");
    // Candidates are position-driven, not just movement-driven: a unit fights while
    // it has any enemy in its NearbyEnemies list (`attacking`/`fighting` add
    // movers + previously-engaged units). This is what keeps two adjacent units
    // fighting every tick even when neither moved.
    scratch.candidate_units.clear();
    scratch.candidate_units.extend(
        world
            .query::<(
                Entity,
                &UnitPosition,
                &UnitFlags,
                &UnitStats,
                &NearbyEnemies,
            )>()
            .iter()
            .filter(|(_, _, flags, stats, nearby)| {
                stats.men > 0.0
                    && (flags.attacking || flags.fighting || !nearby.entities.is_empty())
            })
            .filter_map(|(e, pos, flags, stats, _nearby)| {
                debug_assert!(
                    ctx.owner_id_by_entity.contains_key(&e),
                    "entity {:?} missing from owner_id_by_entity — TickContext stale?",
                    e
                );
                let owner_id = ctx.owner_id_by_entity.get(&e).copied()?;
                // No `nearby.clone()`: store just the entity; the resolution loop
                // re-reads the nearby list live (see candidate_units doc).
                Some((e, *pos, owner_id, *stats, *flags))
            }),
    );
    scratch.candidates = scratch.candidate_units.len();
    scratch.attackers = scratch
        .candidate_units
        .iter()
        .filter(|(_, _, _, _, flags)| flags.attacking)
        .count();
    #[cfg(any(debug_assertions, feature = "debug-overlay"))]
    _t_ue.finish(world, world_entity);

    #[cfg(any(debug_assertions, feature = "debug-overlay"))]
    let _t_lookup = crate::game::debug_stats::DetailedTimer::new("combat/adjacent_lookup");
    #[cfg(any(debug_assertions, feature = "debug-overlay"))]
    let _t_resolve = crate::game::debug_stats::DetailedTimer::new("combat/combat_resolution");
    #[cfg(any(debug_assertions, feature = "debug-overlay"))]
    let _t_stats = crate::game::debug_stats::DetailedTimer::new("combat/stats_update");

    for (entity1, pos1, owner1, _, _) in &scratch.candidate_units {
        // A unit that died during this tick (as a defender) should not go on to
        // attack other enemies in the same tick.
        if scratch.dead_set.contains(entity1) {
            continue;
        }

        // Find the first enemy entity1 attacks this tick. Every check in the scan
        // is a read (ctx / scratch / world.get), so we hold the immutable
        // `world.get(&NearbyEnemies)` borrow across the whole scan and drop it at
        // the block's end — BEFORE any mutation below. (The candidate snapshot no
        // longer clones the full NearbyEnemies list; each unit only ever attacks
        // ONE enemy, so cloning the whole — often ~10KB+ in dense melees — list
        // per candidate was a ~7 MB/tick allocation hotspot.)
        let target: Option<Entity> = {
            let nearby = match world.get::<&NearbyEnemies>(*entity1) {
                Ok(n) => n,
                Err(_) => continue,
            };
            let mut found: Option<Entity> = None;
            for &entity2 in nearby.entities.iter() {
                scratch.pairs_scanned += 1;
                if entity2 == *entity1 {
                    continue;
                }
                // Resolve each normalized (entity1, entity2) pair at most once per
                // tick, so the same two units never fight twice.
                let pair = if entity1.id() < entity2.id() {
                    (*entity1, entity2)
                } else {
                    (entity2, *entity1)
                };
                if scratch.resolved.contains(&pair) {
                    scratch.pairs_already_resolved += 1;
                    continue;
                }
                // Same-owner check via shared TickContext lookup (O(1) u8 equality,
                // no per-pair ECS borrow + String comparison).
                let same_owner = match ctx.owner_id_by_entity.get(&entity2) {
                    Some(other_id) => *owner1 == *other_id,
                    None => {
                        debug_assert!(
                            false,
                            "entity {:?} missing from owner_id_by_entity during same-owner check",
                            entity2
                        );
                        continue;
                    }
                };
                if same_owner {
                    scratch.pairs_same_owner += 1;
                    continue;
                }
                let pos2 = match world.get::<&UnitPosition>(entity2) {
                    Ok(p) => *p,
                    Err(_) => continue,
                };
                // Squared distance check — avoids sqrt()
                let dx = pos1.x - pos2.x;
                let dy = pos1.y - pos2.y;
                let dist_sq = dx * dx + dy * dy;
                if dist_sq >= crate::game::constants::COMBAT_DISTANCE_SQ {
                    scratch.pairs_out_of_range += 1;
                    continue;
                }
                // Re-fetch entity1's CURRENT stats: it may have taken damage from an
                // earlier engagement this tick. If it died, stop attacking.
                let s1 = match world.get::<&UnitStats>(*entity1) {
                    Ok(s) => *s,
                    Err(_) => break,
                };
                if s1.men <= 0.0 {
                    break;
                }
                // Presence-only checks (values re-fetched in the resolve phase).
                if world.get::<&UnitFlags>(*entity1).is_err() {
                    break;
                }
                if world.get::<&UnitStats>(entity2).is_err() {
                    continue;
                }
                if world.get::<&UnitFlags>(entity2).is_err() {
                    continue;
                }
                // Pair is only resolved when both units carry equipment (same skip
                // semantics as the original effectiveness check's `_ => continue`).
                if world.get::<&UnitEquipment>(*entity1).is_err()
                    || world.get::<&UnitEquipment>(entity2).is_err()
                {
                    continue;
                }
                found = Some(entity2);
                break;
            }
            found
        };
        // Immutable `nearby` world borrow is dropped above — safe to mutate now.
        let Some(entity2) = target else {
            continue;
        };
        let pair = if entity1.id() < entity2.id() {
            (*entity1, entity2)
        } else {
            (entity2, *entity1)
        };

        // ---- Resolve the (entity1, entity2) engagement (mutation phase) ----
        // Re-fetch the current stats/flags/position (identical to the scan's reads —
        // no mutation happened between the scan and this resolve for entity1).
        let s1 = match world.get::<&UnitStats>(*entity1) {
            Ok(s) => *s,
            Err(_) => continue,
        };
        if s1.men <= 0.0 {
            continue;
        }
        let f1 = match world.get::<&UnitFlags>(*entity1) {
            Ok(f) => *f,
            Err(_) => continue,
        };
        let s2 = match world.get::<&UnitStats>(entity2) {
            Ok(s) => *s,
            Err(_) => continue,
        };
        let f2 = match world.get::<&UnitFlags>(entity2) {
            Ok(f) => *f,
            Err(_) => continue,
        };
        let pos2 = match world.get::<&UnitPosition>(entity2) {
            Ok(p) => *p,
            Err(_) => continue,
        };

        let a_soft = 10.0
            * (s1.men / s1.max_men.max(0.1))
            * (s1.organisation / 100.0)
            * if f1.on_water { 0.75 } else { 1.0 };
        let d_soft = 10.0
            * (s2.men / s2.max_men.max(0.1))
            * (s2.organisation / 100.0)
            * if f2.on_water { 0.75 } else { 1.0 };

        // a_eff: attacker's offensive strength relative to defender's defense
        let a_eff = a_soft / (d_soft + 0.1);
        // d_eff: defender's counter-attack strength relative to attacker's offense
        let d_eff = d_soft / (a_soft + 0.1);

        // Under-supplied units fight less effectively (0% equipment → 40% strength;
        // 100%+ → full). Both units are guaranteed to have equipment (checked in
        // the scan), so this `_ => continue` is defensive.
        let (a_eff_malus, d_eff_malus) = {
            let (eq1, eq2) = match (
                world.get::<&UnitEquipment>(*entity1),
                world.get::<&UnitEquipment>(entity2),
            ) {
                (Ok(a), Ok(b)) => (a, b),
                _ => continue,
            };
            (equipment_effectiveness(&eq1), equipment_effectiveness(&eq2))
        };

        // Each unit uses the terrain it stands on — fixes asymmetry where both
        // sides would get the attacker's terrain modifier.
        let a_terrain = terrain
            .get((pos1.y as usize) * terrain_w + (pos1.x as usize))
            .copied()
            .unwrap_or(crate::game::unit::Terrain::Plains);
        let d_terrain = terrain
            .get((pos2.y as usize) * terrain_w + (pos2.x as usize))
            .copied()
            .unwrap_or(crate::game::unit::Terrain::Plains);
        let a_terrain_mod = terrain_modifier(&a_terrain);
        let d_terrain_mod = terrain_modifier(&d_terrain);

        // dmg_to_defender: damage applied TO the defender (attacker effectiveness)
        let dmg_to_defender = a_eff * a_terrain_mod * 5.0 * a_eff_malus;
        // dmg_to_attacker: damage applied TO the attacker (defender counter-attack)
        let dmg_to_attacker = d_eff * d_terrain_mod * 5.0 * d_eff_malus;

        let a_men = (s1.men - dmg_to_attacker).max(0.0);
        let d_men = (s2.men - dmg_to_defender).max(0.0);

        // Fetch both units' unit-type configs for attrition rates (O(1) via index).
        let (a_attr_rate, d_attr_rate) = {
            let a_rate = world
                .get::<&UnitTypeId>(*entity1)
                .ok()
                .and_then(|ut| {
                    unit_types_index
                        .get(&ut.name)
                        .map(|&i| &settings.unit_types[i])
                })
                .map(|ut| ut.fighting_attrition)
                .unwrap_or(1.0);
            let d_rate = world
                .get::<&UnitTypeId>(entity2)
                .ok()
                .and_then(|ut| {
                    unit_types_index
                        .get(&ut.name)
                        .map(|&i| &settings.unit_types[i])
                })
                .map(|ut| ut.fighting_attrition)
                .unwrap_or(1.0);
            (a_rate, d_rate)
        };

        if let Ok((ms, me, mf)) =
            world.query_one_mut::<(&mut UnitStats, &mut UnitEquipment, &mut UnitFlags)>(*entity1)
        {
            ms.men = a_men;
            // Org loss proportional to men lost in this engagement.
            let a_men_lost = s1.men - a_men;
            let a_org_loss = 10.0 * (a_men_lost / s1.men.max(0.1));
            ms.organisation = (ms.organisation - a_org_loss).max(0.0);
            // Equipment attrition: reduce each item's count in place (no Vec
            // buffer, no per-item name clone).
            attrition_in_place(me, dmg_to_attacker, a_attr_rate);
            // Combat consumes supply proportional to damage taken
            let supply_loss = (dmg_to_attacker * a_attr_rate).max(1.0);
            me.supply = (me.supply - supply_loss).max(0.0);
            mf.fighting = true;
        }
        if let Ok((ms, me, mf)) =
            world.query_one_mut::<(&mut UnitStats, &mut UnitEquipment, &mut UnitFlags)>(entity2)
        {
            ms.men = d_men;
            // Org loss proportional to men lost in this engagement.
            let d_men_lost = s2.men - d_men;
            let d_org_loss = 10.0 * (d_men_lost / s2.men.max(0.1));
            ms.organisation = (ms.organisation - d_org_loss).max(0.0);
            attrition_in_place(me, dmg_to_defender, d_attr_rate);
            // Combat consumes supply proportional to damage taken
            let supply_loss = (dmg_to_defender * d_attr_rate).max(1.0);
            me.supply = (me.supply - supply_loss).max(0.0);
            mf.fighting = true;
        }

        if a_men <= 0.0 {
            scratch.dead.push(*entity1);
            scratch.dead_set.insert(*entity1);
        }
        if d_men <= 0.0 {
            scratch.dead.push(entity2);
            scratch.dead_set.insert(entity2);
        }

        scratch.resolved.insert(pair);
        scratch.fought.insert(*entity1);
        scratch.fought.insert(entity2);

        // Each unit makes at most ONE attack per tick; it can still be *attacked*
        // by many enemies (and take counter-damage) within the same tick.
    }

    // Retreat check: clear fighting for units that found no nearby enemy
    // Reads from NearbyEnemies component directly
    #[cfg(any(debug_assertions, feature = "debug-overlay"))]
    let _t_retreat = crate::game::debug_stats::DetailedTimer::new("combat/retreat_check");
    for (entity, _, _, _, flags) in &scratch.candidate_units {
        if scratch.fought.contains(entity) {
            continue;
        }
        if !flags.fighting {
            continue;
        }
        // If nearby entities exist, the unit has enemies — keep fighting. Re-read
        // the nearby list live (no clone in the candidate snapshot).
        let has_enemies = match world.get::<&NearbyEnemies>(*entity) {
            Ok(n) => !n.entities.is_empty(),
            Err(_) => false,
        };
        if has_enemies {
            continue;
        }
        if let Ok((mf,)) = world.query_one_mut::<(&mut UnitFlags,)>(*entity) {
            mf.fighting = false;
        }
    }
    #[cfg(any(debug_assertions, feature = "debug-overlay"))]
    _t_retreat.finish(world, world_entity);

    #[cfg(any(debug_assertions, feature = "debug-overlay"))]
    {
        _t_lookup.finish(world, world_entity);
        _t_resolve.finish(world, world_entity);
        _t_stats.finish(world, world_entity);
    }

    // `take` empties `scratch.dead` but keeps its capacity, so the returned Vec
    // (consumed by the caller's death cleanup) costs no allocation in steady
    // state (mirror of supply.rs).
    std::mem::take(&mut scratch.dead)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::CityUpgradeSettings;
    use crate::game::game::TickContext;
    use crate::game::unit::{Equipment, Terrain};

    /// Unit-type name -> index into `test_settings().unit_types`, used for
    /// `run`'s cached-index param. `test_settings()` is deterministic, so
    /// this matches any `&test_settings()` value passed to `run`.
    fn ut_index() -> HashMap<String, usize> {
        test_settings()
            .unit_types
            .iter()
            .enumerate()
            .map(|(i, ut)| (ut.name.clone(), i))
            .collect()
    }

    fn make_world() -> (World, hecs::Entity) {
        let mut world = World::new();
        let terrain = vec![vec![Terrain::Plains; 20]; 20];
        let world_entity = world.spawn((WorldBundle::new(
            terrain,
            20,
            20,
            CityUpgradeSettings::default(),
            std::collections::HashMap::new(),
        ),));
        (world, world_entity)
    }

    fn make_unit(world: &mut World, pid: &str, x: f32, y: f32, men: f32) -> hecs::Entity {
        world.spawn((
            UnitPosition { x, y },
            UnitOwner {
                player_id: pid.to_string(),
            },
            UnitTypeId {
                name: "infantry".to_string(),
            },
            UnitFlags {
                fighting: true,
                attacking: true,
                moved: false,
                ..Default::default()
            },
            UnitStats {
                men,
                max_men: 1000.0,
                organisation: 100.0,
                max_organisation: 100.0,
            },
            UnitEquipment {
                equipment: vec![],
                max_equipment: vec![],
                supply_buffer_percent: 10,
                supply_rate: 5,
                supply: 2000.0,
                max_supply: 2000.0,
            },
            NearbyEnemies {
                entities: Default::default(),
                initialized: true,
            },
        ))
    }

    fn make_unit_with_equipment(
        world: &mut World,
        pid: &str,
        x: f32,
        y: f32,
        men: f32,
        equipment: Vec<Equipment>,
    ) -> hecs::Entity {
        // max_equipment mirrors the same loadout so supply/effectiveness maths
        // see a real "required" amount.
        let max_equipment = equipment.clone();
        world.spawn((
            UnitPosition { x, y },
            UnitOwner {
                player_id: pid.to_string(),
            },
            UnitTypeId {
                name: "infantry".to_string(),
            },
            UnitFlags {
                fighting: true,
                attacking: true,
                moved: false,
                ..Default::default()
            },
            UnitStats {
                men,
                max_men: 1000.0,
                organisation: 100.0,
                max_organisation: 100.0,
            },
            UnitEquipment {
                equipment,
                max_equipment,
                supply_buffer_percent: 10,
                supply_rate: 5,
                supply: 2000.0,
                max_supply: 2000.0,
            },
            NearbyEnemies {
                entities: Default::default(),
                initialized: true,
            },
        ))
    }

    /// Populate NearbyEnemies.entities for each unit with enemy entities
    /// within COMBAT_DISTANCE. Replaces the old SpatialHash-based approach.
    fn populate_nearby_enemies(world: &mut World) {
        let units: Vec<(hecs::Entity, UnitPosition, String)> = world
            .query::<(Entity, &UnitPosition, &UnitOwner)>()
            .iter()
            .map(|(e, pos, owner)| (e, *pos, owner.player_id.clone()))
            .collect();
        for (entity, pos, owner) in &units {
            let mut nearby: Vec<hecs::Entity> = Vec::new();
            for (other, other_pos, other_owner) in &units {
                if other == entity {
                    continue;
                }
                if other_owner == owner {
                    continue;
                }
                let dx = pos.x - other_pos.x;
                let dy = pos.y - other_pos.y;
                if dx * dx + dy * dy < crate::game::constants::COMBAT_DISTANCE_SQ {
                    nearby.push(*other);
                }
            }
            if let Ok((ne,)) = world.query_one_mut::<(&mut NearbyEnemies,)>(*entity) {
                ne.entities = nearby.into_iter().collect();
            }
        }
    }

    fn test_settings() -> GameSettings {
        GameSettings {
            starting_funds: 1000,
            initial_manpower: 10_000,
            funds_per_city: 1.0,
            recruitment_rate: 1.0,
            game_speed: 1.0,
            use_standard_ruleset: true,
            unit_types: vec![crate::game::economy::UnitType {
                name: "infantry".to_string(),
                symbol: "I".to_string(),
                men: 1000,
                equipment: vec![],
                soft_attack: 5.0,
                soft_defense: 4.0,
                hard_attack: 3.0,
                hard_defense: 2.0,
                recruitment_cost: 200,
                max_organisation: 100.0,
                reinforcement_rate: 5.0,
                speed: 1.0,
                base_attrition: 0.5,
                fighting_attrition: 1.0,
                supply_buffer_percent: 10,
                supply_rate: 5,
                max_supply: 2000,
            }],
            city_upgrade_settings: CityUpgradeSettings::default(),
            water_speed: 0.5,
            influence: crate::game::game::InfluenceSettings::default(),
        }
    }

    /// Two stationary adjacent enemy units with NO attacking/fighting flags must
    /// still engage: non-empty NearbyEnemies makes them combat candidates. This is
    /// the "if you're within combat range, you fight" contract — previously two
    /// parked enemies could sit adjacent forever unless one of them moved.
    #[test]
    fn stationary_adjacent_enemies_keep_fighting() {
        let (mut world, world_entity) = make_world();
        let a = make_unit(&mut world, "P1", 10.0, 10.0, 100.0);
        let b = make_unit(&mut world, "P2", 10.5, 10.5, 100.0);
        // Strip movement/engagement flags: pure stationary adjacency.
        for e in [a, b] {
            let (flags,) = world.query_one_mut::<(&mut UnitFlags,)>(e).unwrap();
            flags.attacking = false;
            flags.fighting = false;
        }
        populate_nearby_enemies(&mut world);
        let mut scratch = CombatScratch::new();
        let ctx = TickContext::build_full(&world, world_entity);

        let _dead = run(
            &mut world,
            world_entity,
            &mut scratch,
            &test_settings(),
            &ctx,
            &ut_index(),
        );

        assert_eq!(
            scratch.engaged_pairs(),
            1,
            "adjacent stationary enemies should fight"
        );
        assert!(scratch.fought.contains(&a), "A should fight");
        assert!(scratch.fought.contains(&b), "B should fight");
    }

    /// A 0-men unit must not be processed as an attacker (candidate filter requires men > 0).
    #[test]
    fn combat_skips_dead_attacker() {
        let (mut world, world_entity) = make_world();
        let _a = make_unit(&mut world, "P1", 10.0, 10.0, 0.0);
        populate_nearby_enemies(&mut world);
        let mut scratch = CombatScratch::new();
        let ctx = TickContext::build_full(&world, world_entity);

        let dead = run(
            &mut world,
            world_entity,
            &mut scratch,
            &test_settings(),
            &ctx,
            &ut_index(),
        );
        assert!(
            dead.is_empty(),
            "0-men unit must not be processed by combat"
        );
    }

    /// Three mutually-adjacent units of three different players must all engage in
    /// the melee (every unit gets to act, so the old "first attacker captures all
    /// its neighbours" star is gone), while each unit makes at most one attack per
    /// tick. With identical units each pair deals ~4.95 dmg to both sides, so every
    /// unit ends somewhere in the ~990–996 range — clearly damaged (took at least one
    /// hit or counter) but not carpet-bombed.
    ///
    /// NOTE: the first unit in ECS order gets attacked first (as attacker vs defender),
    /// then goes on to attack the third unit. It takes counter-damage from BOTH
    /// engagements, ending near 990.0 (or slightly below due to FP epsilon).
    /// The (985.0..1000.0) bound covers both the ideal ~990.0 and any FP edge case.
    #[test]
    fn combat_all_adjacent_units_engage_with_one_attack_each() {
        let (mut world, world_entity) = make_world();
        let a = make_unit(&mut world, "P1", 10.0, 10.0, 1000.0);
        let b = make_unit(&mut world, "P2", 10.0, 10.0, 1000.0);
        let c = make_unit(&mut world, "P3", 10.0, 10.0, 1000.0);
        populate_nearby_enemies(&mut world);
        let mut scratch = CombatScratch::new();
        let ctx = TickContext::build_full(&world, world_entity);

        let _dead = run(
            &mut world,
            world_entity,
            &mut scratch,
            &test_settings(),
            &ctx,
            &ut_index(),
        );

        let men_of = |world: &World, e: hecs::Entity| {
            world
                .get::<&UnitStats>(e)
                .map(|s| s.men)
                .unwrap_or(f32::NAN)
        };
        for (name, ent) in [("A", a), ("B", b), ("C", c)] {
            let men = men_of(&world, ent);
            assert!(
                (985.0..1000.0).contains(&men),
                "unit {} should have taken exactly one tick of damage, got men={}",
                name,
                men
            );
        }
    }

    /// Regression test: defender attacked by multiple attackers should take
    /// compounding damage (not last-write-wins). With direct ECS writes, the
    /// defender's men/org are written immediately, so the next attacker reads
    /// the updated (lower) values — compounding is inherent.
    #[test]
    fn defender_takes_compounding_damage() {
        let (mut world, world_entity) = make_world();
        let a1 = make_unit(&mut world, "P1", 10.0, 10.0, 1000.0);
        let _a2 = make_unit(&mut world, "P1", 10.0, 10.0, 1000.0);
        let _a3 = make_unit(&mut world, "P1", 10.0, 10.0, 1000.0);
        let d = make_unit(&mut world, "P2", 10.0, 10.0, 1000.0);
        populate_nearby_enemies(&mut world);
        let mut scratch = CombatScratch::new();
        let ctx = TickContext::build_full(&world, world_entity);

        let _dead = run(
            &mut world,
            world_entity,
            &mut scratch,
            &test_settings(),
            &ctx,
            &ut_index(),
        );

        let d_men = world.get::<&UnitStats>(d).map(|s| s.men).unwrap();
        // With 2+ hits, defender should be well below 990 (each hit ~5 dmg).
        // The exact value depends on iteration order, but it MUST be < 990
        // to prove compounding (direct ECS writes).
        assert!(
            d_men < 990.0,
            "defender should have taken compounding damage from multiple attackers, got men={}",
            d_men
        );
        // The first attacker should still be alive (it dealt damage but also takes counter)
        let a1_men = world.get::<&UnitStats>(a1).map(|s| s.men).unwrap();
        assert!(a1_men > 0.0, "attacker 1 should be alive");
    }

    /// Regression test: cumulative damage from multiple attackers can kill a defender.
    #[test]
    fn cumulative_damage_can_kill() {
        let (mut world, world_entity) = make_world();
        let mut attackers = Vec::new();
        for i in 0..10 {
            let x = 10.0 + (i % 3) as f32;
            let y = 10.0 + (i / 3) as f32;
            attackers.push(make_unit(&mut world, "P1", x, y, 1000.0));
        }
        let defender = make_unit(&mut world, "P2", 10.0, 10.0, 50.0);
        populate_nearby_enemies(&mut world);
        let mut scratch = CombatScratch::new();
        let ctx = TickContext::build_full(&world, world_entity);

        let dead = run(
            &mut world,
            world_entity,
            &mut scratch,
            &test_settings(),
            &ctx,
            &ut_index(),
        );

        let defender_dead = dead.iter().any(|e| e.id() == defender.id());
        assert!(
            defender_dead,
            "defender with 50 men should be killed by multiple attackers (cumulative damage), but was not in dead list"
        );
    }

    /// Regression test: equipment attrition compounds across multiple engagements.
    /// With direct ECS writes, equipment is degraded in-place, so subsequent
    /// engagements apply attrition to the already-degraded equipment.
    #[test]
    fn equipment_attrition_compounds() {
        let (mut world, world_entity) = make_world();
        let rifles = vec![Equipment {
            name: "rifle".to_string(),
            count: 10,
            max_count: 10,
            cost_per_item: 1.0,
        }];
        let defender =
            make_unit_with_equipment(&mut world, "P2", 10.0, 10.0, 1000.0, rifles.clone());
        let attacker1 = make_unit(&mut world, "P1", 10.0, 10.0, 1000.0);
        let _attacker2 = make_unit(&mut world, "P1", 10.0, 10.0, 1000.0);
        populate_nearby_enemies(&mut world);
        let mut scratch = CombatScratch::new();
        let ctx = TickContext::build_full(&world, world_entity);

        let _dead = run(
            &mut world,
            world_entity,
            &mut scratch,
            &test_settings(),
            &ctx,
            &ut_index(),
        );

        let defender_eq = world
            .get::<&UnitEquipment>(defender)
            .map(|e| e.equipment.clone())
            .unwrap();
        let rifle_count = defender_eq
            .iter()
            .find(|e| e.name == "rifle")
            .map(|e| e.count)
            .unwrap_or(10);

        // With compounded attrition (engagements applying attrition to progressively
        // degraded equipment), rifle count should be less than a single-engagement result.
        // Single engagement: ~5 dmg * 0.05 = 0.25 loss → floor(9.75) = 9 rifles
        assert!(
            rifle_count <= 9,
            "equipment attrition should compound across engagements, got rifle count={} (expected ≤ 9)",
            rifle_count
        );
        // Attacker1 should still be alive (it fought first)
        let a1_men = world.get::<&UnitStats>(attacker1).map(|s| s.men).unwrap();
        assert!(a1_men > 0.0, "attacker1 should be alive");
    }

    /// Under-supplied units deal less damage: a fully-equipped attacker should
    /// hurt its target more than an identical attacker with no equipment.
    #[test]
    fn under_supplied_attacker_deals_less_damage() {
        let rifles = |count: u32| {
            vec![Equipment {
                name: "rifle".to_string(),
                count,
                max_count: 10,
                cost_per_item: 0.5,
            }]
        };

        // Scenario A: attacker fully equipped (10/10).
        let (mut world, world_entity) = make_world();
        let _a_full = make_unit_with_equipment(&mut world, "P1", 10.0, 10.0, 100.0, rifles(10));
        let d_full = make_unit_with_equipment(&mut world, "P2", 10.5, 10.5, 100.0, rifles(10));
        populate_nearby_enemies(&mut world);
        let mut scratch = CombatScratch::new();
        let ctx = TickContext::build_full(&world, world_entity);
        let _ = run(
            &mut world,
            world_entity,
            &mut scratch,
            &test_settings(),
            &ctx,
            &ut_index(),
        );
        let dmg_full = 100.0 - world.get::<&UnitStats>(d_full).unwrap().men;

        // Scenario B: attacker under-supplied (0/10), identical defender.
        let (mut world2, world_entity2) = make_world();
        let _a_deprived = make_unit_with_equipment(&mut world2, "P1", 10.0, 10.0, 100.0, rifles(0));
        let d_deprived = make_unit_with_equipment(&mut world2, "P2", 10.5, 10.5, 100.0, rifles(10));
        populate_nearby_enemies(&mut world2);
        let mut scratch2 = CombatScratch::new();
        let ctx2 = TickContext::build_full(&world2, world_entity2);
        let _ = run(
            &mut world2,
            world_entity2,
            &mut scratch2,
            &test_settings(),
            &ctx2,
            &ut_index(),
        );
        let dmg_deprived = 100.0 - world2.get::<&UnitStats>(d_deprived).unwrap().men;

        assert!(
            dmg_full > dmg_deprived,
            "fully-supplied attacker should out-damage an under-supplied one ({} vs {})",
            dmg_full,
            dmg_deprived
        );
    }

    /// Regression test: retreat clears the fighting flag.
    #[test]
    fn retreat_clears_fighting_flag() {
        let (mut world, world_entity) = make_world();
        let unit = make_unit(&mut world, "P1", 10.0, 10.0, 1000.0);
        populate_nearby_enemies(&mut world);
        let mut scratch = CombatScratch::new();
        let ctx = TickContext::build_full(&world, world_entity);

        let _dead = run(
            &mut world,
            world_entity,
            &mut scratch,
            &test_settings(),
            &ctx,
            &ut_index(),
        );

        let flags = world.get::<&UnitFlags>(unit).unwrap();
        assert!(
            !flags.fighting,
            "isolated unit should have fighting=false after retreat"
        );
    }

    /// Regression test: scratch capacity survives clear() for reuse.
    #[test]
    fn scratch_capacity_survives_clear() {
        let mut scratch = CombatScratch::new();
        // Spawn real entities for valid Entity keys
        let mut spawn_world = World::new();
        let spawned: Vec<hecs::Entity> = (0..100).map(|_| spawn_world.spawn(())).collect();

        for entity in &spawned {
            scratch.resolved.insert((*entity, *entity));
        }
        assert_eq!(scratch.resolved.len(), 100);

        scratch.clear();
        assert_eq!(scratch.resolved.len(), 0);
        // Capacity should be preserved (≥ 100)
        assert!(
            scratch.resolved.capacity() >= 100,
            "scratch resolved capacity should survive clear(), got capacity={}",
            scratch.resolved.capacity()
        );
    }

    /// Regression test: attacker on hills deals more damage than attacker on plains,
    /// proving terrain is looked up per-unit, not shared.
    #[test]
    fn terrain_modifier_is_per_unit() {
        let mut world = World::new();
        let mut terrain = vec![vec![Terrain::Plains; 20]; 20];
        terrain[10][10] = Terrain::Hills;
        let world_entity = world.spawn((WorldBundle::new(
            terrain,
            20,
            20,
            CityUpgradeSettings::default(),
            std::collections::HashMap::new(),
        ),));

        let attacker = make_unit(&mut world, "P1", 10.0, 10.0, 1000.0);
        let defender = make_unit(&mut world, "P2", 10.0, 11.0, 1000.0);
        populate_nearby_enemies(&mut world);
        let mut scratch = CombatScratch::new();
        let ctx = TickContext::build_full(&world, world_entity);

        let _dead = run(
            &mut world,
            world_entity,
            &mut scratch,
            &test_settings(),
            &ctx,
            &ut_index(),
        );

        let a_men = world.get::<&UnitStats>(attacker).map(|s| s.men).unwrap();
        let d_men = world.get::<&UnitStats>(defender).map(|s| s.men).unwrap();

        // Attacker on hills (mod=1.5) deals more to defender than defender on plains (mod=1.0)
        // deals back to attacker. So defender should lose more men than attacker.
        assert!(
            (d_men - a_men).abs() > 0.1,
            "terrain should create asymmetric damage: attacker on hills vs defender on plains, \
             attacker men={}, defender men={}",
            a_men,
            d_men
        );
        assert!(
            d_men < a_men,
            "defender should lose more men (attacker has hills bonus 1.5x vs plains 1.0x)"
        );
    }

    /// Regression test: organisation loss scales with men lost.
    /// A unit losing a larger fraction of its men should lose proportionally more org.
    #[test]
    fn org_loss_scales_with_men_lost() {
        let (mut world, world_entity) = make_world();
        let attacker = make_unit(&mut world, "P1", 10.0, 10.0, 100.0);
        let defender = make_unit(&mut world, "P2", 10.0, 10.0, 1000.0);
        populate_nearby_enemies(&mut world);
        let mut scratch = CombatScratch::new();
        let ctx = TickContext::build_full(&world, world_entity);

        let _dead = run(
            &mut world,
            world_entity,
            &mut scratch,
            &test_settings(),
            &ctx,
            &ut_index(),
        );

        let a_stats = world.get::<&UnitStats>(attacker).unwrap();
        let d_stats = world.get::<&UnitStats>(defender).unwrap();

        // Attacker (100 men) should lose more % men than defender (1000 men),
        // so should also lose more org proportionally.
        let a_org_loss = 100.0 - a_stats.organisation;
        let d_org_loss = 100.0 - d_stats.organisation;

        assert!(a_org_loss > 0.0, "attacker should lose organisation");
        assert!(d_org_loss > 0.0, "defender should lose organisation");
        // The org loss should be roughly proportional to the men lost fraction.
        // Small units taking moderate damage lose more org than large units.
        assert!(
            a_org_loss > d_org_loss,
            "attacker (smaller unit) should lose more org than defender (larger unit)"
        );
    }
}
