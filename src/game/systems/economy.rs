//! EconomySystem — generate funds for owned cities every N ticks.
//!
//! Income per city = `funds_per_city` * (1 + manpower_bonus) * income_multiplier(level)
//!
//! - `manpower_bonus` scales from 0 (empty garrison) to 1 (full garrison)
//! - `income_multiplier` comes from city upgrade level

use crate::game::world_ext::WorldExt;
use hecs::World;

/// Economy tick: generate funds and manpower for each owned city into global pools.
///
/// Per city:
/// - Funds: funds_per_city * income_multiplier(level)
/// - Manpower: manpower_regen_per_tick * level
///
/// Per-player funds earned this tick are written into `earned` (a caller-owned
/// scratch cleared here) instead of building a fresh HashMap per call. The
/// persistent funds/manpower maps are updated via `get_mut`, cloning the owner
/// key only on a player's first city this tick.
pub fn run(
    world: &mut World,
    world_entity: hecs::Entity,
    funds_per_city: f32,
    earned: &mut Vec<(String, u32)>,
) {
    let wb = match world.world_data_mut(world_entity) {
        Some(wb) => wb,
        None => return,
    };

    earned.clear();
    let settings = &wb.city_upgrades.settings;

    for city in &wb.cities.cities {
        if let Some(owner) = &city.owner {
            let income_mult = settings.income_multiplier(city.level);
            let funds_gain = funds_per_city * income_mult;
            let gain_rounded = funds_gain as u32;

            // Funds: add to the persistent pool; clone the key only when the
            // player's first city is processed this tick.
            match wb.funds.funds.get_mut(owner) {
                Some(f) => *f = f.saturating_add(gain_rounded),
                None => {
                    wb.funds.funds.insert(owner.clone(), gain_rounded);
                }
            }

            // earned: accumulate per player (linear scan over ≤ ~12 players),
            // cloning the key only on first occurrence.
            match earned.iter_mut().find(|(pid, _)| pid == owner) {
                Some(e) => e.1 = e.1.saturating_add(gain_rounded),
                None => earned.push((owner.clone(), gain_rounded)),
            }

            // Manpower generation into global pool (same clone-once pattern).
            let mp_gain = (settings.manpower_regen_per_tick * city.level as f32) as u32;
            match wb.funds.manpower.get_mut(owner) {
                Some(m) => *m = m.saturating_add(mp_gain),
                None => {
                    wb.funds.manpower.insert(owner.clone(), mp_gain);
                }
            }
        }
    }
}
