//! NearbyEnemies — consume the movement-built spatial grid.
//!
//! `movement` builds a **post-move** spatial grid (see `spatial.rs`) and hands
//! it off via `Game.grid_index_scratch`. This system does **not** rebuild the
//! grid — it only runs the 3×3 enemy-collection query against it, filtering
//! enemies within `COMBAT_DISTANCE` (circle intersection, `dist < 2×radius`),
//! and writes each unit's `NearbyEnemies` list + the `attacking` flag.
//!
//! Pipeline (per unit):
//!   1. `spatial::for_each_within` over the grid (3×3 cell scan + dist² filter).
//!   2. Skip self + same-owner (compact `owner_idx` u8 equality, no String
//!      comparison, no per-pair ECS borrow).
//!   3. Write `NearbyEnemies` (SmallVec field, in-place, no per-tick alloc once
//!      capacity is sufficient) and, for movers only, the `attacking` flag.
//!
//! Complexity: O(n × c) where c ≈ entities in the 3×3 neighbourhood. Because
//! unit density is bounded by the occupancy rule (centers ≥ MIN_UNIT_DISTANCE
//! apart), c is O(1) — this system stays cheap even at high unit counts.

use crate::game::components::*;
use crate::game::constants::COMBAT_DISTANCE_SQ;
use crate::game::spatial::{self, GridIndexScratch};

/// Runs every tick against the grid `movement` just built (fresh, post-move).
/// Every unit's `NearbyEnemies` list is rebuilt from the grid of currently
/// living units. This full-rebuild-every-tick behavior keeps the lists fresh
/// and symmetric — a load-bearing invariant for `death_cleanup`'s reverse
/// lookup and for the upcoming conditional rebuild.
pub fn run(world: &mut hecs::World, grid_scratch: &mut GridIndexScratch) {
    let n = grid_scratch.units.len();
    if n == 0 {
        return;
    }

    // The grid metadata + dense buffers are the post-move grid built by
    // movement. `grid` borrows `grid_scratch.index`; both are read-only here.
    let grid = &grid_scratch.index;

    // Reuse buffer to avoid per-unit allocation.
    let mut enemies = Vec::with_capacity(16);

    // For each unit, run the 3×3 neighbourhood circle query and collect its
    // enemy entities. No grid rebuild, no HashMap lookups, no linear scans.
    for &(entity, pos, my_owner) in grid_scratch.units.iter() {
        enemies.clear();
        spatial::for_each_within(
            grid,
            grid_scratch,
            pos,
            COMBAT_DISTANCE_SQ,
            |other_ent, _, other_owner| {
                if other_ent == entity {
                    return; // skip self
                }
                // Fast owner comparison — direct u8 equality.
                if other_owner == my_owner {
                    return;
                }
                enemies.push(other_ent);
            },
        );

        // Write/update NearbyEnemies.
        // Move the collected entities into the (stack-backed) SmallVec field in
        // O(k) with no allocation once capacity >= the enemy count (SmallVec
        // keeps its heap buffer across clear()).
        let has_enemies = !enemies.is_empty();
        if let Ok((nb,)) = world.query_one_mut::<(&mut NearbyEnemies,)>(entity) {
            nb.entities.clear();
            for e in enemies.drain(..) {
                nb.entities.push(e);
            }
            nb.initialized = true;
        } else {
            let _ = world.insert(
                entity,
                (NearbyEnemies {
                    entities: enemies.drain(..).collect(),
                    initialized: true,
                },),
            );
        }

        // Set `attacking` flag for movers only (non-movers keep their sticky
        // value — movement cleared `moved` this tick, so `moved` marks exactly
        // the applied movers).
        if let Ok((mut_flags,)) = world.query_one_mut::<(&mut UnitFlags,)>(entity) {
            if mut_flags.moved {
                mut_flags.attacking = has_enemies;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::CityUpgradeSettings;
    use crate::game::spatial::GridIndex;
    use crate::game::world_ext::WorldExt;
    use std::collections::HashMap;

    fn setup() -> (hecs::World, hecs::Entity) {
        let mut world = hecs::World::new();
        let entity = world.spawn_world(
            vec![vec![crate::game::unit::Terrain::Plains; 200]; 200],
            200,
            200,
            CityUpgradeSettings::default(),
            std::collections::HashMap::new(),
            Vec::new(),
        );
        (world, entity)
    }

    /// Spawn a unit with the given owner, position and moved flag.
    fn make_unit(
        world: &mut hecs::World,
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

    /// Build the spatial grid the same way `movement` does (populate
    /// `(entity, pos, owner_idx)` + build index), then run nearby_enemies.
    /// Owner idx is assigned deterministically per player string (query order:
    /// P1→0, P2→1, P3→2) — same owner ⇒ same idx, so the same-owner filter is
    /// exercised exactly as in the real flow.
    fn build_grid_and_run(world: &mut hecs::World, grid_scratch: &mut GridIndexScratch) {
        let mut idx_map: HashMap<String, u8> = HashMap::new();
        let mut next: u8 = 0;
        grid_scratch.units.clear();
        for (e, owner, pos) in world
            .query::<(hecs::Entity, &UnitOwner, &UnitPosition)>()
            .iter()
        {
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
        run(world, grid_scratch);
    }

    /// Helper: build the grid and run the system with a fresh scratch (used by
    /// most tests).
    fn build_ctx_and_run(world: &mut hecs::World, _world_entity: hecs::Entity) {
        let mut grid_scratch = GridIndexScratch::default();
        build_grid_and_run(world, &mut grid_scratch);
    }

    #[test]
    fn initializes_new_spawns_and_sees_each_other() {
        let (mut world, world_entity) = setup();
        let a = make_unit(&mut world, "P1", 10.0, 10.0, false);
        let b = make_unit(&mut world, "P2", 10.5, 10.5, false);
        build_ctx_and_run(&mut world, world_entity);

        let nearby_a = world.get::<&NearbyEnemies>(a).unwrap();
        assert!(nearby_a.initialized);
        assert!(nearby_a.entities.contains(&b), "A should see enemy B");

        let nearby_b = world.get::<&NearbyEnemies>(b).unwrap();
        assert!(nearby_b.initialized);
        assert!(nearby_b.entities.contains(&a), "B should see enemy A");
    }

    #[test]
    fn ignores_same_owner() {
        let (mut world, world_entity) = setup();
        let a = make_unit(&mut world, "P1", 10.0, 10.0, false);
        let b = make_unit(&mut world, "P1", 10.5, 10.5, false);
        build_ctx_and_run(&mut world, world_entity);

        let nearby_a = world.get::<&NearbyEnemies>(a).unwrap();
        assert!(
            !nearby_a.entities.contains(&b),
            "same-owner must be excluded"
        );
        let nearby_b = world.get::<&NearbyEnemies>(b).unwrap();
        assert!(
            !nearby_b.entities.contains(&a),
            "same-owner must be excluded"
        );
    }

    #[test]
    fn mover_sets_attacking_with_enemy_nearby() {
        let (mut world, world_entity) = setup();
        // Stationary defender (moved=false).
        let b = make_unit(&mut world, "P2", 10.5, 10.5, false);
        // Mover advancing toward B (moved=true).
        let a = make_unit(&mut world, "P1", 10.0, 10.0, true);
        build_ctx_and_run(&mut world, world_entity);

        assert!(
            world.get::<&UnitFlags>(a).unwrap().attacking,
            "mover with a nearby enemy must have attacking=true"
        );
        let nearby_b = world.get::<&NearbyEnemies>(b).unwrap();
        assert!(
            nearby_b.entities.contains(&a),
            "stationary B should see mover A"
        );
    }

    #[test]
    fn mover_clears_attacking_when_no_enemy_nearby() {
        let (mut world, world_entity) = setup();
        // Mover at a far distance from anyone.
        let a = make_unit(&mut world, "P1", 0.0, 0.0, true);
        let _b = make_unit(&mut world, "P2", 100.0, 100.0, false);
        build_ctx_and_run(&mut world, world_entity);

        assert!(
            !world.get::<&UnitFlags>(a).unwrap().attacking,
            "mover far from enemies should have attacking=false"
        );
    }

    #[test]
    fn stationary_defender_does_not_become_attacker() {
        let (mut world, world_entity) = setup();
        let b = make_unit(&mut world, "P2", 10.5, 10.5, false);
        let _a = make_unit(&mut world, "P1", 10.0, 10.0, true);
        build_ctx_and_run(&mut world, world_entity);

        assert!(
            !world.get::<&UnitFlags>(b).unwrap().attacking,
            "stationary defender must NOT become an attacker"
        );
        assert!(
            !world.get::<&NearbyEnemies>(b).unwrap().entities.is_empty(),
            "B should see mover A"
        );
    }

    #[test]
    fn two_nearby_movers_see_each_other_and_attack() {
        let (mut world, world_entity) = setup();
        let a = make_unit(&mut world, "P1", 10.0, 10.0, true);
        let b = make_unit(&mut world, "P2", 10.5, 10.5, true);
        build_ctx_and_run(&mut world, world_entity);

        assert!(world
            .get::<&NearbyEnemies>(a)
            .unwrap()
            .entities
            .contains(&b));
        assert!(world
            .get::<&NearbyEnemies>(b)
            .unwrap()
            .entities
            .contains(&a));
        assert!(world.get::<&UnitFlags>(a).unwrap().attacking);
        assert!(world.get::<&UnitFlags>(b).unwrap().attacking);
    }

    #[test]
    fn three_owners_all_see_each_other() {
        let (mut world, world_entity) = setup();
        let a = make_unit(&mut world, "P1", 10.0, 10.0, false);
        let b = make_unit(&mut world, "P2", 10.5, 10.5, false);
        let cc = make_unit(&mut world, "P3", 10.2, 10.2, false);
        build_ctx_and_run(&mut world, world_entity);

        let na = world.get::<&NearbyEnemies>(a).unwrap();
        assert!(na.entities.contains(&b) && na.entities.contains(&cc));
        let nb = world.get::<&NearbyEnemies>(b).unwrap();
        assert!(nb.entities.contains(&a) && nb.entities.contains(&cc));
        let nc = world.get::<&NearbyEnemies>(cc).unwrap();
        assert!(nc.entities.contains(&a) && nc.entities.contains(&b));
    }

    #[test]
    fn dense_cluster_filters_self_and_allies() {
        let (mut world, world_entity) = setup();
        let p1a = make_unit(&mut world, "P1", 10.0, 10.0, false);
        let p1b = make_unit(&mut world, "P1", 10.1, 10.1, false);
        let p1c = make_unit(&mut world, "P1", 10.2, 10.2, false);
        let p2a = make_unit(&mut world, "P2", 10.3, 10.3, false);
        let p2b = make_unit(&mut world, "P2", 10.4, 10.4, false);
        build_ctx_and_run(&mut world, world_entity);

        for (unit, label) in [(&p1a, "P1A"), (&p1b, "P1B"), (&p1c, "P1C")] {
            let nb = world.get::<&NearbyEnemies>(*unit).unwrap();
            assert_eq!(
                nb.entities.len(),
                2,
                "{} should see exactly 2 P2 enemies",
                label
            );
            assert!(nb.entities.contains(&p2a) && nb.entities.contains(&p2b));
            assert!(!nb.entities.contains(unit), "{} must not see itself", label);
        }
        for (unit, label) in [(&p2a, "P2A"), (&p2b, "P2B")] {
            let nb = world.get::<&NearbyEnemies>(*unit).unwrap();
            assert_eq!(nb.entities.len(), 3, "{} should see 3 P1 enemies", label);
        }
    }

    #[test]
    fn empty_world_does_not_panic() {
        let (mut world, world_entity) = setup();
        build_ctx_and_run(&mut world, world_entity);
    }

    #[test]
    fn units_beyond_combat_range_not_seen() {
        let (mut world, world_entity) = setup();
        let a = make_unit(&mut world, "P1", 0.0, 0.0, false);
        let b = make_unit(&mut world, "P2", 10.0, 10.0, false);
        build_ctx_and_run(&mut world, world_entity);

        let nearby_a = world.get::<&NearbyEnemies>(a).unwrap();
        assert!(
            nearby_a.entities.is_empty(),
            "far units should not be in NearbyEnemies"
        );
        let nearby_b = world.get::<&NearbyEnemies>(b).unwrap();
        assert!(
            nearby_b.entities.is_empty(),
            "far units should not be in NearbyEnemies"
        );
    }
}
