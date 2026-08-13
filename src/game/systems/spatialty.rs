//! SpatialtySystem — maintains NearbyEnemies via incremental announcements.
//!
//! Runs BETWEEN movement and combat. Movement records every unit that actually
//! moved into its `MovementScratch` as `(entity, pre-move pos, post-move pos)`.
//! Those records — or the ABSENCE of `NearbyEnemies` (fresh spawn) — mark a
//! unit as needing processing this tick. For each such unit we:
//!   1. remove its old position from the SpatialHash (movers),
//!   2. insert its new position,
//!   3. remove ourselves from our previous neighbours' NearbyEnemies lists,
//!   4. query the SpatialHash at the new position,
//!   5. filter candidates by owner (self + same team excluded),
//!   6. write our new NearbyEnemies,
//!   7. announce ourselves to our new neighbours,
//!   8. (movers) refresh the `attacking` flag.
//!
//! Because every list edit is symmetric (units remove themselves when leaving,
//! announce themselves when arriving), NearbyEnemies stays exact WITHOUT
//! recomputing stationary units — per-tick cost scales with movers/spawns, each
//! doing one hash query plus a few small list edits.

use crate::game::components::*;
use crate::game::systems::spatial_hash::SpatialHash;
use crate::game::world_ext::WorldExt;
use std::time::Instant;

/// A unit needing adjacency processing this tick.
///
/// Movers carry their pre-move position (for the hash remove) and post-move
/// position (for hash insert + query) straight from movement's scratch — no ECS
/// re-reads. Fresh spawns have no old position; their post-move position is
/// read once during collection.
struct PendingUnit {
    e: hecs::Entity,
    old_pos: Option<UnitPosition>,
    new_pos: UnitPosition,
    was_fresh: bool,
}

/// Returns the number of units whose NearbyEnemies was created this call.
pub fn run(
    world: &mut hecs::World,
    world_entity: hecs::Entity,
    spatial: &mut SpatialHash,
    movers: &[(hecs::Entity, UnitPosition, UnitPosition)],
    has_movers: bool,
    has_uninitialized: bool,
) -> u32 {
    if !has_movers && !has_uninitialized {
        return 0;
    }

    // Per-phase timing (recorded into WorldBundle debug stats; no-op without
    // the debug-overlay feature). Surface with `--bench-detailed spatialty`.
    let t0 = Instant::now();

    // Pass 1: collect units needing processing. Movers come straight from
    // movement's scratch (with old/new positions attached); fresh spawns are
    // found by scanning for units lacking `NearbyEnemies`. `was_fresh` records
    // whether the unit lacked NearbyEnemies at pass 1, so newly-initialized
    // units are counted exactly once (regardless of whether they were created
    // here or announced-to first).
    let mut pending: Vec<PendingUnit> = Vec::new();
    if has_movers {
        pending.reserve(movers.len());
        for &(e, old, new) in movers {
            pending.push(PendingUnit {
                e,
                old_pos: Some(old),
                new_pos: new,
                was_fresh: false,
            });
        }
    }
    if has_uninitialized {
        let mover_set: std::collections::HashSet<hecs::Entity> =
            pending.iter().map(|p| p.e).collect();
        for (e, nearby) in world
            .query::<(hecs::Entity, Option<&NearbyEnemies>)>()
            .iter()
        {
            if nearby.is_none() && !mover_set.contains(&e) {
                let Ok(pos) = world.get::<&UnitPosition>(e) else {
                    continue;
                };
                pending.push(PendingUnit {
                    e,
                    old_pos: None,
                    new_pos: *pos,
                    was_fresh: true,
                });
            }
        }
    }
    let t1 = Instant::now();

    let mut newly_initialized: u32 = 0;
    let mut scratch: Vec<hecs::Entity> = Vec::with_capacity(32);

    // Pass 2: process each pending unit (mover or fresh spawn).
    for p in &pending {
        let e = p.e;

        // 1) Remove the old position from the SpatialHash (movers only).
        if let Some(old) = p.old_pos {
            spatial.remove(e, old.x, old.y);
        }

        // 2) Insert the new position.
        spatial.insert(e, p.new_pos.x, p.new_pos.y);

        // 3) Drop self from every previous neighbour's NearbyEnemies (I moved away).
        let prev: Vec<hecs::Entity> = match world.get::<&NearbyEnemies>(e) {
            Ok(nearby) => nearby.entities.clone(),
            Err(_) => Vec::new(),
        };
        if !prev.is_empty() {
            for &f in &prev {
                if let Ok((fnb,)) = world.query_one_mut::<(&mut NearbyEnemies,)>(f) {
                    fnb.entities.retain(|&x| x != e);
                }
            }
        }

        // 4) Query the SpatialHash at the new position.
        scratch.clear();
        spatial.query_nearby_into(p.new_pos.x, p.new_pos.y, &mut scratch);

        // 5) Filter candidates by ownership (skip self + same team).
        let my_owner = world.get::<&UnitOwner>(e).map(|o| o.player_id.clone()).ok();
        let enemies: Vec<hecs::Entity> = scratch
            .iter()
            .filter(|&&c| {
                if c == e {
                    return false;
                }
                match (&my_owner, world.get::<&UnitOwner>(c)) {
                    (Some(mine), Ok(other)) => other.player_id.as_str() != mine.as_str(),
                    _ => false,
                }
            })
            .copied()
            .collect();

        // 6) Write/refresh NearbyEnemies for self (create on fresh spawn).
        let has_nearby = world.get::<&NearbyEnemies>(e).is_ok();
        if has_nearby {
            if let Ok((nb,)) = world.query_one_mut::<(&mut NearbyEnemies,)>(e) {
                nb.entities = enemies.clone();
                nb.initialized = true;
            }
        } else {
            let _ = world.insert(
                e,
                (NearbyEnemies {
                    entities: enemies.clone(),
                    initialized: true,
                },),
            );
        }

        // 7) Announce self to every new neighbour (keeps their lists exact).
        for &f in &enemies {
            let has_nearby_f = world.get::<&NearbyEnemies>(f).is_ok();
            if has_nearby_f {
                if let Ok((fnb,)) = world.query_one_mut::<(&mut NearbyEnemies,)>(f) {
                    if !fnb.entities.contains(&e) {
                        fnb.entities.push(e);
                    }
                }
            } else {
                let _ = world.insert(
                    f,
                    (NearbyEnemies {
                        entities: vec![e],
                        initialized: true,
                    },),
                );
            }
        }
    }
    let t2 = Instant::now();

    // Pass 3: movers refresh the attacking flag now that all neighbour
    // announcements have settled; newly-initialized units are counted exactly
    // once.
    for p in &pending {
        if p.was_fresh && world.get::<&NearbyEnemies>(p.e).is_ok() {
            newly_initialized += 1;
        }
        // Only movers refresh `attacking`; stationary units keep whatever state
        // combat left them in.
        if p.old_pos.is_none() {
            continue;
        }
        let has_enemies = world
            .get::<&NearbyEnemies>(p.e)
            .map(|nb| !nb.entities.is_empty())
            .unwrap_or(false);
        if let Ok((flags,)) = world.query_one_mut::<(&mut UnitFlags,)>(p.e) {
            flags.attacking = has_enemies;
        }
    }
    let t3 = Instant::now();

    // Record per-phase timings (keyed under "spatialty/..."; no-op if not
    // enabled or without the debug-overlay feature).
    let to_ms = |d: std::time::Duration| d.as_secs_f64() * 1000.0;
    world.record_detailed(world_entity, "spatialty/p1_collect", to_ms(t1 - t0));
    world.record_detailed(world_entity, "spatialty/p2_process", to_ms(t2 - t1));
    world.record_detailed(world_entity, "spatialty/p3_finalize", to_ms(t3 - t2));

    newly_initialized
}

#[cfg(test)]
mod tests {
    use super::*;

    fn setup() -> (hecs::World, hecs::Entity, SpatialHash) {
        let mut world = hecs::World::new();
        let entity = world.spawn_world(
            vec![vec![crate::game::unit::Terrain::Plains; 200]; 200],
            200,
            200,
            crate::config::CityUpgradeSettings::default(),
            std::collections::HashMap::new(),
            Vec::new(),
        );
        (world, entity, SpatialHash::new())
    }

    /// Spawn a fresh unit. New units have NO `NearbyEnemies` — they are
    /// "uninitialized" and get processed on the first spatialty pass.
    fn make_unit(world: &mut hecs::World, owner: &str, x: f32, y: f32) -> hecs::Entity {
        world.spawn((
            UnitOwner {
                player_id: owner.to_string(),
            },
            UnitPosition { x, y },
            UnitFlags::default(),
        ))
    }

    /// Run spatialty with the given movers. Movers are simulated like movement
    /// would record them: `(entity, pre-move pos, post-move pos)`.
    fn run_test(
        world: &mut hecs::World,
        world_entity: hecs::Entity,
        spatial: &mut SpatialHash,
        movers: &[(hecs::Entity, UnitPosition, UnitPosition)],
    ) {
        run(
            world,
            world_entity,
            spatial,
            movers,
            !movers.is_empty(),
            true,
        );
    }

    /// Convenience for mover-free ticks (just initializes fresh spawns).
    fn run_init(world: &mut hecs::World, world_entity: hecs::Entity, spatial: &mut SpatialHash) {
        run_test(world, world_entity, spatial, &[]);
    }

    #[test]
    fn initializes_new_spawns_and_sees_each_other() {
        let (mut world, world_entity, mut spatial) = setup();
        let a = make_unit(&mut world, "P1", 10.0, 10.0);
        let b = make_unit(&mut world, "P2", 10.5, 10.5);
        run_init(&mut world, world_entity, &mut spatial);

        let nearby_a = world.get::<&NearbyEnemies>(a).unwrap();
        assert!(nearby_a.initialized);
        assert!(
            nearby_a.entities.contains(&b),
            "A should see enemy B (via query or announcement)"
        );

        let nearby_b = world.get::<&NearbyEnemies>(b).unwrap();
        assert!(nearby_b.initialized);
        assert!(nearby_b.entities.contains(&a), "B should see enemy A");
    }

    #[test]
    fn ignores_same_owner() {
        let (mut world, world_entity, mut spatial) = setup();
        let a = make_unit(&mut world, "P1", 10.0, 10.0);
        let b = make_unit(&mut world, "P1", 10.5, 10.5);
        run_init(&mut world, world_entity, &mut spatial);

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
        let (mut world, world_entity, mut spatial) = setup();
        // Initialize a stationary defender first.
        let b = make_unit(&mut world, "P2", 10.5, 10.5);
        run_init(&mut world, world_entity, &mut spatial);

        // Movers advance toward B (old position far away). Movement would have
        // recorded (entity, old pos, new pos) in its scratch; simulate that.
        let a = make_unit(&mut world, "P1", 10.0, 10.0);
        let movers = vec![(
            a,
            UnitPosition { x: 50.0, y: 50.0 },
            UnitPosition { x: 10.0, y: 10.0 },
        )];
        run_test(&mut world, world_entity, &mut spatial, &movers);

        assert!(
            world.get::<&UnitFlags>(a).unwrap().attacking,
            "mover with a nearby enemy must have attacking=true"
        );
        let nearby_b = world.get::<&NearbyEnemies>(b).unwrap();
        assert!(
            nearby_b.entities.contains(&a),
            "stationary B should see mover A after announcement"
        );
    }

    #[test]
    fn mover_clears_attacking_and_is_removed_from_old_neighbours() {
        let (mut world, world_entity, mut spatial) = setup();
        let a = make_unit(&mut world, "P1", 10.0, 10.0);
        let b = make_unit(&mut world, "P2", 10.5, 10.5);
        run_init(&mut world, world_entity, &mut spatial); // A & B initialized, neighbours

        // A now dashes far away: movement would have updated UnitPosition and
        // recorded (a, old pos, new pos) in its scratch; simulate that.
        world.query_one_mut::<(&mut UnitPosition,)>(a).unwrap().0.x = 100.0;
        world.query_one_mut::<(&mut UnitPosition,)>(a).unwrap().0.y = 100.0;
        let movers = vec![(
            a,
            UnitPosition { x: 10.0, y: 10.0 },
            UnitPosition { x: 100.0, y: 100.0 },
        )];
        run_test(&mut world, world_entity, &mut spatial, &movers);

        assert!(
            !world.get::<&UnitFlags>(a).unwrap().attacking,
            "retreating mover should have attacking=false"
        );
        let nearby_b = world.get::<&NearbyEnemies>(b).unwrap();
        assert!(
            !nearby_b.entities.contains(&a),
            "B should no longer see A after A moved away"
        );
    }

    #[test]
    fn two_nearby_movers_see_each_other_and_attack() {
        let (mut world, world_entity, mut spatial) = setup();
        let a = make_unit(&mut world, "P1", 10.0, 10.0);
        let b = make_unit(&mut world, "P2", 10.5, 10.5);
        let movers = vec![
            (
                a,
                UnitPosition { x: 50.0, y: 50.0 },
                UnitPosition { x: 10.0, y: 10.0 },
            ),
            (
                b,
                UnitPosition { x: 40.0, y: 40.0 },
                UnitPosition { x: 10.5, y: 10.5 },
            ),
        ];
        run_test(&mut world, world_entity, &mut spatial, &movers);

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
        // Pass 3 runs after announcements settle, so both movers see each other.
        assert!(world.get::<&UnitFlags>(a).unwrap().attacking);
        assert!(world.get::<&UnitFlags>(b).unwrap().attacking);
    }

    #[test]
    fn stationary_defender_does_not_become_attacker() {
        let (mut world, world_entity, mut spatial) = setup();
        let b = make_unit(&mut world, "P2", 10.5, 10.5);
        run_init(&mut world, world_entity, &mut spatial);
        let a = make_unit(&mut world, "P1", 10.0, 10.0);
        let movers = vec![(
            a,
            UnitPosition { x: 50.0, y: 50.0 },
            UnitPosition { x: 10.0, y: 10.0 },
        )];
        run_test(&mut world, world_entity, &mut spatial, &movers);

        assert!(
            !world.get::<&NearbyEnemies>(b).unwrap().entities.is_empty(),
            "B should see mover A"
        );
        assert!(
            !world.get::<&UnitFlags>(b).unwrap().attacking,
            "stationary defender must NOT become an attacker"
        );
    }

    #[test]
    fn stationary_away_from_movers_is_skipped() {
        let (mut world, world_entity, mut spatial) = setup();
        let c = make_unit(&mut world, "P2", 100.0, 100.0);
        run_init(&mut world, world_entity, &mut spatial); // init C
        let before = {
            let nb = world.get::<&NearbyEnemies>(c).unwrap();
            nb.entities.len()
        };

        // A "mover" at (0,0) -> (0,0): far from C.
        let a = make_unit(&mut world, "P1", 0.0, 0.0);
        let movers = vec![(
            a,
            UnitPosition { x: 0.0, y: 0.0 },
            UnitPosition { x: 0.0, y: 0.0 },
        )];
        run_test(&mut world, world_entity, &mut spatial, &movers);

        let after = {
            let nb = world.get::<&NearbyEnemies>(c).unwrap();
            nb.entities.len()
        };
        assert_eq!(
            before, after,
            "far-away stationary unit should be untouched"
        );
    }

    #[test]
    fn three_owners_all_see_each_other() {
        let (mut world, world_entity, mut spatial) = setup();
        let a = make_unit(&mut world, "P1", 10.0, 10.0);
        let b = make_unit(&mut world, "P2", 10.5, 10.5);
        let cc = make_unit(&mut world, "P3", 10.2, 10.2);
        run_init(&mut world, world_entity, &mut spatial);

        let na = world.get::<&NearbyEnemies>(a).unwrap();
        assert!(na.entities.contains(&b) && na.entities.contains(&cc));
        let nb = world.get::<&NearbyEnemies>(b).unwrap();
        assert!(nb.entities.contains(&a) && nb.entities.contains(&cc));
        let nc = world.get::<&NearbyEnemies>(cc).unwrap();
        assert!(nc.entities.contains(&a) && nc.entities.contains(&b));
    }

    #[test]
    fn empty_world_does_not_panic() {
        let (mut world, world_entity, mut spatial) = setup();
        run_init(&mut world, world_entity, &mut spatial);
    }

    #[test]
    fn dense_cluster_filters_self_and_allies() {
        let (mut world, world_entity, mut spatial) = setup();
        let p1a = make_unit(&mut world, "P1", 10.0, 10.0);
        let p1b = make_unit(&mut world, "P1", 10.1, 10.1);
        let p1c = make_unit(&mut world, "P1", 10.2, 10.2);
        let p2a = make_unit(&mut world, "P2", 10.3, 10.3);
        let p2b = make_unit(&mut world, "P2", 10.4, 10.4);
        run_init(&mut world, world_entity, &mut spatial);

        for (&unit, label) in [(&p1a, "P1A"), (&p1b, "P1B"), (&p1c, "P1C")] {
            let nb = world.get::<&NearbyEnemies>(unit).unwrap();
            assert_eq!(
                nb.entities.len(),
                2,
                "{} should see exactly 2 P2 enemies",
                label
            );
            assert!(nb.entities.contains(&p2a) && nb.entities.contains(&p2b));
            assert!(
                !nb.entities.contains(&unit),
                "{} must not see itself",
                label
            );
        }
        for (&unit, label) in [(&p2a, "P2A"), (&p2b, "P2B")] {
            let nb = world.get::<&NearbyEnemies>(unit).unwrap();
            assert_eq!(nb.entities.len(), 3, "{} should see 3 P1 enemies", label);
        }
    }
}
