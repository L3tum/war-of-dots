//! CityCaptureSystem — capture cities when units stand on them.

use crate::game::components::*;
use crate::game::game::{PlayerGameStats, TickContext};
use crate::game::world_ext::WorldExt;
use hecs::World;
use std::collections::{BTreeMap, HashMap};

/// Squared capture range (1.5^2) — avoids sqrt() per unit-city pair
const CAPTURE_RANGE_SQ: f32 = 2.25;

pub fn run(
    world: &mut World,
    world_entity: hecs::Entity,
    player_stats: &mut HashMap<String, PlayerGameStats>,
    ctx: &TickContext,
) -> Vec<String> {
    // Scoped extraction: cities Vec is small (~20 entries), clone is cheap
    let cities = match world.world_data(world_entity) {
        Some(wb) => wb.cities.cities.clone(),
        None => return Vec::new(),
    };

    // Resolve each city's owner to a u8 player index once (≤ ~20 cities).
    // A city owned by a player with no units (absent from the index) resolves to
    // None — any player's unit may capture it, matching the old String compare.
    let city_owner_idx: Vec<Option<u8>> = cities
        .iter()
        .map(|c| {
            c.owner
                .as_ref()
                .and_then(|o| ctx.player_id_to_index.get(o).copied())
        })
        .collect();

    // Collect capture candidates in one query pass. Owner identity is compared as
    // a u8 index (O(1) equality — no per-unit String clone, no per-pair String
    // compare); the owner String is only cloned when a capture actually fires.
    // BTreeMap ensures deterministic iteration order (sorted by city_id).
    let city_ids_changed: Vec<(String, String)> = {
        let mut changed: BTreeMap<String, String> = BTreeMap::new();
        for (entity, owner, pos) in world
            .query::<(hecs::Entity, &UnitOwner, &UnitPosition)>()
            .iter()
        {
            // Compact owner lookup (see TickContext); skip if the entity is
            // somehow absent — matches combat's mover-scan defensive pattern.
            let unit_owner_idx = match ctx.owner_id_by_entity.get(&entity).copied() {
                Some(i) => i,
                None => continue,
            };
            for (city, city_owner_idx) in cities.iter().zip(city_owner_idx.iter()) {
                let dx = pos.x - city.position.0;
                let dy = pos.y - city.position.1;
                let dist_sq = dx * dx + dy * dy;
                if dist_sq < CAPTURE_RANGE_SQ && *city_owner_idx != Some(unit_owner_idx) {
                    // Last qualifying unit per city wins (deterministic due to BTreeMap).
                    // Clone the owner string only now — a capture is rare.
                    changed.insert(city.id.clone(), owner.player_id.clone());
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

    // Return the list of city IDs whose ownership changed
    city_ids_changed.into_iter().map(|(id, _)| id).collect()
}
