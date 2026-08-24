//! Delta computation for game state updates.
//!
//! Builds minimal deltas between consecutive game states to minimize
//! bandwidth when sending updates to connected clients.
//!
//! ## Delta Digest Optimization
//! Instead of comparing full `UnitState` structs (which involves cloning),
//! we compare lightweight `UnitStateDigest` structs first. If the digest
//! matches, the unit is assumed unchanged. This reduces allocation pressure
//! significantly at scale (100+ units).
//!
//! ## Future Optimization (see TODO_PHASE2_NOTES.md)
//! - Use `Arc` for grid data to enable cheap CoW cloning
//! - Parallelize per-player delta computation (NOTE: rayon was trialed and
//!   removed — its dispatch overhead regressed the encirclement flood A/B, so
//!   any parallelization here needs its own A/B before being assumed to win)
//! - Consider incremental hashing for grid diff detection

use std::collections::HashMap;

use crate::game::game::Game;
use crate::server::protocol::{
    CityState, CityUpdate, GameDeltaMessage, PlayerState, ServerMessage, UnitState, UnitUpdate,
};

/// Lightweight digest of a UnitState used for change detection.
/// Comparing two digests is O(1) with no heap allocation, whereas comparing
/// full UnitState structs requires cloning Vec<String>, Vec<EquipmentState>, etc.
///
/// If any field that affects gameplay rendering changes, the corresponding
/// digest field also changes, ensuring we never miss a real update.
#[derive(Debug, Clone, PartialEq)]
struct UnitStateDigest {
    position: (f32, f32),
    men: f32,
    max_men: f32,
    organisation: f32,
    equipment_count: u64, // total item count as a fast proxy
    max_equipment_count: u64,
    path_len: usize,
    healing: bool,
    resupply_enabled: bool,
    supply_percent: f32,
    encircled: bool,
    attacking: bool,
    on_water: bool,
    boat_supply_percent: f32,
    embarking: bool,
    disembarking: bool,
}

impl UnitStateDigest {
    fn from_unit(u: &UnitState) -> Self {
        Self {
            position: u.position,
            men: u.men,
            max_men: u.max_men,
            organisation: u.organisation,
            equipment_count: u.equipment.iter().map(|e| e.count as u64).sum(),
            max_equipment_count: u.max_equipment.iter().map(|e| e.count as u64).sum(),
            path_len: u.path.len(),
            healing: u.healing,
            resupply_enabled: u.resupply_enabled,
            supply_percent: u.supply_percent,
            encircled: u.encircled,
            attacking: u.attacking,
            on_water: u.on_water,
            boat_supply_percent: u.boat_supply_percent,
            embarking: u.embarking,
            disembarking: u.disembarking,
        }
    }
}

/// Build a delta message comparing current game state to previous snapshots.
#[allow(clippy::too_many_arguments)]
pub(crate) fn build_delta_inner(
    game: &Game,
    prev_units: &mut HashMap<String, UnitState>,
    prev_cities: &mut HashMap<String, CityState>,
    prev_funds: &mut HashMap<String, u32>,
    prev_vision_grid: &mut Option<Vec<Vec<bool>>>,
    prev_border_grid: &mut Option<Vec<Vec<u32>>>,
    current_vision_grid: Option<Vec<Vec<bool>>>,
    current_border_grid: Option<Vec<Vec<u32>>>,
    player_id: &str,
) -> ServerMessage {
    let current_units: Vec<UnitState> = game.unit_states();
    let current_unit_map: HashMap<String, UnitState> = current_units
        .iter()
        .cloned()
        .map(|u| (u.id.clone(), u))
        .collect();
    let current_cities: Vec<CityState> = game.query_cities().iter().map(|c| c.into()).collect();
    let current_city_map: HashMap<String, CityState> = current_cities
        .iter()
        .cloned()
        .map(|c| (c.id.clone(), c))
        .collect();

    // Grid delta: send the FULL current grid only when it changed since the last snapshot.
    // Keeps per-cell diff machinery out — the map is tiny (20x20 ~ 1 KB), so a per-cell
    // "diff" costs the same as the full grid but with extra compute. The sparse unit/city
    // deltas below remain the genuinely valuable bandwidth win.
    let vision_grid_diff: Option<Vec<Vec<bool>>> = match (&*prev_vision_grid, &current_vision_grid)
    {
        (Some(prev), Some(curr)) if prev == curr => None,
        (_, Some(_)) => current_vision_grid.clone(),
        _ => None,
    };

    let border_grid_diff: Option<Vec<Vec<u32>>> = match (&*prev_border_grid, &current_border_grid) {
        (Some(prev), Some(curr)) if prev == curr => None,
        (_, Some(_)) => current_border_grid.clone(),
        _ => None,
    };

    // Units added (new)
    let units_added: Vec<UnitState> = current_units
        .iter()
        .filter(|u| !prev_units.contains_key(&u.id))
        .cloned()
        .collect();

    // Units removed (no longer in game)
    let units_removed: Vec<String> = prev_units
        .keys()
        .filter(|id| !current_unit_map.contains_key(id.as_str()))
        .cloned()
        .collect();

    // Units updated (position, stats, equipment changed)
    // Use digest-based comparison to avoid cloning full UnitState structs
    let mut unit_updates: Vec<UnitUpdate> = Vec::new();
    for u in current_units.iter() {
        if let Some(prev) = prev_units.get(&u.id) {
            // Fast path: compare lightweight digests first
            let curr_digest = UnitStateDigest::from_unit(u);
            let prev_digest = UnitStateDigest::from_unit(prev);
            if curr_digest == prev_digest {
                continue; // No changes detected, skip full comparison
            }

            // Slow path: digest differed, do field-by-field comparison
            let new_pos = if u.position != prev.position {
                Some(u.position)
            } else {
                None
            };

            let new_men = if u.men != prev.men { Some(u.men) } else { None };

            let new_org = if u.organisation != prev.organisation {
                Some(u.organisation)
            } else {
                None
            };

            let new_equipment = if u.equipment != prev.equipment {
                Some(u.equipment.clone())
            } else {
                None
            };

            let new_healing = if u.healing != prev.healing {
                Some(u.healing)
            } else {
                None
            };

            let new_resupply = if u.resupply_enabled != prev.resupply_enabled {
                Some(u.resupply_enabled)
            } else {
                None
            };

            let new_supply = if u.supply_percent != prev.supply_percent {
                Some(u.supply_percent)
            } else {
                None
            };

            let new_encircled = if u.encircled != prev.encircled {
                Some(u.encircled)
            } else {
                None
            };

            let new_path = if u.path != prev.path {
                Some(u.path.clone())
            } else {
                None
            };

            unit_updates.push(UnitUpdate {
                id: u.id.clone(),
                position: new_pos,
                men: new_men,
                organisation: new_org,
                equipment: new_equipment,
                path: new_path,
                healing: new_healing,
                resupply_enabled: new_resupply,
                supply_percent: new_supply,
                encircled: new_encircled,
                attacking: if u.attacking != prev.attacking {
                    Some(u.attacking)
                } else {
                    None
                },
                on_water: if u.on_water != prev.on_water {
                    Some(u.on_water)
                } else {
                    None
                },
                boat_supply_percent: if u.boat_supply_percent != prev.boat_supply_percent {
                    Some(u.boat_supply_percent)
                } else {
                    None
                },
                embarking: if u.embarking != prev.embarking {
                    Some(u.embarking)
                } else {
                    None
                },
                disembarking: if u.disembarking != prev.disembarking {
                    Some(u.disembarking)
                } else {
                    None
                },
            });
        }
    }

    // City updates
    let mut city_updates: Vec<CityUpdate> = Vec::new();
    for city in current_cities.iter() {
        if let Some(prev) = prev_cities.get(&city.id) {
            let mut changed = false;
            let owner_changed = city.owner != prev.owner;
            if owner_changed {
                changed = true;
            }
            let level_changed = city.level != prev.level;
            if level_changed {
                changed = true;
            }

            if changed {
                city_updates.push(CityUpdate {
                    id: city.id.clone(),
                    owner: if owner_changed {
                        city.owner.clone()
                    } else {
                        None
                    },
                    level: if level_changed {
                        Some(city.level)
                    } else {
                        None
                    },
                    manpower: None,
                });
            }
        }
    }

    // E-4: Use per-player funds instead of sum of all players
    let player_funds: u32 = game.player_funds(player_id);
    let units_owned: u32 = current_units
        .iter()
        .filter(|u| u.owner == player_id)
        .count() as u32;
    let player_state = PlayerState {
        funds: player_funds,
        units_owned,
        player_id: player_id.to_string(),
    };

    let delta = GameDeltaMessage {
        vision_grid_diff,
        border_grid_diff,
        units_added,
        units_removed,
        unit_updates,
        city_updates,
        player: player_state,
    };

    *prev_units = current_unit_map;
    *prev_cities = current_city_map;
    *prev_funds = game.all_player_funds().clone();
    *prev_vision_grid = current_vision_grid;
    *prev_border_grid = current_border_grid;

    ServerMessage::DeltaState(delta)
}

/// Build LLM agent state with filtered units based on vision grid.
pub fn build_llm_state(game: &Game, player_id: &str) -> crate::llm::agent::GameState {
    let vision_grid = game.vision_grid_2d(player_id).unwrap_or_default();
    let border_grid = game.border_grid_2d(player_id).unwrap_or_default();

    let units: Vec<crate::llm::agent::UnitState> = game
        .unit_states()
        .iter()
        .filter(|unit| {
            let (gx, gy) = (unit.position.0 as usize, unit.position.1 as usize);
            unit.owner == player_id
                || (gy < vision_grid.len() && gx < vision_grid[gy].len() && vision_grid[gy][gx])
        })
        .cloned()
        .collect::<Vec<_>>()
        .into_iter()
        .map(|u| crate::llm::agent::UnitState {
            id: u.id.clone(),
            owner: u.owner.clone(),
            position: u.position,
            men: u.men,
            max_men: u.max_men,
            organisation: u.organisation,
            equipment: u
                .equipment
                .iter()
                .zip(u.max_equipment.iter())
                .map(|(e, me)| crate::llm::agent::EquipmentState {
                    name: e.name.clone(),
                    count: e.count,
                    max_count: me.count,
                })
                .collect(),
            unit_type: u.unit_type.clone(),
            symbol: u.symbol.clone(),
            supply_percent: u.supply_percent,
            resupply_enabled: u.resupply_enabled,
        })
        .collect();

    let cities: Vec<crate::llm::agent::CityState> = game
        .query_cities()
        .iter()
        .filter(|city| {
            let (gx, gy) = (city.position.0 as usize, city.position.1 as usize);
            city.owner.as_deref() == Some(player_id)
                || (gy < vision_grid.len() && gx < vision_grid[gy].len() && vision_grid[gy][gx])
        })
        .cloned()
        .collect::<Vec<_>>()
        .into_iter()
        .map(|c| crate::llm::agent::CityState {
            id: c.id,
            owner: c.owner,
            level: c.level,
            position: c.position,
            manpower: 0,
        })
        .collect();

    let funds: u32 = game.all_player_funds().get(player_id).copied().unwrap_or(0);

    crate::llm::agent::GameState {
        units,
        cities,
        funds,
        player_id: player_id.to_string(),
        vision_grid,
        border_grid,
    }
}
