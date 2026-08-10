//! DeathCleanupSystem — unified cleanup of dead units from combat, attrition, and surrender.
//!
//! Runs after all death-producing systems have finished. Performs:
//! 1. Clean NearbyEnemies references (single pass, O(total entries) amortized)
//! 2. Delete NearbyEnemies component from dead units
//! 3. Despawn dead entities and clean entity registry

use std::collections::HashMap;

use crate::game::components::NearbyEnemies;
use crate::game::world_ext::WorldExt;

pub fn run(
    world: &mut hecs::World,
    world_entity: hecs::Entity,
    all_dead: Vec<hecs::Entity>,
    entity_reverse: &mut HashMap<hecs::Entity, String>,
) {
    if all_dead.is_empty() {
        return;
    }

    let dead_set: std::collections::HashSet<hecs::Entity> = all_dead.iter().copied().collect();

    // Capture each dead unit's last-known position so its stale SpatialHash
    // entry can be removed (read-only borrows, dropped per iteration).
    let dead_positions: Vec<(hecs::Entity, f32, f32)> = all_dead
        .iter()
        .filter_map(|&e| {
            world
                .get::<&NearbyEnemies>(e)
                .ok()
                .map(|n| (e, n.last_x, n.last_y))
        })
        .collect();

    // Remove stale SpatialHash entries for the dead units.
    if let Some(wb) = world.world_data_mut(world_entity) {
        for (e, lx, ly) in &dead_positions {
            wb.spatial_hash.remove(*e, *lx, *ly);
        }
    }

    // Clean NearbyEnemies references (single pass, O(total entries) amortized)
    for (_entity, nearby) in world.query::<&mut NearbyEnemies>().iter() {
        nearby.entities.retain(|e| !dead_set.contains(e));
    }

    // Despawn + cleanup entity registry (despawn removes all components)
    for dead in &all_dead {
        entity_reverse.remove(dead);
        world.despawn(*dead).ok();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::game::components::NearbyEnemies;

    fn make_entity(world: &mut hecs::World, id: u64) -> hecs::Entity {
        for _ in 0..id {
            world.spawn(((),));
        }
        world.spawn(((),))
    }

    #[test]
    fn cleanup_removes_dead_from_nearby_enemies() {
        let mut world = hecs::World::new();
        let world_entity = world.spawn(((),)); // no WorldBundle -> world_data_mut returns None
        let alive = world.spawn((NearbyEnemies {
            entities: vec![],
            initialized: true,
            last_x: 0.0,
            last_y: 0.0,
        },));
        let dead1 = world.spawn((NearbyEnemies {
            entities: vec![],
            initialized: true,
            last_x: 0.0,
            last_y: 0.0,
        },));
        let dead2 = make_entity(&mut world, 10);

        // alive unit has dead1 and dead2 in its nearby list
        {
            let nearby = world
                .query_one_mut::<(&mut NearbyEnemies,)>(alive)
                .unwrap()
                .0;
            nearby.entities.push(dead1);
            nearby.entities.push(dead2);
        }

        let mut entity_reverse = HashMap::new();
        run(&mut world, world_entity, vec![dead1, dead2], &mut entity_reverse);

        // alive unit's NearbyEnemies should no longer contain dead entities
        let nearby = world.get::<&NearbyEnemies>(alive).unwrap();
        assert!(
            nearby.entities.is_empty(),
            "dead entities should be removed from NearbyEnemies lists"
        );

        // dead entities should be despawned
        assert!(
            world.get::<&NearbyEnemies>(dead1).is_err(),
            "dead entity should be despawned"
        );
        assert!(
            world.get::<&NearbyEnemies>(dead2).is_err(),
            "dead entity should be despawned"
        );
    }

    #[test]
    fn cleanup_empty_input_returns_early() {
        let mut world = hecs::World::new();
        let world_entity = world.spawn(((),));
        let entity = world.spawn((NearbyEnemies {
            entities: vec![],
            initialized: true,
            last_x: 0.0,
            last_y: 0.0,
        },));

        let mut entity_reverse = HashMap::new();
        // Empty input should not affect anything
        run(&mut world, world_entity, vec![], &mut entity_reverse);

        // Entity should still exist
        assert!(
            world.get::<&NearbyEnemies>(entity).is_ok(),
            "entity should not be affected by empty cleanup"
        );
    }

    #[test]
    fn cleanup_removes_from_entity_reverse() {
        let mut world = hecs::World::new();
        let world_entity = world.spawn(((),));
        let dead = world.spawn((NearbyEnemies {
            entities: vec![],
            initialized: true,
            last_x: 0.0,
            last_y: 0.0,
        },));

        let mut entity_reverse: HashMap<hecs::Entity, String> = HashMap::new();
        entity_reverse.insert(dead, "unit_123".to_string());

        run(&mut world, world_entity, vec![dead], &mut entity_reverse);

        assert!(
            entity_reverse.is_empty(),
            "entity_reverse should be cleaned of dead entity"
        );
    }
}
