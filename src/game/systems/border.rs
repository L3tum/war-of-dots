//! BorderSystem — update border influence grids per player.

use crate::game::components::*;
use crate::game::world_ext::WorldExt;
use hecs::World;
use std::collections::{HashMap, HashSet};
use std::sync::Arc;

pub fn run(world: &mut World, world_entity: hecs::Entity) {
    #[cfg(any(debug_assertions, feature = "debug-overlay"))]
    let _t_wdc = crate::game::debug_stats::DetailedTimer::new("border/world_data_clone");
    let wb = match world.world_data_clone(world_entity) {
        Some(wb) => wb,
        None => return,
    };
    #[cfg(any(debug_assertions, feature = "debug-overlay"))]
    _t_wdc.finish(world, world_entity);

    let (w, h) = (wb.terrain.width, wb.terrain.height);
    let border_radius: f32 = 5.0;

    #[cfg(any(debug_assertions, feature = "debug-overlay"))]
    let _t_pe = crate::game::debug_stats::DetailedTimer::new("border/player_enumeration");
    let players: Vec<String> = world
        .query::<(&UnitOwner,)>()
        .iter()
        .map(|(_, (owner,))| owner.player_id.clone())
        .collect::<HashSet<_>>()
        .into_iter()
        .collect();
    #[cfg(any(debug_assertions, feature = "debug-overlay"))]
    _t_pe.finish(world, world_entity);

    let mut grids = HashMap::new();

    for player_id in &players {
        #[cfg(any(debug_assertions, feature = "debug-overlay"))]
        let _t_ga = crate::game::debug_stats::DetailedTimer::new("border/grid_alloc");
        let mut influence = vec![vec![0u32; h as usize]; w as usize];
        #[cfg(any(debug_assertions, feature = "debug-overlay"))]
        _t_ga.finish(world, world_entity);

        #[cfg(any(debug_assertions, feature = "debug-overlay"))]
        let _t_uf = crate::game::debug_stats::DetailedTimer::new("border/unit_filter");
        let units: Vec<(f32, f32)> = world
            .query::<(&UnitOwner, &UnitPosition)>()
            .iter()
            .filter(|(_, (owner, _))| &owner.player_id == player_id)
            .map(|(_, (_, pos))| (*pos).as_tuple())
            .collect();
        #[cfg(any(debug_assertions, feature = "debug-overlay"))]
        _t_uf.finish(world, world_entity);

        #[cfg(any(debug_assertions, feature = "debug-overlay"))]
        let _t_ui = crate::game::debug_stats::DetailedTimer::new("border/unit_influence");
        for (ux, uy) in &units {
            let radius = border_radius as i32;
            for dx in -radius..=radius {
                for dy in -radius..=radius {
                    let x = *ux as i32 + dx;
                    let y = *uy as i32 + dy;
                    if x >= 0 && y >= 0 && x < w as i32 && y < h as i32 {
                        influence[y as usize][x as usize] += 1;
                    }
                }
            }
        }
        #[cfg(any(debug_assertions, feature = "debug-overlay"))]
        _t_ui.finish(world, world_entity);

        #[cfg(any(debug_assertions, feature = "debug-overlay"))]
        let _t_ci = crate::game::debug_stats::DetailedTimer::new("border/city_influence");
        for city in &wb.cities.cities {
            if city.owner.as_deref() == Some(player_id) {
                let (cx, cy) = city.position;
                let city_radius = (border_radius * 1.5) as i32;
                for dx in -city_radius..=city_radius {
                    for dy in -city_radius..=city_radius {
                        let x = cx as i32 + dx;
                        let y = cy as i32 + dy;
                        if x >= 0 && y >= 0 && x < w as i32 && y < h as i32 {
                            influence[y as usize][x as usize] += 3;
                        }
                    }
                }
            }
        }
        #[cfg(any(debug_assertions, feature = "debug-overlay"))]
        _t_ci.finish(world, world_entity);

        grids.insert(player_id.clone(), influence);
    }

    #[cfg(any(debug_assertions, feature = "debug-overlay"))]
    let _t_aw = crate::game::debug_stats::DetailedTimer::new("border/arc_wrap");
    if let Some(wb) = world.world_data_mut(world_entity) {
        wb.borders.grids = Arc::new(grids);
    }
    #[cfg(any(debug_assertions, feature = "debug-overlay"))]
    _t_aw.finish(world, world_entity);
}
