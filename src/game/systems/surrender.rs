//! SurrenderSystem — check for units below 5% men that should surrender.
//!
//! Uses two-phase pattern: query + collect → return dead entities.
//!
//! # Deterministic rule (intentional — see D-3)
//! This system is deliberately **deterministic**, not probabilistic: a unit surrenders
//! when it is at or below 0 men, OR when its men has fallen below 5% of its max men
//! (`men / max_men < 0.05`). There is no random chance and no `surrender_chance()`
//! model here. A unit sitting just above the 5% threshold survives indefinitely
//! regardless of encirclement; a unit below it is removed unconditionally. The 5%
//! cutoff doubles as the cleanup sweep for combat/attrition casualties that reached
//! 0 men outside combat (see `game.rs tick_game`, which despawns the returned
//! entities). This behavior is covered by the tests below.

use crate::game::components::*;
use hecs::World;

pub fn run(world: &mut World, _world_entity: hecs::Entity) -> Vec<hecs::Entity> {
    // Direct query — no entity-collect-then-refetch
    world
        .query::<(&UnitStats, &UnitFlags)>()
        .iter()
        .filter_map(|(entity, (stats, _flags))| {
            if stats.men <= 0.0 {
                return Some(entity);
            }
            if stats.max_men > 0.0 && (stats.men / stats.max_men) < 0.05 {
                return Some(entity);
            }
            None
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn surrender_zero_men() {
        let mut world = World::new();
        let e = world.spawn((
            UnitStats {
                men: 0.0,
                max_men: 1000.0,
                organisation: 50.0,
                max_organisation: 100.0,
            },
            UnitFlags::default(),
        ));
        // Use a dummy world entity
        let world_entity = world.spawn(((),));

        let dead = run(&mut world, world_entity);
        assert!(dead.contains(&e));
    }

    #[test]
    fn surrender_below_5_percent_not_encircled() {
        let mut world = World::new();
        let e = world.spawn((
            UnitStats {
                men: 40.0, // 4% of 1000
                max_men: 1000.0,
                organisation: 50.0,
                max_organisation: 100.0,
            },
            UnitFlags {
                encircled: false,
                ..Default::default()
            },
        ));
        let world_entity = world.spawn(((),));

        let dead = run(&mut world, world_entity);
        assert!(dead.contains(&e));
    }

    #[test]
    fn surrender_below_5_percent_encircled_also_surrenders() {
        let mut world = World::new();
        let e = world.spawn((
            UnitStats {
                men: 40.0, // 4% of 1000
                max_men: 1000.0,
                organisation: 50.0,
                max_organisation: 100.0,
            },
            UnitFlags {
                encircled: true,
                ..Default::default()
            },
        ));
        let world_entity = world.spawn(((),));

        let dead = run(&mut world, world_entity);
        // Encircled units surrender at the same rate as non-encircled units
        assert!(dead.contains(&e));
    }

    #[test]
    fn surrender_at_5_percent_stays() {
        let mut world = World::new();
        let e = world.spawn((
            UnitStats {
                men: 50.0, // exactly 5% of 1000
                max_men: 1000.0,
                organisation: 50.0,
                max_organisation: 100.0,
            },
            UnitFlags::default(),
        ));
        let world_entity = world.spawn(((),));

        let dead = run(&mut world, world_entity);
        assert!(!dead.contains(&e));
    }

    #[test]
    fn surrender_healthy_unit_stays() {
        let mut world = World::new();
        let e = world.spawn((
            UnitStats {
                men: 500.0,
                max_men: 1000.0,
                organisation: 50.0,
                max_organisation: 100.0,
            },
            UnitFlags::default(),
        ));
        let world_entity = world.spawn(((),));

        let dead = run(&mut world, world_entity);
        assert!(!dead.contains(&e));
    }

    #[test]
    fn surrender_no_units_returns_empty() {
        let mut world = World::new();
        let world_entity = world.spawn(((),));
        let dead = run(&mut world, world_entity);
        assert!(dead.is_empty());
    }

    #[test]
    fn surrender_mixed_units() {
        let mut world = World::new();
        // Dead unit
        let dead_unit = world.spawn((
            UnitStats {
                men: 0.0,
                max_men: 1000.0,
                organisation: 0.0,
                max_organisation: 100.0,
            },
            UnitFlags::default(),
        ));
        // Low men, not encircled
        let low_unit = world.spawn((
            UnitStats {
                men: 30.0,
                max_men: 1000.0,
                organisation: 10.0,
                max_organisation: 100.0,
            },
            UnitFlags {
                encircled: false,
                ..Default::default()
            },
        ));
        // Healthy unit
        let healthy_unit = world.spawn((
            UnitStats {
                men: 800.0,
                max_men: 1000.0,
                organisation: 80.0,
                max_organisation: 100.0,
            },
            UnitFlags::default(),
        ));
        let world_entity = world.spawn(((),));

        // Encircled low-men unit (should also surrender)
        let encircled_low_unit = world.spawn((
            UnitStats {
                men: 30.0,
                max_men: 1000.0,
                organisation: 10.0,
                max_organisation: 100.0,
            },
            UnitFlags {
                encircled: true,
                ..Default::default()
            },
        ));

        let dead = run(&mut world, world_entity);
        assert!(dead.contains(&dead_unit));
        assert!(dead.contains(&low_unit));
        assert!(dead.contains(&encircled_low_unit));
        assert!(!dead.contains(&healthy_unit));
        assert_eq!(dead.len(), 3);
    }
}
