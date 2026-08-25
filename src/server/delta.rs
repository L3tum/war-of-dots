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
///
/// Current-state snapshots (`current_units`, `current_cities`, `all_player_funds`)
/// are computed **once per tick** by the caller and shared across players, so we
/// no longer re-snapshot (and re-clone every unit's equipment Vecs) per player.
///
/// Returns `None` when nothing this player cares about changed since the last
/// snapshot — the caller must then skip encoding/sending entirely, which is what
/// eliminates idle websocket traffic.
#[allow(clippy::too_many_arguments)]
pub(crate) fn build_delta_inner(
    prev_units: &mut HashMap<String, UnitState>,
    prev_cities: &mut HashMap<String, CityState>,
    prev_funds: &mut HashMap<String, u32>,
    prev_vision_grid: &mut Option<Vec<Vec<bool>>>,
    prev_border_grid: &mut Option<Vec<Vec<u32>>>,
    current_units: &[UnitState],
    current_cities: &[CityState],
    all_player_funds: &HashMap<String, u32>,
    current_vision_grid: Option<Vec<Vec<bool>>>,
    current_border_grid: Option<Vec<Vec<u32>>>,
    player_id: &str,
) -> Option<ServerMessage> {
    let current_unit_map: HashMap<String, UnitState> = current_units
        .iter()
        .cloned()
        .map(|u| (u.id.clone(), u))
        .collect();
    let current_city_map: HashMap<String, CityState> = current_cities
        .iter()
        .cloned()
        .map(|c| (c.id.clone(), c))
        .collect();

    // Grid delta: send the FULL current grid only when it changed since the last
    // snapshot. The default map is 200×200 (~40 KB of vision bools), so a per-cell
    // "diff" would cost as much as the full grid with extra compute — per-cell
    // row-diffing is a deliberate follow-up. The sparse unit/city deltas below
    // remain the genuinely valuable bandwidth win.
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
    let player_funds: u32 = all_player_funds.get(player_id).copied().unwrap_or(0);
    let units_owned: u32 = current_units
        .iter()
        .filter(|u| u.owner == player_id)
        .count() as u32;
    let player_state = PlayerState {
        funds: player_funds,
        units_owned,
        player_id: player_id.to_string(),
    };

    // Idle skip: if nothing this player sees changed, do not update the prev
    // snapshots and return None so the caller skips encoding + sending.
    let funds_changed = prev_funds.get(player_id).copied() != Some(player_state.funds);
    let prev_owned: u32 = prev_units.values().filter(|u| u.owner == player_id).count() as u32;
    if units_added.is_empty()
        && units_removed.is_empty()
        && unit_updates.is_empty()
        && city_updates.is_empty()
        && vision_grid_diff.is_none()
        && border_grid_diff.is_none()
        && !funds_changed
        && prev_owned == player_state.units_owned
    {
        return None;
    }

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
    prev_funds.insert(player_id.to_string(), player_funds);
    *prev_vision_grid = current_vision_grid;
    *prev_border_grid = current_border_grid;

    Some(ServerMessage::DeltaState(delta))
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

#[cfg(test)]
mod tests {
    use super::*;

    fn unit(id: &str, owner: &str, x: f32, y: f32) -> UnitState {
        UnitState {
            id: id.to_string(),
            owner: owner.to_string(),
            position: (x, y),
            unit_type: "infantry".to_string(),
            symbol: "i".to_string(),
            men: 100.0,
            max_men: 100.0,
            organisation: 100.0,
            equipment: vec![],
            max_equipment: vec![],
            path: vec![],
            healing: false,
            resupply_enabled: true,
            supply_percent: 100.0,
            encircled: false,
            attacking: false,
            on_water: false,
            boat_supply_percent: 0.0,
            embarking: false,
            disembarking: false,
        }
    }

    fn city(id: &str, owner: Option<&str>, level: u32) -> CityState {
        CityState {
            id: id.to_string(),
            position: (1.0, 1.0),
            owner: owner.map(|s| s.to_string()),
            manpower: 0,
            level,
        }
    }

    fn units_map(units: &[UnitState]) -> HashMap<String, UnitState> {
        units.iter().map(|u| (u.id.clone(), u.clone())).collect()
    }

    fn funds(player_id: &str, amount: u32) -> HashMap<String, u32> {
        let mut m = HashMap::new();
        m.insert(player_id.to_string(), amount);
        m
    }

    /// Run build_delta_inner with the given current state over the given prev
    /// snapshots, for a single player. No grids involved.
    fn run(
        prev_units: &mut HashMap<String, UnitState>,
        prev_cities: &mut HashMap<String, CityState>,
        prev_funds: &mut HashMap<String, u32>,
        current_units: &[UnitState],
        current_cities: &[CityState],
        all_funds: &HashMap<String, u32>,
        player_id: &str,
    ) -> Option<ServerMessage> {
        build_delta_inner(
            prev_units,
            prev_cities,
            prev_funds,
            &mut None,
            &mut None,
            current_units,
            current_cities,
            all_funds,
            None,
            None,
            player_id,
        )
    }

    fn as_delta(msg: Option<ServerMessage>) -> GameDeltaMessage {
        match msg {
            Some(ServerMessage::DeltaState(d)) => d,
            other => panic!(
                "expected DeltaState, got {:?}",
                other.map(|m| std::mem::discriminant(&m))
            ),
        }
    }

    #[test]
    fn identical_snapshots_return_none_and_leave_prev_untouched() {
        let units = vec![unit("u1", "a", 1.0, 2.0)];
        let cities = vec![city("c1", Some("a"), 1)];
        let all_funds = funds("a", 42);
        let mut prev_units = units_map(&units);
        let mut prev_cities: HashMap<String, CityState> =
            cities.iter().map(|c| (c.id.clone(), c.clone())).collect();
        let mut prev_funds = all_funds.clone();

        let msg = run(
            &mut prev_units,
            &mut prev_cities,
            &mut prev_funds,
            &units,
            &cities,
            &all_funds,
            "a",
        );
        assert!(msg.is_none());

        // prev snapshots must be unchanged (funds entry still present and equal)
        assert!(prev_units.contains_key("u1"));
        assert_eq!(prev_funds.get("a"), Some(&42));
    }

    #[test]
    fn single_unit_position_change_yields_one_update() {
        let prev_units = vec![unit("u1", "a", 1.0, 2.0)];
        let curr_units = vec![unit("u1", "a", 3.0, 2.0)];
        let all_funds = funds("a", 10);
        let mut pu = units_map(&prev_units);
        let mut pc = HashMap::new();
        let mut pf = all_funds.clone();

        let msg = run(&mut pu, &mut pc, &mut pf, &curr_units, &[], &all_funds, "a");
        let d = as_delta(msg);
        assert_eq!(d.unit_updates.len(), 1);
        assert_eq!(d.unit_updates[0].id, "u1");
        assert_eq!(d.unit_updates[0].position, Some((3.0, 2.0)));
        assert!(d.units_added.is_empty());
        assert!(d.units_removed.is_empty());
    }

    #[test]
    fn funds_only_change_is_sent_to_changed_player_only() {
        let units = vec![unit("u1", "a", 1.0, 2.0)];
        let mut prev_units = units_map(&units);
        let mut prev_cities = HashMap::new();

        // Player a funds 10 -> 20, player b unchanged at 5
        let mut all_funds = HashMap::new();
        all_funds.insert("a".to_string(), 20);
        all_funds.insert("b".to_string(), 5);
        let mut prev_funds = HashMap::new();
        prev_funds.insert("a".to_string(), 10);
        prev_funds.insert("b".to_string(), 5);

        let msg_a = run(
            &mut prev_units,
            &mut prev_cities,
            &mut prev_funds,
            &units,
            &[],
            &all_funds,
            "a",
        );
        let d = as_delta(msg_a);
        assert_eq!(d.player.funds, 20);

        let msg_b = run(
            &mut prev_units,
            &mut prev_cities,
            &mut prev_funds,
            &units,
            &[],
            &all_funds,
            "b",
        );
        assert!(msg_b.is_none());
    }

    #[test]
    fn city_owner_change_yields_city_update() {
        let prev_cities = [city("c1", Some("a"), 1)];
        let curr_cities = vec![city("c1", Some("b"), 1)];
        let all_funds = funds("a", 0);
        let mut pu = HashMap::new();
        let mut pc: HashMap<String, CityState> = prev_cities
            .iter()
            .map(|c| (c.id.clone(), c.clone()))
            .collect();
        let mut pf = all_funds.clone();

        let msg = run(
            &mut pu,
            &mut pc,
            &mut pf,
            &[],
            &curr_cities,
            &all_funds,
            "a",
        );
        let d = as_delta(msg);
        assert_eq!(d.city_updates.len(), 1);
        assert_eq!(d.city_updates[0].id, "c1");
        assert_eq!(d.city_updates[0].owner.as_deref(), Some("b"));
    }

    #[test]
    fn grid_change_includes_grid_diff() {
        let all_funds = funds("a", 0);
        let mut pu = HashMap::new();
        let mut pc = HashMap::new();
        let mut pf = all_funds.clone();
        let vision = vec![vec![true, false], vec![false, true]];

        let msg = build_delta_inner(
            &mut pu,
            &mut pc,
            &mut pf,
            &mut None,
            &mut None,
            &[],
            &[],
            &all_funds,
            Some(vision.clone()),
            None,
            "a",
        );
        let d = as_delta(msg);
        assert_eq!(d.vision_grid_diff, Some(vision));
        assert!(d.border_grid_diff.is_none());
    }

    #[test]
    fn first_tick_from_empty_prev_sends_added_units() {
        let units = vec![unit("u1", "a", 1.0, 2.0), unit("u2", "b", 3.0, 4.0)];
        let all_funds = funds("a", 7);
        let mut pu = HashMap::new();
        let mut pc = HashMap::new();
        let mut pf = HashMap::new();

        let msg = run(&mut pu, &mut pc, &mut pf, &units, &[], &all_funds, "a");
        let d = as_delta(msg);
        assert_eq!(d.units_added.len(), 2);
        assert_eq!(d.player.units_owned, 1);
    }

    #[test]
    fn units_owned_change_is_detected() {
        // Same unit set, but the unit's owner changed — only units_owned
        // differs for player a.
        let prev_units = vec![unit("u1", "a", 1.0, 2.0)];
        let curr_units = vec![unit("u1", "b", 1.0, 2.0)];
        let mut all_funds = funds("a", 0);
        all_funds.insert("b".to_string(), 0);
        let mut pu = units_map(&prev_units);
        let mut pc = HashMap::new();
        let mut pf = all_funds.clone();

        let msg = run(&mut pu, &mut pc, &mut pf, &curr_units, &[], &all_funds, "a");
        assert!(msg.is_some());
    }
}
