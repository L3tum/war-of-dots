//! SupplySystem — resupply units from global economy pools.
//!
//! Each tick, units within supply range of friendly cities receive:
//!
//! 1. **Manpower (men)**: deducted from global player manpower pool,
//!    added to unit.men (capped at max_men). Rate limited by `recruitment_rate`.
//! 2. **Equipment**: deducted from global player funds at `cost_per_item`.
//!
//! Supply range uses per-city `supply_range` (grows with city level).
//! Multi-city: closest city at full rate, secondary at decayed rate.

use std::collections::HashMap;

use crate::game::components::*;
use crate::game::game::TickContext;
use crate::game::world_ext::WorldExt;
use hecs::World;

/// Snapshot of a city for planning resupply.
#[derive(Clone)]
struct CitySnapshot {
    position: (f32, f32),
    supply_range_sq: f32,
}

/// Planned resupply for a single unit.
type ResupplyPlan = Vec<(hecs::Entity, String, f32, Vec<(String, u32)>)>;

pub fn run(
    world: &mut World,
    world_entity: hecs::Entity,
    ctx: &TickContext,
    recruitment_rate: f32,
) {
    let decay = {
        let wb = match world.world_data(world_entity) {
            Some(wb) => wb,
            None => return,
        };
        wb.city_upgrades.settings.secondary_city_decay
    };

    // Phase 1: snapshot city data + snapshot entity data
    let all_cities: Vec<CitySnapshot> = {
        let wb = match world.world_data(world_entity) {
            Some(wb) => wb,
            None => return,
        };
        wb.cities
            .cities
            .iter()
            .map(|c| CitySnapshot {
                position: c.position,
                supply_range_sq: c.supply_range * c.supply_range,
            })
            .collect()
    };

    // Collect entities needing resupply with their current stats
    let mut resupply_needed: ResupplyPlan = Vec::new();

    for (player_id, entities) in &ctx.units_by_player {
        let player_city_indices: Vec<usize> = all_cities
            .iter()
            .enumerate()
            .filter(|(i, _)| {
                let city = &ctx.cities.get(*i);
                city.map(|c| c.owner.as_deref() == Some(player_id.as_str()))
                    .unwrap_or(false)
            })
            .map(|(i, _)| i)
            .collect();

        if player_city_indices.is_empty() {
            continue;
        }

        for (entity, pos) in entities {
            // Find cities in range
            let mut cities_count = 0usize;
            for &ci in &player_city_indices {
                let city = &all_cities[ci];
                let dx = pos.0 - city.position.0;
                let dy = pos.1 - city.position.1;
                let dist_sq = dx * dx + dy * dy;
                if dist_sq < city.supply_range_sq {
                    cities_count += 1;
                }
            }

            if cities_count == 0 {
                continue;
            }

            // Compute total rate: closest at full rate, others decayed
            let total_rate: f32 = if cities_count == 1 {
                recruitment_rate
            } else {
                recruitment_rate + (cities_count as f32 - 1.0) * recruitment_rate * decay
            };

            // Check if unit needs resupply
            if let Ok(stats) = world.get::<&UnitStats>(*entity) {
                if stats.men >= stats.max_men {
                    continue;
                }
                let deficit = stats.max_men - stats.men;
                let gain = total_rate.min(deficit);
                if gain > 0.0 {
                    resupply_needed.push((*entity, player_id.clone(), gain, Vec::new()));
                }
            }
        }
    }

    // Phase 2: deduct from global pools
    let mp_deductions: HashMap<String, u32> = resupply_needed
        .iter()
        .map(|(_, pid, gain, _)| (pid.clone(), *gain as u32))
        .fold(HashMap::new(), |mut acc, (pid, amt)| {
            *acc.entry(pid).or_insert(0) += amt;
            acc
        });

    {
        let wb = match world.world_data_mut(world_entity) {
            Some(wb) => wb,
            None => return,
        };
        for (pid, amount) in mp_deductions {
            if let Some(mp) = wb.funds.manpower.get_mut(&pid) {
                *mp = mp.saturating_sub(amount);
            }
        }
    }

    // Phase 3: apply to ECS
    for (entity, _pid, gain, _eq_adds) in resupply_needed {
        if let Ok((stats,)) = world.query_one_mut::<(&mut UnitStats,)>(entity) {
            stats.men = (stats.men + gain).min(stats.max_men);
        }
    }
}
