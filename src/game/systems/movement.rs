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
use std::collections::HashMap;
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
}

/// Returns the number of units that moved this tick.
pub fn run(
    world: &mut World,
    world_entity: hecs::Entity,
    settings: &GameSettings,
    scratch: &mut MovementScratch,
    unit_types_index: &HashMap<String, usize>,
    ctx: &TickContext,
    grid_scratch: &mut GridIndexScratch,
) -> usize {
    // Scratch is filled by the apply loop below; clear the previous tick's
    // records up front so a fresh population happens every tick.
    scratch.movers.clear();

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

    // Unit-type speed is resolved via the cached `unit_types_index` (name -> index
    // into `settings.unit_types`) built once per `Game` — no per-tick HashMap build.
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
                    unit_types_index,
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
    // closer than `MIN_UNIT_DISTANCE`, unit density is bounded, which is what
    // keeps every `NearbyEnemies` list O(constant).
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

    // Build the PRE-move spatial grid (reused buffers, in place, no alloc at
    // steady state). The gate queries it; nearby_enemies later consumes the
    // post-move grid built at the end of this function.
    populate_grid_units(world, ctx, grid_scratch);
    let pre_grid = spatial::GridIndex::build(
        &grid_scratch.units,
        &mut grid_scratch.entries,
        &mut grid_scratch.cell_offsets,
        &mut grid_scratch.cell_sizes,
    );

    // Only units that actually moved count as movers (drives spatialty).
    let mut applied_movers = 0usize;
    for (entity, change) in &scratch.changes {
        let entity = *entity;
        if change.moved {
            // Gate against the pre-move grid: block if any *other* unit is within
            // MIN_UNIT_DISTANCE of the (clamped) target. Self-exclusion is
            // required — the grid holds this unit at its OLD position, a small
            // step away, which must not block the unit itself.
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
                // Skip the move entirely: no position write, no waypoint pop, no
                // mover record, no moved flag. The unit stays put and retries next
                // tick (movement recomputes from the path every tick).
                continue;
            }
            applied_movers += 1;
            // Record this mover for spatialty: (entity, pre-move pos, post-move
            // pos). Spatialty uses these directly — no marker component, no
            // extra ECS writes.
            scratch
                .movers
                .push((entity, change.old_pos, change.new_pos));
        }
        if let Ok((mpos,)) = world.query_one_mut::<(&mut UnitPosition,)>(entity) {
            // Clamp position to map bounds to prevent out-of-bounds access
            mpos.x = change.new_pos.x.clamp(0.0, map_w - 1.0);
            mpos.y = change.new_pos.y.clamp(0.0, map_h - 1.0);
        }
        if let Some(new_path) = &change.new_path {
            if let Ok((mpath,)) = world.query_one_mut::<(&mut UnitPath,)>(entity) {
                *mpath = (*new_path).clone();
            }
        } else if change.pop_head {
            // Reached first waypoint: pop the head of the path in place. Guarded
            // + asserted because compute only sets pop_head after `path.first()?`
            // succeeded and no other system writes UnitPath mid-tick.
            if let Ok((mpath,)) = world.query_one_mut::<(&mut UnitPath,)>(entity) {
                debug_assert!(!mpath.path.is_empty());
                if !mpath.path.is_empty() {
                    mpath.path.remove(0);
                }
            }
        }
        if let Some(new_org) = change.new_org {
            if let Ok((mstats,)) = world.query_one_mut::<(&mut UnitStats,)>(entity) {
                mstats.organisation = new_org;
            }
        }
        if change.moved {
            if let Ok((mf,)) = world.query_one_mut::<(&mut UnitFlags,)>(entity) {
                mf.moved = true;
            }
        }
    }
    let t4 = Instant::now();

    // Build the POST-move spatial grid (reflecting applied positions) — this is
    // the grid `nearby_enemies`/`combat` consume this tick (fresh, post-move).
    populate_grid_units(world, ctx, grid_scratch);
    grid_scratch.index = spatial::GridIndex::build(
        &grid_scratch.units,
        &mut grid_scratch.entries,
        &mut grid_scratch.cell_offsets,
        &mut grid_scratch.cell_sizes,
    );
    let t5 = Instant::now();

    // Record per-phase timings (keyed under "movement/..."; no-op if not
    // enabled or without the debug-overlay feature).
    let to_ms = |d: std::time::Duration| d.as_secs_f64() * 1000.0;
    world.record_detailed(world_entity, "movement/p1_setup", to_ms(t1 - t0));
    world.record_detailed(world_entity, "movement/p2_query", to_ms(t2 - t1));
    world.record_detailed(world_entity, "movement/p3_apply", to_ms(t4 - t2));
    world.record_detailed(world_entity, "movement/p4_spatial", to_ms(t5 - t4));

    applied_movers
}

/// Repopulate `grid_scratch.units` with every living unit's
/// `(entity, position, owner_idx)`. `owner_idx` comes from the shared
/// `TickContext` compact-owner lookup. Units missing an owner entry are skipped
/// (all living units have one — `recruit_unit` inserts synchronously), matching
/// the prior `nearby_enemies` behaviour.
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

#[allow(clippy::too_many_arguments)] // Pure per-unit computation: pos/path/components + cached unit-type index/settings
fn compute_movement(
    pos: UnitPosition,
    path: &UnitPath,
    utype: &UnitTypeId,
    stats: &UnitStats,
    equipment: &UnitEquipment,
    terrain: &Arc<Vec<Terrain>>,
    terrain_width: usize,
    unit_types_index: &HashMap<String, usize>,
    settings: &GameSettings,
) -> Option<MovementChange> {
    let (nx, ny) = path.path.first()?;
    let dx = nx - pos.x;
    let dy = ny - pos.y;
    let dist = (dx * dx + dy * dy).sqrt();

    // O(1) lookup via the cached index into settings.unit_types (no per-tick map).
    let unit_type_speed = unit_types_index
        .get(&utype.name)
        .map(|&i| settings.unit_types[i].speed)
        .unwrap_or(1.0);

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
            pos,
            &path,
            &utype,
            &stats,
            &equipment,
            &terrain,
            20,
            &ut_index(&settings),
            &settings,
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
            pos,
            &path,
            &utype,
            &stats,
            &equipment,
            &terrain,
            20,
            &ut_index(&settings),
            &settings,
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
            pos,
            &path,
            &utype,
            &stats,
            &equipment,
            &terrain,
            20,
            &ut_index(&settings),
            &settings,
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
            pos,
            &path,
            &utype,
            &stats,
            &equipment,
            &terrain,
            20,
            &ut_index(&settings),
            &settings,
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
            pos,
            &path,
            &utype,
            &stats,
            &equipment,
            &terrain,
            20,
            &ut_index(&settings),
            &settings,
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
            &ut_index(settings),
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
