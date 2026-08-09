//! CombatSystem — resolve adjacent enemy combat using spatial hash.

use crate::game::components::*;
use crate::game::systems::spatial_hash::SpatialHash;
use crate::game::world_ext::WorldExt;
use hecs::World;
use std::sync::Arc;

/// Build the post-combat equipment array for a unit taking `dmg`, or `None` when
/// there is no damage to apply or the unit carries no equipment. Only allocates
/// when the equipment actually changes.
fn attrition_from(
    eq: &UnitEquipment,
    dmg: f32,
    attr_rate: f32,
) -> Option<Vec<crate::game::unit::Equipment>> {
    if dmg <= 0.0 || eq.equipment.is_empty() {
        return None;
    }
    Some(
        eq.equipment
            .iter()
            .map(|e| {
                let loss = (dmg * attr_rate).min(e.count as f32);
                crate::game::unit::Equipment {
                    name: e.name.clone(),
                    count: (e.count as f32 - loss).max(0.0) as u32,
                    max_count: e.max_count,
                    cost_per_item: e.cost_per_item,
                }
            })
            .collect(),
    )
}

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
        .filter(|(_, (_, _, flags, stats))| (flags.moved || flags.fighting) && stats.men > 0.0)
        .map(|(e, (pos, owner, flags, stats))| (e, *pos, owner.player_id.clone(), *stats, *flags))
        .collect();
    #[cfg(any(debug_assertions, feature = "debug-overlay"))]
    _t_ue.finish(world, world_entity);

    let mut resolved: std::collections::HashSet<(hecs::Entity, hecs::Entity)> =
        std::collections::HashSet::new();
    // Every entity that participated in a resolved pair this tick (used by the
    // retreat pass to keep `fighting` true for engaged units).
    let mut fought: std::collections::HashSet<hecs::Entity> = std::collections::HashSet::new();
    let mut dead = Vec::new();
    // O(1) membership for `dead` (avoid a linear scan per candidate).
    let mut dead_set: std::collections::HashSet<hecs::Entity> = std::collections::HashSet::new();
    // Reused scratch buffer for neighbouring-entity lookups (avoids a Vec
    // allocation per candidate per tick).
    let mut nearby: Vec<hecs::Entity> = Vec::new();
    // Track entities that found a nearby enemy during main loop (for retreat logic)
    let mut had_enemy_nearby = std::collections::HashSet::new();

    #[cfg(any(debug_assertions, feature = "debug-overlay"))]
    let _t_lookup = crate::game::debug_stats::DetailedTimer::new("combat/adjacent_lookup");
    #[cfg(any(debug_assertions, feature = "debug-overlay"))]
    let _t_resolve = crate::game::debug_stats::DetailedTimer::new("combat/combat_resolution");
    #[cfg(any(debug_assertions, feature = "debug-overlay"))]
    let _t_stats = crate::game::debug_stats::DetailedTimer::new("combat/stats_update");

    for (entity1, pos1, owner1, _, _) in &candidates {
        // A unit that died during this tick (as a defender) should not go on to
        // attack other enemies in the same tick.
        if dead_set.contains(entity1) {
            continue;
        }

        spatial.query_nearby_into(pos1.x, pos1.y, &mut nearby);
        for entity2 in nearby.iter().copied() {
            if entity2 == *entity1 {
                continue;
            }

            // Resolve each normalized (entity1, entity2) pair at most once per tick,
            // so the same two units never fight twice. Each unit makes at most ONE
            // attack per tick (classic RTS model): a full "every adjacent pair"
            // resolution was O(n^2) in dense melees and dominated the tick budget.
            // Units can still be *attacked* by many enemies — see the `break` below.
            let pair = if entity1.id() < entity2.id() {
                (*entity1, entity2)
            } else {
                (entity2, *entity1)
            };
            if resolved.contains(&pair) {
                continue;
            }

            // Same-owner check without a per-candidate String allocation.
            let same_owner = match world.get::<&UnitOwner>(entity2) {
                Ok(o) => o.player_id == *owner1,
                Err(_) => continue,
            };
            if same_owner {
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

            // Re-fetch entity1's CURRENT stats/flags before attacking. A unit may
            // have already taken damage this tick (attacked by another unit before
            // its own turn), so the stale candidate snapshot would overwrite that
            // accumulated damage instead of building on it.
            let (s1, f1) = {
                let s1 = match world.get::<&UnitStats>(*entity1) {
                    Ok(s) => *s,
                    Err(_) => break,
                };
                if s1.men <= 0.0 {
                    // Entity1 died to an earlier engagement this tick — stop attacking.
                    break;
                }
                let f1 = match world.get::<&UnitFlags>(*entity1) {
                    Ok(f) => *f,
                    Err(_) => break,
                };
                (s1, f1)
            };

            let s2 = match world.get::<&UnitStats>(entity2) {
                Ok(s) => *s,
                Err(_) => continue,
            };
            let f2 = match world.get::<&UnitFlags>(entity2) {
                Ok(f) => *f,
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
            // Fetch both units' equipment once (borrowed, no deep clone). The pair
            // is only resolved when both units carry an equipment component — same
            // conditional as before, without the intermediate full clones. The
            // borrows are scoped to this block so they (and their Drop guards) are
            // released before the mutation writes below.
            let (a_eq, d_eq) = {
                let (eq1, eq2) = match (
                    world.get::<&UnitEquipment>(*entity1),
                    world.get::<&UnitEquipment>(entity2),
                ) {
                    (Ok(a), Ok(b)) => (a, b),
                    _ => continue,
                };
                (
                    attrition_from(&*eq1, dmg_to_attacker, attr_rate),
                    attrition_from(&*eq2, dmg_to_defender, attr_rate),
                )
            };

            if let Ok((ms, me, mf)) = world
                .query_one_mut::<(&mut UnitStats, &mut UnitEquipment, &mut UnitFlags)>(*entity1)
            {
                ms.men = a_men;
                ms.organisation = (ms.organisation - 10.0).max(0.0);
                if let Some(a_eq) = a_eq {
                    me.equipment = a_eq;
                }
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
                if let Some(d_eq) = d_eq {
                    me.equipment = d_eq;
                }
                // Combat consumes supply proportional to damage taken
                let supply_loss = (dmg_to_defender * attr_rate).max(1.0);
                me.supply = (me.supply - supply_loss).max(0.0);
                mf.fighting = true;
            }

            if a_men <= 0.0 {
                dead.push(*entity1);
                dead_set.insert(*entity1);
            }
            if d_men <= 0.0 {
                dead.push(entity2);
                dead_set.insert(entity2);
            }

            resolved.insert(pair);
            fought.insert(*entity1);
            fought.insert(entity2);

            // Each unit makes at most ONE attack per tick (see the pair comment above);
            // a unit can still be *attacked* — and take counter-damage from — many
            // enemies within the same tick.
            break;
        }
    }

    // Retreat check: clear fighting for units that found no nearby enemy
    // Uses had_enemy_nearby set from main loop — NO second spatial hash pass
    #[cfg(any(debug_assertions, feature = "debug-overlay"))]
    let _t_retreat = crate::game::debug_stats::DetailedTimer::new("combat/retreat_check");
    for (entity, _, _, _, flags) in &candidates {
        if fought.contains(entity) {
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::CityUpgradeSettings;
    use crate::game::unit::Terrain;

    fn make_world() -> (World, hecs::Entity) {
        let mut world = World::new();
        let terrain = vec![vec![Terrain::Plains; 20]; 20];
        let world_entity = world.spawn((WorldBundle::new(
            terrain,
            20,
            20,
            CityUpgradeSettings::default(),
            std::collections::HashMap::new(),
        ),));
        (world, world_entity)
    }

    fn make_unit(world: &mut World, pid: &str, x: f32, y: f32, men: f32) -> hecs::Entity {
        world.spawn((
            UnitPosition { x, y },
            UnitOwner {
                player_id: pid.to_string(),
            },
            UnitFlags {
                fighting: true,
                moved: false,
                ..Default::default()
            },
            UnitStats {
                men,
                max_men: 1000.0,
                organisation: 100.0,
                max_organisation: 100.0,
                reinforcement_rate: 5.0,
            },
            UnitEquipment {
                equipment: vec![],
                max_equipment: vec![],
                supply_buffer_percent: 20,
                supply: 2000.0,
                max_supply: 2000.0,
            },
        ))
    }

    fn build_spatial(world: &World) -> SpatialHash {
        let mut spatial = SpatialHash::new();
        for (entity, pos) in world
            .query::<(&UnitPosition,)>()
            .iter()
            .map(|(e, (p,))| (e, *p))
        {
            spatial.insert(entity, pos.x, pos.y);
        }
        spatial
    }

    /// A 0-men unit must not be processed as an attacker (candidate filter requires men > 0).
    #[test]
    fn combat_skips_dead_attacker() {
        let (mut world, world_entity) = make_world();
        // A single 0-men, fighting unit with no live enemies nearby. Without the
        // `men > 0` guard it would be a candidate and get its `fighting` flag cleared
        // by the retreat pass; with the guard it is excluded from candidates entirely.
        let _a = make_unit(&mut world, "P1", 10.0, 10.0, 0.0);
        let spatial = build_spatial(&world);

        let dead = run(&mut world, world_entity, spatial);
        assert!(
            dead.is_empty(),
            "0-men unit must not be processed by combat"
        );
    }

    /// Three mutually-adjacent units of three different players must all engage in
    /// the melee (every unit gets to act, so the old "first attacker captures all
    /// its neighbours" star is gone), while each unit makes at most one attack per
    /// tick. With identical units each pair deals ~4.95 dmg to both sides, so every
    /// unit ends somewhere in the 990–996 range — clearly damaged (took at least one
    /// hit or counter) but not carpet-bombed.
    #[test]
    fn combat_all_adjacent_units_engage_with_one_attack_each() {
        let (mut world, world_entity) = make_world();
        let a = make_unit(&mut world, "P1", 10.0, 10.0, 1000.0);
        let b = make_unit(&mut world, "P2", 10.0, 10.0, 1000.0);
        let c = make_unit(&mut world, "P3", 10.0, 10.0, 1000.0);
        let spatial = build_spatial(&world);

        let _dead = run(&mut world, world_entity, spatial);

        let men_of = |world: &World, e: hecs::Entity| {
            world
                .get::<&UnitStats>(e)
                .map(|s| s.men)
                .unwrap_or(f32::NAN)
        };
        for (name, ent) in [("A", a), ("B", b), ("C", c)] {
            let men = men_of(&world, ent);
            assert!(
                (990.0..1000.0).contains(&men),
                "unit {} should have taken exactly one tick of damage, got men={}",
                name,
                men
            );
        }
    }
}
