//! WaterTransitionSystem — handle embark/disembark timers.
//!
//! Uses two-phase pattern: query + collect changes → apply mutations in batch.

use crate::game::components::*;
use crate::game::unit::Terrain;
use crate::game::world_ext::WorldExt;
use hecs::World;

/// Collected changes for water transition.
#[derive(Clone)]
enum WaterChange {
    Timer {
        new_wt: UnitWaterTransition,
        new_flags: Option<UnitFlags>,
    },
    TerrainCheck {
        pos: (f32, f32),
        current_on_water: bool,
    },
}

pub fn run(world: &mut World, world_entity: hecs::Entity) {
    // Phase 1: query all components, compute changes
    let changes: Vec<(hecs::Entity, WaterChange)> = world
        .query::<(&UnitWaterTransition, &UnitFlags, &UnitPosition)>()
        .iter()
        .filter_map(|(entity, (wt, flags, pos))| compute_water_change(entity, wt, flags, pos))
        .collect();

    // Phase 2: apply changes
    for (entity, change) in changes {
        match change {
            WaterChange::Timer { new_wt, new_flags } => {
                if let Ok((mwt,)) = world.query_one_mut::<(&mut UnitWaterTransition,)>(entity) {
                    *mwt = new_wt;
                }
                if let Some(new_flags) = new_flags {
                    if let Ok((mf,)) = world.query_one_mut::<(&mut UnitFlags,)>(entity) {
                        *mf = new_flags;
                    }
                }
            }
            WaterChange::TerrainCheck {
                pos,
                current_on_water,
            } => {
                let current_terrain = world.terrain_at(world_entity, pos.0, pos.1);
                if current_terrain != Terrain::Water {
                    if let Ok((mf,)) = world.query_one_mut::<(&mut UnitFlags,)>(entity) {
                        mf.on_water = false;
                    }
                } else if !current_on_water {
                    if let Ok((mf,)) = world.query_one_mut::<(&mut UnitFlags,)>(entity) {
                        mf.on_water = true;
                    }
                }
            }
        }
    }
}

fn compute_water_change(
    entity: hecs::Entity,
    wt: &UnitWaterTransition,
    flags: &UnitFlags,
    pos: &UnitPosition,
) -> Option<(hecs::Entity, WaterChange)> {
    // Embark timer
    if wt.embarking {
        let ticks_left = wt.embarking_ticks_left.saturating_sub(1);
        let mut new_wt = (*wt).clone();
        new_wt.embarking_ticks_left = ticks_left;
        let new_flags = if ticks_left == 0 {
            new_wt.embarking = false;
            let mut nf = *flags;
            nf.on_water = true;
            Some(nf)
        } else {
            None
        };
        return Some((entity, WaterChange::Timer { new_wt, new_flags }));
    }

    // Disembark timer
    if wt.disembarking {
        let ticks_left = wt.disembarking_ticks_left.saturating_sub(1);
        let mut new_wt = (*wt).clone();
        new_wt.disembarking_ticks_left = ticks_left;
        let new_flags = if ticks_left == 0 {
            new_wt.disembarking = false;
            new_wt.boat_supply.clear();
            new_wt.max_boat_supply.clear();
            let mut nf = *flags;
            nf.on_water = false;
            Some(nf)
        } else {
            None
        };
        return Some((entity, WaterChange::Timer { new_wt, new_flags }));
    }

    // Terrain check: only if on_water flag might need updating
    Some((
        entity,
        WaterChange::TerrainCheck {
            pos: (*pos).as_tuple(),
            current_on_water: flags.on_water,
        },
    ))
}
