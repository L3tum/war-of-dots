//! MovementSystem — move units along paths with terrain speed.
//!
//! Uses two-phase pattern: query + collect changes → apply mutations in batch.

use crate::game::components::*;
use crate::game::constants::MIN_UNIT_DISTANCE_SQ;
use crate::game::game::{GameSettings, TickContext};
use crate::game::spatial::{self, GridIndexScratch};
use crate::game::unit::Terrain;
use crate::game::world_ext::WorldExt;
use hecs::Entity;
use hecs::World;
use std::sync::Arc;
use std::time::Instant;

/// Collected movement changes.
struct MovementChange {
    old_pos: UnitPosition,
    new_pos: UnitPosition,
    new_path: Option<UnitPath>,
    new_org: Option<f32>,
    moved: bool,
    /// When the unit reached its first waypoint, the apply phase pops the head
    /// of the path in place (avoids the per-waypoint SmallVec clone + O(path)
    /// remove(0) memmove in phase 1). Never set together with a Some(new_path).
    pop_head: bool,
}

/// Pre-allocated mover record for spatialty — `.clear()`'d each tick, capacity
/// preserved. Owned by `Game` and passed by mutable reference into `run()`.
///
/// Movement writes `(entity, pre-move pos, post-move pos)` for every unit that
/// actually moved. Spatialty consumes this directly — no marker component, no
/// ECS writes, no marker lifecycle.
#[derive(Default)]
pub struct MovementScratch {
    /// Units that moved this tick: (entity, pre-move pos, post-move pos).
    pub movers: Vec<(hecs::Entity, UnitPosition, UnitPosition)>,
    /// Per-unit computed changes, cleared and reused each tick (capacity
    /// preserved so steady-state ticks do not grow/allocate the Vec). Private
    /// because `MovementChange` is module-private. (Compile rule R1.)
    changes: Vec<(hecs::Entity, MovementChange)>,
    /// Pass-A gate decisions, parallel to `changes` (true = move allowed).
    /// Reused each tick; index i is only meaningful when `changes[i].moved`.
    gate_passed: Vec<bool>,
    /// W1-④ blocked-gate backoff slots, indexed by entity id (low 32 bits of
    /// `Entity::to_bits`): high 56 bits = entity generation, low 8 bits = ticks
    /// remaining. Grows as entity ids increase; steady-state ticks do not
    /// allocate. A reused id (post-despawn) has a bumped generation ⇒ clear.
    blocked_cd: Vec<u64>,
    /// Diagnostics counters (W1 sub-profiling; read via `Game::movement_diag`).
    pub diag_scans: usize,
    pub diag_blocked: usize,
    pub diag_noop_skipped: usize,
    /// Gate re-scans skipped because the unit was still in its backoff window.
    pub diag_cooldown: usize,
}

impl MovementScratch {
    /// Number of units with a non-empty path in the current change list.
    pub fn changes_len(&self) -> usize {
        self.changes.len()
    }
}

/// Ticks a gate-blocked unit skips re-scanning before retrying (W1-④).
/// Run 11w2m: 7 117 of 9 747 gate scans/tick were blocked units re-scanning the
/// same jam — a 3-tick backoff saves ~73 % of the gate work (≈ 0.23 ms/tick)
/// at a gameplay cost of at most 3 ticks of delay behind an impassable
/// front (the old behavior retried every tick and was blocked every tick).
const GATE_BACKOFF: u8 = 3;

/// Cap on the backoff vec's entity-id span. Live hecs ids are bounded by total
/// spawns (tens of thousands); the cap keeps a pathological id (e.g.
/// `Entity::DANGLING` = u32::MAX) from triggering a multi-GB resize. Entities
/// beyond the cap simply never get a backoff (correctness unaffected).
const GATE_BACKOFF_MAX_ID: usize = 1 << 20;

/// Returns `true` — and decrements — when `entity` is still in its blocked-gate
/// backoff window; `false` when the slot is clear to scan. Grows the vec as
/// entity ids increase (no shrink). Slot layout (u64): generation in the high
/// 56 bits, ticks remaining in the low 8 — full generation range, no packing
/// truncation. `Entity::to_bits()` is the public (id, generation) projection.
fn gate_cooldown_active(cd: &mut Vec<u64>, entity: hecs::Entity) -> bool {
    let bits = entity.to_bits().get();
    let idx = (bits & 0xFFFF_FFFF) as usize;
    if idx >= GATE_BACKOFF_MAX_ID {
        return false;
    }
    let gen = bits >> 32;
    if idx >= cd.len() {
        cd.resize(idx + 1, 0);
    }
    let slot = cd[idx];
    if slot >> 8 == gen && slot & 0xFF > 0 {
        cd[idx] = (slot & !0xFF) | ((slot & 0xFF) - 1);
        true
    } else {
        false
    }
}

/// Puts `entity` on its blocked-gate backoff window.
fn gate_cooldown_set(cd: &mut Vec<u64>, entity: hecs::Entity, ticks: u8) {
    let bits = entity.to_bits().get();
    let idx = (bits & 0xFFFF_FFFF) as usize;
    if idx >= GATE_BACKOFF_MAX_ID {
        return;
    }
    let gen = bits >> 32;
    if idx >= cd.len() {
        cd.resize(idx + 1, 0);
    }
    cd[idx] = (gen << 8) | u64::from(ticks);
}

/// Clears `entity`'s backoff slot once a scan passes (unit is unblocked).
fn gate_cooldown_clear(cd: &mut [u64], entity: hecs::Entity) {
    let bits = entity.to_bits().get();
    let idx = (bits & 0xFFFF_FFFF) as usize;
    if idx >= GATE_BACKOFF_MAX_ID {
        return;
    }
    let gen = bits >> 32;
    if idx < cd.len() {
        cd[idx] = gen << 8;
    }
}

/// Returns the number of units that moved this tick.
pub fn run(
    world: &mut World,
    world_entity: hecs::Entity,
    settings: &GameSettings,
    scratch: &mut MovementScratch,
    ctx: &TickContext,
    grid_scratch: &mut GridIndexScratch,
) -> usize {
    // Scratch is filled by the apply loop below; clear the previous tick's
    // records up front so a fresh population happens every tick.
    scratch.movers.clear();
    scratch.diag_scans = 0;
    scratch.diag_blocked = 0;
    scratch.diag_noop_skipped = 0;
    scratch.diag_cooldown = 0;

    // Per-phase timing (recorded into WorldBundle debug stats; no-op without
    // the debug-overlay feature). Surface with `--bench-detailed movement`.
    let t0 = Instant::now();

    // Clear moved flags from the previous tick. Movement owns the full
    // moved-flag lifecycle: clear at start, set for movers at end.
    for (_, flags) in world.query::<(Entity, &mut UnitFlags)>().iter() {
        flags.moved = false;
    }

    // Scoped Arc extraction: drops hecs::Ref borrow immediately, only clones terrain Arc (24 bytes)
    let (terrain, terrain_width) = match world.world_data(world_entity) {
        Some(wb) => (Arc::clone(&wb.terrain.grid), wb.terrain.width as usize),
        None => return 0,
    };

    let t1 = Instant::now();

    // Phase 1: query all components, compute movement changes into the reused
    // scratch buffer so steady-state ticks do not grow/allocate the Vec.
    scratch.changes.clear();
    scratch.changes.extend(
        world
            .query::<(
                Entity,
                &UnitPosition,
                &UnitPath,
                &UnitFlags,
                &UnitTypeId,
                &UnitStats,
                &UnitEquipment,
            )>()
            .iter()
            .filter_map(|(entity, pos, path, flags, utype, stats, equipment)| {
                if path.path.is_empty() || flags.on_water {
                    return None;
                }
                compute_movement(
                    *pos,
                    path,
                    utype,
                    stats,
                    equipment,
                    &terrain,
                    terrain_width,
                    settings,
                )
                .map(|change| (entity, change))
            }),
    );

    // Phase 2: apply changes.
    //
    // Occupancy (circle model): a unit may not move so that any *other* unit's
    // center comes within `MIN_UNIT_DISTANCE` of its target. The gate is checked
    // against a **pre-move** spatial grid built from everyone's current
    // positions. A blocked unit is skipped entirely — no position write, no
    // waypoint pop (it did not reach its waypoint), no mover record, no `moved`
    // flag, no org drain — and retries next tick. Because centers can't approach
    // closer than `MIN_UNIT_DISTANCE`, unit density is bounded, which keeps each
    // `for_each_within` spatial scan O(constant) (a 3×3 cell touch, at most a few
    // units per cell) — the same bound that keeps combat's target-acquisition
    // scan cheap.
    //
    // Soft equilibrium (documented): two units converging on the *same* point in
    // the *same* tick both pass the pre-move check (neither is there yet) and may
    // overlap for one tick; the next tick one is blocked and resolves. Harmless —
    // combat treats them as engaged regardless (dist < COMBAT_DISTANCE).
    let t2 = Instant::now();
    // Get map bounds for clamping
    let (map_w, map_h) = match world.world_data(world_entity) {
        Some(wb) => (wb.terrain.width as f32, wb.terrain.height as f32),
        None => return 0,
    };

    // Build the tick's ONE spatial grid (reused buffers, in place, no alloc at
    // steady state) from PRE-move positions. Three consumers:
    //  1. the occupancy gate below — needs the pre-move snapshot: blocking is
    //     decided against a frozen position set, so it never depends on apply
    //     order (the documented "soft equilibrium" for converging units).
    //  2. combat's mover scan (target acquisition) and 3. city capture, which
    //     read `grid_scratch.index` AFTER the writes below, so they see
    //     pre-move positions: a unit that moved this tick is still at its old
    //     position for them. Combat rescans every mover every tick, so a unit
    //     that just walked into range acquires/releases its target one tick
    //     later — accepted in exchange for dropping the former second
    //     (post-move) build, which cost ~0.21 ms/tick for that freshness.
    populate_grid_units(world, ctx, grid_scratch);
    grid_scratch.index = spatial::GridIndex::build(
        &grid_scratch.units,
        &mut grid_scratch.entries,
        &mut grid_scratch.cell_offsets,
        &mut grid_scratch.cell_sizes,
    );
    let pre_grid = grid_scratch.index;

    // W1 two-pass apply so the occupancy gate (read-only grid scans) is
    // measured separately from the ECS writes. Run 11w1 left p3_apply flat
    // after ①+②, so this split decides ③ (single-pass apply) vs ④ (gate work).
    // Pass order and per-entity order are identical to the old single loop, so
    // writes, the movers vec, and the blocked-skip are all bit-identical.
    let changes = &scratch.changes;
    let is_noop =
        |c: &MovementChange| !c.moved && c.new_path.is_none() && !c.pop_head && c.new_org.is_none();

    let t3a = Instant::now();
    // Pass A: occupancy gate — decides which movers proceed. Blocked units get
    // no writes at all and retry next tick.
    scratch.gate_passed.clear();
    scratch.gate_passed.resize(changes.len(), true);
    let mut applied_movers = 0usize;
    for (i, (entity, change)) in changes.iter().enumerate() {
        if is_noop(change) {
            // W1-①: a no-op change writes nothing — `moved == false` implies
            // `new_pos == old_pos` exactly (compute flagging rule), so with no
            // path change and no org drain there is nothing to apply.
            scratch.diag_noop_skipped += 1;
            continue;
        }
        if change.moved {
            let entity = *entity;
            // W1-④: a unit blocked at the gate is jammed behind another unit;
            // re-scanning the same target every tick while still stuck (the
            // 11w2m case: 7 117 blocked / 9 747 scans) is pure waste. Count
            // down its backoff once per tick and skip both the scan and the
            // apply; it retries after GATE_BACKOFF ticks.
            if gate_cooldown_active(&mut scratch.blocked_cd, entity) {
                scratch.gate_passed[i] = false;
                scratch.diag_cooldown += 1;
                continue;
            }
            // Gate against the pre-move grid: block if any *other* unit is
            // within MIN_UNIT_DISTANCE of the (clamped) target. Self-exclusion
            // is required — the grid holds this unit at its OLD position, a
            // small step away, which must not block the unit itself.
            scratch.diag_scans += 1;
            let target = UnitPosition {
                x: change.new_pos.x.clamp(0.0, map_w - 1.0),
                y: change.new_pos.y.clamp(0.0, map_h - 1.0),
            };
            let mut blocked = false;
            spatial::for_each_within(
                &pre_grid,
                grid_scratch,
                target,
                MIN_UNIT_DISTANCE_SQ,
                |other, _, _| {
                    if other != entity {
                        blocked = true;
                    }
                },
            );
            if blocked {
                scratch.gate_passed[i] = false;
                scratch.diag_blocked += 1;
                gate_cooldown_set(&mut scratch.blocked_cd, entity, GATE_BACKOFF);
            } else {
                gate_cooldown_clear(&mut scratch.blocked_cd, entity);
                applied_movers += 1;
            }
        }
    }
    let t3b = Instant::now();
    // Pass B: apply writes, same order as pass A.
    for (i, (entity, change)) in changes.iter().enumerate() {
        if is_noop(change) {
            continue;
        }
        let entity = *entity;
        if change.moved && !scratch.gate_passed[i] {
            // Blocked in pass A: skip the move entirely — no position write,
            // no waypoint pop, no mover record, no moved flag, no org drain.
            continue;
        }
        if change.moved {
            // Record this mover for spatialty: (entity, pre-move pos, post-move
            // pos). Spatialty uses these directly — no marker component, no
            // extra ECS writes.
            scratch
                .movers
                .push((entity, change.old_pos, change.new_pos));
        }
        // W1-②: ONE multi-component lookup instead of up to four per-entity
        // query_one_mut calls (position / path / stats / flags). The unit always
        // has all four (the phase-1 query proves presence); hecs permits one
        // &mut per distinct component type. Conditionals become field-level
        // writes; bit-identical results.
        if let Ok((mpos, mpath, mstats, mflags)) = world.query_one_mut::<(
            &mut UnitPosition,
            &mut UnitPath,
            &mut UnitStats,
            &mut UnitFlags,
        )>(entity)
        {
            // Clamp position to map bounds to prevent out-of-bounds access
            mpos.x = change.new_pos.x.clamp(0.0, map_w - 1.0);
            mpos.y = change.new_pos.y.clamp(0.0, map_h - 1.0);
            if let Some(new_path) = &change.new_path {
                *mpath = (*new_path).clone();
            } else if change.pop_head {
                // Reached first waypoint: pop the head of the path in place.
                // Guarded + asserted because compute only sets pop_head after
                // `path.first()?` succeeded and no other system writes
                // UnitPath mid-tick.
                debug_assert!(!mpath.path.is_empty());
                if !mpath.path.is_empty() {
                    mpath.path.remove(0);
                }
            }
            if let Some(new_org) = change.new_org {
                mstats.organisation = new_org;
            }
            if change.moved {
                mflags.moved = true;
            }
        }
    }
    let t4 = Instant::now();

    // (The former post-move grid build was dropped: the pre-move grid is the
    // tick's single grid — combat/city capture trade one tick of position
    // freshness for the ~0.21 ms/tick saved; see the build above.)

    // Record per-phase timings (keyed under "movement/..."; no-op if not
    // enabled or without the debug-overlay feature).
    let to_ms = |d: std::time::Duration| d.as_secs_f64() * 1000.0;
    world.record_detailed(world_entity, "movement/p1_setup", to_ms(t1 - t0));
    world.record_detailed(world_entity, "movement/p2_query", to_ms(t2 - t1));
    world.record_detailed(world_entity, "movement/p3_apply", to_ms(t4 - t2));
    world.record_detailed(world_entity, "movement/p3a_gate", to_ms(t3b - t3a));
    world.record_detailed(world_entity, "movement/p3b_write", to_ms(t4 - t3b));

    applied_movers
}

/// Repopulate `grid_scratch.units` with every living unit's
/// `(entity, position, owner_idx)`. `owner_idx` comes from the shared
/// `TickContext` compact-owner lookup. Units missing an owner entry are skipped
/// (all living units have one — `recruit_unit` inserts synchronously), matching
/// the prior behaviour.
/// Public so the microbenchmark fixtures (which run systems without the full
/// `Game` tick loop) can build the same grid movement would build.
#[doc(hidden)]
pub fn populate_grid_units(world: &World, ctx: &TickContext, grid_scratch: &mut GridIndexScratch) {
    grid_scratch.units.clear();
    grid_scratch
        .units
        .extend(
            world
                .query::<(Entity, &UnitPosition)>()
                .iter()
                .filter_map(|(e, pos)| {
                    debug_assert!(
                        ctx.owner_id_by_entity.contains_key(&e),
                        "entity {:?} missing from owner_id_by_entity — TickContext stale?",
                        e
                    );
                    let owner_id = ctx.owner_id_by_entity.get(&e).copied()?;
                    Some((e, *pos, owner_id))
                }),
        );
}

#[allow(clippy::too_many_arguments)] // Pure per-unit computation: pos/path/components + settings/settings
fn compute_movement(
    pos: UnitPosition,
    path: &UnitPath,
    utype: &UnitTypeId,
    stats: &UnitStats,
    equipment: &UnitEquipment,
    terrain: &Arc<Vec<Terrain>>,
    terrain_width: usize,
    settings: &GameSettings,
) -> Option<MovementChange> {
    let (nx, ny) = path.path.first()?;
    let dx = nx - pos.x;
    let dy = ny - pos.y;
    let dist = (dx * dx + dy * dy).sqrt();

    // Resolve the type's base speed by scanning the (few) unit types. A short
    // string compare (~2–4 ns for the handful of types) beats the old
    // `unit_types_index.get(&utype.name)` SipHash name→index probe (~15 ns, 55 %
    // of compute_movement's per-unit cost — see movement_compute_cost_breakdown).
    // Identical value + the same 1.0 fallback for an unknown type: the old
    // `unit_types_index` was exactly name→position in `settings.unit_types`.
    let unit_type_speed = settings
        .unit_types
        .iter()
        .find(|ut| ut.name == utype.name)
        .map_or(1.0, |ut| ut.speed);

    let current_terrain = terrain
        .get((pos.y as usize) * terrain_width + (pos.x as usize))
        .copied()
        .unwrap_or(Terrain::Plains);
    let mut speed =
        unit_speed_with_terrain(&current_terrain, settings.water_speed, unit_type_speed) / 10.0;

    // Overload penalty
    if stats.men > 0.0 {
        let total_weight: u32 = equipment.equipment.iter().map(|e| e.count).sum();
        let capacity = stats.men as u32;
        if total_weight > capacity {
            speed *= 1.0 / (total_weight as f32 / capacity as f32);
        }
    }

    let (new_pos, new_path, pop_head) = if dist < speed {
        // Reached waypoint: pop the head of the path in the apply phase (no
        // SmallVec copy here, no O(path) remove(0) memmove in phase 1).
        (UnitPosition { x: *nx, y: *ny }, None, true)
    } else {
        // Moving towards waypoint
        let ratio = speed / dist;
        (
            UnitPosition {
                x: pos.x + dx * ratio,
                y: pos.y + dy * ratio,
            },
            None,
            false,
        )
    };

    // Org drain from movement
    let new_org = if stats.organisation > 0.0 {
        Some((stats.organisation - 0.05).max(0.0))
    } else {
        None
    };

    // A unit is only a "mover" (drives spatialty re-computation) if its position
    // actually changed this tick. Reaching a waypoint the unit is already at
    // produces new_pos == pos and must NOT be flagged as movement.
    let actually_moved =
        (new_pos.x - pos.x).abs() > f32::EPSILON || (new_pos.y - pos.y).abs() > f32::EPSILON;

    Some(MovementChange {
        old_pos: pos,
        new_pos,
        new_path,
        new_org,
        moved: actually_moved,
        pop_head,
    })
}

fn unit_speed_with_terrain(terrain: &Terrain, water_speed: f32, unit_type_speed: f32) -> f32 {
    let terrain_mod = match terrain {
        Terrain::Hills => 0.7,
        Terrain::Forest => 0.8,
        Terrain::Water => water_speed / unit_type_speed.max(0.01),
        _ => 1.0,
    };
    unit_type_speed * terrain_mod
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    /// Build the name -> index lookup passed to compute_movement (mirrors the
    /// cached `Game.unit_types_index` that `run()` receives).
    fn ut_index(settings: &GameSettings) -> HashMap<String, usize> {
        settings
            .unit_types
            .iter()
            .enumerate()
            .map(|(i, ut)| (ut.name.clone(), i))
            .collect()
    }

    // ---------------------------------------------------------------------------
    // unit_speed_with_terrain
    // ---------------------------------------------------------------------------

    #[test]
    fn speed_plains_full_speed() {
        let speed = unit_speed_with_terrain(&Terrain::Plains, 0.5, 1.0);
        assert_eq!(speed, 1.0);
    }

    #[test]
    fn speed_hills_slower() {
        let speed = unit_speed_with_terrain(&Terrain::Hills, 0.5, 1.0);
        assert!((speed - 0.7).abs() < 0.001);
    }

    #[test]
    fn speed_forest_slightly_slower() {
        let speed = unit_speed_with_terrain(&Terrain::Forest, 0.5, 1.0);
        assert!((speed - 0.8).abs() < 0.001);
    }

    #[test]
    fn speed_mountain_full_speed() {
        let speed = unit_speed_with_terrain(&Terrain::Mountain, 0.5, 1.0);
        assert_eq!(speed, 1.0);
    }

    // ---------------------------------------------------------------------------
    // gate backoff (W1-④)
    // ---------------------------------------------------------------------------

    #[test]
    fn gate_backoff_counts_down_then_expires() {
        let mut cd: Vec<u64> = Vec::new();
        let e = hecs::Entity::from_bits(1u64 << 32).unwrap(); // id 0, gen 1
        assert!(!gate_cooldown_active(&mut cd, e), "fresh slot is clear");
        gate_cooldown_set(&mut cd, e, GATE_BACKOFF);
        for _ in 0..GATE_BACKOFF {
            assert!(gate_cooldown_active(&mut cd, e));
        }
        assert!(!gate_cooldown_active(&mut cd, e), "backoff expired");
    }

    #[test]
    fn gate_backoff_generation_mismatch_is_clear() {
        let mut cd: Vec<u64> = Vec::new();
        let e3 = hecs::Entity::from_bits((1u64 << 32) | 3).unwrap(); // id 3, gen 1
        let e3g2 = hecs::Entity::from_bits((2u64 << 32) | 3).unwrap(); // id 3, gen 2
        gate_cooldown_set(&mut cd, e3, GATE_BACKOFF);
        // Id reused by a new entity (generation bumped) ⇒ no stale backoff.
        assert!(!gate_cooldown_active(&mut cd, e3g2));
    }

    #[test]
    fn gate_backoff_clear_resets_window() {
        let mut cd: Vec<u64> = Vec::new();
        let e = hecs::Entity::from_bits((1u64 << 32) | 1).unwrap();
        gate_cooldown_set(&mut cd, e, GATE_BACKOFF);
        assert!(gate_cooldown_active(&mut cd, e));
        gate_cooldown_clear(&mut cd, e);
        assert!(
            !gate_cooldown_active(&mut cd, e),
            "clear ⇒ immediately retryable"
        );
    }

    #[test]
    fn gate_backoff_independent_per_id() {
        let mut cd: Vec<u64> = Vec::new();
        let e5 = hecs::Entity::from_bits(1u64 << 32 | 5).unwrap();
        let e6 = hecs::Entity::from_bits(1u64 << 32 | 6).unwrap();
        gate_cooldown_set(&mut cd, e5, GATE_BACKOFF);
        assert!(!gate_cooldown_active(&mut cd, e6), "other id unaffected");
    }

    #[test]
    fn gate_backoff_pathological_id_is_ignored() {
        // Entity::DANGLING has id = u32::MAX — beyond the cap, so it must be
        // treated as "no backoff" instead of resizing the vec to gigabytes.
        let mut cd: Vec<u64> = Vec::new();
        let d = hecs::Entity::DANGLING;
        gate_cooldown_set(&mut cd, d, GATE_BACKOFF);
        assert!(!gate_cooldown_active(&mut cd, d));
        assert!(cd.is_empty(), "no allocation for out-of-cap ids");
    }

    #[test]
    fn speed_water_uses_water_speed() {
        let water_speed = 0.5;
        let unit_speed = 1.0;
        let speed = unit_speed_with_terrain(&Terrain::Water, water_speed, unit_speed);
        assert!((speed - water_speed).abs() < 0.001);
    }

    #[test]
    fn speed_water_faster_unit() {
        let water_speed = 0.5;
        let unit_speed = 2.0;
        let speed = unit_speed_with_terrain(&Terrain::Water, water_speed, unit_speed);
        // water_speed / unit_speed * unit_speed = water_speed
        assert!((speed - water_speed).abs() < 0.001);
    }

    #[test]
    fn speed_custom_unit_type() {
        let speed = unit_speed_with_terrain(&Terrain::Plains, 0.5, 2.0);
        assert_eq!(speed, 2.0);

        let speed_hills = unit_speed_with_terrain(&Terrain::Hills, 0.5, 2.0);
        assert!((speed_hills - 1.4).abs() < 0.001); // 2.0 * 0.7
    }

    // ---------------------------------------------------------------------------
    // compute_movement
    // ---------------------------------------------------------------------------

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
            city_upgrade_settings: crate::config::CityUpgradeSettings::default(),
            water_speed: 0.5,
            influence: crate::game::game::InfluenceSettings::default(),
        }
    }

    fn test_terrain(width: u32, height: u32) -> Arc<Vec<Terrain>> {
        Arc::new(vec![Terrain::Plains; (width * height) as usize])
    }

    /// Fast micro-timing of `compute_movement`'s per-unit cost, to decide
    /// whether a specific sub-op (the name→index HashMap probe, the `sqrt`, or
    /// the equipment `iter().sum()`) is worth removing. Pure function, no world.
    /// Run: `cargo test --release movement_compute_cost -- --test-threads=1 --nocapture`
    #[test]
    fn movement_compute_cost_breakdown() {
        use std::hint::black_box;
        use std::time::Instant;

        let settings = test_settings();
        let ut_index = ut_index(&settings);
        let terrain = test_terrain(100, 100);
        let pos = UnitPosition { x: 1.0, y: 1.0 };
        let path = UnitPath {
            path: [(100.0, 100.0)].into_iter().collect(), // far ⇒ "moving" branch (sqrt)
        };
        let utype = UnitTypeId {
            name: "infantry".to_string(),
        };
        let stats = UnitStats {
            men: 1000.0,
            max_men: 1000.0,
            organisation: 100.0,
            max_organisation: 100.0,
        };
        // Realistic equipment: a couple of items (exercises iter().sum()).
        use crate::game::unit::Equipment;
        let equipment = UnitEquipment {
            equipment: vec![
                Equipment {
                    name: "a".to_string(),
                    count: 3,
                    max_count: 3,
                    cost_per_item: 1.0,
                },
                Equipment {
                    name: "b".to_string(),
                    count: 5,
                    max_count: 5,
                    cost_per_item: 1.0,
                },
            ],
            max_equipment: vec![],
            supply_buffer_percent: 10,
            supply_rate: 5,
            supply: 2000.0,
            max_supply: 2000.0,
        };

        const N: usize = 2_000_000;
        // Warm.
        for _ in 0..200_000 {
            black_box(compute_movement(
                pos, &path, &utype, &stats, &equipment, &terrain, 100, &settings,
            ));
        }

        let t0 = Instant::now();
        let mut sink = 0usize;
        for _ in 0..N {
            sink += compute_movement(
                pos, &path, &utype, &stats, &equipment, &terrain, 100, &settings,
            )
            .is_some() as usize;
        }
        let full = t0.elapsed() / N as u32;
        black_box(sink);

        // Isolate the name→index HashMap probe (the old approach, kept for reference).
        let t1 = Instant::now();
        let mut s1 = 0.0f32;
        for _ in 0..N {
            s1 += ut_index
                .get(&utype.name)
                .map_or(1.0, |&i| settings.unit_types[i].speed);
        }
        let probe = t1.elapsed() / N as u32;
        black_box(s1);

        // Isolate the sqrt.
        let t2 = Instant::now();
        let s2 = (0..N)
            .map(|_| (99.0f32 * 99.0f32 + 99.0f32 * 99.0f32).sqrt())
            .sum::<f32>();
        let sqrt = t2.elapsed() / N as u32;
        black_box(s2);

        eprintln!(
            "compute_movement = {:?}/unit   |   HashMap probe = {:?}/unit   |   sqrt = {:?}/unit",
            full, probe, sqrt
        );
        assert!(full > std::time::Duration::ZERO);
    }

    #[test]
    fn movement_no_path_returns_none() {
        let pos = UnitPosition { x: 5.0, y: 5.0 };
        let path = UnitPath::default();
        let utype = UnitTypeId {
            name: "infantry".to_string(),
        };
        let stats = UnitStats {
            men: 1000.0,
            max_men: 1000.0,
            organisation: 100.0,
            max_organisation: 100.0,
        };
        let equipment = UnitEquipment {
            equipment: vec![],
            max_equipment: vec![],
            supply_buffer_percent: 10,
            supply_rate: 5,
            supply: 2000.0,
            max_supply: 2000.0,
        };
        let terrain = test_terrain(20, 20);
        let settings = test_settings();

        let result = compute_movement(
            pos, &path, &utype, &stats, &equipment, &terrain, 20, &settings,
        );
        assert!(result.is_none());
    }

    #[test]
    fn movement_reaches_waypoint() {
        let pos = UnitPosition { x: 5.0, y: 5.0 };
        let path = UnitPath {
            path: [(5.05, 5.0)].into_iter().collect(), // very close
        };
        let utype = UnitTypeId {
            name: "infantry".to_string(),
        };
        let stats = UnitStats {
            men: 1000.0,
            max_men: 1000.0,
            organisation: 100.0,
            max_organisation: 100.0,
        };
        let equipment = UnitEquipment {
            equipment: vec![],
            max_equipment: vec![],
            supply_buffer_percent: 10,
            supply_rate: 5,
            supply: 2000.0,
            max_supply: 2000.0,
        };
        let terrain = test_terrain(20, 20);
        let settings = test_settings();

        let change = compute_movement(
            pos, &path, &utype, &stats, &equipment, &terrain, 20, &settings,
        );
        assert!(change.is_some());
        let change = change.unwrap();
        assert_eq!(change.new_pos.x, 5.05);
        assert_eq!(change.new_pos.y, 5.0);
        // Reaching a waypoint now pops the path head in the apply phase rather
        // than materializing a cloned path here.
        assert!(change.new_path.is_none());
        assert!(change.pop_head);
        assert!(change.moved);
    }

    #[test]
    fn movement_towards_waypoint() {
        let pos = UnitPosition { x: 0.0, y: 0.0 };
        let path = UnitPath {
            path: [(10.0, 0.0)].into_iter().collect(), // far away
        };
        let utype = UnitTypeId {
            name: "infantry".to_string(),
        };
        let stats = UnitStats {
            men: 1000.0,
            max_men: 1000.0,
            organisation: 100.0,
            max_organisation: 100.0,
        };
        let equipment = UnitEquipment {
            equipment: vec![],
            max_equipment: vec![],
            supply_buffer_percent: 10,
            supply_rate: 5,
            supply: 2000.0,
            max_supply: 2000.0,
        };
        let terrain = test_terrain(20, 20);
        let settings = test_settings();

        let change = compute_movement(
            pos, &path, &utype, &stats, &equipment, &terrain, 20, &settings,
        );
        assert!(change.is_some());
        let change = change.unwrap();
        // Speed is 1.0 / 10.0 = 0.1 on plains
        assert!(change.new_pos.x > 0.0);
        assert!(change.new_pos.x < 10.0);
        assert_eq!(change.new_pos.y, 0.0);
        // Path not consumed
        assert!(change.new_path.is_none());
    }

    #[test]
    fn movement_organisation_drains() {
        let pos = UnitPosition { x: 0.0, y: 0.0 };
        let path = UnitPath {
            path: [(10.0, 0.0)].into_iter().collect(),
        };
        let utype = UnitTypeId {
            name: "infantry".to_string(),
        };
        let stats = UnitStats {
            men: 1000.0,
            max_men: 1000.0,
            organisation: 50.0,
            max_organisation: 100.0,
        };
        let equipment = UnitEquipment {
            equipment: vec![],
            max_equipment: vec![],
            supply_buffer_percent: 10,
            supply_rate: 5,
            supply: 2000.0,
            max_supply: 2000.0,
        };
        let terrain = test_terrain(20, 20);
        let settings = test_settings();

        let change = compute_movement(
            pos, &path, &utype, &stats, &equipment, &terrain, 20, &settings,
        );
        assert!(change.is_some());
        let change = change.unwrap();
        assert!(change.new_org.is_some());
        assert!((change.new_org.unwrap() - 49.95).abs() < 0.001);
    }

    #[test]
    fn movement_zero_org_does_not_drain() {
        let pos = UnitPosition { x: 0.0, y: 0.0 };
        let path = UnitPath {
            path: [(10.0, 0.0)].into_iter().collect(),
        };
        let utype = UnitTypeId {
            name: "infantry".to_string(),
        };
        let stats = UnitStats {
            men: 1000.0,
            max_men: 1000.0,
            organisation: 0.0,
            max_organisation: 100.0,
        };
        let equipment = UnitEquipment {
            equipment: vec![],
            max_equipment: vec![],
            supply_buffer_percent: 10,
            supply_rate: 5,
            supply: 2000.0,
            max_supply: 2000.0,
        };
        let terrain = test_terrain(20, 20);
        let settings = test_settings();

        let change = compute_movement(
            pos, &path, &utype, &stats, &equipment, &terrain, 20, &settings,
        );
        assert!(change.is_some());
        let change = change.unwrap();
        assert!(change.new_org.is_none());
    }

    // ---------------------------------------------------------------------------
    // run() — occupancy gate (circle model, MIN_UNIT_DISTANCE)
    // ---------------------------------------------------------------------------

    /// Build a minimal hecs world + world_entity (all-Plains terrain) for
    /// `run()`-level tests (the gate lives in `run`, not `compute_movement`).
    fn make_run_world(width: u32, height: u32) -> (World, Entity) {
        let mut world = World::new();
        let mut funds = HashMap::new();
        funds.insert("player_0".to_string(), 1000);
        funds.insert("player_1".to_string(), 1000);
        let terrain = vec![vec![Terrain::Plains; height as usize]; width as usize];
        let world_entity = world.spawn_world(
            terrain,
            width,
            height,
            crate::config::CityUpgradeSettings::default(),
            funds,
            vec![],
        );
        (world, world_entity)
    }

    /// Spawn a unit; if `path` is non-empty, install it (replacing the default
    /// empty path, turning the unit into a mover). An empty path leaves the
    /// unit static (skipped by the movement query).
    fn spawn_mover(
        world: &mut World,
        utype: &crate::game::economy::UnitType,
        owner: &str,
        pos: (f32, f32),
        path: &[(f32, f32)],
    ) -> Entity {
        let e = world.spawn_unit(utype, owner, pos);
        if !path.is_empty() {
            // `insert` errors when the component already exists (spawn_unit made
            // an empty `UnitPath`), so overwrite in place instead.
            if let Ok((mpath,)) = world.query_one_mut::<(&mut UnitPath,)>(e) {
                mpath.path = path.iter().copied().collect();
            }
        }
        e
    }

    /// Run one movement tick on the harness; returns the applied-mover count.
    fn tick(
        world: &mut World,
        world_entity: Entity,
        settings: &GameSettings,
        ctx: &TickContext,
        grid_scratch: &mut GridIndexScratch,
    ) -> usize {
        let mut scratch = MovementScratch::default();
        run(
            world,
            world_entity,
            settings,
            &mut scratch,
            ctx,
            grid_scratch,
        )
    }

    /// A lone unit in empty space must move: the gate must NOT be tripped by
    /// the unit's own old position (self-exclusion) or an empty neighbour set.
    #[test]
    fn run_move_into_empty_space_succeeds() {
        let settings = test_settings();
        let utype = &settings.unit_types[0];
        let (mut world, we) = make_run_world(30, 30);
        let b = spawn_mover(&mut world, utype, "player_0", (5.0, 5.0), &[(15.0, 5.0)]);
        let ctx = TickContext::build_full(&world, we);
        let mut grid = GridIndexScratch::default();

        let movers = tick(&mut world, we, &settings, &ctx, &mut grid);

        assert_eq!(
            movers, 1,
            "lone unit in empty space must move (self not blocked)"
        );
        let (pos, flags) = world
            .query_one_mut::<(&UnitPosition, &UnitFlags)>(b)
            .unwrap();
        assert!(
            pos.x > 5.0,
            "unit must advance toward waypoint, got x={}",
            pos.x
        );
        assert!(flags.moved, "mover must set the moved flag");
    }

    /// A unit whose step would bring another unit's center within
    /// MIN_UNIT_DISTANCE (8) is blocked: no position write, no moved flag, and
    /// it is not counted as a mover.
    #[test]
    fn run_move_blocked_within_min_distance() {
        let settings = test_settings();
        let utype = &settings.unit_types[0];
        let (mut world, we) = make_run_world(30, 30);
        // A static unit at (10,10). B at (12,10) — already within 8 of A —
        // moving toward it. Any step toward A lands within MIN_UNIT_DISTANCE.
        let _a = spawn_mover(&mut world, utype, "player_1", (10.0, 10.0), &[]);
        let b = spawn_mover(&mut world, utype, "player_0", (12.0, 10.0), &[(10.0, 10.0)]);
        let ctx = TickContext::build_full(&world, we);
        let mut grid = GridIndexScratch::default();

        let movers = tick(&mut world, we, &settings, &ctx, &mut grid);

        assert_eq!(movers, 0, "B within MIN_UNIT_DISTANCE of A must be blocked");
        let (pos, flags, path) = world
            .query_one_mut::<(&UnitPosition, &UnitFlags, &UnitPath)>(b)
            .unwrap();
        assert_eq!(pos.x, 12.0, "blocked unit's x must be unchanged");
        assert_eq!(pos.y, 10.0, "blocked unit's y must be unchanged");
        assert!(!flags.moved, "blocked unit must not set the moved flag");
        assert_eq!(
            path.path.len(),
            1,
            "blocked unit must not pop its waypoint (it did not reach it)"
        );
    }

    /// A blocked unit retries next tick and moves once the blocking neighbour
    /// is gone (stable-shell / self-correcting equilibrium).
    #[test]
    fn run_blocked_unit_moves_after_neighbour_leaves() {
        let settings = test_settings();
        let utype = &settings.unit_types[0];
        let (mut world, we) = make_run_world(30, 30);
        let a = spawn_mover(&mut world, utype, "player_1", (10.0, 10.0), &[]);
        let b = spawn_mover(&mut world, utype, "player_0", (12.0, 10.0), &[(10.0, 10.0)]);
        let mut grid = GridIndexScratch::default();

        // Tick 1: A present → B blocked.
        let ctx = TickContext::build_full(&world, we);
        assert_eq!(
            tick(&mut world, we, &settings, &ctx, &mut grid),
            0,
            "B blocked while A is present"
        );
        let (pos,) = world.query_one_mut::<(&UnitPosition,)>(b).unwrap();
        assert_eq!(pos.x, 12.0, "B must not move while blocked");

        // A leaves (despawned) → B's retry succeeds on the next tick.
        world.despawn(a).unwrap();
        let ctx = TickContext::build_full(&world, we);
        assert_eq!(
            tick(&mut world, we, &settings, &ctx, &mut grid),
            1,
            "B moves once the blocker is gone"
        );
        let (pos,) = world.query_one_mut::<(&UnitPosition,)>(b).unwrap();
        assert!(
            pos.x < 12.0,
            "B must advance after the blocker is gone, got x={}",
            pos.x
        );
    }
}
