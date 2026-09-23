//! Lobby message flow: create/join/LLM-slot/start handling.
//!
//! Extracted verbatim from `handlers::lobby::handle_client_message`
//! (Room split, run 2) — each arm is unchanged apart from indentation
//! and the enclosing function boundary.

use bytes::Bytes;
use std::sync::Arc;
use tokio::sync::Mutex;
use uuid::Uuid;

use crate::server::game_server::GameServer;
use crate::server::handlers::lobby::lobby_bridge_sender;
use crate::server::protocol::{
    encode_message_result, CborEncodeError, LobbyMessage, LobbySettings, ServerMessage,
};
use crate::server::room::PlayerSender;

/// M3: fallible — returns `Err` when a server reply cannot be CBOR-encoded.
pub(crate) async fn handle_lobby_message(
    conn_id: &Uuid,
    server_state: Arc<Mutex<GameServer>>,
    lobby_msg: &LobbyMessage,
    player_sender: &PlayerSender,
) -> Result<(), CborEncodeError> {
    match lobby_msg {
        LobbyMessage::CreateLobby {
            settings,
            password,
            player_name,
        } => {
            // M2: one lobby per connection — a connection that is
            // already in a lobby (creator or guest) cannot create
            // another: the player_map entry is single-valued, so a
            // second lobby would be unreachable from the disconnect
            // cleanup and leak (retaining up to ~1 MB of
            // client-controlled custom unit types).
            {
                let server = server_state.lock().await;
                let player_map = server.player_map.lock().await;
                if player_map.contains_key(conn_id) {
                    let resp = ServerMessage::Error {
                        message: "Already in a lobby — start it or leave before creating another"
                            .to_string(),
                    };
                    let encoded = encode_message_result(&resp)?;
                    let _ = player_sender.send_reliable(encoded.into()).await;
                    return Ok(());
                }
                // M2: global lobby cap (config.network.max_lobbies).
                // Existing lobbies are unaffected; the over-cap
                // creation is rejected with a clear error.
                let lobby_count = server.lobby_manager.lock().await.lobbies.len();
                if lobby_count >= server.config.network.max_lobbies {
                    tracing::warn!(
                        lobby_count,
                        cap = server.config.network.max_lobbies,
                        "CreateLobby rejected: lobby cap reached"
                    );
                    let resp = ServerMessage::Error {
                        message: "Server lobby limit reached — try again later".to_string(),
                    };
                    let encoded = encode_message_result(&resp)?;
                    let _ = player_sender.send_reliable(encoded.into()).await;
                    return Ok(());
                }
            }
            let player_id = format!("player_{}", &Uuid::new_v4().to_string()[..8]);
            let player_name = player_name.clone().unwrap_or_else(|| "Player".to_string());
            let code;
            // Clone the player_sender for the lobby
            let lobby_sender = lobby_bridge_sender(player_sender);
            {
                let server = server_state.lock().await;
                let mut lobby_manager = server.lobby_manager.lock().await;
                let custom_unit_types = if settings.use_standard_ruleset {
                    Vec::new()
                } else {
                    settings.custom_unit_types.clone()
                };
                code = lobby_manager.create_lobby(
                    player_id.clone(),
                    player_name.clone(),
                    settings.clone(),
                    password.clone(),
                    lobby_sender,
                    custom_unit_types.clone(),
                );
                let mut player_map = server.player_map.lock().await;
                player_map.insert(*conn_id, (player_id.clone(), code.clone()));
            }
            // TODO: Store player name in database when game ends
            let resp = ServerMessage::LobbyCreated {
                lobby_code: code.clone(),
            };
            let encoded = encode_message_result(&resp)?;
            let _ = player_sender.send_reliable(encoded.into()).await;

            // Broadcast LobbyUpdate so the creator sees themselves in the list
            {
                let server = server_state.lock().await;
                let lobby_manager = server.lobby_manager.lock().await;
                if let Some(lobby) = lobby_manager.get_lobby(&code) {
                    let players: Vec<crate::server::protocol::LobbyPlayer> = lobby
                        .players
                        .iter()
                        .map(|p| crate::server::protocol::LobbyPlayer {
                            id: p.id.clone(),
                            name: p.name.clone(),
                            is_human: p.is_human,
                            llm_provider: p.llm_provider.clone(),
                        })
                        .collect();
                    let settings = lobby.settings.clone();
                    let update = ServerMessage::LobbyUpdate { players, settings };
                    let update_encoded = Bytes::from(encode_message_result(&update)?);
                    for p in &lobby.players {
                        let _ = p.connection.send_reliable(update_encoded.clone()).await;
                    }
                }
            }
        }
        LobbyMessage::JoinLobby {
            lobby_code,
            password,
            player_name: optional_player_name,
        } => {
            let player_id = format!("player_{}", &Uuid::new_v4().to_string()[..8]);
            let lobby_code_clone = lobby_code.clone();
            let password_clone = password.clone();
            // Use the provided player name or auto-generate one
            let lobby_player_name = {
                let server = server_state.lock().await;
                let lobby_manager = server.lobby_manager.lock().await;
                if let Some(name) = optional_player_name {
                    name.clone()
                } else {
                    lobby_manager
                        .get_lobby(lobby_code)
                        .map(|l| format!("Player {}", l.players.len() + 1))
                        .unwrap_or_else(|| "Player".to_string())
                }
            };
            // Create a sender for the new lobby player
            let lobby_sender = lobby_bridge_sender(player_sender);
            // Now join the lobby — complete all lobby operations inside this lock
            let result: Result<
                (
                    String,
                    Vec<crate::server::protocol::LobbyPlayer>,
                    LobbySettings,
                ),
                String,
            > = {
                let server = server_state.lock().await;
                let mut lobby_manager = server.lobby_manager.lock().await;
                match lobby_manager.join_lobby(
                    &lobby_code_clone,
                    player_id.clone(),
                    lobby_player_name,
                    password_clone,
                    lobby_sender,
                ) {
                    Ok(_) => {
                        let lobby = lobby_manager.get_lobby(&lobby_code_clone).unwrap();
                        let players: Vec<crate::server::protocol::LobbyPlayer> = lobby
                            .players
                            .iter()
                            .map(|p| crate::server::protocol::LobbyPlayer {
                                id: p.id.clone(),
                                name: p.name.clone(),
                                is_human: p.is_human,
                                llm_provider: p.llm_provider.clone(),
                            })
                            .collect();
                        let settings = lobby.settings.clone();
                        Ok((lobby.code.clone(), players, settings))
                    }
                    Err(e) => Err(e),
                }
            };
            match result {
                Ok((lobby_code_inner, players, settings)) => {
                    // Register player connection
                    {
                        let server = server_state.lock().await;
                        let mut player_map = server.player_map.lock().await;
                        player_map.insert(*conn_id, (player_id.clone(), lobby_code_inner.clone()));
                    }
                    // Send LobbyJoined to the joining player
                    let join_resp = ServerMessage::LobbyJoined {
                        lobby_code: lobby_code_inner.clone(),
                    };
                    let join_encoded = encode_message_result(&join_resp)?;
                    let _ = player_sender.send_reliable(join_encoded.into()).await;

                    // Broadcast LobbyUpdate to all players in the lobby
                    let server = server_state.lock().await;
                    let lobby_manager = server.lobby_manager.lock().await;
                    if let Some(l) = lobby_manager.get_lobby(&lobby_code_inner) {
                        let encoded =
                            Bytes::from(encode_message_result(&ServerMessage::LobbyUpdate {
                                players: players.clone(),
                                settings: settings.clone(),
                            })?);
                        for p in &l.players {
                            let _ = p.connection.send_reliable(encoded.clone()).await;
                        }
                    }
                }
                Err(e) => {
                    let resp = ServerMessage::Error { message: e };
                    let encoded = encode_message_result(&resp)?;
                    let _ = player_sender.send_reliable(encoded.into()).await;
                }
            }
        }
        LobbyMessage::SetSlotLlm { slot, llm_provider } => {
            if let Some((player_id, lobby_code)) = {
                let server = server_state.lock().await;
                let player_map = server.player_map.lock().await;
                player_map.get(conn_id).cloned()
            } {
                // Only lobby creator can set LLM slots
                let is_creator = {
                    let server = server_state.lock().await;
                    let lobby_manager = server.lobby_manager.lock().await;
                    lobby_manager
                        .get_lobby(&lobby_code)
                        .map(|l| l.creator == player_id)
                        .unwrap_or(false)
                };
                if !is_creator {
                    let resp = ServerMessage::Error {
                        message: "Only the lobby creator can configure LLM slots".to_string(),
                    };
                    let encoded = encode_message_result(&resp)?;
                    let _ = player_sender.send_reliable(encoded.into()).await;
                    return Ok(());
                }
                let result = {
                    let server = server_state.lock().await;
                    let mut lobby_manager = server.lobby_manager.lock().await;
                    lobby_manager.set_slot_llm(&lobby_code, *slot, llm_provider.clone())
                };
                if let Err(e) = result {
                    let resp = ServerMessage::Error { message: e };
                    let encoded = encode_message_result(&resp)?;
                    let _ = player_sender.send_reliable(encoded.into()).await;
                } else {
                    let (players, settings) = {
                        let server = server_state.lock().await;
                        let lobby_manager = server.lobby_manager.lock().await;
                        let lobby = lobby_manager.get_lobby(&lobby_code).unwrap();
                        let players: Vec<crate::server::protocol::LobbyPlayer> = lobby
                            .players
                            .iter()
                            .map(|p| crate::server::protocol::LobbyPlayer {
                                id: p.id.clone(),
                                name: p.name.clone(),
                                is_human: p.is_human,
                                llm_provider: p.llm_provider.clone(),
                            })
                            .collect();
                        let settings = lobby.settings.clone();
                        (players, settings)
                    };
                    let resp = ServerMessage::LobbyUpdate { players, settings };
                    let encoded = encode_message_result(&resp)?;
                    let _ = player_sender.send_reliable(encoded.into()).await;
                }
            } else {
                let resp = ServerMessage::Error {
                    message: "Not in a lobby".to_string(),
                };
                let encoded = encode_message_result(&resp)?;
                let _ = player_sender.send_reliable(encoded.into()).await;
            }
        }
        LobbyMessage::SetSlotLlmConfig { slot, model } => {
            if let Some((player_id, lobby_code)) = {
                let server = server_state.lock().await;
                let player_map = server.player_map.lock().await;
                player_map.get(conn_id).cloned()
            } {
                // Only lobby creator can configure LLM slots
                let is_creator = {
                    let server = server_state.lock().await;
                    let lobby_manager = server.lobby_manager.lock().await;
                    lobby_manager
                        .get_lobby(&lobby_code)
                        .map(|l| l.creator == player_id)
                        .unwrap_or(false)
                };
                if !is_creator {
                    let resp = ServerMessage::Error {
                        message: "Only the lobby creator can configure LLM slots".to_string(),
                    };
                    let encoded = encode_message_result(&resp)?;
                    let _ = player_sender.send_reliable(encoded.into()).await;
                    return Ok(());
                }

                // The wire message carries only { slot, model }. Resolve the
                // provider and its secret(s) (API key, base URL) server-side from
                // the configured LLM providers, so no secrets ever cross the wire.
                let (provider_name, api_key, base_url) = {
                    let server = server_state.lock().await;
                    let lobby_manager = server.lobby_manager.lock().await;
                    let provider_name = lobby_manager
                        .get_lobby(&lobby_code)
                        .and_then(|l| l.players.get(*slot as usize))
                        .and_then(|p| p.llm_provider.clone())
                        .unwrap_or_default();
                    let (api_key, base_url) = match server
                        .config
                        .llm
                        .providers
                        .iter()
                        .find(|c| c.name == provider_name || c.provider == provider_name)
                    {
                        Some(pc) => (pc.api_key.clone(), pc.base_url.clone()),
                        None => (None, None),
                    };
                    (provider_name, api_key, base_url)
                };

                let config = crate::llm::agent::LlmAgentConfig {
                    provider: provider_name.clone(),
                    model: model.clone(),
                    api_key,
                    base_url,
                };
                let result = {
                    let server = server_state.lock().await;
                    let mut lobby_manager = server.lobby_manager.lock().await;
                    lobby_manager.set_slot_llm_config(&lobby_code, *slot, config)
                };
                if let Err(e) = result {
                    let resp = ServerMessage::Error { message: e };
                    let encoded = encode_message_result(&resp)?;
                    let _ = player_sender.send_reliable(encoded.into()).await;
                } else {
                    // Also update the player's llm_provider for display (idempotent
                    // when the provider was already selected via SetSlotLlm).
                    if !provider_name.is_empty() {
                        let _result = {
                            let server = server_state.lock().await;
                            let mut lobby_manager = server.lobby_manager.lock().await;
                            lobby_manager.set_slot_llm(&lobby_code, *slot, provider_name.clone())
                        };
                    }
                    // Broadcast LobbyUpdate
                    let (players, settings) = {
                        let server = server_state.lock().await;
                        let lobby_manager = server.lobby_manager.lock().await;
                        let lobby = lobby_manager.get_lobby(&lobby_code).unwrap();
                        let players: Vec<crate::server::protocol::LobbyPlayer> = lobby
                            .players
                            .iter()
                            .map(|p| crate::server::protocol::LobbyPlayer {
                                id: p.id.clone(),
                                name: p.name.clone(),
                                is_human: p.is_human,
                                llm_provider: p.llm_provider.clone(),
                            })
                            .collect();
                        let settings = lobby.settings.clone();
                        (players, settings)
                    };
                    let resp = ServerMessage::LobbyUpdate { players, settings };
                    let encoded = encode_message_result(&resp)?;
                    let _ = player_sender.send_reliable(encoded.into()).await;
                }
            } else {
                let resp = ServerMessage::Error {
                    message: "Not in a lobby".to_string(),
                };
                let encoded = encode_message_result(&resp)?;
                let _ = player_sender.send_reliable(encoded.into()).await;
            }
        }
        LobbyMessage::AddLlmPlayer { name, llm_provider } => {
            if let Some((player_id, lobby_code)) = {
                let server = server_state.lock().await;
                let player_map = server.player_map.lock().await;
                player_map.get(conn_id).cloned()
            } {
                // Only lobby creator can add LLM players
                let is_creator = {
                    let server = server_state.lock().await;
                    let lobby_manager = server.lobby_manager.lock().await;
                    lobby_manager
                        .get_lobby(&lobby_code)
                        .map(|l| l.creator == player_id)
                        .unwrap_or(false)
                };
                if !is_creator {
                    let resp = ServerMessage::Error {
                        message: "Only the lobby creator can configure LLM slots".to_string(),
                    };
                    let encoded = encode_message_result(&resp)?;
                    let _ = player_sender.send_reliable(encoded.into()).await;
                    return Ok(());
                }
                let result = {
                    let server = server_state.lock().await;
                    let mut lobby_manager = server.lobby_manager.lock().await;
                    lobby_manager.add_llm_player(&lobby_code, name.clone(), llm_provider.clone())
                };
                if let Err(e) = result {
                    let resp = ServerMessage::Error { message: e };
                    let encoded = encode_message_result(&resp)?;
                    let _ = player_sender.send_reliable(encoded.into()).await;
                } else {
                    // Broadcast LobbyUpdate
                    let (players, settings) = {
                        let server = server_state.lock().await;
                        let lobby_manager = server.lobby_manager.lock().await;
                        let lobby = lobby_manager.get_lobby(&lobby_code).unwrap();
                        let players: Vec<crate::server::protocol::LobbyPlayer> = lobby
                            .players
                            .iter()
                            .map(|p| crate::server::protocol::LobbyPlayer {
                                id: p.id.clone(),
                                name: p.name.clone(),
                                is_human: p.is_human,
                                llm_provider: p.llm_provider.clone(),
                            })
                            .collect();
                        let settings = lobby.settings.clone();
                        (players, settings)
                    };
                    let resp = ServerMessage::LobbyUpdate { players, settings };
                    let encoded = encode_message_result(&resp)?;
                    let _ = player_sender.send_reliable(encoded.into()).await;
                }
            } else {
                let resp = ServerMessage::Error {
                    message: "Not in a lobby".to_string(),
                };
                let encoded = encode_message_result(&resp)?;
                let _ = player_sender.send_reliable(encoded.into()).await;
            }
        }
        LobbyMessage::StartGame => {
            // Extracted verbatim into `crate::server::room::start::start_game`.
            crate::server::room::start::start_game(conn_id, server_state, player_sender).await?;
        }
    }
    Ok(())
}
