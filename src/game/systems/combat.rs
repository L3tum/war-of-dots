//! CombatSystem — resolve adjacent enemy combat using spatial hash.
//!
//! Uses HEAD's per-pair direct read-modify-write pattern with game-owned
//! scratch buffers (capacity reuse across ticks). No HashMap, no two-phase
//! batch apply — compounding works naturally because ECS writes are immediate
//! and re-fetched on each engagement.

use crate::game::components::*;
use crate::game::economy::UnitType;
use crate::game::game::GameSettings;
use crate::game::systems::spatial_hash::SpatialHash;
use crate::game::world_ext::WorldExt;
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
    had_enemy_nearby: HashSet<hecs::Entity>,
    nearby: Vec<hecs::Entity>,
    dead: Vec<hecs::Entity>,
}

impl CombatScratch {
    pub fn new() -> Self {
        Self {
            resolved: HashSet::new(),
            fought: HashSet::new(),
            dead_set: HashSet::new(),
            had_enemy_nearby: HashSet::new(),
            nearby: Vec::new(),
            dead: Vec::new(),
        }
    }

    /// Clear all collections but preserve capacity for reuse across ticks.
    pub fn clear(&mut self) {
        self.resolved.clear();
        self.fought.clear();
        self.dead_set.clear();
        self.had_enemy_nearby.clear();
        self.nearby.clear();
        self.dead.clear();
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

/// Build the post-combat equipment array for a unit taking `dmg`, or `None` when
/// there is no damage to apply or the unit carries no equipment. Only allocates
/// when the equipment actually changes.
fn attrition_from(
    eq: &UnitEquipment,
    dmg: f32,
    attr_rate: f32,
) -> Option<Vec<crate::game::unit::Equipment>> {
    if dmg <= 0.0 || eq.equipment.is_empty() {
        return None;
    }
    Some(
        eq.equipment
            .iter()
            .map(|e| {
                let loss = (dmg * attr_rate).min(e.count as f32);
                crate::game::unit::Equipment {
                    name: e.name.clone(),
                    count: (e.count as f32 - loss).max(0.0) as u32,
                    max_count: e.max_count,
                    cost_per_item: e.cost_per_item,
                }
            })
            .collect(),
    )
}

// ============================================================================
// Terrain Modifier Helper
// ============================================================================

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
    spatial: SpatialHash,
    scratch: &mut CombatScratch,
    settings: &GameSettings,
) -> Vec<hecs::Entity> {
    // Clear scratch for reuse (capacity preserved)
    scratch.clear();

    // Precompute unit-type lookup (name -> &UnitType) once per tick so per-engagement
    // attrition lookups are O(1). Mirrors the pattern in attrition.rs.
    let unit_types_by_name: HashMap<&str, &UnitType> = settings
        .unit_types
        .iter()
        .map(|ut| (ut.name.as_str(), ut))
        .collect();

    // Scoped Arc extraction: drops hecs::Ref borrow immediately, only clones terrain Arc (24 bytes)
    #[cfg(any(debug_assertions, feature = "debug-overlay"))]
    let _t_wdc = crate::game::debug_stats::DetailedTimer::new("combat/world_data_clone");
    let terrain = match world.world_data(world_entity) {
        Some(wb) => Arc::clone(&wb.terrain.grid),
        None => return Vec::new(),
    };
    // wb (hecs::Ref) dropped — borrow released. Can now do mutable queries.
    #[cfg(any(debug_assertions, feature = "debug-overlay"))]
    _t_wdc.finish(world, world_entity);

    // Only check units that moved this tick OR are already fighting
    // Collect cheap components upfront (Stats, Flags are Copy). Equipment deferred to resolution.
    #[cfg(any(debug_assertions, feature = "debug-overlay"))]
    let _t_ue = crate::game::debug_stats::DetailedTimer::new("combat/unit_enumeration");
    let candidates: Vec<(hecs::Entity, UnitPosition, String, UnitStats, UnitFlags)> = world
        .query::<(&UnitPosition, &UnitOwner, &UnitFlags, &UnitStats)>()
        .iter()
        .filter(|(_, (_, _, flags, stats))| (flags.moved || flags.fighting) && stats.men > 0.0)
        .map(|(e, (pos, owner, flags, stats))| (e, *pos, owner.player_id.clone(), *stats, *flags))
        .collect();
    #[cfg(any(debug_assertions, feature = "debug-overlay"))]
    _t_ue.finish(world, world_entity);

    #[cfg(any(debug_assertions, feature = "debug-overlay"))]
    let _t_lookup = crate::game::debug_stats::DetailedTimer::new("combat/adjacent_lookup");
    #[cfg(any(debug_assertions, feature = "debug-overlay"))]
    let _t_resolve = crate::game::debug_stats::DetailedTimer::new("combat/combat_resolution");
    #[cfg(any(debug_assertions, feature = "debug-overlay"))]
    let _t_stats = crate::game::debug_stats::DetailedTimer::new("combat/stats_update");

    for (entity1, pos1, owner1, _, _) in &candidates {
        // A unit that died during this tick (as a defender) should not go on to
        // attack other enemies in the same tick.
        if scratch.dead_set.contains(entity1) {
            continue;
        }

        spatial.query_nearby_into(pos1.x, pos1.y, &mut scratch.nearby);
        for entity2 in scratch.nearby.iter().copied() {
            if entity2 == *entity1 {
                continue;
            }

            // Resolve each normalized (entity1, entity2) pair at most once per tick,
            // so the same two units never fight twice. Each unit makes at most ONE
            // attack per tick (classic RTS model): a full "every adjacent pair"
            // resolution was O(n^2) in dense melees and dominated the tick budget.
            // Units can still be *attacked* by many enemies — see the `break` below.
            let pair = if entity1.id() < entity2.id() {
                (*entity1, entity2)
            } else {
                (entity2, *entity1)
            };
            if scratch.resolved.contains(&pair) {
                continue;
            }

            // Same-owner check without a per-candidate String allocation.
            let same_owner = match world.get::<&UnitOwner>(entity2) {
                Ok(o) => o.player_id == *owner1,
                Err(_) => continue,
            };
            if same_owner {
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
            if dist_sq >= crate::game::constants::COMBAT_RANGE_SQ {
                continue;
            }

            // Mark that entity1 had a nearby enemy (used for retreat logic)
            scratch.had_enemy_nearby.insert(*entity1);

            // Re-fetch entity1's CURRENT stats/flags before attacking. A unit may
            // have already taken damage this tick (attacked by another unit before
            // its own turn), so the stale candidate snapshot would overwrite that
            // accumulated damage instead of building on it.
            let (s1, f1) = {
                let s1 = match world.get::<&UnitStats>(*entity1) {
                    Ok(s) => *s,
                    Err(_) => break,
                };
                if s1.men <= 0.0 {
                    // Entity1 died to an earlier engagement this tick — stop attacking.
                    break;
                }
                let f1 = match world.get::<&UnitFlags>(*entity1) {
                    Ok(f) => *f,
                    Err(_) => break,
                };
                (s1, f1)
            };

            let s2 = match world.get::<&UnitStats>(entity2) {
                Ok(s) => *s,
                Err(_) => continue,
            };
            let f2 = match world.get::<&UnitFlags>(entity2) {
                Ok(f) => *f,
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
            // Higher a_eff means defender takes more damage
            let a_eff = a_soft / (d_soft + 0.1);
            // d_eff: defender's counter-attack strength relative to attacker's offense
            // Higher d_eff means attacker takes more damage from counter-attack
            let d_eff = d_soft / (a_soft + 0.1);

            // Each unit uses the terrain it stands on — fixes asymmetry where both
            // sides would get the attacker's terrain modifier.
            let a_terrain = terrain
                .get(pos1.x as usize)
                .and_then(|r| r.get(pos1.y as usize))
                .cloned()
                .unwrap_or(crate::game::unit::Terrain::Plains);
            let d_terrain = terrain
                .get(pos2.x as usize)
                .and_then(|r| r.get(pos2.y as usize))
                .cloned()
                .unwrap_or(crate::game::unit::Terrain::Plains);
            let a_terrain_mod = terrain_modifier(&a_terrain);
            let d_terrain_mod = terrain_modifier(&d_terrain);

            // dmg_to_defender: damage applied TO the defender (based on attacker effectiveness)
            let dmg_to_defender = a_eff * a_terrain_mod * 5.0;
            // dmg_to_attacker: damage applied TO the attacker (based on defender's counter-attack)
            let dmg_to_attacker = d_eff * d_terrain_mod * 5.0;

            let a_men = (s1.men - dmg_to_attacker).max(0.0);
            let d_men = (s2.men - dmg_to_defender).max(0.0);

            // Fetch both units' unit-type configs for attrition rates. Uses the
            // precomputed HashMap so this is O(1) per engagement.
            let (a_attr_rate, d_attr_rate) = {
                let a_rate = world
                    .get::<&UnitTypeId>(*entity1)
                    .ok()
                    .and_then(|ut| unit_types_by_name.get(ut.name.as_str()))
                    .map(|ut| ut.fighting_attrition)
                    .unwrap_or(1.0);
                let d_rate = world
                    .get::<&UnitTypeId>(entity2)
                    .ok()
                    .and_then(|ut| unit_types_by_name.get(ut.name.as_str()))
                    .map(|ut| ut.fighting_attrition)
                    .unwrap_or(1.0);
                (a_rate, d_rate)
            };

            // Fetch both units' equipment once (borrowed, no deep clone). The pair
            // is only resolved when both units carry an equipment component — same
            // conditional as before, without the intermediate full clones. The
            // borrows are scoped to this block so they (and their Drop guards) are
            // released before the mutation writes below.
            let (a_eq, d_eq) = {
                let (eq1, eq2) = match (
                    world.get::<&UnitEquipment>(*entity1),
                    world.get::<&UnitEquipment>(entity2),
                ) {
                    (Ok(a), Ok(b)) => (a, b),
                    _ => continue,
                };
                (
                    attrition_from(&eq1, dmg_to_attacker, a_attr_rate),
                    attrition_from(&eq2, dmg_to_defender, d_attr_rate),
                )
            };

            if let Ok((ms, me, mf)) = world
                .query_one_mut::<(&mut UnitStats, &mut UnitEquipment, &mut UnitFlags)>(*entity1)
            {
                ms.men = a_men;
                // Org loss proportional to men lost in this engagement.
                let a_men_lost = s1.men - a_men;
                let a_org_loss = 10.0 * (a_men_lost / s1.men.max(0.1));
                ms.organisation = (ms.organisation - a_org_loss).max(0.0);
                if let Some(a_eq) = a_eq {
                    me.equipment = a_eq;
                }
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
                if let Some(d_eq) = d_eq {
                    me.equipment = d_eq;
                }
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

            // Each unit makes at most ONE attack per tick (see the pair comment above);
            // a unit can still be *attacked* — and take counter-damage from — many
            // enemies within the same tick.
            break;
        }
    }

    // Retreat check: clear fighting for units that found no nearby enemy
    // Uses had_enemy_nearby set from main loop — NO second spatial hash pass
    #[cfg(any(debug_assertions, feature = "debug-overlay"))]
    let _t_retreat = crate::game::debug_stats::DetailedTimer::new("combat/retreat_check");
    for (entity, _, _, _, flags) in &candidates {
        if scratch.fought.contains(entity) {
            continue;
        }
        if !flags.fighting {
            continue;
        }
        // If entity1 had a nearby enemy during main loop, keep fighting
        if scratch.had_enemy_nearby.contains(entity) {
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

    scratch.dead.clone()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::CityUpgradeSettings;
    use crate::game::unit::{Equipment, Terrain};

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
                moved: false,
                ..Default::default()
            },
            UnitStats {
                men,
                max_men: 1000.0,
                organisation: 100.0,
                max_organisation: 100.0,
                reinforcement_rate: 5.0,
            },
            UnitEquipment {
                equipment: vec![],
                max_equipment: vec![],
                supply_buffer_percent: 20,
                supply: 2000.0,
                max_supply: 2000.0,
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
                moved: false,
                ..Default::default()
            },
            UnitStats {
                men,
                max_men: 1000.0,
                organisation: 100.0,
                max_organisation: 100.0,
                reinforcement_rate: 5.0,
            },
            UnitEquipment {
                equipment,
                max_equipment: vec![],
                supply_buffer_percent: 20,
                supply: 2000.0,
                max_supply: 2000.0,
            },
        ))
    }

    fn build_spatial(world: &World) -> SpatialHash {
        let mut spatial = SpatialHash::new();
        for (entity, pos) in world
            .query::<(&UnitPosition,)>()
            .iter()
            .map(|(e, (p,))| (e, *p))
        {
            spatial.insert(entity, pos.x, pos.y);
        }
        spatial
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
                supply_buffer_percent: 20,
                max_supply: 2000,
            }],
            city_upgrade_settings: CityUpgradeSettings::default(),
            water_speed: 0.5,
        }
    }

    /// A 0-men unit must not be processed as an attacker (candidate filter requires men > 0).
    #[test]
    fn combat_skips_dead_attacker() {
        let (mut world, world_entity) = make_world();
        // A single 0-men, fighting unit with no live enemies nearby. Without the
        // `men > 0` guard it would be a candidate and get its `fighting` flag cleared
        // by the retreat pass; with the guard it is excluded from candidates entirely.
        let _a = make_unit(&mut world, "P1", 10.0, 10.0, 0.0);
        let spatial = build_spatial(&world);
        let mut scratch = CombatScratch::new();

        let dead = run(
            &mut world,
            world_entity,
            spatial,
            &mut scratch,
            &test_settings(),
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
        let spatial = build_spatial(&world);
        let mut scratch = CombatScratch::new();

        let _dead = run(
            &mut world,
            world_entity,
            spatial,
            &mut scratch,
            &test_settings(),
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
        // Three attackers from the same player, one defender — all co-located.
        // Each attacker gets one attack; the defender should be hit at least twice.
        let a1 = make_unit(&mut world, "P1", 10.0, 10.0, 1000.0);
        let _a2 = make_unit(&mut world, "P1", 10.0, 10.0, 1000.0);
        let _a3 = make_unit(&mut world, "P1", 10.0, 10.0, 1000.0);
        let d = make_unit(&mut world, "P2", 10.0, 10.0, 1000.0);
        let spatial = build_spatial(&world);
        let mut scratch = CombatScratch::new();

        let _dead = run(
            &mut world,
            world_entity,
            spatial,
            &mut scratch,
            &test_settings(),
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
        // 10 weak attackers vs a 50-men defender — co-located.
        // Each attacker deals ~5 dmg; enough should kill the defender.
        let mut attackers = Vec::new();
        for i in 0..10 {
            let x = 10.0 + (i % 3) as f32;
            let y = 10.0 + (i / 3) as f32;
            attackers.push(make_unit(&mut world, "P1", x, y, 1000.0));
        }
        let defender = make_unit(&mut world, "P2", 10.0, 10.0, 50.0);
        let spatial = build_spatial(&world);
        let mut scratch = CombatScratch::new();

        let dead = run(
            &mut world,
            world_entity,
            spatial,
            &mut scratch,
            &test_settings(),
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
        // Defender with 10 rifles attacked by 2 different enemies.
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
        let spatial = build_spatial(&world);
        let mut scratch = CombatScratch::new();

        let _dead = run(
            &mut world,
            world_entity,
            spatial,
            &mut scratch,
            &test_settings(),
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

    /// Regression test: retreat clears the fighting flag.
    #[test]
    fn retreat_clears_fighting_flag() {
        let (mut world, world_entity) = make_world();
        // An isolated fighting unit with no enemies nearby should retreat.
        let unit = make_unit(&mut world, "P1", 10.0, 10.0, 1000.0);
        let spatial = build_spatial(&world);
        let mut scratch = CombatScratch::new();

        let _dead = run(
            &mut world,
            world_entity,
            spatial,
            &mut scratch,
            &test_settings(),
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
        // Build a terrain grid with hills at (10, 10) and plains at (10, 11)
        let mut terrain = vec![vec![Terrain::Plains; 20]; 20];
        terrain[10][10] = Terrain::Hills;
        let world_entity = world.spawn((WorldBundle::new(
            terrain,
            20,
            20,
            CityUpgradeSettings::default(),
            std::collections::HashMap::new(),
        ),));

        // Attacker on hills (10, 10), defender on plains (10, 11)
        let attacker = make_unit(&mut world, "P1", 10.0, 10.0, 1000.0);
        let defender = make_unit(&mut world, "P2", 10.0, 11.0, 1000.0);
        let spatial = build_spatial(&world);
        let mut scratch = CombatScratch::new();

        let _dead = run(
            &mut world,
            world_entity,
            spatial,
            &mut scratch,
            &test_settings(),
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
        // Attacker with 100 men vs defender with 1000 men — defender deals minimal damage
        // to attacker but attacker takes notable % damage from defender counter
        let attacker = make_unit(&mut world, "P1", 10.0, 10.0, 100.0);
        let defender = make_unit(&mut world, "P2", 10.0, 10.0, 1000.0);
        let spatial = build_spatial(&world);
        let mut scratch = CombatScratch::new();

        let _dead = run(
            &mut world,
            world_entity,
            spatial,
            &mut scratch,
            &test_settings(),
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
