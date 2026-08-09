//! Replay playback
#![allow(dead_code)]

use crate::game::game::Game;
use serde::{Deserialize, Serialize};
use std::fs::File;
use std::io::{BufRead, BufReader};

pub struct ReplayPlayer {
    pub game: Game,
    pub moves: Vec<ReplayEvent>,
    pub current_index: usize,
    /// Stored from the GameRecord header — contains game settings
    /// so we can replay with the exact same settings as the original game.
    pub header: Option<crate::replay::recorder::GameRecord>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum ReplayEvent {
    Move {
        tick: u64,
        player: String,
        action_type: String,
        params: serde_json::Value,
    },
    End {
        time: u64,
    },
}

impl ReplayPlayer {
    pub fn from_file(path: &str, game: Game) -> anyhow::Result<Self> {
        let file = File::open(path)?;
        let reader = BufReader::new(file);
        let mut moves = Vec::new();
        let mut header: Option<crate::replay::recorder::GameRecord> = None;
        for line in reader.lines() {
            let line = line?;
            if line.starts_with('{') {
                // Try to parse as GameRecord header first
                if header.is_none() {
                    if let Ok(h) =
                        serde_json::from_str::<crate::replay::recorder::GameRecord>(&line)
                    {
                        header = Some(h);
                        continue;
                    }
                }
                // Then try as MoveEvent
                if !line.contains("event") {
                    // GameRecord header already parsed, skip
                } else if let Ok(event) = serde_json::from_str::<ReplayEvent>(&line) {
                    moves.push(event);
                }
            }
        }
        Ok(ReplayPlayer {
            game,
            moves,
            current_index: 0,
            header,
        })
    }

    #[allow(clippy::should_implement_trait)]
    pub fn next(&mut self) -> Option<&ReplayEvent> {
        let event = self.moves.get(self.current_index)?;
        self.current_index += 1;
        Some(event)
    }

    pub fn apply_event(&mut self, event: &ReplayEvent) {
        if let ReplayEvent::Move {
            player: _player,
            action_type,
            params,
            ..
        } = event
        {
            // Apply action to game
            match action_type.as_str() {
                "move_troops" => {
                    if let (Some(troop_id), Some(path)) = (
                        params.get("troop_id").and_then(|v| v.as_str()),
                        params.get("path").and_then(|v| v.as_array()),
                    ) {
                        let path_coords: Vec<(f32, f32)> = path
                            .iter()
                            .filter_map(|arr| {
                                let [x, y] = [&arr[0], &arr[1]];
                                Some((x.as_f64()? as f32, y.as_f64()? as f32))
                            })
                            .collect();
                        if let Some(owner) = self.game.get_unit_owner(troop_id) {
                            self.game.move_troops(troop_id, &owner, path_coords);
                        }
                    }
                }
                "recruit_unit" => {
                    if let (Some(city_id), Some(unit_type_name), Some(owner)) = (
                        params.get("city_id").and_then(|v| v.as_str()),
                        params.get("unit_type_name").and_then(|v| v.as_str()),
                        params.get("owner").and_then(|v| v.as_str()),
                    ) {
                        // Clone the unit type to avoid borrow conflict
                        let ut: Option<crate::game::economy::UnitType> = self
                            .game
                            .settings()
                            .unit_types
                            .iter()
                            .find(|ut| ut.name == unit_type_name)
                            .cloned();
                        if let Some(u) = ut {
                            self.game.recruit_unit(city_id, &u, owner);
                        }
                    }
                }
                "toggle_healing" => {
                    if let Some(unit_id) = params.get("unit_id").and_then(|v| v.as_str()) {
                        if let Some(enabled) = params.get("enabled").and_then(|v| v.as_bool()) {
                            if let Some(owner) = self.game.get_unit_owner(unit_id) {
                                self.game.toggle_healing(unit_id, &owner, enabled);
                            }
                        }
                    }
                }
                "toggle_resupply" => {
                    if let Some(unit_id) = params.get("unit_id").and_then(|v| v.as_str()) {
                        if let Some(enabled) = params.get("enabled").and_then(|v| v.as_bool()) {
                            if let Some(owner) = self.game.get_unit_owner(unit_id) {
                                self.game.toggle_resupply(unit_id, &owner, enabled);
                            }
                        }
                    }
                }
                "upgrade_city" => {
                    if let (Some(city_id), Some(owner)) = (
                        params.get("city_id").and_then(|v| v.as_str()),
                        params.get("owner").and_then(|v| v.as_str()),
                    ) {
                        let _ = self.game.upgrade_city(city_id, owner);
                    }
                }
                "embark_unit" => {
                    if let (Some(unit_id), Some(owner)) = (
                        params.get("unit_id").and_then(|v| v.as_str()),
                        params.get("owner").and_then(|v| v.as_str()),
                    ) {
                        self.game.embark_unit(unit_id, owner);
                    }
                }
                "disembark_unit" => {
                    if let (Some(unit_id), Some(owner)) = (
                        params.get("unit_id").and_then(|v| v.as_str()),
                        params.get("owner").and_then(|v| v.as_str()),
                    ) {
                        self.game.disembark_unit(unit_id, owner);
                    }
                }
                _ => {
                    tracing::warn!("Unknown replay action type: {}", action_type);
                }
            }
        }
    }

    /// Build full state for serialization.
    pub fn full_state(&self) -> crate::server::protocol::GameStateMessage {
        use crate::server::protocol::GameStateMessage;
        let units: Vec<crate::server::protocol::UnitState> = self.game.unit_states();
        let units_owned = units.len() as u32;
        let cities: Vec<crate::server::protocol::CityState> =
            self.game.query_cities().iter().map(|c| c.into()).collect();
        let all_funds = self.game.all_player_funds();
        let player_funds: u32 = all_funds.values().sum();
        GameStateMessage {
            vision_grid: self.game.supervisor_vision_grid(),
            border_grid: self.game.supervisor_border_grid(),
            terrain: self.game.terrain_grid_2d(),
            units,
            cities,
            player: crate::server::protocol::PlayerState {
                funds: player_funds,
                units_owned,
                player_id: String::new(),
            },
        }
    }

    /// Get all player funds.
    pub fn all_player_funds(&self) -> std::collections::HashMap<String, u32> {
        self.game.all_player_funds()
    }
}
