//! DeathCleanupSystem — unified cleanup of dead units from combat, attrition, and surrender.
//!
//! Runs after all death-producing systems have finished. Performs:
//! 1. Clear the explicit combat target (`UnitFlags.attacking`) of units
//!    pointing at a unit that died post-combat this tick (target-lifecycle
//!    site 3, plans/COMBAT_TARGETING.md §8.4)
//! 2. Despawn dead entities and clean entity registry

use std::collections::HashMap;

use crate::game::components::UnitFlags;

/// Grow-only scratch for death cleanup: the set is `clear()`'d each run
/// (capacity preserved), so a death-spike tick's size is retained and later
/// spikes free the HashSet rebuild. `clear()` keeps capacity.
#[derive(Default)]
pub struct DeathCleanupScratch {
    /// Entities dying this tick — built once, reused per run.
    pub dead_set: std::collections::HashSet<hecs::Entity>,
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

    // Target-lifecycle site 3 (plans/COMBAT_TARGETING.md §8.4): units whose
    // explicit combat target died to supply/surrender (post-combat deaths)
    // get `attacking = None`. Combat's own validation (site 2) handles
    // combat deaths; this pass covers the deaths that happen AFTER combat
    // runs in the same tick. Done before despawn (components vanish with
    // the entity; we only read `dead_set` + clear flags, so order vs the
    // despawn is immaterial, but grouping the cleanup in one place keeps it
    // reviewable). Stale entries are unmatchable once despawned (hecs
    // generations), so any reference that slips past this tick is tolerated
    // by combat's target-validation skip path.
    // Collect first, then mutate: the query iterator holds an immutable
    // world borrow across the loop.
    let mut to_clear: Vec<hecs::Entity> = Vec::new();
    for (entity, flags) in world.query::<(hecs::Entity, &UnitFlags)>().iter() {
        if let Some(target) = flags.attacking {
            if scratch.dead_set.contains(&target) {
                to_clear.push(entity);
            }
        }
    }
    for entity in to_clear {
        if let Ok((mf,)) = world.query_one_mut::<(&mut UnitFlags,)>(entity) {
            mf.attacking = None;
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

    fn spawn_unit(world: &mut hecs::World) -> hecs::Entity {
        world.spawn((UnitPosition { x: 0.0, y: 0.0 },))
    }

    #[test]
    fn cleanup_despawns_dead_and_clears_registry() {
        let mut world = hecs::World::new();
        let alive = spawn_unit(&mut world);
        let dead1 = spawn_unit(&mut world);
        let dead2 = spawn_unit(&mut world);

        let mut entity_reverse = HashMap::new();
        entity_reverse.insert(dead1, "owner1".to_string());
        entity_reverse.insert(dead2, "owner2".to_string());
        entity_reverse.insert(alive, "owner0".to_string());

        let mut scratch = DeathCleanupScratch::default();
        let deads = [dead1, dead2];
        let count = run(&mut world, &[&deads[..]], &mut entity_reverse, &mut scratch);

        // All dead units counted and despawned; the alive unit is untouched.
        assert_eq!(count, 2);
        assert!(
            world.get::<&UnitPosition>(dead1).is_err(),
            "dead1 despawned"
        );
        assert!(
            world.get::<&UnitPosition>(dead2).is_err(),
            "dead2 despawned"
        );
        assert!(world.get::<&UnitPosition>(alive).is_ok(), "alive untouched");

        // entity_reverse cleaned of dead, alive entry kept.
        assert!(!entity_reverse.contains_key(&dead1));
        assert!(!entity_reverse.contains_key(&dead2));
        assert!(entity_reverse.contains_key(&alive));
    }

    #[test]
    fn cleanup_empty_input_returns_early() {
        let mut world = hecs::World::new();
        let entity = spawn_unit(&mut world);

        let mut entity_reverse = HashMap::new();
        let mut scratch = DeathCleanupScratch::default();
        let empty: Vec<hecs::Entity> = Vec::new();
        // Empty input should not affect anything.
        run(&mut world, &[&empty[..]], &mut entity_reverse, &mut scratch);

        // Entity should still exist.
        assert!(
            world.get::<&UnitPosition>(entity).is_ok(),
            "entity should not be affected by empty cleanup"
        );
    }

    #[test]
    fn cleanup_multiple_sides() {
        let mut world = hecs::World::new();
        let dead_a = spawn_unit(&mut world);
        let dead_b = spawn_unit(&mut world);
        let alive = spawn_unit(&mut world);

        let mut entity_reverse = HashMap::new();
        entity_reverse.insert(dead_a, "a".to_string());
        entity_reverse.insert(dead_b, "b".to_string());

        let mut scratch = DeathCleanupScratch::default();
        let count = run(
            &mut world,
            &[&[dead_a][..], &[dead_b][..]],
            &mut entity_reverse,
            &mut scratch,
        );

        assert_eq!(count, 2);
        assert!(world.get::<&UnitPosition>(dead_a).is_err());
        assert!(world.get::<&UnitPosition>(dead_b).is_err());
        assert!(!entity_reverse.contains_key(&dead_a));
        assert!(!entity_reverse.contains_key(&dead_b));
        assert!(world.get::<&UnitPosition>(alive).is_ok());
    }

    #[test]
    fn cleanup_removes_from_entity_reverse() {
        let mut world = hecs::World::new();
        let dead = spawn_unit(&mut world);

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

    #[test]
    fn scratch_is_reusable_across_calls() {
        // Two successive calls must reuse scratch without leaking state
        // (dead_set is cleared per run).
        let mut world = hecs::World::new();
        let alive = spawn_unit(&mut world);
        let dead1 = spawn_unit(&mut world);

        let mut entity_reverse = HashMap::new();
        entity_reverse.insert(dead1, "1".to_string());

        let mut scratch = DeathCleanupScratch::default();
        let deads = [dead1];
        let count = run(&mut world, &[&deads[..]], &mut entity_reverse, &mut scratch);
        assert_eq!(count, 1);

        // Second call with a fresh death in the same world must work (no
        // stale dead_set entries from the first call).
        let dead2 = spawn_unit(&mut world);
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
        assert!(world.get::<&UnitPosition>(alive).is_ok());
    }

    /// Safety invariant site 3 depends on: once an entity is despawned, its
    /// `Entity` value can never match a successor (hecs bumps the generation),
    /// so a stale `attacking` reference pointing at a dead unit is unmatchable
    /// and combat's target-validation skip path tolerates it.
    #[test]
    fn stale_entity_value_never_matches_successor() {
        let mut world = hecs::World::new();
        let dead = spawn_unit(&mut world);
        world.despawn(dead).unwrap();
        assert!(world.get::<&UnitPosition>(dead).is_err());

        // Index may be reused by a fresh entity, but the stale value (old
        // generation) must never match it.
        let fresh = spawn_unit(&mut world);
        assert!(world.get::<&UnitPosition>(fresh).is_ok());
        assert!(
            world.get::<&UnitPosition>(dead).is_err(),
            "stale Entity value must never match a successor"
        );
    }

    /// Site 3: a unit whose explicit combat target died to supply/surrender
    /// (post-combat, i.e. in this same cleanup's dead set) gets
    /// `attacking = None`. A unit targeting a *living* unit keeps its target.
    #[test]
    fn site3_clears_target_pointing_at_post_combat_death() {
        let mut world = hecs::World::new();
        let attacker = spawn_unit(&mut world);
        let dead_target = spawn_unit(&mut world);
        let live_target = spawn_unit(&mut world);

        // attacker targets the unit about to die; a second unit targets the
        // one that lives.
        world
            .insert(
                attacker,
                (UnitFlags {
                    attacking: Some(dead_target),
                    ..UnitFlags::default()
                },),
            )
            .unwrap();
        let third = spawn_unit(&mut world);
        world
            .insert(
                third,
                (UnitFlags {
                    attacking: Some(live_target),
                    ..UnitFlags::default()
                },),
            )
            .unwrap();

        let mut entity_reverse = HashMap::new();
        entity_reverse.insert(dead_target, "dead".to_string());

        let mut scratch = DeathCleanupScratch::default();
        let deads = [dead_target];
        run(&mut world, &[&deads[..]], &mut entity_reverse, &mut scratch);

        // Dead target despawned.
        assert!(world.get::<&UnitPosition>(dead_target).is_err());

        // Attacker's stale target cleared (site 3).
        assert_eq!(
            world.get::<&UnitFlags>(attacker).unwrap().attacking,
            None,
            "target died post-combat this tick → attacking must be cleared"
        );

        // The unit targeting a living unit is untouched.
        assert_eq!(
            world.get::<&UnitFlags>(third).unwrap().attacking,
            Some(live_target),
            "target still alive → attacking must be kept"
        );
    }
}
