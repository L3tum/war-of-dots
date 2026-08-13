//! MovementSystem — move units along paths with terrain speed.
//!
//! Uses two-phase pattern: query + collect changes → apply mutations in batch.

use crate::game::components::*;
use crate::game::game::GameSettings;
#[cfg(feature = "perfect-movement")]
use crate::game::systems::spatial_hash::SpatialHash;
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
}

/// Returns the number of units that moved this tick.
pub fn run(
    world: &mut World,
    world_entity: hecs::Entity,
    settings: &GameSettings,
    scratch: &mut MovementScratch,
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
    let terrain = match world.world_data(world_entity) {
        Some(wb) => Arc::clone(&wb.terrain.grid),
        None => return 0,
    };

    // Precompute unit-type lookup (name -> speed) once per tick instead of a linear
    // scan per unit per tick.
    let unit_type_speeds: HashMap<&str, f32> = settings
        .unit_types
        .iter()
        .map(|ut| (ut.name.as_str(), ut.speed))
        .collect();
    let t1 = Instant::now();

    // Phase 1: query all components, compute movement changes
    let changes: Vec<(hecs::Entity, MovementChange)> = world
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
                &unit_type_speeds,
                settings,
            )
            .map(|change| (entity, change))
        })
        .collect();

    // Phase 2: apply changes
    // Only units that actually moved count as movers (drives spatialty).
    let mover_count = changes.iter().filter(|(_, c)| c.moved).count();
    let t2 = Instant::now();
    // Get map bounds for clamping
    let (map_w, map_h) = match world.world_data(world_entity) {
        Some(wb) => (wb.terrain.width as f32, wb.terrain.height as f32),
        None => return 0,
    };

    // ---------------------------------------------------------------------------
    // perfect-movement: build spatial hash + check segment-circle interception
    // ---------------------------------------------------------------------------
    #[cfg(feature = "perfect-movement")]
    let (spatial, positions_map): (SpatialHash, HashMap<hecs::Entity, UnitPosition>) = {
        let positions: Vec<(hecs::Entity, UnitPosition)> = world
            .query::<(Entity, &UnitPosition)>()
            .iter()
            .map(|(e, pos)| (e, *pos))
            .collect();
        let mut sh = SpatialHash::new();
        for (e, pos) in &positions {
            sh.insert(*e, pos.x, pos.y);
        }
        let map: HashMap<hecs::Entity, UnitPosition> = positions.into_iter().collect();
        (sh, map)
    };

    // (No marker lifecycle here anymore: movers are recorded into
    // `scratch.movers` inside the apply loop below and consumed by spatialty.)

    for (entity, change) in changes {
        // perfect-movement: interception check before applying
        #[cfg(feature = "perfect-movement")]
        let (effective_new_pos, set_attacking) = if change.moved {
            let old_pos = positions_map.get(&entity).copied();
            if let Some(old_pos) = old_pos {
                let nearby_ids = spatial.query_nearby(old_pos.x, old_pos.y);
                let nearby_pos: Vec<(hecs::Entity, UnitPosition)> = nearby_ids
                    .iter()
                    .filter_map(|&e| positions_map.get(&e).map(|p| (e, *p)))
                    .collect();
                let (clamped_pos, intercepted) =
                    check_segment_interception(old_pos, change.new_pos, &nearby_pos, entity);
                if intercepted {
                    (clamped_pos, true)
                } else {
                    (change.new_pos, false)
                }
            } else {
                (change.new_pos, false)
            }
        } else {
            (change.new_pos, false)
        };

        if change.moved {
            // Record this mover for spatialty: (entity, pre-move pos, post-move
            // pos). Spatialty uses these directly for hash remove/insert, so no
            // marker component is written to the ECS.
            scratch
                .movers
                .push((entity, change.old_pos, change.new_pos));
        }
        if let Ok((mpos,)) = world.query_one_mut::<(&mut UnitPosition,)>(entity) {
            #[cfg(feature = "perfect-movement")]
            {
                let final_pos = effective_new_pos;
                mpos.x = final_pos.x.clamp(0.0, map_w - 1.0);
                mpos.y = final_pos.y.clamp(0.0, map_h - 1.0);
            }
            #[cfg(not(feature = "perfect-movement"))]
            {
                // Clamp position to map bounds to prevent out-of-bounds access
                mpos.x = change.new_pos.x.clamp(0.0, map_w - 1.0);
                mpos.y = change.new_pos.y.clamp(0.0, map_h - 1.0);
            }
        }
        if let Some(new_path) = change.new_path {
            if let Ok((mpath,)) = world.query_one_mut::<(&mut UnitPath,)>(entity) {
                *mpath = new_path;
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
                #[cfg(feature = "perfect-movement")]
                if set_attacking {
                    mf.attacking = true;
                }
            }
        }
    }
    let t4 = Instant::now();

    // Record per-phase timings (keyed under "movement/..."; no-op if not
    // enabled or without the debug-overlay feature).
    let to_ms = |d: std::time::Duration| d.as_secs_f64() * 1000.0;
    world.record_detailed(world_entity, "movement/p1_setup", to_ms(t1 - t0));
    world.record_detailed(world_entity, "movement/p2_query", to_ms(t2 - t1));
    world.record_detailed(world_entity, "movement/p3_apply", to_ms(t4 - t2));

    mover_count
}

#[allow(clippy::too_many_arguments)] // Pure per-unit computation: pos/path/components + precomputed lookup/settings
fn compute_movement(
    pos: UnitPosition,
    path: &UnitPath,
    utype: &UnitTypeId,
    stats: &UnitStats,
    equipment: &UnitEquipment,
    terrain: &Arc<Vec<Vec<Terrain>>>,
    unit_type_speeds: &HashMap<&str, f32>,
    settings: &GameSettings,
) -> Option<MovementChange> {
    let (nx, ny) = path.path.first()?;
    let dx = nx - pos.x;
    let dy = ny - pos.y;
    let dist = (dx * dx + dy * dy).sqrt();

    // O(1) lookup instead of a linear scan over settings.unit_types per unit.
    let unit_type_speed = unit_type_speeds
        .get(utype.name.as_str())
        .copied()
        .unwrap_or(1.0);

    let current_terrain = terrain
        .get(pos.x as usize)
        .and_then(|r| r.get(pos.y as usize))
        .cloned()
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

    let (new_pos, new_path) = if dist < speed {
        // Reached waypoint
        let mut new_path_copy = (*path).clone();
        new_path_copy.path.remove(0);
        (UnitPosition { x: *nx, y: *ny }, Some(new_path_copy))
    } else {
        // Moving towards waypoint
        let ratio = speed / dist;
        (
            UnitPosition {
                x: pos.x + dx * ratio,
                y: pos.y + dy * ratio,
            },
            None,
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

/// Check if the movement segment intersects any enemy's combat circle.
/// Returns (clamped_position, was_intercepted).
///
/// Uses segment-circle distance: projects the enemy position onto the movement
/// segment and checks the closest-point distance against COMBAT_RANGE.
#[cfg(feature = "perfect-movement")]
fn check_segment_interception(
    from: UnitPosition,
    to: UnitPosition,
    nearby: &[(hecs::Entity, UnitPosition)],
    mover_entity: hecs::Entity,
) -> (UnitPosition, bool) {
    let mut best_t = f32::MAX;
    let mut best_pos = to;
    let mut intercepted = false;

    for &(entity, enemy_pos) in nearby {
        if entity == mover_entity {
            continue;
        }
        // Compute closest point on segment [from → to] to enemy_pos
        let dx = to.x - from.x;
        let dy = to.y - from.y;
        let seg_len_sq = dx * dx + dy * dy;
        if seg_len_sq < f32::EPSILON {
            continue;
        }
        let t = ((enemy_pos.x - from.x) * dx + (enemy_pos.y - from.y) * dy) / seg_len_sq;
        let t_clamped = t.clamp(0.0, 1.0);
        let closest_x = from.x + t_clamped * dx;
        let closest_y = from.y + t_clamped * dy;
        let dist_sq = (closest_x - enemy_pos.x).powi(2) + (closest_y - enemy_pos.y).powi(2);
        if dist_sq < crate::game::constants::COMBAT_RANGE_SQ {
            // Intercepted — clamp to this point if it's closer to `from`
            if t_clamped < best_t {
                best_t = t_clamped;
                best_pos = UnitPosition {
                    x: closest_x,
                    y: closest_y,
                };
                intercepted = true;
            }
        }
    }

    (best_pos, intercepted)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    /// Build the name -> speed lookup map passed to compute_movement (mirrors run()).
    fn speeds_map(settings: &GameSettings) -> HashMap<&str, f32> {
        settings
            .unit_types
            .iter()
            .map(|ut| (ut.name.as_str(), ut.speed))
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
        }
    }

    fn test_terrain(width: u32, height: u32) -> Arc<Vec<Vec<Terrain>>> {
        Arc::new(vec![vec![Terrain::Plains; height as usize]; width as usize])
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
            &speeds_map(&settings),
            &settings,
        );
        assert!(result.is_none());
    }

    #[test]
    fn movement_reaches_waypoint() {
        let pos = UnitPosition { x: 5.0, y: 5.0 };
        let path = UnitPath {
            path: vec![(5.05, 5.0)], // very close
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
            &speeds_map(&settings),
            &settings,
        );
        assert!(change.is_some());
        let change = change.unwrap();
        assert_eq!(change.new_pos.x, 5.05);
        assert_eq!(change.new_pos.y, 5.0);
        assert!(change.new_path.is_some());
        assert!(change.new_path.unwrap().path.is_empty());
        assert!(change.moved);
    }

    #[test]
    fn movement_towards_waypoint() {
        let pos = UnitPosition { x: 0.0, y: 0.0 };
        let path = UnitPath {
            path: vec![(10.0, 0.0)], // far away
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
            &speeds_map(&settings),
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
            path: vec![(10.0, 0.0)],
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
            &speeds_map(&settings),
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
            path: vec![(10.0, 0.0)],
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
            &speeds_map(&settings),
            &settings,
        );
        assert!(change.is_some());
        let change = change.unwrap();
        assert!(change.new_org.is_none());
    }
}
