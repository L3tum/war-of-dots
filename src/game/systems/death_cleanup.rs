//! DeathCleanupSystem — unified cleanup of dead units from combat, attrition, and surrender.
//!
//! Runs after all death-producing systems have finished. Performs:
//! 1. Purge dead units from the `NearbyEnemies` lists of the units that
//!    reference them — via the dead units' own lists (reverse lookup),
//!    O(sum of dead units' neighborhood sizes), not O(all lists)
//! 2. Despawn dead entities and clean entity registry

use std::collections::HashMap;

use crate::game::components::NearbyEnemies;

/// Grow-only scratch for death cleanup: both sets are `clear()`'d each run
/// (capacity preserved), so a death-spike tick's size is retained and later
/// spikes free the HashSet rebuild. `clear()` keeps capacity.
#[derive(Default)]
pub struct DeathCleanupScratch {
    /// Entities dying this tick — built once, reused for the purge pass.
    pub dead_set: std::collections::HashSet<hecs::Entity>,
    /// Units whose lists reference a dead unit (reverse lookup) — the lists
    /// to purge. Rebuilt each run from the dead units' own lists.
    pub seen: std::collections::HashSet<hecs::Entity>,
}

pub fn run(
    world: &mut hecs::World,
    dead_lists: &[&[hecs::Entity]],
    entity_reverse: &mut HashMap<hecs::Entity, String>,
    scratch: &mut DeathCleanupScratch,
) -> usize {
    if dead_lists.iter().all(|l| l.is_empty()) {
        return 0;
    }

    // Sum of list lengths — do NOT dedupe. An entity appearing in two lists is
    // `entity_reverse.remove`+`despawn`'d twice (second `.ok()`-swallowed) and
    // counted twice, matching the pre-change semantics. List order is preserved
    // (combat -> supply -> surrender) so processing is bit-identical.
    let dead_count: usize = dead_lists.iter().map(|l| l.len()).sum();

    scratch.dead_set.clear();
    for list in dead_lists {
        for e in *list {
            scratch.dead_set.insert(*e);
        }
    }

    // REVERSE-LOOKUP PURGE (why this is exact under the current architecture):
    //
    // (a) `nearby_enemies` rebuilds every list from a single grid snapshot
    //     each tick (runs before combat; tick order: movement →
    //     nearby_enemies → combat → … → death_cleanup, and no system moves
    //     units in between), using a symmetric distance predicate. So at this
    //     point the lists are FRESH AND SYMMETRIC: for dead unit D, the set
    //     of units whose lists contain D is exactly D's own list. Reading
    //     each dead unit's own `NearbyEnemies` therefore covers every
    //     reference to D — no unit can reference a dead unit without
    //     appearing in the dead unit's list.
    //
    // (b) The coverage in (a) depends on list freshness/symmetry. If
    //     `nearby_enemies` ever becomes conditional (see its TODO), a dead
    //     unit's own list could miss a unit that still references it, and
    //     that stale one-sided entry would survive the purge. It is safe:
    //     combat tolerates it (`world.get::<&UnitPosition>` on a despawned
    //     entity is `Err` → skip; see the stale-window test below) and the
    //     next rebuild overwrites it.
    //
    // (c) hecs generations make stale entries unmatchable: despawn bumps the
    //     generation, so even if the entity index is reused, `world.get`
    //     with the old (index, generation) value returns `Err`.
    //
    // (d) Determinism: the purge removes exactly the (U, D) pairs the old
    //     flat world-wide `retain` removed (U references D ⟺ D's list
    //     contains U ⟺ U is a candidate), so the world state after this
    //     system is bit-identical to the pre-change behavior.
    //
    // NOTE: lists must be read BEFORE despawn — components vanish with the
    // entity, so the reverse lookup has to happen first.

    // 2. Build the candidate set: every unit whose list references a dead
    //    unit appears in some dead unit's own list (fresh & symmetric, (a)).
    //    `seen` is the set of lists to purge. O(sum of dead units' list
    //    sizes) — no world-wide list scan.
    scratch.seen.clear();
    for list in dead_lists {
        for dead in *list {
            if let Ok((nb,)) = world.query_one_mut::<(&NearbyEnemies,)>(*dead) {
                for &u in nb.entities.iter() {
                    scratch.seen.insert(u);
                }
            }
        }
    }

    // 3. Single query pass; retain only on candidate lists. Two rejected
    //    alternatives, and why:
    //      • world-wide flat `retain` — scans every list (O(total entries))
    //        even when one unit dies.
    //      • per-candidate `query_one_mut` — pays an archetype lookup per
    //        candidate; on a death spike where candidates ≈ all units that
    //        is a full extra pass stacked on the same retain work (this is
    //        exactly what made the first reverse-lookup cut ~2× slower on
    //        the stress benchmark).
    //    One pass + a per-unit membership test gives the best of both:
    //    light deaths touch only the affected lists; spikes cost the
    //    flat-retain amount with no extra pass.
    for (entity, nb) in world.query::<(hecs::Entity, &mut NearbyEnemies)>().iter() {
        if scratch.seen.contains(&entity) {
            nb.entities.retain(|e| !scratch.dead_set.contains(e));
        }
    }

    // Despawn + cleanup entity registry (despawn removes all components)
    for list in dead_lists {
        for dead in *list {
            entity_reverse.remove(dead);
            world.despawn(*dead).ok();
        }
    }

    dead_count
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::game::components::UnitPosition;

    fn spawn_with_nearby(world: &mut hecs::World) -> hecs::Entity {
        world.spawn((
            NearbyEnemies {
                entities: Default::default(),
                initialized: true,
            },
            UnitPosition { x: 0.0, y: 0.0 },
        ))
    }

    #[test]
    fn cleanup_removes_dead_via_reverse_lookup() {
        let mut world = hecs::World::new();
        let alive = spawn_with_nearby(&mut world);
        let dead1 = spawn_with_nearby(&mut world);
        // dead2 has a position but NO NearbyEnemies — a dead unit without a
        // list contributes nothing to the candidate set (its own reverse
        // lookup finds no one).
        let dead2 = world.spawn((UnitPosition { x: 0.0, y: 0.0 },));

        // Setup reflects the fresh symmetric invariant: dead1's own list
        // references the alive unit (so the reverse lookup finds `alive` as a
        // candidate), and the alive unit references both dead units.
        {
            let nb = world
                .query_one_mut::<(&mut NearbyEnemies,)>(alive)
                .unwrap()
                .0;
            nb.entities.push(dead1);
            nb.entities.push(dead2);
        }
        {
            let nb = world
                .query_one_mut::<(&mut NearbyEnemies,)>(dead1)
                .unwrap()
                .0;
            nb.entities.push(alive);
        }

        let mut entity_reverse = HashMap::new();
        entity_reverse.insert(dead1, "owner1".to_string());
        entity_reverse.insert(dead2, "owner2".to_string());

        let mut scratch = DeathCleanupScratch::default();
        let deads = [dead1, dead2];
        let count = run(&mut world, &[&deads[..]], &mut entity_reverse, &mut scratch);

        // All dead units cleaned up (dead2 counted even without a list).
        assert_eq!(count, 2);

        // dead entities should be despawned
        assert!(
            world.get::<&NearbyEnemies>(dead1).is_err(),
            "dead entity should be despawned"
        );
        assert!(
            world.get::<&UnitPosition>(dead2).is_err(),
            "dead entity without a list should be despawned too"
        );

        // entity_reverse cleaned
        assert!(!entity_reverse.contains_key(&dead1));
        assert!(!entity_reverse.contains_key(&dead2));

        // alive's list [dead1, dead2] is fully purged: `alive` is a candidate
        // (dead1's own list contains it), so the single-query pass retains its
        // WHOLE list, dropping BOTH dead entries — including dead2, which has
        // no list of its own and so contributes nothing to the candidate set.
        // (A one-sided reference with no dead-unit coverage is the separate
        // stale-window case: see `stale_one_sided_reference_survives_is_safe`.)
        let nearby = world.get::<&NearbyEnemies>(alive).unwrap();
        assert!(
            nearby.entities.is_empty(),
            "candidate's whole list is retained, dropping all dead entries"
        );
    }

    #[test]
    fn stale_one_sided_reference_survives_is_safe() {
        let mut world = hecs::World::new();
        let alive = spawn_with_nearby(&mut world);
        let dead = spawn_with_nearby(&mut world);

        // One-sided reference: alive lists dead, but dead's list is EMPTY.
        // This simulates a stale list that could exist if the rebuild ever
        // becomes conditional — the reverse lookup cannot find `alive` via
        // `dead`, so the reference is NOT purged (documented limitation).
        {
            let nb = world
                .query_one_mut::<(&mut NearbyEnemies,)>(alive)
                .unwrap()
                .0;
            nb.entities.push(dead);
        }

        let mut entity_reverse = HashMap::new();
        entity_reverse.insert(dead, "owner1".to_string());

        let mut scratch = DeathCleanupScratch::default();
        let deads = [dead];
        let count = run(&mut world, &[&deads[..]], &mut entity_reverse, &mut scratch);

        assert_eq!(count, 1);
        // dead is despawned and entity_reverse is cleaned.
        assert!(world.get::<&UnitPosition>(dead).is_err());
        assert!(!entity_reverse.contains_key(&dead));

        // The stale one-sided entry SURVIVES the purge (coverage requires
        // the dead unit's own list to be fresh)...
        {
            let nearby = world.get::<&NearbyEnemies>(alive).unwrap();
            assert_eq!(
                nearby.entities.as_slice(),
                [dead].as_slice(),
                "one-sided stale entry survives the reverse-lookup purge"
            );
        }

        // ...and it is safe: exactly what combat's continue path relies on.
        assert!(world.get::<&UnitPosition>(dead).is_err());

        // Generation non-aliasing: even if the index is reused by a fresh
        // entity, the stale Entity value never matches it.
        let fresh = spawn_with_nearby(&mut world);
        assert!(world.get::<&UnitPosition>(fresh).is_ok());
        assert!(
            world.get::<&UnitPosition>(dead).is_err(),
            "stale Entity value must never match a successor"
        );
    }

    #[test]
    fn dying_candidates_are_skipped() {
        let mut world = hecs::World::new();
        let dead1 = spawn_with_nearby(&mut world);
        let dead2 = spawn_with_nearby(&mut world);

        // Both dead units reference each other — each is a "candidate"
        // found via the other's list, but both are dying this tick, so the
        // purge step must skip them (no panic, no double-processing).
        {
            let nb = world
                .query_one_mut::<(&mut NearbyEnemies,)>(dead1)
                .unwrap()
                .0;
            nb.entities.push(dead2);
        }
        {
            let nb = world
                .query_one_mut::<(&mut NearbyEnemies,)>(dead2)
                .unwrap()
                .0;
            nb.entities.push(dead1);
        }

        let mut entity_reverse = HashMap::new();
        entity_reverse.insert(dead1, "o1".to_string());
        entity_reverse.insert(dead2, "o2".to_string());

        let mut scratch = DeathCleanupScratch::default();
        let deads = [dead1, dead2];
        let count = run(&mut world, &[&deads[..]], &mut entity_reverse, &mut scratch);

        assert_eq!(count, 2);
        assert!(world.get::<&UnitPosition>(dead1).is_err());
        assert!(world.get::<&UnitPosition>(dead2).is_err());
        assert!(!entity_reverse.contains_key(&dead1));
        assert!(!entity_reverse.contains_key(&dead2));
    }

    #[test]
    fn cleanup_empty_input_returns_early() {
        let mut world = hecs::World::new();
        let entity = spawn_with_nearby(&mut world);
        assert!(world.get::<&NearbyEnemies>(entity).is_ok());

        let mut entity_reverse = HashMap::new();
        let mut scratch = DeathCleanupScratch::default();
        let empty: Vec<hecs::Entity> = Vec::new();
        // Empty input should not affect anything
        run(&mut world, &[&empty[..]], &mut entity_reverse, &mut scratch);

        // Entity should still exist
        assert!(
            world.get::<&NearbyEnemies>(entity).is_ok(),
            "entity should not be affected by empty cleanup"
        );
    }

    #[test]
    fn cleanup_multiple_sides() {
        let mut world = hecs::World::new();
        let dead_a = spawn_with_nearby(&mut world);
        let dead_b = spawn_with_nearby(&mut world);
        let alive = spawn_with_nearby(&mut world);

        // Both dead units (from different sides) are referenced by alive,
        // and each dead unit's own list contains alive.
        {
            let nb = world
                .query_one_mut::<(&mut NearbyEnemies,)>(alive)
                .unwrap()
                .0;
            nb.entities.push(dead_a);
            nb.entities.push(dead_b);
        }
        {
            let nb = world
                .query_one_mut::<(&mut NearbyEnemies,)>(dead_a)
                .unwrap()
                .0;
            nb.entities.push(alive);
        }
        {
            let nb = world
                .query_one_mut::<(&mut NearbyEnemies,)>(dead_b)
                .unwrap()
                .0;
            nb.entities.push(alive);
        }

        let mut entity_reverse = HashMap::new();
        entity_reverse.insert(dead_a, "a".to_string());
        entity_reverse.insert(dead_b, "b".to_string());

        let mut scratch = DeathCleanupScratch::default();
        let dead_lists = [dead_a, dead_b];
        let count = run(
            &mut world,
            &[&[dead_lists[0]][..], &[dead_lists[1]][..]],
            &mut entity_reverse,
            &mut scratch,
        );

        assert_eq!(count, 2);
        assert!(world.get::<&UnitPosition>(dead_a).is_err());
        assert!(world.get::<&UnitPosition>(dead_b).is_err());
        assert!(!entity_reverse.contains_key(&dead_a));
        assert!(!entity_reverse.contains_key(&dead_b));

        // alive's list was purged via both dead units' own lists.
        let nearby = world.get::<&NearbyEnemies>(alive).unwrap();
        assert!(nearby.entities.is_empty());
    }

    #[test]
    fn cleanup_preserves_order_of_entities() {
        let mut world = hecs::World::new();
        let alive = spawn_with_nearby(&mut world);
        let dead1 = spawn_with_nearby(&mut world);
        let dead2 = spawn_with_nearby(&mut world);
        let keep1 = spawn_with_nearby(&mut world);
        let keep2 = spawn_with_nearby(&mut world);

        // alive references [dead1, keep1, dead2, keep2] — order of kept
        // entities must be preserved by the retain pass.
        {
            let nb = world
                .query_one_mut::<(&mut NearbyEnemies,)>(alive)
                .unwrap()
                .0;
            nb.entities.push(dead1);
            nb.entities.push(keep1);
            nb.entities.push(dead2);
            nb.entities.push(keep2);
        }
        {
            let nb = world
                .query_one_mut::<(&mut NearbyEnemies,)>(dead1)
                .unwrap()
                .0;
            nb.entities.push(alive);
        }
        {
            let nb = world
                .query_one_mut::<(&mut NearbyEnemies,)>(dead2)
                .unwrap()
                .0;
            nb.entities.push(alive);
        }

        let mut entity_reverse = HashMap::new();
        entity_reverse.insert(dead1, "1".to_string());
        entity_reverse.insert(dead2, "2".to_string());

        let mut scratch = DeathCleanupScratch::default();
        let deads = [dead1, dead2];
        let count = run(&mut world, &[&deads[..]], &mut entity_reverse, &mut scratch);

        assert_eq!(count, 2);
        let nearby = world.get::<&NearbyEnemies>(alive).unwrap();
        assert_eq!(
            nearby.entities.len(),
            2,
            "only non-dead entities should remain"
        );
        // retain preserves the original relative order.
        assert_eq!(
            nearby.entities.as_slice(),
            [keep1, keep2].as_slice(),
            "kept entity order must be preserved"
        );
    }

    #[test]
    fn scratch_is_reusable_across_calls() {
        // Two successive calls must reuse scratch without leaking state
        // (dead_set/candidates/seen are cleared per run).
        let mut world = hecs::World::new();
        let alive = spawn_with_nearby(&mut world);
        let dead1 = spawn_with_nearby(&mut world);
        {
            let nb = world
                .query_one_mut::<(&mut NearbyEnemies,)>(alive)
                .unwrap()
                .0;
            nb.entities.push(dead1);
        }
        {
            let nb = world
                .query_one_mut::<(&mut NearbyEnemies,)>(dead1)
                .unwrap()
                .0;
            nb.entities.push(alive);
        }

        let mut entity_reverse = HashMap::new();
        entity_reverse.insert(dead1, "1".to_string());

        let mut scratch = DeathCleanupScratch::default();
        let deads = [dead1];
        let count = run(&mut world, &[&deads[..]], &mut entity_reverse, &mut scratch);
        assert_eq!(count, 1);

        // Second call with a fresh death in the same world must work
        // (no stale candidates/dead_set entries from the first call).
        let dead2 = spawn_with_nearby(&mut world);
        {
            let nb = world
                .query_one_mut::<(&mut NearbyEnemies,)>(alive)
                .unwrap()
                .0;
            nb.entities.push(dead2);
        }
        {
            let nb = world
                .query_one_mut::<(&mut NearbyEnemies,)>(dead2)
                .unwrap()
                .0;
            nb.entities.push(alive);
        }
        entity_reverse.insert(dead2, "2".to_string());

        let deads2 = [dead2];
        let count2 = run(
            &mut world,
            &[&deads2[..]],
            &mut entity_reverse,
            &mut scratch,
        );
        assert_eq!(count2, 1);
        assert!(world.get::<&UnitPosition>(dead2).is_err());
        let nearby = world.get::<&NearbyEnemies>(alive).unwrap();
        assert!(
            nearby.entities.is_empty(),
            "both deaths must be purged with a reused scratch"
        );
    }

    #[test]
    fn cleanup_removes_from_entity_reverse() {
        let mut world = hecs::World::new();
        let dead = spawn_with_nearby(&mut world);

        let mut entity_reverse: HashMap<hecs::Entity, String> = HashMap::new();
        entity_reverse.insert(dead, "unit_123".to_string());

        let mut scratch = DeathCleanupScratch::default();
        let deads = [dead];
        run(&mut world, &[&deads[..]], &mut entity_reverse, &mut scratch);

        assert!(
            entity_reverse.is_empty(),
            "entity_reverse should be cleaned of dead entity"
        );
    }
}
