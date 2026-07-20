//! CityCaptureSystem — capture cities when units stand on them.

use crate::game::components::*;
use crate::game::game::PlayerGameStats;
use crate::game::world_ext::WorldExt;
use hecs::World;
use std::collections::{BTreeMap, HashMap};

/// Squared capture range (1.5^2) — avoids sqrt() per unit-city pair
const CAPTURE_RANGE_SQ: f32 = 2.25;

pub fn run(
    world: &mut World,
    world_entity: hecs::Entity,
    player_stats: &mut HashMap<String, PlayerGameStats>,
) {
    // Scoped extraction: cities Vec is small (~20 entries), clone is cheap
    let cities = match world.world_data(world_entity) {
        Some(wb) => wb.cities.cities.clone(),
        None => return,
    };

    let units: Vec<(String, UnitPosition)> = world
        .query::<(&UnitOwner, &UnitPosition)>()
        .iter()
        .map(|(_, (owner, pos))| (owner.player_id.clone(), *pos))
        .collect();

    // Collect all capture candidates, then pick one winner per city deterministically
    // BTreeMap ensures deterministic iteration order (sorted by city_id)
    let city_ids_changed: Vec<(String, String)> = {
        let mut changed: BTreeMap<String, String> = BTreeMap::new();
        for (owner_id, pos) in &units {
            for city in &cities {
                let dx = pos.x - city.position.0;
                let dy = pos.y - city.position.1;
                let dist_sq = dx * dx + dy * dy;
                if dist_sq < CAPTURE_RANGE_SQ && city.owner.as_deref() != Some(owner_id.as_str()) {
                    // Last qualifying unit per city wins (deterministic due to BTreeMap)
                    changed.insert(city.id.clone(), owner_id.clone());
                }
            }
        }
        changed.into_iter().collect()
    };

    // Apply city ownership changes in a single mutable borrow
    if !city_ids_changed.is_empty() {
        if let Some(wb) = world.world_data_mut(world_entity) {
            for (city_id, new_owner) in &city_ids_changed {
                if let Some(city) = wb.cities.cities.iter_mut().find(|c| c.id == *city_id) {
                    city.owner = Some(new_owner.clone());
                }
            }
        }
    }

    // Update player stats
    for (_, new_owner) in &city_ids_changed {
        if let Some(stats) = player_stats.get_mut(new_owner) {
            stats.cities_captured += 1;
        }
    }
}
