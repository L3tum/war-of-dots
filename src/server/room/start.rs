//! `LobbyMessage::StartGame` handler, extracted verbatim from the
//! `LobbyMessage::StartGame` arm of
//! [`crate::server::handlers::lobby::handle_client_message`]: lobby
//! lookup + creator check, custom-settings validation, unit-type
//! conversion, `GameRoom::new` spawn, player_map remap for all lobby
//! connections, leaderboard-name storage registration, the
//! ClearSpectator/LobbyUpdate broadcast, and lobby consumption.

use bytes::Bytes;
use sqlite::Value;
use std::sync::Arc;
use tokio::sync::Mutex;
use uuid::Uuid;

use crate::game::game::GameSettings;
use crate::map::map_config::load_default_map;
use crate::map::map_registry;
use crate::server::game_server::GameServer;
use crate::server::protocol::{encode_message_result, CborEncodeError, ServerMessage};
use crate::server::room::settings::{
    build_default_unit_types, convert_custom_unit_types, convert_unit_types,
    validate_custom_settings,
};
use crate::server::room::{GameRoom, LlmPlayerConfig, PlayerConnection, PlayerSender};

/// Start the game for the caller's lobby (creator only).
///
/// The body is the former `LobbyMessage::StartGame` arm of
/// [`crate::server::handlers::lobby::handle_client_message`] verbatim. The
/// early `return Ok(())` exits here; the call site propagates the result
/// with `.await?`, so observable behavior is unchanged.
pub(crate) async fn start_game(
    conn_id: &Uuid,
    server_state: Arc<Mutex<GameServer>>,
    player_sender: &PlayerSender,
) -> Result<(), CborEncodeError> {
    // Only the lobby creator can start the game
    let lobby_code = {
        let server = server_state.lock().await;
        let player_map = server.player_map.lock().await;
        player_map.get(conn_id).map(|(_, code)| code.clone())
    };
    let Some(lobby_code) = lobby_code else {
        let resp = ServerMessage::Error {
            message: "Not in a lobby".to_string(),
        };
        let encoded = encode_message_result(&resp)?;
        let _ = player_sender.send_reliable(encoded.into()).await;
        return Ok(());
    };
    let is_creator = {
        let server = server_state.lock().await;
        let player_map = server.player_map.lock().await;
        let Some((player_id, _)) = player_map.get(conn_id) else {
            return Ok(());
        };
        let lobby_manager = server.lobby_manager.lock().await;
        lobby_manager
            .get_lobby(&lobby_code)
            .map(|l| l.creator == *player_id)
            .unwrap_or(false)
    };
    if !is_creator {
        let resp = ServerMessage::Error {
            message: "Only the lobby creator can start the game".to_string(),
        };
        let encoded = encode_message_result(&resp)?;
        let _ = player_sender.send_reliable(encoded.into()).await;
        return Ok(());
    }

    let (
        game_settings,
        map,
        human_players,
        llm_configs,
        lobby_code,
        settings,
        _config_clone,
        lobby_players,
        spectator_vision,
    ) = {
        let server = server_state.lock().await;
        let lobby_manager = server.lobby_manager.lock().await;
        let lobby = match lobby_manager.get_lobby(&lobby_code) {
            Some(l) => l,
            None => {
                let resp = ServerMessage::Error {
                    message: "Lobby not found".to_string(),
                };
                let encoded = encode_message_result(&resp)?;
                let _ = player_sender.send_reliable(encoded.into()).await;
                return Ok(());
            }
        };
        if !lobby.players.is_empty() {
            // G-3: Validate custom ruleset settings. Custom-ruleset lobbies
            // (use_standard_ruleset == false) are validated in full (lobby-level
            // fields + unit types). A standard-ruleset lobby is validated too
            // when the client still sent custom_unit_types: those types are
            // honored once the server's standard ruleset is disabled, so
            // validating them "when present" closes the flag-bypass (item 7a).
            if !lobby.settings.use_standard_ruleset || !lobby.settings.custom_unit_types.is_empty()
            {
                if let Some(err) = validate_custom_settings(&lobby.settings) {
                    let resp = ServerMessage::Error { message: err };
                    let encoded = encode_message_result(&resp)?;
                    let _ = player_sender.send_reliable(encoded.into()).await;
                    return Ok(());
                }
            }

            let unit_types =
                if lobby.settings.use_standard_ruleset && server.config.standard_ruleset.enabled {
                    convert_unit_types(&server.config.standard_ruleset.unit_types)
                } else if !lobby.custom_unit_types.is_empty() {
                    // Convert custom unit types from lobby
                    convert_custom_unit_types(&lobby.custom_unit_types)
                } else {
                    build_default_unit_types()
                };

            let settings = lobby.settings.clone();
            // CityUpgradeSettings is now a single shared type (config.rs),
            // so no protocol->config conversion is needed.
            // Standard ruleset: pull all defaults from config.toml.
            // Custom ruleset: use client-sent values.
            let std = &server.config.standard_ruleset;
            let game_settings = GameSettings {
                starting_funds: if settings.use_standard_ruleset {
                    std.starting_funds
                } else {
                    settings.starting_funds
                },
                initial_manpower: if settings.use_standard_ruleset {
                    std.initial_manpower
                } else {
                    settings.initial_manpower
                },
                funds_per_city: if settings.use_standard_ruleset {
                    std.funds_per_city
                } else {
                    settings.funds_per_city
                },
                recruitment_rate: if settings.use_standard_ruleset {
                    std.recruitment_rate
                } else {
                    settings.recruitment_rate
                },
                game_speed: if settings.use_standard_ruleset {
                    std.game_speed
                } else {
                    settings.game_speed
                },
                use_standard_ruleset: settings.use_standard_ruleset,
                unit_types,
                city_upgrade_settings: if settings.use_standard_ruleset {
                    server.config.city_upgrade_settings()
                } else {
                    settings.city_upgrade_settings.clone()
                },
                water_speed: if settings.use_standard_ruleset {
                    std.water_speed
                } else {
                    settings.water_speed
                },
                influence: if settings.use_standard_ruleset {
                    std.influence.clone().unwrap_or_default()
                } else {
                    crate::game::game::InfluenceSettings::default()
                },
            };
            let map = map_registry::get(&settings.map).unwrap_or_else(load_default_map);
            let mut human_players: Vec<PlayerConnection> = Vec::new();
            let mut llm_configs: Vec<LlmPlayerConfig> = Vec::new();

            for (i, p) in lobby.players.iter().enumerate() {
                let player_id = format!("player_{}", i);
                if p.is_human {
                    human_players.push(PlayerConnection::Player {
                        id: player_id,
                        tx: p.connection.clone(),
                    });
                } else {
                    let llm_provider = p.llm_provider.clone().unwrap_or_default();
                    let thinking_delay = lobby.settings.llm_thinking_delay;

                    // Use the per-slot config if available, otherwise fall back to server config
                    let (model, api_key, base_url) =
                        if let Some(config) = lobby.llm_agent_configs.get(&(i as u32)) {
                            (
                                config.model.clone(),
                                config.api_key.clone(),
                                config.base_url.clone(),
                            )
                        } else {
                            // Fall back to first matching global config provider
                            let provider_config = server
                                .config
                                .llm
                                .providers
                                .iter()
                                .find(|c| c.provider == llm_provider || c.name == llm_provider);
                            (
                                provider_config
                                    .map(|c| c.model.clone())
                                    .unwrap_or_else(|| "llama3".to_string()),
                                provider_config.and_then(|c| c.api_key.clone()),
                                provider_config.and_then(|c| c.base_url.clone()),
                            )
                        };

                    llm_configs.push(LlmPlayerConfig {
                        player_id,
                        player_name: p.name.clone(),
                        provider: llm_provider,
                        model,
                        api_key,
                        base_url,
                        thinking_delay,
                    });
                }
            }

            let lobby_code = lobby.code.clone();
            let settings = lobby.settings.clone();
            let lobby_players: Vec<_> = lobby
                .players
                .iter()
                .map(|p| (p.id.clone(), p.name.clone()))
                .collect();
            let spectator_vision = settings.spectator_vision;
            (
                game_settings,
                map,
                human_players,
                llm_configs,
                lobby_code,
                settings,
                server.config.clone(),
                lobby_players,
                spectator_vision,
            )
        } else {
            return Ok(());
        }
    };
    // Guard: don't start a second game for a lobby that already has
    // a running game. `games` is the source of truth (keyed by the
    // same lobby code used when spawning the GameRoom).
    let game_already_running = {
        let server = server_state.lock().await;
        let games = server.games.lock().await;
        games.contains_key(&lobby_code)
    };
    if game_already_running {
        let resp = ServerMessage::Error {
            message: "A game is already running for this lobby".to_string(),
        };
        let encoded = encode_message_result(&resp)?;
        let _ = player_sender.send_reliable(encoded.into()).await;
        return Ok(());
    }
    // Spawn the game with human and LLM players
    let server_state_inner = server_state.clone();
    let config = {
        let server = server_state.lock().await;
        server.config.clone()
    };
    // Store player names in database for leaderboard display
    let human_players_inner = human_players.clone();
    let llm_configs_inner = llm_configs.clone();
    {
        let server_inner = server_state_inner.lock().await;
        let storage = server_inner.storage.clone();
        let db = match storage.db.lock() {
            Ok(d) => d,
            Err(e) => {
                tracing::error!(%e, "Failed to acquire DB lock for player_names");
                return Ok(());
            }
        };
        // Update human player names (parameterized — no SQL injection)
        for player in human_players_inner {
            let player_id = player.id().cloned().unwrap_or_default();
            let name = lobby_players
                .iter()
                .find(|(id, _)| id == &player_id)
                .map(|(_, name)| name.clone())
                .unwrap_or_else(|| format!("Player {}", player_id));
            let name_json = serde_json::to_string(&name).unwrap_or(format!("\"{}\"", name));
            let mut stmt = match db
                .prepare("INSERT OR REPLACE INTO player_names (player_id, name) VALUES (?, ?)")
            {
                Ok(s) => s,
                Err(e) => {
                    tracing::error!(%e, "Failed to prepare player_names insert");
                    continue;
                }
            };
            let params: Vec<Value> = vec![Value::String(player_id), Value::String(name_json)];
            if let Err(e) = stmt.bind(params.as_slice()) {
                tracing::error!(%e, "Failed to bind player_names params");
                continue;
            }
            if let Err(e) = stmt.next() {
                tracing::error!(%e, "Failed to insert player_names");
            }
        }
        // Update LLM agent names (parameterized — no SQL injection)
        for config in llm_configs_inner {
            let name_json = serde_json::to_string(&config.player_name)
                .unwrap_or(format!("\"{}\"", config.player_name));
            let mut stmt = match db
                .prepare("INSERT OR REPLACE INTO player_names (player_id, name) VALUES (?, ?)")
            {
                Ok(s) => s,
                Err(e) => {
                    tracing::error!(%e, "Failed to prepare player_names insert");
                    continue;
                }
            };
            let params: Vec<Value> = vec![
                Value::String(config.player_id.clone()),
                Value::String(name_json),
            ];
            if let Err(e) = stmt.bind(params.as_slice()) {
                tracing::error!(%e, "Failed to bind player_names params");
                continue;
            }
            if let Err(e) = stmt.next() {
                tracing::error!(%e, "Failed to insert player_names");
            }
        }
    }
    let lobby_code_clone = lobby_code.clone();
    let map_clone = map;
    let map_name_clone = settings.map.clone();
    let game_settings_clone = game_settings;
    let human_players_clone = human_players;
    let llm_configs_clone = llm_configs;
    tokio::spawn(async move {
        let server_inner = server_state_inner.lock().await;
        let mut games = server_inner.games.lock().await;
        let room = GameRoom::new(
            lobby_code_clone.clone(),
            map_clone,
            map_name_clone,
            game_settings_clone,
            human_players_clone,
            llm_configs_clone,
            lobby_code_clone.clone(),
            Arc::clone(&server_state_inner),
            config,
            server_inner.storage.clone(),
            spectator_vision,
        );
        let room_arc = Arc::new(Mutex::new(room));
        games.insert(lobby_code_clone, room_arc);
    });
    // Remap every lobby member's player_map entry to the in-game
    // slot IDs minted above: slot i == lobby.players[i] =>
    // "player_{i}" (identical ordering to the mint loop). Each
    // connection keeps its room code; a member whose connection
    // is gone has no entry and is skipped. Previously only the
    // starter was remapped to "player_0", so players 2..N kept
    // their lobby-era IDs and the engine's ownership gate
    // silently dropped every command they sent.
    {
        let server = server_state.lock().await;
        let mut player_map = server.player_map.lock().await;
        for entry in player_map.values_mut() {
            if let Some(slot) = lobby_players.iter().position(|(id, _)| id == &entry.0) {
                entry.0 = format!("player_{}", slot);
                entry.1 = lobby_code.clone();
            }
        }
    }
    // Capture the lobby's player connections and build the all-players
    // list BEFORE removing the lobby, so the broadcast below has targets
    // even after the lobby is consumed.
    let lobby_player_senders: Vec<_> = {
        let server = server_state.lock().await;
        let lobby_manager = server.lobby_manager.lock().await;
        match lobby_manager.get_lobby(&lobby_code) {
            Some(lobby) => lobby.players.iter().map(|p| p.connection.clone()).collect(),
            None => Vec::new(),
        }
    };
    let all_players: Vec<crate::server::protocol::LobbyPlayer> = {
        let server = server_state.lock().await;
        let lobby_manager = server.lobby_manager.lock().await;
        match lobby_manager.get_lobby(&lobby_code) {
            Some(lobby) => lobby
                .players
                .iter()
                .map(|p| crate::server::protocol::LobbyPlayer {
                    id: p.id.clone(),
                    name: p.name.clone(),
                    is_human: p.is_human,
                    llm_provider: p.llm_provider.clone(),
                })
                .collect(),
            None => Vec::new(),
        }
    };
    // Broadcast ClearSpectator then LobbyUpdate
    let clear_resp = ServerMessage::ClearSpectator;
    let clear_encoded = Bytes::from(encode_message_result(&clear_resp)?);
    let lobby_update_resp = ServerMessage::LobbyUpdate {
        players: all_players.clone(),
        settings: settings.clone(),
    };
    let lobby_encoded = Bytes::from(encode_message_result(&lobby_update_resp)?);
    for sender in &lobby_player_senders {
        let _ = sender.send_reliable(clear_encoded.clone()).await;
        let _ = sender.send_reliable(lobby_encoded.clone()).await;
    }
    // Consume the lobby so it cannot be found again by get_first_lobby()
    // for a duplicate StartGame.
    {
        let server = server_state.lock().await;
        let mut lobby_manager = server.lobby_manager.lock().await;
        lobby_manager.remove_lobby(&lobby_code);
    }
    Ok(())
}
