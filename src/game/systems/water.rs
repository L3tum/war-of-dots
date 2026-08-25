//! WaterTransitionSystem — handle embark/disembark timers.
//!
//! Uses two-phase pattern: query + collect changes → apply mutations in batch.

use crate::game::components::*;
use crate::game::unit::Terrain;
use crate::game::world_ext::WorldExt;
use hecs::World;
use std::sync::Arc;

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

/// Grow-only scratch buffer owned by `Game`, reused each tick. The `changes`
/// field is module-private (`WaterChange` is private) so only `run` touches it;
/// `Game` merely passes `&mut WaterScratch`. (Compile rule R1.)
#[derive(Default)]
pub struct WaterScratch {
    changes: Vec<(hecs::Entity, WaterChange)>,
}

pub fn run(world: &mut World, world_entity: hecs::Entity, scratch: &mut WaterScratch) {
    // Phase 1: query all components, compute changes into the reused scratch
    // buffer so steady-state ticks do not grow/allocate the Vec.
    scratch.changes.clear();
    scratch.changes.extend(
        world
            .query::<(
                hecs::Entity,
                &UnitWaterTransition,
                &UnitFlags,
                &UnitPosition,
            )>()
            .iter()
            .filter_map(|(entity, wt, flags, pos)| compute_water_change(entity, wt, flags, pos)),
    );

    // Phase 2: apply changes.
    // Hoist one terrain lookup out of the loop: clone the terrain Arc (a cheap
    // refcount bump) and index the flat grid directly, instead of a full
    // `terrain_at` ECS `get::<&WorldBundle>()` per TerrainCheck entry (W-2).
    // The hecs::Ref is dropped immediately so the `query_one_mut` borrows below
    // stay legal. On the (invariant-impossible) missing-world case, an empty
    // grid yields Terrain::Plains for every index — exactly what terrain_at's
    // `unwrap_or(Terrain::Plains)` did, so behavior is bit-identical.
    let (terrain, terrain_width): (Arc<Vec<Terrain>>, usize) = match world.world_data(world_entity)
    {
        Some(wb) => (Arc::clone(&wb.terrain.grid), wb.terrain.width as usize),
        None => (Arc::new(Vec::new()), 0),
    };

    for (entity, change) in scratch.changes.iter() {
        let entity = *entity;
        match change {
            WaterChange::Timer { new_wt, new_flags } => {
                if let Ok((mwt,)) = world.query_one_mut::<(&mut UnitWaterTransition,)>(entity) {
                    *mwt = (*new_wt).clone();
                }
                if let Some(new_flags) = new_flags {
                    if let Ok((mf,)) = world.query_one_mut::<(&mut UnitFlags,)>(entity) {
                        *mf = *new_flags;
                    }
                }
            }
            WaterChange::TerrainCheck {
                pos,
                current_on_water,
            } => {
                let current_terrain = terrain
                    .get((pos.1 as usize) * terrain_width + (pos.0 as usize))
                    .copied()
                    .unwrap_or(Terrain::Plains);
                if current_terrain != Terrain::Water {
                    if let Ok((mf,)) = world.query_one_mut::<(&mut UnitFlags,)>(entity) {
                        mf.on_water = false;
                    }
                } else if !*current_on_water {
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

    // Terrain check: only when the unit moved since the last check (W-1).
    // A static unit's on_water flag is only ever written when the terrain
    // actually differs (apply phase above), so a non-mover's flag is already
    // correct and the per-tick terrain re-check is pure waste. `flags.moved`
    // reflects the previous tick's movement: water runs before movement in the
    // tick loop, and movement clears moved at its start and sets it for movers
    // at its end. on_water is initialized at spawn, so a just-recruited static
    // unit is correct too. (Units that are embarking/disembarking are handled
    // above and return before reaching this gate.)
    if !flags.moved {
        return None;
    }
    Some((
        entity,
        WaterChange::TerrainCheck {
            pos: (*pos).as_tuple(),
            current_on_water: flags.on_water,
        },
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::CityUpgradeSettings;
    use std::collections::HashMap;

    /// 4×4 map with a single water tile at (x=2, y=1).
    /// (Terrain input is x-major: `terrain[x][y]`, flattened to `flat[y*w + x]`.)
    fn world_with_water_tile() -> (World, hecs::Entity) {
        let mut terrain = vec![vec![Terrain::Plains; 4]; 4];
        terrain[2][1] = Terrain::Water;
        let mut world = World::new();
        let wb = WorldBundle::new(
            terrain,
            4,
            4,
            CityUpgradeSettings::default(),
            HashMap::new(),
        );
        let world_entity = world.spawn((wb,));
        (world, world_entity)
    }

    fn spawn_unit(world: &mut World, x: f32, y: f32, moved: bool, on_water: bool) -> hecs::Entity {
        world.spawn((
            UnitPosition { x, y },
            UnitFlags {
                moved,
                on_water,
                ..Default::default()
            },
            UnitWaterTransition::default(),
        ))
    }

    /// W-1: a unit that moved onto water last tick (`moved = true`,
    /// `on_water = false`) gets its flag corrected by the terrain check.
    #[test]
    fn moved_unit_onto_water_gets_flag_set() {
        let (mut world, we) = world_with_water_tile();
        let e = spawn_unit(&mut world, 2.0, 1.0, true, false);
        let mut scratch = WaterScratch::default();
        run(&mut world, we, &mut scratch);
        let flags = world.get::<&UnitFlags>(e).unwrap();
        assert!(flags.on_water, "moved unit on water must get on_water set");
    }

    /// W-1: a static unit (never moved) emits no terrain re-check, so its
    /// on_water flag set at spawn is stable across ticks — no per-tick
    /// re-read or writes, and no spurious transitions.
    #[test]
    fn static_unit_flag_is_stable_across_ticks() {
        let (mut world, we) = world_with_water_tile();
        // Spawn-init equivalent: a static unit sitting on the water tile.
        let e = spawn_unit(&mut world, 2.0, 1.0, false, true);
        let mut scratch = WaterScratch::default();
        for _ in 0..3 {
            run(&mut world, we, &mut scratch);
        }
        let flags = world.get::<&UnitFlags>(e).unwrap();
        assert!(flags.on_water, "static water unit must keep on_water");
        // No water transition timers were started on a static unit.
        let wt = world.get::<&UnitWaterTransition>(e).unwrap();
        assert!(!wt.embarking && !wt.disembarking);
    }

    /// W-1: a static unit on land stays `on_water = false` across ticks.
    #[test]
    fn static_land_unit_stays_off_water() {
        let (mut world, we) = world_with_water_tile();
        let e = spawn_unit(&mut world, 3.0, 3.0, false, false);
        let mut scratch = WaterScratch::default();
        for _ in 0..3 {
            run(&mut world, we, &mut scratch);
        }
        let flags = world.get::<&UnitFlags>(e).unwrap();
        assert!(!flags.on_water);
    }

    /// A unit moving from water back onto land (moved = true, on_water = true)
    /// has its flag cleared.
    #[test]
    fn moved_unit_off_water_gets_flag_cleared() {
        let (mut world, we) = world_with_water_tile();
        let e = spawn_unit(&mut world, 3.0, 3.0, true, true);
        let mut scratch = WaterScratch::default();
        run(&mut world, we, &mut scratch);
        let flags = world.get::<&UnitFlags>(e).unwrap();
        assert!(!flags.on_water, "moved unit on land must lose on_water");
    }
}
