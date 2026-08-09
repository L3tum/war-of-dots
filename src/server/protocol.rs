//! Message protocol between server and client

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct GameStateMessage {
    pub vision_grid: Option<Vec<Vec<bool>>>,
    pub border_grid: Option<Vec<Vec<u32>>>,
    pub terrain: Vec<Vec<String>>,
    pub units: Vec<UnitState>,
    pub cities: Vec<CityState>,
    pub player: PlayerState,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct GameDeltaMessage {
    pub vision_grid_diff: Option<Vec<Vec<bool>>>,
    pub border_grid_diff: Option<Vec<Vec<u32>>>,
    pub units_added: Vec<UnitState>,
    pub units_removed: Vec<String>,
    pub unit_updates: Vec<UnitUpdate>,
    pub city_updates: Vec<CityUpdate>,
    pub player: PlayerState,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct UnitState {
    pub id: String,
    pub owner: String,
    pub position: (f32, f32),
    pub unit_type: String,
    pub symbol: String,
    pub men: f32,
    pub max_men: f32,
    pub organisation: f32,
    pub equipment: Vec<EquipmentState>,
    pub max_equipment: Vec<EquipmentState>,
    pub path: Vec<(f32, f32)>,
    pub healing: bool,
    pub resupply_enabled: bool,
    pub supply_percent: f32,
    pub encircled: bool,
    pub attacking: bool,
    /// Whether this unit is currently traveling on water
    pub on_water: bool,
    /// Boat supply as percentage of max boat supply (0-100)
    pub boat_supply_percent: f32,
    /// Whether this unit is embarking (transitioning from land to water)
    pub embarking: bool,
    /// Whether this unit is disembarking (transitioning from water to land)
    pub disembarking: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, JsonSchema)]
pub struct EquipmentState {
    pub name: String,
    pub count: u32,
    pub cost_per_item: f32,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct UnitUpdate {
    pub id: String,
    pub position: Option<(f32, f32)>,
    pub men: Option<f32>,
    pub organisation: Option<f32>,
    pub equipment: Option<Vec<EquipmentState>>,
    pub path: Option<Vec<(f32, f32)>>,
    pub healing: Option<bool>,
    pub resupply_enabled: Option<bool>,
    pub supply_percent: Option<f32>,
    pub encircled: Option<bool>,
    pub attacking: Option<bool>,
    pub on_water: Option<bool>,
    pub boat_supply_percent: Option<f32>,
    pub embarking: Option<bool>,
    pub disembarking: Option<bool>,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct CityState {
    pub id: String,
    pub position: (f32, f32),
    pub owner: Option<String>,
    pub manpower: u32,
    pub level: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct CityUpdate {
    pub id: String,
    pub manpower: Option<u32>,
    pub owner: Option<String>,
    pub level: Option<u32>,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct PlayerState {
    pub funds: u32,
    pub units_owned: u32,
    /// The connected player's own id (e.g. "player_a3f2b1c0"). Empty for spectators.
    pub player_id: String,
}

// Client -> server messages
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[allow(dead_code)]
pub enum ClientMessage {
    MoveTroop {
        troop_id: String,
        path: Vec<(f32, f32)>,
    },
    MoveUnit {
        #[serde(rename = "unitId")]
        unit_id: String,
        path: Vec<(f32, f32)>,
    },
    RecruitUnit {
        city_id: String,
        unit_type: String,
    },
    ToggleHealing {
        unit_id: String,
        enabled: bool,
    },
    ToggleResupply {
        #[serde(rename = "unitId")]
        unit_id: String,
        enabled: bool,
    },
    UpgradeCity {
        #[serde(rename = "cityId")]
        city_id: String,
    },
    ToggleAdvisor {
        enabled: bool,
    },
    EmbarkUnit {
        #[serde(rename = "unitId")]
        unit_id: String,
    },
    DisembarkUnit {
        #[serde(rename = "unitId")]
        unit_id: String,
    },
    GetLeaderboard,
    LobbyMessage(LobbyMessage),
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[allow(dead_code)]
pub enum LobbyMessage {
    CreateLobby {
        settings: LobbySettings,
        password: Option<String>,
        player_name: Option<String>,
    },
    JoinLobby {
        lobby_code: String,
        password: Option<String>,
        player_name: Option<String>,
    },
    SetSlotLlm {
        slot: u32,
        llm_provider: String,
    },
    /// Add a new LLM-only player to the lobby (no WebSocket connection required)
    AddLlmPlayer {
        name: String,
        llm_provider: String,
    },
    /// Set the LLM model for a slot. The provider, API key, and base URL are resolved
    /// server-side from the configured LLM providers — no secrets cross the wire
    /// (see D-1).
    SetSlotLlmConfig {
        slot: u32,
        model: String,
    },
    StartGame,
}

/// Configuration for a custom unit type in lobby settings
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct CustomUnitType {
    pub name: String,
    pub symbol: String,
    pub men: u32,
    pub equipment: Vec<(String, u32)>, // (name, count)
    pub soft_attack: f32,
    pub soft_defense: f32,
    pub hard_attack: f32,
    pub hard_defense: f32,
    pub speed: f32,
    pub max_organisation: f32,
    pub reinforcement_rate: f32,
    pub recruitment_cost: u32,
}

/// City upgrade and supply system settings (single definition lives in
/// `crate::config` and is shared between the wire protocol and game logic).
pub use crate::config::CityUpgradeSettings;

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct LobbySettings {
    pub map: String,
    #[serde(default)]
    pub starting_funds: u32,
    /// Starting manpower pool per player. Always applies when recruiting —
    /// not gated behind city upgrades. Used for custom rule sets; the standard
    /// ruleset takes its value from the server config.
    #[serde(default)]
    pub initial_manpower: u32,
    #[serde(default, deserialize_with = "float_deser::deserialize_f32")]
    pub funds_per_city: f32,
    #[serde(default, deserialize_with = "float_deser::deserialize_f32")]
    pub game_speed: f32,
    pub use_standard_ruleset: bool,
    #[serde(default, deserialize_with = "float_deser::deserialize_f64")]
    pub llm_thinking_delay: f64,
    #[serde(default)]
    pub max_players: u32,
    /// Custom unit types when use_standard_ruleset is false
    #[serde(default)]
    pub custom_unit_types: Vec<CustomUnitType>,
    /// If true, spectators see union of all players' vision
    #[serde(default)]
    pub spectator_vision: bool,
    /// Ticks between recruitment actions for a player (0 = unlimited)
    #[serde(
        default = "default_recruitment_rate",
        deserialize_with = "float_deser::deserialize_f32"
    )]
    pub recruitment_rate: f32,
    /// Speed multiplier for water tiles
    #[serde(
        default = "default_water_speed",
        deserialize_with = "float_deser::deserialize_f32"
    )]
    pub water_speed: f32,
    /// City upgrade and supply system settings
    #[serde(default)]
    pub city_upgrade_settings: CityUpgradeSettings,
}

fn default_recruitment_rate() -> f32 {
    10.0
}

fn default_water_speed() -> f32 {
    1.2
}

/// Deserialize CBOR integers as floats. `cbor-x` encodes whole-number floats
/// (e.g. `3.0`) as CBOR integers, which ciborium rejects for `f32`/`f64`.
mod float_deser {
    use serde::{self, Deserialize, Deserializer};

    pub fn deserialize_f32<'de, D>(deserializer: D) -> Result<f32, D::Error>
    where
        D: Deserializer<'de>,
    {
        #[derive(Deserialize)]
        #[serde(untagged)]
        enum Number {
            F32(f32),
            F64(f64),
            I64(i64),
            U64(u64),
        }
        match Number::deserialize(deserializer)? {
            Number::F32(v) => Ok(v),
            Number::F64(v) => Ok(v as f32),
            Number::I64(v) => Ok(v as f32),
            Number::U64(v) => Ok(v as f32),
        }
    }

    pub fn deserialize_f64<'de, D>(deserializer: D) -> Result<f64, D::Error>
    where
        D: Deserializer<'de>,
    {
        #[derive(Deserialize)]
        #[serde(untagged)]
        enum Number {
            F64(f64),
            I64(i64),
            U64(u64),
        }
        match Number::deserialize(deserializer)? {
            Number::F64(v) => Ok(v),
            Number::I64(v) => Ok(v as f64),
            Number::U64(v) => Ok(v as f64),
        }
    }
}

/// Leaderboard entry
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct LeaderboardEntry {
    pub player_id: String,
    pub name: String,
    pub games_played: i64,
    pub games_won: i64,
    pub avg_casualties: f32,
}

// Server -> client messages
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub enum ServerMessage {
    FullState(GameStateMessage),
    DeltaState(GameDeltaMessage),
    LobbyCreated {
        lobby_code: String,
    },
    LobbyJoined {
        lobby_code: String,
    },
    LobbyUpdate {
        players: Vec<LobbyPlayer>,
        settings: LobbySettings,
    },
    Error {
        message: String,
    },
    LlmReasonLog {
        player_name: String,
        action: String,
        reason: String,
    },
    Advice {
        text: String,
    },
    ClearSpectator,
    Leaderboard {
        entries: Vec<LeaderboardEntry>,
    },
    GameOver {
        winner: String,
        game_id: String,
        message: String,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct LobbyPlayer {
    pub id: String,
    pub name: String,
    pub is_human: bool,
    pub llm_provider: Option<String>,
}

#[allow(dead_code)]
pub fn encode_message(msg: &ServerMessage) -> Vec<u8> {
    let mut buf = Vec::new();
    ciborium::into_writer(msg, &mut buf).unwrap_or_default();
    buf
}

#[allow(dead_code)]
pub fn decode_client_message(data: &[u8]) -> anyhow::Result<ClientMessage> {
    let msg: ClientMessage = ciborium::from_reader(data)?;
    Ok(msg)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Serialize a message to CBOR bytes for testing.
    fn to_cbor_bytes<T: serde::Serialize>(msg: &T) -> Vec<u8> {
        let mut buf = Vec::new();
        ciborium::into_writer(msg, &mut buf).unwrap();
        buf
    }

    // ---------------------------------------------------------------------------
    // CBOR encode/decode round-trips (structural checks, no PartialEq)
    // ---------------------------------------------------------------------------

    #[test]
    fn encode_decode_full_state() {
        let msg = ServerMessage::FullState(GameStateMessage {
            vision_grid: Some(vec![vec![true; 3]; 3]),
            border_grid: Some(vec![vec![1u32; 3]; 3]),
            terrain: vec![vec!["Plains".to_string(); 3]; 3],
            units: vec![UnitState {
                id: "unit_1".to_string(),
                owner: "p1".to_string(),
                position: (10.0, 20.0),
                unit_type: "infantry".to_string(),
                symbol: "I".to_string(),
                men: 1000.0,
                max_men: 1000.0,
                organisation: 100.0,
                equipment: vec![EquipmentState {
                    name: "rifle".to_string(),
                    count: 100,
                    cost_per_item: 2.5,
                }],
                max_equipment: vec![EquipmentState {
                    name: "rifle".to_string(),
                    count: 100,
                    cost_per_item: 2.5,
                }],
                path: vec![(11.0, 20.0)],
                healing: true,
                resupply_enabled: true,
                supply_percent: 100.0,
                encircled: false,
                attacking: false,
                on_water: false,
                boat_supply_percent: 0.0,
                embarking: false,
                disembarking: false,
            }],
            cities: vec![CityState {
                id: "city_0".to_string(),
                position: (5.0, 5.0),
                owner: Some("p1".to_string()),
                manpower: 10000,
                level: 1,
            }],
            player: PlayerState {
                funds: 1000,
                units_owned: 1,
                player_id: "player_a1b2c3d4".to_string(),
            },
        });

        let encoded = encode_message(&msg);
        let decoded: ServerMessage = ciborium::from_reader(encoded.as_slice()).unwrap();
        // Verify structure: should be FullState with correct data
        if let ServerMessage::FullState(state) = decoded {
            assert_eq!(state.units.len(), 1);
            assert_eq!(state.units[0].id, "unit_1");
            assert_eq!(state.units[0].position, (10.0, 20.0));
            assert_eq!(state.cities.len(), 1);
            assert_eq!(state.cities[0].id, "city_0");
            assert_eq!(state.player.funds, 1000);
            assert_eq!(state.player.units_owned, 1);
        } else {
            panic!("Expected FullState");
        }
    }

    #[test]
    fn encode_decode_delta_state() {
        let msg = ServerMessage::DeltaState(GameDeltaMessage {
            vision_grid_diff: None,
            border_grid_diff: None,
            units_added: vec![],
            units_removed: vec!["unit_old".to_string()],
            unit_updates: vec![UnitUpdate {
                id: "unit_1".to_string(),
                position: Some((11.0, 20.0)),
                men: Some(950.0),
                organisation: Some(90.0),
                equipment: None,
                path: None,
                healing: None,
                resupply_enabled: None,
                supply_percent: Some(95.0),
                encircled: Some(false),
                attacking: Some(true),
                on_water: None,
                boat_supply_percent: None,
                embarking: None,
                disembarking: None,
            }],
            city_updates: vec![CityUpdate {
                id: "city_0".to_string(),
                manpower: Some(9000),
                owner: None,
                level: None,
            }],
            player: PlayerState {
                funds: 1100,
                units_owned: 2,
                player_id: "player_a1b2c3d4".to_string(),
            },
        });

        let encoded = encode_message(&msg);
        let decoded: ServerMessage = ciborium::from_reader(encoded.as_slice()).unwrap();
        if let ServerMessage::DeltaState(delta) = decoded {
            assert_eq!(delta.units_removed.len(), 1);
            assert_eq!(delta.units_removed[0], "unit_old");
            assert_eq!(delta.unit_updates.len(), 1);
            assert_eq!(delta.unit_updates[0].id, "unit_1");
            assert_eq!(delta.unit_updates[0].position, Some((11.0, 20.0)));
            assert_eq!(delta.player.funds, 1100);
        } else {
            panic!("Expected DeltaState");
        }
    }

    // ---------------------------------------------------------------------------
    // ClientMessage encode/decode (structural checks)
    // ---------------------------------------------------------------------------

    #[test]
    fn client_message_move_troop_roundtrip() {
        let msg = ClientMessage::MoveTroop {
            troop_id: "unit_1".to_string(),
            path: vec![(10.0, 10.0), (11.0, 10.0), (12.0, 10.0)],
        };
        let bytes = to_cbor_bytes(&msg);
        let decoded = decode_client_message(&bytes).unwrap();
        if let ClientMessage::MoveTroop { troop_id, path } = decoded {
            assert_eq!(troop_id, "unit_1");
            assert_eq!(path.len(), 3);
            assert_eq!(path[0], (10.0, 10.0));
        } else {
            panic!("Expected MoveTroop");
        }
    }

    #[test]
    fn client_message_recruit_unit_roundtrip() {
        let msg = ClientMessage::RecruitUnit {
            city_id: "city_0".to_string(),
            unit_type: "infantry".to_string(),
        };
        let bytes = to_cbor_bytes(&msg);
        let decoded = decode_client_message(&bytes).unwrap();
        if let ClientMessage::RecruitUnit { city_id, unit_type } = decoded {
            assert_eq!(city_id, "city_0");
            assert_eq!(unit_type, "infantry");
        } else {
            panic!("Expected RecruitUnit");
        }
    }

    #[test]
    fn client_message_toggle_healing_roundtrip() {
        let msg = ClientMessage::ToggleHealing {
            unit_id: "unit_1".to_string(),
            enabled: true,
        };
        let bytes = to_cbor_bytes(&msg);
        let decoded = decode_client_message(&bytes).unwrap();
        if let ClientMessage::ToggleHealing { unit_id, enabled } = decoded {
            assert_eq!(unit_id, "unit_1");
            assert!(enabled);
        } else {
            panic!("Expected ToggleHealing");
        }
    }

    #[test]
    fn client_message_toggle_resupply_roundtrip() {
        let msg = ClientMessage::ToggleResupply {
            unit_id: "unit_1".to_string(),
            enabled: false,
        };
        let bytes = to_cbor_bytes(&msg);
        let decoded = decode_client_message(&bytes).unwrap();
        if let ClientMessage::ToggleResupply { unit_id, enabled } = decoded {
            assert_eq!(unit_id, "unit_1");
            assert!(!enabled);
        } else {
            panic!("Expected ToggleResupply");
        }
    }

    #[test]
    fn client_message_upgrade_city_roundtrip() {
        let msg = ClientMessage::UpgradeCity {
            city_id: "city_0".to_string(),
        };
        let bytes = to_cbor_bytes(&msg);
        let decoded = decode_client_message(&bytes).unwrap();
        if let ClientMessage::UpgradeCity { city_id } = decoded {
            assert_eq!(city_id, "city_0");
        } else {
            panic!("Expected UpgradeCity");
        }
    }

    #[test]
    fn client_message_get_leaderboard_roundtrip() {
        let msg = ClientMessage::GetLeaderboard;
        let bytes = to_cbor_bytes(&msg);
        let decoded = decode_client_message(&bytes).unwrap();
        assert!(matches!(decoded, ClientMessage::GetLeaderboard));
    }

    #[test]
    fn client_message_move_unit_roundtrip() {
        let msg = ClientMessage::MoveUnit {
            unit_id: "unit_42".to_string(),
            path: vec![(5.0, 5.0)],
        };
        let bytes = to_cbor_bytes(&msg);
        let decoded = decode_client_message(&bytes).unwrap();
        if let ClientMessage::MoveUnit { unit_id, path } = decoded {
            assert_eq!(unit_id, "unit_42");
            assert_eq!(path, vec![(5.0, 5.0)]);
        } else {
            panic!("Expected MoveUnit");
        }
    }

    #[test]
    fn client_message_toggle_advisor_roundtrip() {
        let msg = ClientMessage::ToggleAdvisor { enabled: true };
        let bytes = to_cbor_bytes(&msg);
        let decoded = decode_client_message(&bytes).unwrap();
        if let ClientMessage::ToggleAdvisor { enabled } = decoded {
            assert!(enabled);
        } else {
            panic!("Expected ToggleAdvisor");
        }
    }

    // ---------------------------------------------------------------------------
    // LobbyMessage encode/decode
    // ---------------------------------------------------------------------------

    #[test]
    fn lobby_message_create_lobby_roundtrip() {
        let msg = ClientMessage::LobbyMessage(LobbyMessage::CreateLobby {
            settings: LobbySettings {
                map: "test_map".to_string(),
                starting_funds: 1000,
                initial_manpower: 2000,
                funds_per_city: 1.0,
                game_speed: 1.0,
                use_standard_ruleset: true,
                llm_thinking_delay: 1.0,
                max_players: 4,
                custom_unit_types: vec![],
                spectator_vision: false,
                recruitment_rate: 15.0,
                water_speed: 0.8,
                city_upgrade_settings: CityUpgradeSettings {
                    enabled: true,
                    max_level: 50,
                    base_upgrade_cost: 300,
                    income_growth_per_level: 0.3,
                    base_supply_range: 7.0,
                    supply_range_growth: 2.0,
                    secondary_city_decay: 0.7,
                    manpower_regen_per_tick: 5.0,
                },
            },
            password: None,
            player_name: Some("TestPlayer".to_string()),
        });
        let bytes = to_cbor_bytes(&msg);
        let decoded = decode_client_message(&bytes).unwrap();
        if let ClientMessage::LobbyMessage(LobbyMessage::CreateLobby {
            settings,
            password,
            player_name,
        }) = decoded
        {
            assert_eq!(settings.map, "test_map");
            assert_eq!(settings.starting_funds, 1000);
            assert_eq!(settings.max_players, 4);
            assert_eq!(settings.recruitment_rate, 15.0);
            assert_eq!(settings.water_speed, 0.8);
            assert!(settings.city_upgrade_settings.enabled);
            assert_eq!(settings.city_upgrade_settings.max_level, 50);
            assert_eq!(settings.city_upgrade_settings.base_upgrade_cost, 300);
            assert!(password.is_none());
            assert_eq!(player_name.as_deref(), Some("TestPlayer"));
        } else {
            panic!("Expected CreateLobby");
        }
    }

    #[test]
    fn lobby_message_join_lobby_roundtrip() {
        let msg = ClientMessage::LobbyMessage(LobbyMessage::JoinLobby {
            lobby_code: "ABC123".to_string(),
            password: Some("secret".to_string()),
            player_name: Some("Player1".to_string()),
        });
        let bytes = to_cbor_bytes(&msg);
        let decoded = decode_client_message(&bytes).unwrap();
        if let ClientMessage::LobbyMessage(LobbyMessage::JoinLobby {
            lobby_code,
            password,
            player_name,
        }) = decoded
        {
            assert_eq!(lobby_code, "ABC123");
            assert_eq!(password, Some("secret".to_string()));
            assert_eq!(player_name, Some("Player1".to_string()));
        } else {
            panic!("Expected JoinLobby");
        }
    }

    #[test]
    fn lobby_message_start_game_roundtrip() {
        let msg = ClientMessage::LobbyMessage(LobbyMessage::StartGame);
        let bytes = to_cbor_bytes(&msg);
        let decoded = decode_client_message(&bytes).unwrap();
        if let ClientMessage::LobbyMessage(LobbyMessage::StartGame) = decoded {
        } else {
            panic!("Expected StartGame");
        }
    }

    #[test]
    fn lobby_message_standard_ruleset_minimal_fields() {
        // Standard ruleset: only map + use_standard_ruleset sent, rest defaults
        let msg = ClientMessage::LobbyMessage(LobbyMessage::CreateLobby {
            settings: LobbySettings {
                map: "small_map".to_string(),
                use_standard_ruleset: true,
                starting_funds: 0,
                initial_manpower: 0,
                funds_per_city: 0.0,
                game_speed: 0.0,
                llm_thinking_delay: 0.0,
                max_players: 0,
                custom_unit_types: vec![],
                spectator_vision: false,
                recruitment_rate: 0.0,
                water_speed: 0.0,
                city_upgrade_settings: CityUpgradeSettings::default(),
            },
            password: None,
            player_name: None,
        });
        let bytes = to_cbor_bytes(&msg);
        let decoded = decode_client_message(&bytes).unwrap();
        if let ClientMessage::LobbyMessage(LobbyMessage::CreateLobby { settings, .. }) = decoded {
            assert_eq!(settings.map, "small_map");
            assert!(settings.use_standard_ruleset);
            assert!(settings.custom_unit_types.is_empty());
            assert!(!settings.city_upgrade_settings.enabled);
        } else {
            panic!("Expected CreateLobby");
        }
    }

    #[test]
    fn lobby_message_custom_ruleset_with_unit_types() {
        let msg = ClientMessage::LobbyMessage(LobbyMessage::CreateLobby {
            settings: LobbySettings {
                map: "medium_map".to_string(),
                starting_funds: 2000,
                initial_manpower: 2000,
                funds_per_city: 2.0,
                game_speed: 120.0,
                use_standard_ruleset: false,
                llm_thinking_delay: 5.0,
                max_players: 6,
                custom_unit_types: vec![CustomUnitType {
                    name: "Knight".to_string(),
                    symbol: "K".to_string(),
                    men: 200,
                    equipment: vec![("sword".to_string(), 1), ("armor".to_string(), 1)],
                    soft_attack: 10.0,
                    soft_defense: 8.0,
                    hard_attack: 5.0,
                    hard_defense: 12.0,
                    speed: 2.0,
                    max_organisation: 100.0,
                    reinforcement_rate: 0.5,
                    recruitment_cost: 500,
                }],
                spectator_vision: true,
                recruitment_rate: 5.0,
                water_speed: 1.5,
                city_upgrade_settings: CityUpgradeSettings {
                    enabled: true,
                    max_level: 20,
                    base_upgrade_cost: 100,
                    income_growth_per_level: 1.0,
                    base_supply_range: 3.0,
                    supply_range_growth: 1.5,
                    secondary_city_decay: 0.3,
                    manpower_regen_per_tick: 20.0,
                },
            },
            password: Some("mypass".to_string()),
            player_name: Some("KnightPlayer".to_string()),
        });
        let bytes = to_cbor_bytes(&msg);
        let decoded = decode_client_message(&bytes).unwrap();
        if let ClientMessage::LobbyMessage(LobbyMessage::CreateLobby {
            settings,
            password,
            player_name,
        }) = decoded
        {
            assert_eq!(settings.map, "medium_map");
            assert!(!settings.use_standard_ruleset);
            assert_eq!(settings.starting_funds, 2000);
            assert_eq!(settings.funds_per_city, 2.0);
            assert_eq!(settings.game_speed, 120.0);
            assert_eq!(settings.max_players, 6);
            assert_eq!(settings.recruitment_rate, 5.0);
            assert_eq!(settings.water_speed, 1.5);
            assert!(settings.spectator_vision);
            assert_eq!(settings.custom_unit_types.len(), 1);
            assert_eq!(settings.custom_unit_types[0].name, "Knight");
            assert_eq!(settings.custom_unit_types[0].symbol, "K");
            assert_eq!(settings.custom_unit_types[0].men, 200);
            assert_eq!(settings.custom_unit_types[0].equipment.len(), 2);
            assert!(settings.city_upgrade_settings.enabled);
            assert_eq!(settings.city_upgrade_settings.max_level, 20);
            assert_eq!(settings.city_upgrade_settings.base_upgrade_cost, 100);
            assert_eq!(settings.city_upgrade_settings.income_growth_per_level, 1.0);
            assert_eq!(settings.city_upgrade_settings.base_supply_range, 3.0);
            assert_eq!(settings.city_upgrade_settings.supply_range_growth, 1.5);
            assert_eq!(password.as_deref(), Some("mypass"));
            assert_eq!(player_name.as_deref(), Some("KnightPlayer"));
            assert_eq!(settings.city_upgrade_settings.secondary_city_decay, 0.3);
            assert_eq!(settings.city_upgrade_settings.manpower_regen_per_tick, 20.0);
            assert_eq!(password, Some("mypass".to_string()));
        } else {
            panic!("Expected CreateLobby");
        }
    }

    // ---------------------------------------------------------------------------
    // Server error/advice messages
    // ---------------------------------------------------------------------------

    #[test]
    fn server_error_roundtrip() {
        let msg = ServerMessage::Error {
            message: "Invalid action".to_string(),
        };
        let encoded = encode_message(&msg);
        let decoded: ServerMessage = ciborium::from_reader(encoded.as_slice()).unwrap();
        if let ServerMessage::Error { message } = decoded {
            assert_eq!(message, "Invalid action");
        } else {
            panic!("Expected Error");
        }
    }

    #[test]
    fn server_advice_roundtrip() {
        let msg = ServerMessage::Advice {
            text: "Consider moving units north".to_string(),
        };
        let encoded = encode_message(&msg);
        let decoded: ServerMessage = ciborium::from_reader(encoded.as_slice()).unwrap();
        if let ServerMessage::Advice { text } = decoded {
            assert_eq!(text, "Consider moving units north");
        } else {
            panic!("Expected Advice");
        }
    }

    #[test]
    fn server_game_over_roundtrip() {
        let msg = ServerMessage::GameOver {
            winner: "Player 1".to_string(),
            game_id: "game_123".to_string(),
            message: "Player 1 wins!".to_string(),
        };
        let encoded = encode_message(&msg);
        let decoded: ServerMessage = ciborium::from_reader(encoded.as_slice()).unwrap();
        if let ServerMessage::GameOver {
            winner,
            game_id,
            message,
        } = decoded
        {
            assert_eq!(winner, "Player 1");
            assert_eq!(game_id, "game_123");
            assert_eq!(message, "Player 1 wins!");
        } else {
            panic!("Expected GameOver");
        }
    }

    #[test]
    fn server_clear_spectator_roundtrip() {
        let msg = ServerMessage::ClearSpectator;
        let encoded = encode_message(&msg);
        let decoded: ServerMessage = ciborium::from_reader(encoded.as_slice()).unwrap();
        assert!(matches!(decoded, ServerMessage::ClearSpectator));
    }

    #[test]
    fn server_lobby_created_roundtrip() {
        let msg = ServerMessage::LobbyCreated {
            lobby_code: "ABC".to_string(),
        };
        let encoded = encode_message(&msg);
        let decoded: ServerMessage = ciborium::from_reader(encoded.as_slice()).unwrap();
        if let ServerMessage::LobbyCreated { lobby_code } = decoded {
            assert_eq!(lobby_code, "ABC");
        } else {
            panic!("Expected LobbyCreated");
        }
    }

    #[test]
    fn server_lobby_joined_roundtrip() {
        let msg = ServerMessage::LobbyJoined {
            lobby_code: "DEF".to_string(),
        };
        let encoded = encode_message(&msg);
        let decoded: ServerMessage = ciborium::from_reader(encoded.as_slice()).unwrap();
        if let ServerMessage::LobbyJoined { lobby_code } = decoded {
            assert_eq!(lobby_code, "DEF");
        } else {
            panic!("Expected LobbyJoined");
        }
    }

    #[test]
    fn server_leaderboard_roundtrip() {
        let msg = ServerMessage::Leaderboard {
            entries: vec![LeaderboardEntry {
                player_id: "p1".to_string(),
                name: "Player1".to_string(),
                games_played: 10,
                games_won: 7,
                avg_casualties: 500.0,
            }],
        };
        let encoded = encode_message(&msg);
        let decoded: ServerMessage = ciborium::from_reader(encoded.as_slice()).unwrap();
        if let ServerMessage::Leaderboard { entries } = decoded {
            assert_eq!(entries.len(), 1);
            assert_eq!(entries[0].player_id, "p1");
            assert_eq!(entries[0].games_won, 7);
        } else {
            panic!("Expected Leaderboard");
        }
    }

    // ---------------------------------------------------------------------------
    // JSON serialization
    // ---------------------------------------------------------------------------

    #[test]
    fn game_state_json_roundtrip() {
        let state = GameStateMessage {
            vision_grid: None,
            border_grid: None,
            terrain: vec![],
            units: vec![],
            cities: vec![],
            player: PlayerState {
                funds: 0,
                units_owned: 0,
                player_id: String::new(),
            },
        };
        let json = serde_json::to_string(&state).unwrap();
        let decoded: GameStateMessage = serde_json::from_str(&json).unwrap();
        assert_eq!(decoded.player.funds, state.player.funds);
        assert_eq!(decoded.units.len(), state.units.len());
    }

    #[test]
    fn city_state_json_roundtrip() {
        let city = CityState {
            id: "city_0".to_string(),
            position: (5.0, 5.0),
            owner: Some("p1".to_string()),
            manpower: 10000,
            level: 2,
        };
        let json = serde_json::to_string(&city).unwrap();
        let decoded: CityState = serde_json::from_str(&json).unwrap();
        assert_eq!(decoded.id, "city_0");
        assert_eq!(decoded.position, (5.0, 5.0));
        assert_eq!(decoded.manpower, 10000);
        assert_eq!(decoded.level, 2);
    }

    #[test]
    fn empty_decode_returns_error() {
        let result = decode_client_message(&[]);
        assert!(result.is_err());
    }

    #[test]
    fn invalid_cbor_returns_error() {
        let result = decode_client_message(b"not cbor data");
        assert!(result.is_err());
    }

    #[test]
    fn equipment_state_equality() {
        let eq1 = EquipmentState {
            name: "rifle".to_string(),
            count: 100,
            cost_per_item: 2.5,
        };
        let eq2 = EquipmentState {
            name: "rifle".to_string(),
            count: 100,
            cost_per_item: 2.5,
        };
        // EquipmentState derives PartialEq
        assert_eq!(eq1, eq2);
    }

    /// Test that a minimal CreateLobby payload (like what the frontend sends for
    /// standard ruleset) decodes successfully with serde defaults filling in the rest.
    #[test]
    fn lobby_message_minimal_standard_ruleset_decode() {
        use serde::{Deserialize, Serialize};

        // Helper: use small structs with only the fields the frontend sends,
        // encode as CBOR, and verify the server-side types decode them with defaults.
        #[derive(Serialize, Deserialize)]
        struct MinimalCreateLobby {
            #[serde(rename = "LobbyMessage")]
            lobby_message: MinimalLobbyMessage,
        }

        #[derive(Serialize, Deserialize)]
        struct MinimalLobbyMessage {
            #[serde(rename = "CreateLobby")]
            create_lobby: MinimalCreateLobbyInner,
        }

        #[derive(Serialize, Deserialize)]
        struct MinimalCreateLobbyInner {
            settings: MinimalSettings,
            #[serde(skip_serializing_if = "Option::is_none")]
            password: Option<String>,
            #[serde(skip_serializing_if = "Option::is_none")]
            player_name: Option<String>,
        }

        #[derive(Serialize, Deserialize)]
        struct MinimalSettings {
            map: String,
            use_standard_ruleset: bool,
            llm_thinking_delay: f64,
        }

        let minimal = MinimalCreateLobby {
            lobby_message: MinimalLobbyMessage {
                create_lobby: MinimalCreateLobbyInner {
                    settings: MinimalSettings {
                        map: "default".to_string(),
                        use_standard_ruleset: true,
                        llm_thinking_delay: 3.0,
                    },
                    password: None,
                    player_name: None,
                },
            },
        };

        // Encode as CBOR (same structure as frontend)
        let bytes = to_cbor_bytes(&minimal);
        // Now try to decode as ClientMessage (server-side type)
        let result = decode_client_message(&bytes);
        assert!(result.is_ok(), "Decode failed: {:?}", result.err());
        if let Ok(ClientMessage::LobbyMessage(LobbyMessage::CreateLobby {
            settings,
            password,
            ..
        })) = result
        {
            assert_eq!(settings.map, "default");
            assert!(settings.use_standard_ruleset);
            assert_eq!(settings.llm_thinking_delay, 3.0);
            // Defaults should fill in the rest
            assert_eq!(settings.starting_funds, 0);
            assert_eq!(settings.funds_per_city, 0.0);
            assert_eq!(settings.game_speed, 0.0);
            assert_eq!(settings.max_players, 0);
            assert!(!settings.spectator_vision);
            assert!(settings.custom_unit_types.is_empty());
            assert!(password.is_none());
        } else {
            panic!("Expected CreateLobby variant");
        }
    }

    /// Test with the EXACT CBOR bytes captured from the browser network tab.
    /// This reproduces the "invalid type: map, expected variant identifier" error.
    #[test]
    fn lobby_message_browser_hex_roundtrip() {
        // Hex dump from browser: standard ruleset CreateLobby with stripUndefined applied
        // Structure: { LobbyMessage: { CreateLobby: { settings: { map, use_standard_ruleset, llm_thinking_delay } } } }
        let bytes = hex_to_bytes("b900016c4c6f6262794d657373616765b900016b4372656174654c6f626279b900016873657474696e6773b90003636d61706764656661756c74747573655f7374616e646172645f72756c65736574f5726c6c6d5f7468696e6b696e675f64656c617903");
        let result = decode_client_message(&bytes);
        assert!(
            result.is_ok(),
            "Failed to decode browser CBOR: {:?}",
            result.err()
        );
        if let Ok(ClientMessage::LobbyMessage(LobbyMessage::CreateLobby {
            settings,
            password,
            ..
        })) = result
        {
            assert_eq!(settings.map, "default");
            assert!(settings.use_standard_ruleset);
            assert!((settings.llm_thinking_delay - 3.0).abs() < f64::EPSILON);
            assert!(password.is_none());
        } else {
            panic!("Expected CreateLobby variant");
        }
    }

    #[cfg(test)]
    fn hex_to_bytes(hex: &str) -> Vec<u8> {
        (0..hex.len())
            .step_by(2)
            .map(|i| u8::from_str_radix(&hex[i..i + 2], 16).expect("valid hex byte"))
            .collect()
    }
}
