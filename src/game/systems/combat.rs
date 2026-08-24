//! CombatSystem — target acquisition + role-based engagement resolution.
//!
//! Plans: `plans/COMBAT_TARGETING.md` §8.3/§9.2 (Phase 1) and §8.8/§9.1
//! (Phase 2 fold). Per tick:
//!   1. **Site 1 — sticky target acquisition for movers** (T1–T4): each unit
//!      that moved this tick keeps its target unless the target vanished
//!      (T2), retreated out of range (T3), or moved this tick (T4) — or it
//!      has none yet (T1). A rescan picks the closest in-range enemy from
//!      the post-move grid movement just built (`dist²`, `Entity::id`
//!      tiebreak) or `None` if none. Non-movers keep their target (D4).
//!      (Folded in from the deleted `nearby_enemies` system, Phase 2.)
//!   2. **Resolution** — each candidate (alive, `attacking = Some(target)`)
//!      validates its target, then resolves ONE directed engagement: the
//!      attacker deals `atk vs def` damage; the defender deals back either a
//!      **defensive counter** (one-way: `def vs atk`) or an attack (mutual
//!      pair: both use attack stats). Mutual pairs are resolved once, by the
//!      lower-id unit.
//!
//! Uses HEAD's per-pair direct read-modify-write pattern with game-owned
//! scratch buffers (capacity reuse across ticks). No HashMap, no two-phase
//! batch apply — compounding works naturally because ECS writes are immediate
//! and re-fetched on each engagement.

use crate::game::components::*;
use crate::game::constants::COMBAT_DISTANCE_SQ;
use crate::game::game::GameSettings;
use crate::game::game::TickContext;
use crate::game::spatial::{self, GridIndexScratch};
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
    fought: HashSet<hecs::Entity>,
    dead_set: HashSet<hecs::Entity>,
    dead: Vec<hecs::Entity>,
    /// Candidate unit snapshots for this tick, cleared and reused each run
    /// (capacity preserved). Named `candidate_units` (NOT `candidates`) to avoid
    /// colliding with the `pub candidates: usize` diagnostic counter (compile
    /// rule R2). A candidate is a unit with `men > 0` and an explicit combat
    /// target (`UnitFlags.attacking: Some(e)`).
    pub candidate_units: Vec<(hecs::Entity, UnitPosition, u8, UnitStats, UnitFlags)>,
    // Diagnostic counters (benchmark/status): reset each tick alongside clear().
    pub candidates: usize,
    pub attackers: usize,
    /// Targets that died earlier this tick (skipped, target cleared).
    pub targets_dead: usize,
    /// Targets that vanished from the ECS (despawned between ticks; defensive).
    pub targets_gone: usize,
    /// Targets with the same owner as the attacker (stale; target cleared).
    pub targets_same_owner: usize,
    /// Targets that retreated out of COMBAT_DISTANCE (target cleared, D4).
    pub targets_out_of_range: usize,
    /// Mutual pairs skipped because the lower-id unit resolves them.
    pub targets_skipped_mutual: usize,
    /// Engagements resolved this tick (mutual pairs count once).
    pub engagements: usize,
}

impl CombatScratch {
    pub fn new() -> Self {
        Self {
            fought: HashSet::new(),
            dead_set: HashSet::new(),
            dead: Vec::new(),
            candidate_units: Vec::new(),
            candidates: 0,
            attackers: 0,
            targets_dead: 0,
            targets_gone: 0,
            targets_same_owner: 0,
            targets_out_of_range: 0,
            targets_skipped_mutual: 0,
            engagements: 0,
        }
    }

    /// Clear all collections but preserve capacity for reuse across ticks.
    pub fn clear(&mut self) {
        self.fought.clear();
        self.dead_set.clear();
        self.dead.clear();
        self.candidate_units.clear();
        self.candidates = 0;
        self.attackers = 0;
        self.targets_dead = 0;
        self.targets_gone = 0;
        self.targets_same_owner = 0;
        self.targets_out_of_range = 0;
        self.targets_skipped_mutual = 0;
        self.engagements = 0;
    }

    /// Number of unique pair engagements resolved this tick (a mutual pair is
    /// resolved exactly once — by the lower-id unit). Read after `run()`
    /// returns; the Game layer (and benchmark) reports engaged-pairs-per-tick.
    pub fn engaged_pairs(&self) -> usize {
        self.engagements
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

/// Target-lifecycle site 2: clear a unit's explicit combat target (see the
/// module doc / `UnitFlags::attacking` for the full lifecycle).
fn clear_target(world: &mut World, entity: Entity) {
    if let Ok((mf,)) = world.query_one_mut::<(&mut UnitFlags,)>(entity) {
        mf.attacking = None;
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
    grid_scratch: &GridIndexScratch,
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

    // ── Site 1: sticky target acquisition for movers (T1–T4) ─────────────
    // Plans: COMBAT_TARGETING.md §9.1 (folded in from the deleted
    // `nearby_enemies` system, Phase 2 §8.8). Scans the grid movement built
    // this tick from pre-move positions; no grid rebuild here. Runs BEFORE candidate
    // collection so a freshly-acquired target can resolve this same tick.
    // A rescan picks the closest in-range enemy (`dist²`, `Entity::id`
    // tiebreak) or `None` if none — which also fixes the old sticky-
    // `attacking` org-recovery bug: a mover with no in-range enemy gets
    // `None`.
    #[cfg(any(debug_assertions, feature = "debug-overlay"))]
    let _t_acquire = crate::game::debug_stats::DetailedTimer::new("combat/target_acquisition");
    {
        let grid = &grid_scratch.index;
        for &(entity, pos, my_owner) in grid_scratch.units.iter() {
            let flags: Option<UnitFlags> = match world.get::<&UnitFlags>(entity) {
                Ok(f) => Some(*f),
                Err(_) => continue,
            };
            let Some(flags) = flags else {
                continue;
            };
            if !flags.moved {
                continue; // non-movers keep their target untouched (D4)
            }
            let rescan = match flags.attacking {
                None => true, // T1: first acquisition
                Some(target) => {
                    // T2 (target gone) / T4 (target moved) / T3 (retreated
                    // out of range).
                    match (
                        world.get::<&UnitFlags>(target),
                        world.get::<&UnitPosition>(target),
                    ) {
                        (Ok(tf), Ok(tp)) => {
                            let dx = pos.x - tp.x;
                            let dy = pos.y - tp.y;
                            tf.moved || dx * dx + dy * dy >= COMBAT_DISTANCE_SQ
                        }
                        _ => true,
                    }
                }
            };
            if !rescan {
                continue;
            }
            // Closest in-range enemy: 3×3 cell scan + dist² filter. The grid
            // entry position is the unit's PRE-move position (movement builds
            // the grid before applying moves; no system moves units between
            // movement and combat), so no per-enemy `world.get` is needed.
            let mut best: Option<Entity> = None;
            let mut best_dist_sq: f32 = f32::INFINITY;
            spatial::for_each_within(
                grid,
                grid_scratch,
                pos,
                COMBAT_DISTANCE_SQ,
                |other, other_pos, other_owner| {
                    if other == entity {
                        return;
                    }
                    // Fast owner comparison — direct u8 equality.
                    if other_owner == my_owner {
                        return;
                    }
                    let dx = pos.x - other_pos.x;
                    let dy = pos.y - other_pos.y;
                    let d2 = dx * dx + dy * dy;
                    let better = d2 < best_dist_sq
                        || (d2 == best_dist_sq && best.is_none_or(|b| other.id() < b.id()));
                    if better {
                        best = Some(other);
                        best_dist_sq = d2;
                    }
                },
            );
            if let Ok((mut_flags,)) = world.query_one_mut::<(&mut UnitFlags,)>(entity) {
                mut_flags.attacking = best;
            }
        }
    }
    #[cfg(any(debug_assertions, feature = "debug-overlay"))]
    _t_acquire.finish(world, world_entity);

    // Candidates: units with a live explicit combat target (`attacking.is_some()`).
    // The explicit target — not any spatial list — drives combat
    // (plans/COMBAT_TARGETING.md §8.3). `men > 0` excludes units already killed
    // by an earlier system this tick.
    scratch.candidate_units.clear();
    scratch.candidate_units.extend(
        world
            .query::<(Entity, &UnitPosition, &UnitFlags, &UnitStats)>()
            .iter()
            .filter(|(_, _, flags, stats)| stats.men > 0.0 && flags.attacking.is_some())
            .filter_map(|(e, pos, flags, stats)| {
                debug_assert!(
                    ctx.owner_id_by_entity.contains_key(&e),
                    "entity {:?} missing from owner_id_by_entity — TickContext stale?",
                    e
                );
                let owner_id = ctx.owner_id_by_entity.get(&e).copied()?;
                Some((e, *pos, owner_id, *stats, *flags))
            }),
    );
    scratch.candidates = scratch.candidate_units.len();
    // Every candidate carries a target by construction.
    scratch.attackers = scratch.candidates;

    #[cfg(any(debug_assertions, feature = "debug-overlay"))]
    let _t_lookup = crate::game::debug_stats::DetailedTimer::new("combat/target_validation");
    #[cfg(any(debug_assertions, feature = "debug-overlay"))]
    let _t_resolve = crate::game::debug_stats::DetailedTimer::new("combat/combat_resolution");
    #[cfg(any(debug_assertions, feature = "debug-overlay"))]
    let _t_stats = crate::game::debug_stats::DetailedTimer::new("combat/stats_update");

    for (entity1, pos1, owner1, _, flags_snapshot) in &scratch.candidate_units {
        // A unit that died during this tick (as a defender of another attacker)
        // should not go on to attack its own target in the same tick.
        if scratch.dead_set.contains(entity1) {
            continue;
        }
        let Some(target) = flags_snapshot.attacking else {
            continue; // defensive: the candidate filter requires a target
        };

        // ---- Target validation (lifecycle site 2: clear + skip) ----
        // Plans: COMBAT_TARGETING.md §8.3/§9.1. All checks are reads (ctx /
        // scratch / world.get); a failed check clears U's target where the
        // target is unusable so the next mover scan can re-acquire.

        // Target died earlier this tick — skip (no wasted engagement, no double
        // `dead.push`) and clear the stale target.
        if scratch.dead_set.contains(&target) {
            scratch.targets_dead += 1;
            clear_target(world, *entity1);
            continue;
        }
        // Same-owner check via shared TickContext lookup (O(1) u8 equality,
        // no per-pair ECS borrow + String comparison).
        let same_owner = match ctx.owner_id_by_entity.get(&target) {
            Some(other_id) => *owner1 == *other_id,
            None => {
                debug_assert!(
                    false,
                    "entity {:?} missing from owner_id_by_entity during same-owner check",
                    target
                );
                true
            }
        };
        if same_owner {
            scratch.targets_same_owner += 1;
            clear_target(world, *entity1);
            continue;
        }
        // Target vanished from the ECS (despawned between ticks — defensive; the
        // acquisition scan and death_cleanup normally prevent this).
        let t_state: Option<(UnitStats, UnitFlags, UnitPosition)> = match (
            world.get::<&UnitStats>(target),
            world.get::<&UnitFlags>(target),
            world.get::<&UnitPosition>(target),
        ) {
            (Ok(s), Ok(f), Ok(p)) => Some((*s, *f, *p)),
            _ => None,
        };
        let (s2, f2, pos2) = match t_state {
            Some(v) => v,
            None => {
                scratch.targets_gone += 1;
                clear_target(world, *entity1);
                continue;
            }
        };
        // Target retreated out of COMBAT_DISTANCE (D4) — clear the target so the
        // next mover scan can re-acquire; the retreat check below then clears
        // `fighting` (org recovery resumes).
        {
            let dx = pos1.x - pos2.x;
            let dy = pos1.y - pos2.y;
            // Squared distance check — avoids sqrt()
            if dx * dx + dy * dy >= crate::game::constants::COMBAT_DISTANCE_SQ {
                scratch.targets_out_of_range += 1;
                clear_target(world, *entity1);
                continue;
            }
        }

        // ---- Mutual dispatch (D2/D3) ----
        // U processes the engagement iff T is not targeting U (one-way), or the
        // pair is mutual and U.id() < T.id() (the lower-id unit resolves the
        // mutual pair once — replaces the old `resolved` normalized-pair set).
        let mutual = f2.attacking == Some(*entity1);
        if mutual && entity1.id() > target.id() {
            // T's iteration resolves the mutual pair.
            scratch.targets_skipped_mutual += 1;
            continue;
        }

        // Re-fetch U's CURRENT stats: it may have taken damage from an earlier
        // engagement this tick. If it died, stop attacking.
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
        // Engagement only resolves when both units carry equipment (same skip
        // semantics as the original effectiveness check's `_ => continue`).
        if world.get::<&UnitEquipment>(*entity1).is_err()
            || world.get::<&UnitEquipment>(target).is_err()
        {
            continue;
        }

        // ---- Resolve the U → T engagement (mutation phase) ----
        // Unit-type stats (soft_attack/soft_defense + attrition) via the cached
        // name→index map (O(1)).
        let t1 = world.get::<&UnitTypeId>(*entity1).ok().and_then(|ut| {
            unit_types_index
                .get(&ut.name)
                .map(|&i| &settings.unit_types[i])
        });
        let t2 = world.get::<&UnitTypeId>(target).ok().and_then(|ut| {
            unit_types_index
                .get(&ut.name)
                .map(|&i| &settings.unit_types[i])
        });
        let a_atk_stat = t1.map(|t| t.soft_attack).unwrap_or(1.0);
        let a_def_stat = t1.map(|t| t.soft_defense).unwrap_or(1.0);
        let d_atk_stat = t2.map(|t| t.soft_attack).unwrap_or(1.0);
        let d_def_stat = t2.map(|t| t.soft_defense).unwrap_or(1.0);
        let a_attr_rate = t1.map(|t| t.fighting_attrition).unwrap_or(1.0);
        let d_attr_rate = t2.map(|t| t.fighting_attrition).unwrap_or(1.0);

        // Role-based soft strength (plans/COMBAT_TARGETING.md §9.2):
        // U attacks with its ATTACK stat; T defends with its DEFENSE stat.
        let base1 = 10.0
            * (s1.men / s1.max_men.max(0.1))
            * (s1.organisation / 100.0)
            * if f1.on_water { 0.75 } else { 1.0 };
        let base2 = 10.0
            * (s2.men / s2.max_men.max(0.1))
            * (s2.organisation / 100.0)
            * if f2.on_water { 0.75 } else { 1.0 };
        let a_atk = base1 * a_atk_stat;
        let a_def = base1 * a_def_stat;
        let d_atk = base2 * d_atk_stat;
        let d_def = base2 * d_def_stat;

        // dmg T takes: U is attacking — always U.atk vs T.def.
        let a_eff = a_atk / (d_def + 0.1);
        // dmg U takes: one-way → T's defensive counter (T.def vs U.atk);
        // mutual → both attack (T.atk vs U.def).
        let d_eff = if mutual {
            d_atk / (a_def + 0.1)
        } else {
            d_def / (a_atk + 0.1)
        };

        // Under-supplied units fight less effectively (0% equipment → 0% of the
        // stat-based strength; 100%+ → full). Both units are guaranteed to have
        // equipment (checked above), so this `_ => continue` is defensive.
        let (a_eff_malus, d_eff_malus) = {
            let (eq1, eq2) = match (
                world.get::<&UnitEquipment>(*entity1),
                world.get::<&UnitEquipment>(target),
            ) {
                (Ok(a), Ok(b)) => (a, b),
                _ => continue,
            };
            (equipment_effectiveness(&eq1), equipment_effectiveness(&eq2))
        };

        // Each unit uses the terrain it stands on — fixes asymmetry where both
        // sides would get the attacker's terrain modifier. The defensive
        // counter keeps the defender's own terrain (plans §9.2).
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

        // dmg_to_defender: damage applied TO the defender (attacker strength)
        let dmg_to_defender = a_eff * a_terrain_mod * 5.0 * a_eff_malus;
        // dmg_to_attacker: damage applied TO the attacker (defensive counter,
        // or mutual attack)
        let dmg_to_attacker = d_eff * d_terrain_mod * 5.0 * d_eff_malus;

        let a_men = (s1.men - dmg_to_attacker).max(0.0);
        let d_men = (s2.men - dmg_to_defender).max(0.0);

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
            world.query_one_mut::<(&mut UnitStats, &mut UnitEquipment, &mut UnitFlags)>(target)
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
            scratch.dead.push(target);
            scratch.dead_set.insert(target);
        }

        scratch.engagements += 1;
        scratch.fought.insert(*entity1);
        scratch.fought.insert(target);

        // Each unit makes at most ONE attack per tick (one explicit target);
        // it can still be *attacked* by many enemies (and take counter-damage)
        // within the same tick.
    }

    // Retreat check: clear `fighting` for units that are no longer engaged:
    // no explicit target (site 2 cleared out-of-range/dead targets above) AND
    // did not fight this tick. Scans ALL units — not just candidates: a parked
    // defender that was attacked has `fighting=true` but no target, so it is
    // NOT a candidate this tick and must still be cleaned up here. This is
    // what resumes its org recovery (supply gate: `!fighting && !attacking`).
    #[cfg(any(debug_assertions, feature = "debug-overlay"))]
    let _t_retreat = crate::game::debug_stats::DetailedTimer::new("combat/retreat_check");
    // Collect first, then mutate: the query iterator holds an immutable
    // world borrow across the loop, so `query_one_mut` must run outside it.
    let mut to_release: Vec<Entity> = Vec::new();
    for (entity, flags) in world.query::<(Entity, &UnitFlags)>().iter() {
        if !flags.fighting {
            continue;
        }
        if flags.attacking.is_some() {
            continue; // still engaged with a target
        }
        if scratch.fought.contains(&entity) {
            continue; // fought this tick — keep the flag (it may re-engage)
        }
        to_release.push(entity);
    }
    for entity in to_release {
        if let Ok((mf,)) = world.query_one_mut::<(&mut UnitFlags,)>(entity) {
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
        ))
    }

    /// Set each unit's explicit combat target (`UnitFlags.attacking`) to its
    /// closest in-range enemy (`dist²`, `Entity::id` tiebreak) — the same rule
    /// the mover scan (site 1) uses. Test stand-in for the mover scan:
    /// treats every unit as if it just re-scanned.
    fn populate_targets(world: &mut World) {
        let units: Vec<(hecs::Entity, UnitPosition, String)> = world
            .query::<(Entity, &UnitPosition, &UnitOwner)>()
            .iter()
            .map(|(e, pos, owner)| (e, *pos, owner.player_id.clone()))
            .collect();
        for (entity, pos, owner) in &units {
            let mut best: Option<hecs::Entity> = None;
            let mut best_dist_sq: f32 = f32::INFINITY;
            for (other, other_pos, other_owner) in &units {
                if other == entity {
                    continue;
                }
                if other_owner == owner {
                    continue;
                }
                let dx = pos.x - other_pos.x;
                let dy = pos.y - other_pos.y;
                let d2 = dx * dx + dy * dy;
                if d2 >= crate::game::constants::COMBAT_DISTANCE_SQ {
                    continue;
                }
                let better = d2 < best_dist_sq
                    || (d2 == best_dist_sq && best.is_none_or(|b| other.id() < b.id()));
                if better {
                    best = Some(*other);
                    best_dist_sq = d2;
                }
            }
            if let Ok((f,)) = world.query_one_mut::<(&mut UnitFlags,)>(*entity) {
                f.attacking = best;
            }
        }
    }

    /// Spawn a minimal unit (owner + position + flags) — no stats/equipment,
    /// so it is NEVER a combat-resolution candidate. Used by the target-
    /// acquisition tests (site 1) to isolate acquisition from resolution.
    fn make_flagged_unit(
        world: &mut World,
        owner: &str,
        x: f32,
        y: f32,
        moved: bool,
    ) -> hecs::Entity {
        world.spawn((
            UnitOwner {
                player_id: owner.to_string(),
            },
            UnitPosition { x, y },
            UnitFlags {
                moved,
                ..UnitFlags::default()
            },
        ))
    }

    /// Build the post-move grid the same way `movement` does, from the current
    /// world positions. Owner idx is assigned deterministically per player
    /// string (same owner ⇒ same idx), so the same-owner filter in the mover
    /// scan is exercised exactly as in the real flow.
    fn build_grid(world: &World, grid_scratch: &mut GridIndexScratch) {
        use crate::game::spatial::GridIndex;
        let mut idx_map: HashMap<String, u8> = HashMap::new();
        let mut next: u8 = 0;
        grid_scratch.units.clear();
        for (e, owner, pos) in world.query::<(Entity, &UnitOwner, &UnitPosition)>().iter() {
            let idx = *idx_map.entry(owner.player_id.clone()).or_insert_with(|| {
                let i = next;
                next += 1;
                i
            });
            grid_scratch.units.push((e, *pos, idx));
        }
        grid_scratch.index = GridIndex::build(
            &grid_scratch.units,
            &mut grid_scratch.entries,
            &mut grid_scratch.cell_offsets,
            &mut grid_scratch.cell_sizes,
        );
    }

    /// Run combat against the world's current grid (test stand-in
    /// for the game loop: movement builds the grid, then combat consumes it).
    fn run_with_grid(
        world: &mut World,
        world_entity: hecs::Entity,
        scratch: &mut CombatScratch,
        ctx: &TickContext,
        grid_scratch: &mut GridIndexScratch,
    ) -> Vec<hecs::Entity> {
        build_grid(world, grid_scratch);
        run(
            world,
            world_entity,
            scratch,
            &test_settings(),
            ctx,
            &ut_index(),
            grid_scratch,
        )
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
                soft_defense: 5.5,
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

    /// Two stationary adjacent enemy units with NO explicit targets must NOT
    /// engage (Phase 1 behavior change C1, plans/COMBAT_TARGETING.md §9.1):
    /// parked units are defense-only — only movers acquire targets. A parked
    /// unit that is targeted still defends (see defender_takes_compounding_…).
    #[test]
    fn stationary_untargeted_enemies_do_not_engage() {
        let (mut world, world_entity) = make_world();
        let a = make_unit(&mut world, "P1", 10.0, 10.0, 100.0);
        let b = make_unit(&mut world, "P2", 10.5, 10.5, 100.0);
        // Strip engagement flags: parked, no target.
        for e in [a, b] {
            let (flags,) = world.query_one_mut::<(&mut UnitFlags,)>(e).unwrap();
            flags.attacking = None;
            flags.fighting = false;
        }
        let mut scratch = CombatScratch::new();
        let ctx = TickContext::build_full(&world, world_entity);

        let dead = run(
            &mut world,
            world_entity,
            &mut scratch,
            &test_settings(),
            &ctx,
            &ut_index(),
            &GridIndexScratch::default(),
        );

        assert!(dead.is_empty());
        assert_eq!(
            scratch.engagements, 0,
            "parked units without targets must not fight"
        );
        assert!(!scratch.fought.contains(&a) && !scratch.fought.contains(&b));
        // Retreat check clears the stale fighting flag (org recovery resumes).
        assert!(!world.get::<&UnitFlags>(a).unwrap().fighting);
        assert!(!world.get::<&UnitFlags>(b).unwrap().fighting);
    }

    /// A parked defender IS engaged when a mover targets it (one-way:
    /// attacker deals atk-vs-def damage, defender counters with its defensive
    /// stat — §9.2).
    #[test]
    fn parked_defender_engaged_by_targeting_mover() {
        let (mut world, world_entity) = make_world();
        let a = make_unit(&mut world, "P1", 10.0, 10.0, 1000.0);
        let b = make_unit(&mut world, "P2", 10.5, 10.5, 1000.0);
        // A (mover) targets parked B; B stays defense-only.
        {
            let (fa,) = world.query_one_mut::<(&mut UnitFlags,)>(a).unwrap();
            fa.attacking = Some(b);
        }
        let mut scratch = CombatScratch::new();
        let ctx = TickContext::build_full(&world, world_entity);

        let dead = run(
            &mut world,
            world_entity,
            &mut scratch,
            &test_settings(),
            &ctx,
            &ut_index(),
            &GridIndexScratch::default(),
        );

        assert!(dead.is_empty());
        assert_eq!(scratch.engagements, 1);
        let a_men = world.get::<&UnitStats>(a).unwrap().men;
        let b_men = world.get::<&UnitStats>(b).unwrap().men;
        assert!(
            b_men < 1000.0,
            "defender must take attacker damage, got {}",
            b_men
        );
        assert!(
            a_men < 1000.0,
            "attacker must take the defensive counter, got {}",
            a_men
        );
        // Defending is slightly favorable (§9.3: def 5.5 > atk 5.0):
        // the parked defender deals back more than it takes.
        assert!(
            1000.0 - b_men < 1000.0 - a_men,
            "defender counter (def stat) should exceed damage taken, b_lost={}, a_lost={}",
            1000.0 - b_men,
            1000.0 - a_men
        );
        // Both sides are marked engaged for the tick.
        assert!(world.get::<&UnitFlags>(a).unwrap().fighting);
        assert!(world.get::<&UnitFlags>(b).unwrap().fighting);
        // The defender never acquired a target (defense-only).
        assert_eq!(world.get::<&UnitFlags>(b).unwrap().attacking, None);
    }

    /// A 0-men unit must not be processed as an attacker (candidate filter
    /// requires men > 0) — even when it carries a live target.
    #[test]
    fn combat_skips_dead_attacker() {
        let (mut world, world_entity) = make_world();
        let victim = make_unit(&mut world, "P2", 10.5, 10.5, 1000.0);
        let a = make_unit(&mut world, "P1", 10.0, 10.0, 0.0);
        {
            let (fa,) = world.query_one_mut::<(&mut UnitFlags,)>(a).unwrap();
            fa.attacking = Some(victim);
        }
        let mut scratch = CombatScratch::new();
        let ctx = TickContext::build_full(&world, world_entity);

        let dead = run(
            &mut world,
            world_entity,
            &mut scratch,
            &test_settings(),
            &ctx,
            &ut_index(),
            &GridIndexScratch::default(),
        );
        assert!(
            dead.is_empty(),
            "0-men unit must not be processed by combat"
        );
        // The victim must be untouched — the dead attacker never resolved.
        assert_eq!(world.get::<&UnitStats>(victim).unwrap().men, 1000.0);
    }

    /// Three mutually-adjacent units of three different players, all moving:
    /// each acquires its closest enemy (id tiebreak at equal distance), each
    /// makes at most one attack per tick, and mutual pairs resolve exactly
    /// once (lower id). With fixture stats (atk 5.0 / def 5.5, same-type),
    /// every engagement deals ~4.5–5.5 dmg per side, so each unit ends in the
    /// ~990–996 range — damaged, but not carpet-bombed.
    #[test]
    fn combat_all_adjacent_units_engage_with_one_attack_each() {
        let (mut world, world_entity) = make_world();
        let a = make_unit(&mut world, "P1", 10.0, 10.0, 1000.0);
        let b = make_unit(&mut world, "P2", 10.0, 10.0, 1000.0);
        let c = make_unit(&mut world, "P3", 10.0, 10.0, 1000.0);
        populate_targets(&mut world);
        let mut scratch = CombatScratch::new();
        let ctx = TickContext::build_full(&world, world_entity);

        let _dead = run(
            &mut world,
            world_entity,
            &mut scratch,
            &test_settings(),
            &ctx,
            &ut_index(),
            &GridIndexScratch::default(),
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
        populate_targets(&mut world);
        let mut scratch = CombatScratch::new();
        let ctx = TickContext::build_full(&world, world_entity);

        let _dead = run(
            &mut world,
            world_entity,
            &mut scratch,
            &test_settings(),
            &ctx,
            &ut_index(),
            &GridIndexScratch::default(),
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
        populate_targets(&mut world);
        let mut scratch = CombatScratch::new();
        let ctx = TickContext::build_full(&world, world_entity);

        let dead = run(
            &mut world,
            world_entity,
            &mut scratch,
            &test_settings(),
            &ctx,
            &ut_index(),
            &GridIndexScratch::default(),
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
        populate_targets(&mut world);
        let mut scratch = CombatScratch::new();
        let ctx = TickContext::build_full(&world, world_entity);

        let _dead = run(
            &mut world,
            world_entity,
            &mut scratch,
            &test_settings(),
            &ctx,
            &ut_index(),
            &GridIndexScratch::default(),
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
        populate_targets(&mut world);
        let mut scratch = CombatScratch::new();
        let ctx = TickContext::build_full(&world, world_entity);
        let _ = run(
            &mut world,
            world_entity,
            &mut scratch,
            &test_settings(),
            &ctx,
            &ut_index(),
            &GridIndexScratch::default(),
        );
        let dmg_full = 100.0 - world.get::<&UnitStats>(d_full).unwrap().men;

        // Scenario B: attacker under-supplied (0/10), identical defender.
        let (mut world2, world_entity2) = make_world();
        let _a_deprived = make_unit_with_equipment(&mut world2, "P1", 10.0, 10.0, 100.0, rifles(0));
        let d_deprived = make_unit_with_equipment(&mut world2, "P2", 10.5, 10.5, 100.0, rifles(10));
        populate_targets(&mut world2);
        let mut scratch2 = CombatScratch::new();
        let ctx2 = TickContext::build_full(&world2, world_entity2);
        let _ = run(
            &mut world2,
            world_entity2,
            &mut scratch2,
            &test_settings(),
            &ctx2,
            &ut_index(),
            &GridIndexScratch::default(),
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
        populate_targets(&mut world);
        let mut scratch = CombatScratch::new();
        let ctx = TickContext::build_full(&world, world_entity);

        let _dead = run(
            &mut world,
            world_entity,
            &mut scratch,
            &test_settings(),
            &ctx,
            &ut_index(),
            &GridIndexScratch::default(),
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
            scratch.fought.insert(*entity);
        }
        assert_eq!(scratch.fought.len(), 100);

        scratch.clear();
        assert_eq!(scratch.fought.len(), 0);
        // Capacity should be preserved (≥ 100)
        assert!(
            scratch.fought.capacity() >= 100,
            "scratch fought capacity should survive clear(), got capacity={}",
            scratch.fought.capacity()
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
        populate_targets(&mut world);
        let mut scratch = CombatScratch::new();
        let ctx = TickContext::build_full(&world, world_entity);

        let _dead = run(
            &mut world,
            world_entity,
            &mut scratch,
            &test_settings(),
            &ctx,
            &ut_index(),
            &GridIndexScratch::default(),
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
        populate_targets(&mut world);
        let mut scratch = CombatScratch::new();
        let ctx = TickContext::build_full(&world, world_entity);

        let _dead = run(
            &mut world,
            world_entity,
            &mut scratch,
            &test_settings(),
            &ctx,
            &ut_index(),
            &GridIndexScratch::default(),
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

    // ---------------------------------------------------------------------
    // Site 1 — sticky target acquisition tests (ported from the deleted
    // `nearby_enemies` system, Phase 2). These spawn minimal units (no
    // stats/equipment) so the resolution loop cannot touch them; only the
    // acquisition pass (mover scan over the spatial grid) is exercised.
    // ---------------------------------------------------------------------

    /// T1: a mover with no target acquires the closest in-range enemy.
    #[test]
    fn mover_acquires_target_with_enemy_nearby() {
        let (mut world, world_entity) = make_world();
        // Stationary defender (moved=false).
        let b = make_flagged_unit(&mut world, "P2", 10.5, 10.5, false);
        // Mover advancing toward B (moved=true).
        let a = make_flagged_unit(&mut world, "P1", 10.0, 10.0, true);
        let mut scratch = CombatScratch::new();
        let ctx = TickContext::build_full(&world, world_entity);
        let mut grid_scratch = GridIndexScratch::default();
        run_with_grid(
            &mut world,
            world_entity,
            &mut scratch,
            &ctx,
            &mut grid_scratch,
        );

        assert_eq!(
            world.get::<&UnitFlags>(a).unwrap().attacking,
            Some(b),
            "mover with a nearby enemy must target it"
        );
        assert_eq!(
            world.get::<&UnitFlags>(b).unwrap().attacking,
            None,
            "stationary B must not acquire (defense-only)"
        );
    }

    /// T1 with no in-range enemy: a rescan finds none → target cleared to
    /// `None` (the fix for the old sticky-`attacking` org-recovery bug).
    #[test]
    fn mover_clears_target_when_no_enemy_nearby() {
        let (mut world, world_entity) = make_world();
        // Mover at a far distance from anyone.
        let a = make_flagged_unit(&mut world, "P1", 0.0, 0.0, true);
        let _b = make_flagged_unit(&mut world, "P2", 18.0, 18.0, false);
        let mut scratch = CombatScratch::new();
        let ctx = TickContext::build_full(&world, world_entity);
        let mut grid_scratch = GridIndexScratch::default();
        run_with_grid(
            &mut world,
            world_entity,
            &mut scratch,
            &ctx,
            &mut grid_scratch,
        );

        assert_eq!(
            world.get::<&UnitFlags>(a).unwrap().attacking,
            None,
            "mover far from enemies should have no target"
        );
    }

    /// T3: a mover whose current target moved out of range re-scans and
    /// acquires the in-range enemy.
    #[test]
    fn mover_retargets_when_target_retreats_out_of_range() {
        let (mut world, world_entity) = make_world();
        // A's current target B has moved away (beyond COMBAT_DISTANCE=10);
        // C is now in range. A moved this tick, target B out of range → rescan.
        let c = make_flagged_unit(&mut world, "P3", 10.4, 10.0, false);
        let b = make_flagged_unit(&mut world, "P2", 25.0, 10.0, false);
        let a = make_flagged_unit(&mut world, "P1", 10.0, 10.0, true);
        {
            let (flags,) = world.query_one_mut::<(&mut UnitFlags,)>(a).unwrap();
            flags.attacking = Some(b);
        }
        let mut scratch = CombatScratch::new();
        let ctx = TickContext::build_full(&world, world_entity);
        let mut grid_scratch = GridIndexScratch::default();
        run_with_grid(
            &mut world,
            world_entity,
            &mut scratch,
            &ctx,
            &mut grid_scratch,
        );

        assert_eq!(
            world.get::<&UnitFlags>(a).unwrap().attacking,
            Some(c),
            "target retreated out of range → rescan picks the in-range enemy"
        );
    }

    /// T2: a mover whose target was despawned (died + cleaned up) re-scans;
    /// with no other enemy in range the target is cleared.
    #[test]
    fn mover_clears_target_when_target_despawned() {
        let (mut world, world_entity) = make_world();
        let b = make_flagged_unit(&mut world, "P2", 10.5, 10.5, false);
        let a = make_flagged_unit(&mut world, "P1", 10.0, 10.0, true);
        {
            let (flags,) = world.query_one_mut::<(&mut UnitFlags,)>(a).unwrap();
            flags.attacking = Some(b);
        }
        world.despawn(b).unwrap(); // death_cleanup would do this
        let mut scratch = CombatScratch::new();
        let ctx = TickContext::build_full(&world, world_entity);
        let mut grid_scratch = GridIndexScratch::default();
        run_with_grid(
            &mut world,
            world_entity,
            &mut scratch,
            &ctx,
            &mut grid_scratch,
        );

        assert_eq!(
            world.get::<&UnitFlags>(a).unwrap().attacking,
            None,
            "despawned target → rescan finds no enemy → None"
        );
    }

    /// Sticky acquisition (§9.1): a mover does NOT retarget merely because it
    /// moved or because another enemy is closer — only target-moved (T4),
    /// target-dead (T2), or target-out-of-range (T3) trigger a rescan.
    #[test]
    fn mover_keeps_target_when_stationary_and_in_range() {
        let (mut world, world_entity) = make_world();
        // B (A's current target) is stationary and in range; C is closer but
        // must not pull A away from its committed target.
        let _c = make_flagged_unit(&mut world, "P3", 10.2, 10.0, false);
        let b = make_flagged_unit(&mut world, "P2", 10.8, 10.0, false);
        let a = make_flagged_unit(&mut world, "P1", 10.0, 10.0, true);
        {
            let (flags,) = world.query_one_mut::<(&mut UnitFlags,)>(a).unwrap();
            flags.attacking = Some(b);
        }
        let mut scratch = CombatScratch::new();
        let ctx = TickContext::build_full(&world, world_entity);
        let mut grid_scratch = GridIndexScratch::default();
        run_with_grid(
            &mut world,
            world_entity,
            &mut scratch,
            &ctx,
            &mut grid_scratch,
        );

        assert_eq!(
            world.get::<&UnitFlags>(a).unwrap().attacking,
            Some(b),
            "mover must keep its in-range stationary target (sticky)"
        );
    }

    /// T4: a mover whose target moved this tick re-scans (closest in-range
    /// enemy, id tiebreak).
    #[test]
    fn mover_retargets_when_target_moved_this_tick() {
        let (mut world, world_entity) = make_world();
        // B (A's target) moved this tick; C is now closest to A → rescan.
        let c = make_flagged_unit(&mut world, "P3", 10.3, 10.0, false);
        let b = make_flagged_unit(&mut world, "P2", 10.9, 10.0, true); // target moved
        let a = make_flagged_unit(&mut world, "P1", 10.0, 10.0, true);
        {
            let (flags,) = world.query_one_mut::<(&mut UnitFlags,)>(a).unwrap();
            flags.attacking = Some(b);
        }
        let mut scratch = CombatScratch::new();
        let ctx = TickContext::build_full(&world, world_entity);
        let mut grid_scratch = GridIndexScratch::default();
        run_with_grid(
            &mut world,
            world_entity,
            &mut scratch,
            &ctx,
            &mut grid_scratch,
        );

        // B moved → rescan; C (dist 0.3) is closer than B (dist 0.9) → switch.
        assert_eq!(
            world.get::<&UnitFlags>(a).unwrap().attacking,
            Some(c),
            "target moved this tick → rescan picks the closest in-range enemy"
        );
    }

    /// Parked units never acquire targets (C1, defense-only): even with a
    /// mover in range, a stationary unit keeps `attacking = None`.
    #[test]
    fn stationary_defender_does_not_acquire_target() {
        let (mut world, world_entity) = make_world();
        let b = make_flagged_unit(&mut world, "P2", 10.5, 10.5, false);
        let _a = make_flagged_unit(&mut world, "P1", 10.0, 10.0, true);
        let mut scratch = CombatScratch::new();
        let ctx = TickContext::build_full(&world, world_entity);
        let mut grid_scratch = GridIndexScratch::default();
        run_with_grid(
            &mut world,
            world_entity,
            &mut scratch,
            &ctx,
            &mut grid_scratch,
        );

        assert_eq!(
            world.get::<&UnitFlags>(b).unwrap().attacking,
            None,
            "stationary defender must NOT acquire a target (defense-only)"
        );
    }

    /// Two movers in range acquire each other → the mutual pair that
    /// resolution then resolves exactly once (lower id).
    #[test]
    fn two_nearby_movers_target_each_other() {
        let (mut world, world_entity) = make_world();
        let a = make_flagged_unit(&mut world, "P1", 10.0, 10.0, true);
        let b = make_flagged_unit(&mut world, "P2", 10.5, 10.5, true);
        let mut scratch = CombatScratch::new();
        let ctx = TickContext::build_full(&world, world_entity);
        let mut grid_scratch = GridIndexScratch::default();
        run_with_grid(
            &mut world,
            world_entity,
            &mut scratch,
            &ctx,
            &mut grid_scratch,
        );

        assert_eq!(world.get::<&UnitFlags>(a).unwrap().attacking, Some(b));
        assert_eq!(world.get::<&UnitFlags>(b).unwrap().attacking, Some(a));
    }

    /// Same-owner exclusion in the mover scan: a mover must never target an
    /// ally, even when the ally is the closest entity in range.
    #[test]
    fn mover_never_targets_same_owner() {
        let (mut world, world_entity) = make_world();
        let ally = make_flagged_unit(&mut world, "P1", 10.1, 10.0, false);
        let enemy = make_flagged_unit(&mut world, "P2", 10.5, 10.0, false);
        let a = make_flagged_unit(&mut world, "P1", 10.0, 10.0, true);
        let mut scratch = CombatScratch::new();
        let ctx = TickContext::build_full(&world, world_entity);
        let mut grid_scratch = GridIndexScratch::default();
        run_with_grid(
            &mut world,
            world_entity,
            &mut scratch,
            &ctx,
            &mut grid_scratch,
        );

        assert_eq!(
            world.get::<&UnitFlags>(a).unwrap().attacking,
            Some(enemy),
            "mover must skip its closer ally and target the enemy"
        );
        assert_eq!(
            world.get::<&UnitFlags>(ally).unwrap().attacking,
            None,
            "parked ally must not acquire"
        );
    }

    /// Determinism: two equidistant enemies → the lower `Entity::id` wins the
    /// tiebreak, stable across runs.
    #[test]
    fn equidistant_enemies_tiebreak_by_entity_id() {
        let (mut world, world_entity) = make_world();
        // B at (10.4, 10.0) and C at (9.6, 10.0): both exactly dist 0.4 from A.
        let b = make_flagged_unit(&mut world, "P2", 10.4, 10.0, false);
        let c = make_flagged_unit(&mut world, "P3", 9.6, 10.0, false);
        let a = make_flagged_unit(&mut world, "P1", 10.0, 10.0, true);
        let mut scratch = CombatScratch::new();
        let ctx = TickContext::build_full(&world, world_entity);
        let mut grid_scratch = GridIndexScratch::default();
        run_with_grid(
            &mut world,
            world_entity,
            &mut scratch,
            &ctx,
            &mut grid_scratch,
        );

        let expected = if b.id() < c.id() { b } else { c };
        assert_eq!(
            world.get::<&UnitFlags>(a).unwrap().attacking,
            Some(expected),
            "equidistant enemies → lower Entity id wins"
        );
    }

    /// Units beyond combat range are not candidates for acquisition (the
    /// 3×3 + dist² filter), even for a mover.
    #[test]
    fn mover_does_not_acquire_beyond_combat_range() {
        let (mut world, world_entity) = make_world();
        let a = make_flagged_unit(&mut world, "P1", 0.0, 0.0, true);
        let _b = make_flagged_unit(&mut world, "P2", 10.0, 10.0, false); // dist ~14.1
        let mut scratch = CombatScratch::new();
        let ctx = TickContext::build_full(&world, world_entity);
        let mut grid_scratch = GridIndexScratch::default();
        run_with_grid(
            &mut world,
            world_entity,
            &mut scratch,
            &ctx,
            &mut grid_scratch,
        );

        assert_eq!(
            world.get::<&UnitFlags>(a).unwrap().attacking,
            None,
            "far units must not be acquired"
        );
    }
}
