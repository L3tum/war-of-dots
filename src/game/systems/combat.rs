//! CombatSystem — resolve adjacent enemy combat using spatial hash.

use crate::game::components::*;
use crate::game::systems::spatial_hash::SpatialHash;
use crate::game::world_ext::WorldExt;
use hecs::World;
use std::sync::Arc;

pub fn run(
    world: &mut World,
    world_entity: hecs::Entity,
    spatial: SpatialHash,
) -> Vec<hecs::Entity> {
    // Scoped Arc extraction: drops hecs::Ref borrow immediately, only clones terrain Arc (24 bytes)
    #[cfg(any(debug_assertions, feature = "debug-overlay"))]
    let _t_wdc = crate::game::debug_stats::DetailedTimer::new("combat/world_data_clone");
    let terrain = match world.world_data(world_entity) {
        Some(wb) => Arc::clone(&wb.terrain.grid),
        None => return Vec::new(),
    };
    // wb (hecs::Ref) dropped — borrow released. Can now do mutable queries.
    #[cfg(any(debug_assertions, feature = "debug-overlay"))]
    _t_wdc.finish(world, world_entity);

    // Only check units that moved this tick OR are already fighting
    // Collect cheap components upfront (Stats, Flags are Copy). Equipment deferred to resolution.
    #[cfg(any(debug_assertions, feature = "debug-overlay"))]
    let _t_ue = crate::game::debug_stats::DetailedTimer::new("combat/unit_enumeration");
    let candidates: Vec<(hecs::Entity, UnitPosition, String, UnitStats, UnitFlags)> = world
        .query::<(&UnitPosition, &UnitOwner, &UnitFlags, &UnitStats)>()
        .iter()
        .filter(|(_, (_, _, flags, _))| flags.moved || flags.fighting)
        .map(|(e, (pos, owner, flags, stats))| (e, *pos, owner.player_id.clone(), *stats, *flags))
        .collect();
    #[cfg(any(debug_assertions, feature = "debug-overlay"))]
    _t_ue.finish(world, world_entity);

    let mut resolved = std::collections::HashSet::new();
    let mut dead = Vec::new();
    // Track entities that found a nearby enemy during main loop (for retreat logic)
    let mut had_enemy_nearby = std::collections::HashSet::new();

    #[cfg(any(debug_assertions, feature = "debug-overlay"))]
    let _t_lookup = crate::game::debug_stats::DetailedTimer::new("combat/adjacent_lookup");
    #[cfg(any(debug_assertions, feature = "debug-overlay"))]
    let _t_resolve = crate::game::debug_stats::DetailedTimer::new("combat/combat_resolution");
    #[cfg(any(debug_assertions, feature = "debug-overlay"))]
    let _t_stats = crate::game::debug_stats::DetailedTimer::new("combat/stats_update");

    for (entity1, pos1, owner1, s1, f1) in &candidates {
        if resolved.contains(entity1) {
            continue;
        }

        for entity2 in spatial.query_nearby(pos1.x, pos1.y) {
            if entity2 == *entity1 || resolved.contains(&entity2) {
                continue;
            }

            // Entity2 from spatial hash — must refetch (unavoidable with hecs API)
            let owner2 = match world.get::<&UnitOwner>(entity2) {
                Ok(o) => o.player_id.clone(),
                Err(_) => continue,
            };
            if *owner1 == owner2 {
                continue;
            }

            let pos2 = match world.get::<&UnitPosition>(entity2) {
                Ok(p) => *p,
                Err(_) => continue,
            };
            // Squared distance check — avoids sqrt()
            let dx = pos1.x - pos2.x;
            let dy = pos1.y - pos2.y;
            let dist_sq = dx * dx + dy * dy;
            if dist_sq >= crate::game::constants::COMBAT_RANGE_SQ {
                continue;
            }

            // Mark that entity1 had a nearby enemy (used for retreat logic)
            had_enemy_nearby.insert(*entity1);

            let s2 = match world.get::<&UnitStats>(entity2) {
                Ok(s) => *s,
                Err(_) => continue,
            };
            let e2 = match world.get::<&UnitEquipment>(entity2) {
                Ok(e) => (*e).clone(),
                Err(_) => continue,
            };
            let f2 = match world.get::<&UnitFlags>(entity2) {
                Ok(f) => *f,
                Err(_) => continue,
            };

            // Entity1 equipment — clone only when combat actually occurs
            let e1 = match world.get::<&UnitEquipment>(*entity1) {
                Ok(e) => (*e).clone(),
                Err(_) => continue,
            };

            let cell_terrain = terrain
                .get(pos1.x as usize)
                .and_then(|r| r.get(pos1.y as usize))
                .cloned()
                .unwrap_or(crate::game::unit::Terrain::Plains);

            let a_soft = 10.0
                * (s1.men / s1.max_men.max(0.1))
                * (s1.organisation / 100.0)
                * if f1.on_water { 0.75 } else { 1.0 };
            let d_soft = 10.0
                * (s2.men / s2.max_men.max(0.1))
                * (s2.organisation / 100.0)
                * if f2.on_water { 0.75 } else { 1.0 };

            // a_eff: attacker's offensive strength relative to defender's defense
            // Higher a_eff means defender takes more damage
            let a_eff = a_soft / (d_soft + 0.1);
            // d_eff: defender's counter-attack strength relative to attacker's offense
            // Higher d_eff means attacker takes more damage from counter-attack
            let d_eff = d_soft / (a_soft + 0.1);

            let terrain_mod = match cell_terrain {
                crate::game::unit::Terrain::Hills => 1.5,
                crate::game::unit::Terrain::Forest => 0.75,
                crate::game::unit::Terrain::Water => 0.5,
                _ => 1.0,
            };

            // dmg_to_defender: damage applied TO the defender (based on attacker effectiveness)
            let dmg_to_defender = a_eff * terrain_mod * 5.0;
            // dmg_to_attacker: damage applied TO the attacker (based on defender's counter-attack)
            let dmg_to_attacker = d_eff * terrain_mod * 5.0;

            let a_men = (s1.men - dmg_to_attacker).max(0.0);
            let d_men = (s2.men - dmg_to_defender).max(0.0);

            let attr_rate = 0.05;
            let a_eq: Vec<_> = e1
                .equipment
                .iter()
                .map(|eq| {
                    let loss = (dmg_to_attacker * attr_rate).min(eq.count as f32);
                    crate::game::unit::Equipment {
                        name: eq.name.clone(),
                        count: (eq.count as f32 - loss).max(0.0) as u32,
                        max_count: eq.max_count,
                        cost_per_item: eq.cost_per_item,
                    }
                })
                .collect();
            let d_eq: Vec<_> = e2
                .equipment
                .iter()
                .map(|eq| {
                    let loss = (dmg_to_defender * attr_rate).min(eq.count as f32);
                    crate::game::unit::Equipment {
                        name: eq.name.clone(),
                        count: (eq.count as f32 - loss).max(0.0) as u32,
                        max_count: eq.max_count,
                        cost_per_item: eq.cost_per_item,
                    }
                })
                .collect();

            if let Ok((ms, me, mf)) = world
                .query_one_mut::<(&mut UnitStats, &mut UnitEquipment, &mut UnitFlags)>(*entity1)
            {
                ms.men = a_men;
                ms.organisation = (ms.organisation - 10.0).max(0.0);
                me.equipment = a_eq;
                // Combat consumes supply proportional to damage taken
                let supply_loss = (dmg_to_attacker * attr_rate).max(1.0);
                me.supply = (me.supply - supply_loss).max(0.0);
                mf.fighting = true;
            }
            if let Ok((ms, me, mf)) =
                world.query_one_mut::<(&mut UnitStats, &mut UnitEquipment, &mut UnitFlags)>(entity2)
            {
                ms.men = d_men;
                ms.organisation = (ms.organisation - 10.0).max(0.0);
                me.equipment = d_eq;
                // Combat consumes supply proportional to damage taken
                let supply_loss = (dmg_to_defender * attr_rate).max(1.0);
                me.supply = (me.supply - supply_loss).max(0.0);
                mf.fighting = true;
            }

            if a_men <= 0.0 {
                dead.push(*entity1);
            }
            if d_men <= 0.0 {
                dead.push(entity2);
            }

            resolved.insert(*entity1);
            resolved.insert(entity2);
        }
    }

    // Retreat check: clear fighting for units that found no nearby enemy
    // Uses had_enemy_nearby set from main loop — NO second spatial hash pass
    #[cfg(any(debug_assertions, feature = "debug-overlay"))]
    let _t_retreat = crate::game::debug_stats::DetailedTimer::new("combat/retreat_check");
    for (entity, _, _, _, flags) in &candidates {
        if resolved.contains(entity) {
            continue;
        }
        if !flags.fighting {
            continue;
        }
        // If entity1 had a nearby enemy during main loop, keep fighting
        if had_enemy_nearby.contains(entity) {
            continue;
        }
        if let Ok((mf,)) = world.query_one_mut::<(&mut UnitFlags,)>(*entity) {
            mf.fighting = false;
        }
    }
    #[cfg(any(debug_assertions, feature = "debug-overlay"))]
    _t_retreat.finish(world, world_entity);

    // Clear moved flags for all units
    #[cfg(any(debug_assertions, feature = "debug-overlay"))]
    let _t_clear = crate::game::debug_stats::DetailedTimer::new("combat/clear_moved");
    for (_, flags) in world.query::<&mut UnitFlags>().iter() {
        flags.moved = false;
    }
    #[cfg(any(debug_assertions, feature = "debug-overlay"))]
    _t_clear.finish(world, world_entity);

    #[cfg(any(debug_assertions, feature = "debug-overlay"))]
    {
        _t_lookup.finish(world, world_entity);
        _t_resolve.finish(world, world_entity);
        _t_stats.finish(world, world_entity);
    }

    dead
}
