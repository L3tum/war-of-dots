//! HealingSystem — heal units near friendly cities.

use crate::game::components::*;
use crate::game::game::TickContext;
use hecs::World;

pub fn run(world: &mut World, ctx: &TickContext) {
    let heal_rate = 2.0f32;
    let heal_range_sq = 1.5 * 1.5; // compare squared distance to avoid sqrt

    // Collect entities that need healing per player
    for (player_id, entities) in &ctx.units_by_player {
        // Get player's cities (filter once per player)
        let player_cities: Vec<_> = ctx
            .cities
            .iter()
            .filter(|c| c.owner.as_deref() == Some(player_id.as_str()))
            .collect();

        if player_cities.is_empty() {
            continue;
        }

        for (entity, pos) in entities {
            // Check flags and healing status
            let (healing_flag,) = match world.query_one_mut::<(&UnitFlags,)>(*entity) {
                Ok(f) => f,
                Err(_) => continue,
            };
            if !healing_flag.healing {
                continue;
            }

            // Check if near any friendly city
            let mut near_city = false;
            for city in &player_cities {
                let dx = pos.0 - city.position.0;
                let dy = pos.1 - city.position.1;
                let dist_sq = dx * dx + dy * dy;
                if dist_sq < heal_range_sq {
                    near_city = true;
                    break;
                }
            }

            if near_city {
                // Heal organisation
                if let Ok((ms,)) = world.query_one_mut::<(&mut UnitStats,)>(*entity) {
                    ms.organisation = (ms.organisation + heal_rate).min(ms.max_organisation);
                }
                // Heal equipment
                if let Ok((me,)) = world.query_one_mut::<(&mut UnitEquipment,)>(*entity) {
                    for (eq, max_eq) in me.equipment.iter_mut().zip(&me.max_equipment) {
                        if eq.count < max_eq.max_count {
                            eq.count += 1;
                        }
                    }
                }
            }
        }
    }
}
