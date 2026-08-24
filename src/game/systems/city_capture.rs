//! CityCaptureSystem — capture cities when units stand on them.

use crate::game::components::*;
use crate::game::game::{PlayerGameStats, TickContext};
use crate::game::spatial::{self, GridIndexScratch};
use crate::game::world_ext::WorldExt;
use hecs::World;
use std::collections::{BTreeMap, HashMap};

/// Squared capture range (1.5^2) — avoids sqrt() per unit-city pair
const CAPTURE_RANGE_SQ: f32 = 2.25;

pub fn run(
    world: &mut World,
    world_entity: hecs::Entity,
    player_stats: &mut HashMap<String, PlayerGameStats>,
    ctx: &TickContext,
    grid_scratch: &GridIndexScratch,
) -> Vec<String> {
    // Candidate collection is a *city-outer* spatial scan, not the old
    // O(units × cities) double loop. Each city asks the spatial grid (built once
    // per tick by `movement` from pre-move positions, reused by `combat`) for
    // the handful of units within
    // `CAPTURE_RANGE_SQ` of its centre — cost O(cities × units_in_range) instead
    // of O(units × cities), which at 100 cities was ~1 M distance checks/tick.
    //
    // Cities are borrowed immutably (`world_data`) alongside the grid read —
    // both are shared borrows, so no `Vec<City>` clone is needed (that clone was
    // the system's dominant per-tick allocation at high city counts). The
    // mutation phase takes a separate `world_data_mut` borrow after the
    // immutable one is dropped.
    //
    // Capture semantics match the old loop: same squared-distance test (strict
    // `<`), same "last qualifying unit per city wins" reduce. The winner's
    // owner is recorded as a compact u8 player index during the scan (no String
    // clones in the hot path) and resolved to a `player_id` String only for the
    // cities that actually change hands (rare). `BTreeMap` keeps iteration
    // deterministic (sorted by city_id). Note: the per-city winner tie-break is
    // now "last in spatial entry order" rather than "last in ECS query order" —
    // both are deterministic for a given world state; they only diverge when two
    // *different* players' units contest the same city in the same tick.
    let city_ids_changed: Vec<(String, String)> = {
        let wb = match world.world_data(world_entity) {
            Some(wb) => wb,
            None => return Vec::new(),
        };
        let cities = &wb.cities.cities;
        let grid = &grid_scratch.index;
        // city_id -> winner's compact owner index (u8). Insert overwrites, so
        // the last qualifying unit per city wins.
        let mut changed_idx: BTreeMap<String, u8> = BTreeMap::new();
        for city in cities.iter() {
            // A city owned by a player with no units (absent from the index)
            // resolves to None — any player's unit may capture it, matching the
            // old Option<String> compare.
            let city_owner_idx = city
                .owner
                .as_ref()
                .and_then(|o| ctx.player_id_to_index.get(o).copied());
            let city_pos = UnitPosition {
                x: city.position.0,
                y: city.position.1,
            };
            let city_id = &city.id;
            spatial::for_each_within(
                grid,
                grid_scratch,
                city_pos,
                CAPTURE_RANGE_SQ,
                |_entity, _pos, unit_owner_idx| {
                    if city_owner_idx != Some(unit_owner_idx) {
                        changed_idx.insert(city_id.clone(), unit_owner_idx);
                    }
                },
            );
        }
        // wb (immutable Ref) dropped here.
        // Resolve winner owner index -> player_id String, only for changed
        // cities. `player_id_to_index` is tiny (one entry per player), so a
        // linear find per changed city is cheaper than building a reverse map
        // every tick.
        changed_idx
            .into_iter()
            .filter_map(|(city_id, owner_idx)| {
                let owner = ctx
                    .player_id_to_index
                    .iter()
                    .find(|(_, &i)| i == owner_idx)?
                    .0
                    .clone();
                Some((city_id, owner))
            })
            .collect()
    };

    // Apply city ownership changes in a single mutable borrow
    if !city_ids_changed.is_empty() {
        if let Some(wb) = world.world_data_mut(world_entity) {
            for (city_id, new_owner) in &city_ids_changed {
                if let Some(city) = wb.cities.cities.iter_mut().find(|c| c.id == *city_id) {
                    city.owner = Some(new_owner.clone());
                }
            }
        }
    }

    // Update player stats
    for (_, new_owner) in &city_ids_changed {
        if let Some(stats) = player_stats.get_mut(new_owner) {
            stats.cities_captured += 1;
        }
    }

    // Return the list of city IDs whose ownership changed
    city_ids_changed.into_iter().map(|(id, _)| id).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::game::economy::{default_unit_types, City};
    use crate::game::spatial::GridIndex;
    use crate::game::systems::movement::populate_grid_units;
    use crate::game::unit::Terrain;
    use crate::game::world_ext::WorldExt;

    /// Build a 30×30 world with the given cities (id, pos, owner) and units
    /// (owner, pos), plus a TickContext and the spatial grid the real
    /// tick loop hands to `run` (movement builds it pre-move; combat/city_capture reuse).
    fn setup(
        cities: &[(&str, (f32, f32), Option<&str>)],
        units: &[(&str, (f32, f32))],
    ) -> (World, hecs::Entity, TickContext, GridIndexScratch) {
        let mut world = World::new();
        let mut funds = HashMap::new();
        funds.insert("player_0".to_string(), 1000);
        funds.insert("player_1".to_string(), 1000);
        let city_vec: Vec<City> = cities
            .iter()
            .map(|(id, pos, owner)| {
                let mut c = City::new(id, *pos);
                c.owner = owner.map(|o| o.to_string());
                c
            })
            .collect();
        let terrain = vec![vec![Terrain::Plains; 30]; 30];
        let world_entity = world.spawn_world(
            terrain,
            30,
            30,
            crate::config::CityUpgradeSettings::default(),
            funds,
            city_vec,
        );
        let ut = &default_unit_types()[0];
        for (owner, pos) in units {
            world.spawn_unit(ut, owner, *pos);
        }
        let ctx = TickContext::build_full(&world, world_entity);
        let mut grid = GridIndexScratch::default();
        populate_grid_units(&world, &ctx, &mut grid);
        grid.index = GridIndex::build(
            &grid.units,
            &mut grid.entries,
            &mut grid.cell_offsets,
            &mut grid.cell_sizes,
        );
        (world, world_entity, ctx, grid)
    }

    fn stats() -> HashMap<String, PlayerGameStats> {
        let mut m = HashMap::new();
        m.insert("player_0".to_string(), Default::default());
        m.insert("player_1".to_string(), Default::default());
        m
    }

    fn owner_of(world: &World, we: hecs::Entity) -> Option<String> {
        world.world_data(we).unwrap().cities.cities[0].owner.clone()
    }

    #[test]
    fn unit_on_neutral_city_captures_it() {
        let (mut world, we, ctx, grid) = setup(
            &[("city_a", (10.0, 10.0), None)],
            &[("player_0", (10.2, 10.2))],
        );
        let mut ps = stats();
        let changed = run(&mut world, we, &mut ps, &ctx, &grid);
        assert_eq!(changed, vec!["city_a".to_string()]);
        assert_eq!(owner_of(&world, we).as_deref(), Some("player_0"));
        assert_eq!(ps.get("player_0").unwrap().cities_captured, 1);
    }

    #[test]
    fn own_unit_does_not_capture_owned_city() {
        let (mut world, we, ctx, grid) = setup(
            &[("city_a", (10.0, 10.0), Some("player_0"))],
            &[("player_0", (10.2, 10.2))],
        );
        let mut ps = stats();
        let changed = run(&mut world, we, &mut ps, &ctx, &grid);
        assert!(changed.is_empty());
        assert_eq!(ps.get("player_0").unwrap().cities_captured, 0);
    }

    #[test]
    fn enemy_unit_captures_owned_city() {
        let (mut world, we, ctx, grid) = setup(
            &[("city_a", (10.0, 10.0), Some("player_0"))],
            &[("player_1", (10.0, 11.0))],
        );
        let mut ps = stats();
        let changed = run(&mut world, we, &mut ps, &ctx, &grid);
        assert_eq!(changed, vec!["city_a".to_string()]);
        assert_eq!(owner_of(&world, we).as_deref(), Some("player_1"));
    }

    #[test]
    fn unit_out_of_range_does_not_capture() {
        let (mut world, we, ctx, grid) = setup(
            &[("city_a", (10.0, 10.0), None)],
            &[("player_0", (13.0, 10.0))],
        );
        let mut ps = stats();
        let changed = run(&mut world, we, &mut ps, &ctx, &grid);
        assert!(changed.is_empty());
    }

    #[test]
    fn multiple_same_owner_units_capture_once() {
        let (mut world, we, ctx, grid) = setup(
            &[("city_a", (10.0, 10.0), None)],
            &[
                ("player_0", (10.0, 10.0)),
                ("player_0", (10.5, 10.5)),
                ("player_0", (9.5, 9.5)),
            ],
        );
        let mut ps = stats();
        let changed = run(&mut world, we, &mut ps, &ctx, &grid);
        // One changed city even with several qualifying units; stats count once.
        assert_eq!(changed, vec!["city_a".to_string()]);
        assert_eq!(ps.get("player_0").unwrap().cities_captured, 1);
    }

    #[test]
    fn contested_city_goes_to_exactly_one_eligible_owner_deterministically() {
        // Two different players' units on a neutral city: a single deterministic
        // winner among the eligible owners (tie-break = last in spatial order).
        let mk = || {
            setup(
                &[("city_a", (10.0, 10.0), None)],
                &[("player_0", (10.0, 10.0)), ("player_1", (10.4, 10.0))],
            )
        };
        let (mut world, we, ctx, grid) = mk();
        let mut ps = stats();
        let changed = run(&mut world, we, &mut ps, &ctx, &grid);
        assert_eq!(changed, vec!["city_a".to_string()]);
        let owner = owner_of(&world, we).unwrap();
        assert!(owner == "player_0" || owner == "player_1");
        // A fresh identical world yields the same winner.
        let (mut world2, we2, ctx2, grid2) = mk();
        let mut ps2 = stats();
        let _ = run(&mut world2, we2, &mut ps2, &ctx2, &grid2);
        assert_eq!(owner, owner_of(&world2, we2).unwrap());
    }
}
