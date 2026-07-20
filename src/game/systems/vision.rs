//! VisionSystem — update vision grids per player.
//!
//! [DEAD CODE] This system was superseded by the merged influence system (city_capture + influence).
//! It is NOT called in the main tick loop. Kept for potential future use as a standalone vision
//! pass or debugging aid.

use crate::game::components::*;
use crate::game::unit::Terrain;
use crate::game::world_ext::WorldExt;
use hecs::World;
use std::sync::Arc;

pub fn run(world: &mut World, world_entity: hecs::Entity) {
    #[cfg(any(debug_assertions, feature = "debug-overlay"))]
    let _t_wdc = crate::game::debug_stats::DetailedTimer::new("vision/world_data_clone");
    let wb = match world.world_data_clone(world_entity) {
        Some(wb) => wb,
        None => return,
    };
    #[cfg(any(debug_assertions, feature = "debug-overlay"))]
    _t_wdc.finish(world, world_entity);

    let (w, h) = (wb.terrain.width, wb.terrain.height);
    let vision_radius = crate::game::constants::VISION_RADIUS;

    #[cfg(any(debug_assertions, feature = "debug-overlay"))]
    let _t_pe = crate::game::debug_stats::DetailedTimer::new("vision/player_enumeration");
    let players: Vec<String> = world
        .query::<(&UnitOwner,)>()
        .iter()
        .map(|(_, (owner,))| owner.player_id.clone())
        .collect::<std::collections::HashSet<_>>()
        .into_iter()
        .collect();
    #[cfg(any(debug_assertions, feature = "debug-overlay"))]
    _t_pe.finish(world, world_entity);

    let mut updated_grids = std::collections::HashMap::new();

    for player_id in &players {
        #[cfg(any(debug_assertions, feature = "debug-overlay"))]
        let _t_uf = crate::game::debug_stats::DetailedTimer::new("vision/unit_filter");
        let units: Vec<(f32, f32)> = world
            .query::<(&UnitOwner, &UnitPosition)>()
            .iter()
            .filter(|(_, (owner, _))| &owner.player_id == player_id)
            .map(|(_, (_, pos))| (*pos).as_tuple())
            .collect();
        #[cfg(any(debug_assertions, feature = "debug-overlay"))]
        _t_uf.finish(world, world_entity);

        #[cfg(any(debug_assertions, feature = "debug-overlay"))]
        let _t_ga = crate::game::debug_stats::DetailedTimer::new("vision/grid_alloc");
        let mut grid = vec![vec![false; h as usize]; w as usize];
        #[cfg(any(debug_assertions, feature = "debug-overlay"))]
        _t_ga.finish(world, world_entity);

        #[cfg(any(debug_assertions, feature = "debug-overlay"))]
        let _t_uv = crate::game::debug_stats::DetailedTimer::new("vision/unit_visibility");
        for (ux, uy) in &units {
            let terrain_mod = match wb.terrain.grid.get(*ux as usize).and_then(|r| r.get(*uy as usize)) {
                Some(Terrain::Hills) => 2.0,
                _ => 1.0,
            };
            let radius = (vision_radius * terrain_mod) as i32;
            for dx in -radius..=radius {
                for dy in -radius..=radius {
                    let x = *ux as i32 + dx;
                    let y = *uy as i32 + dy;
                    if x >= 0 && y >= 0 && x < w as i32 && y < h as i32 {
                        grid[y as usize][x as usize] = true;
                    }
                }
            }
        }
        #[cfg(any(debug_assertions, feature = "debug-overlay"))]
        _t_uv.finish(world, world_entity);

        #[cfg(any(debug_assertions, feature = "debug-overlay"))]
        let _t_cv = crate::game::debug_stats::DetailedTimer::new("vision/city_visibility");
        for city in &wb.cities.cities {
            if city.owner.as_deref() == Some(player_id) {
                let (cx, cy) = city.position;
                let city_radius = (vision_radius * crate::game::constants::CITY_VISION_MULTIPLIER) as i32;
                for dx in -city_radius..=city_radius {
                    for dy in -city_radius..=city_radius {
                        let x = cx as i32 + dx;
                        let y = cy as i32 + dy;
                        if x >= 0 && y >= 0 && x < w as i32 && y < h as i32 {
                            grid[y as usize][x as usize] = true;
                        }
                    }
                }
            }
        }
        #[cfg(any(debug_assertions, feature = "debug-overlay"))]
        _t_cv.finish(world, world_entity);

        updated_grids.insert(player_id.clone(), grid);
    }

    #[cfg(any(debug_assertions, feature = "debug-overlay"))]
    let _t_aw = crate::game::debug_stats::DetailedTimer::new("vision/arc_wrap");
    if let Some(wb) = world.world_data_mut(world_entity) {
        wb.vision.grids = Arc::new(updated_grids);
    }
    #[cfg(any(debug_assertions, feature = "debug-overlay"))]
    _t_aw.finish(world, world_entity);
}
