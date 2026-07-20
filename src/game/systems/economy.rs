//! EconomySystem — generate funds for owned cities every N ticks.
//!
//! Income per city = `funds_per_city` * (1 + manpower_bonus) * income_multiplier(level)
//!
//! - `manpower_bonus` scales from 0 (empty garrison) to 1 (full garrison)
//! - `income_multiplier` comes from city upgrade level

use std::collections::HashMap;

use crate::game::world_ext::WorldExt;
use hecs::World;

/// Economy tick: generate funds and manpower for each owned city into global pools.
///
/// Per city:
/// - Funds: funds_per_city * income_multiplier(level)
/// - Manpower: manpower_regen_per_tick * level
///
/// Returns a map of player_id -> funds earned this tick.
pub fn run(
    world: &mut World,
    world_entity: hecs::Entity,
    funds_per_city: f32,
) -> HashMap<String, u32> {
    let wb = match world.world_data_mut(world_entity) {
        Some(wb) => wb,
        None => return HashMap::new(),
    };

    let mut earned: HashMap<String, u32> = HashMap::new();
    let settings = &wb.city_upgrades.settings;

    for city in &wb.cities.cities {
        if let Some(owner) = &city.owner {
            let income_mult = settings.income_multiplier(city.level);
            let funds_gain = funds_per_city * income_mult;
            let gain_rounded = funds_gain as u32;
            let funds = wb.funds.funds.entry(owner.clone()).or_insert(0);
            *funds = funds.saturating_add(gain_rounded);
            *earned.entry(owner.clone()).or_insert(0) += gain_rounded;

            // Manpower generation into global pool
            let mp_gain = (settings.manpower_regen_per_tick * city.level as f32) as u32;
            let manpower = wb.funds.manpower.entry(owner.clone()).or_insert(0);
            *manpower = manpower.saturating_add(mp_gain);
        }
    }

    earned
}
