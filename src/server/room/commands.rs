//! In-game command dispatch: move/recruit/toggles/upgrade/advisor/
//! leaderboard/embark — shared by the TCP and WebSocket transports.
//!
//! Extracted verbatim from `handlers::lobby::handle_client_message`
//! (Room split, run 2) — each arm is unchanged apart from indentation
//! and the enclosing function boundary.

use std::sync::Arc;
use tokio::sync::Mutex;
use uuid::Uuid;

use crate::server::game_server::GameServer;
use crate::server::protocol::{
    encode_message_result, CborEncodeError, ClientMessage, ServerMessage,
};
use crate::server::room::{Command, PlayerSender};

/// M3: fallible — returns `Err` when a server reply cannot be CBOR-encoded.
pub(crate) async fn handle_game_command(
    conn_id: &Uuid,
    server_state: Arc<Mutex<GameServer>>,
    msg: &ClientMessage,
    player_sender: &PlayerSender,
) -> Result<(), CborEncodeError> {
    match msg {
        ClientMessage::MoveTroop { troop_id, path }
        | ClientMessage::MoveUnit {
            unit_id: troop_id,
            path,
        } => {
            let server = server_state.lock().await;
            let entry = server.player_map.lock().await.get(conn_id).cloned();
            drop(server);
            if let Some((player_id, room_id)) = entry {
                let room_arc = server_state
                    .lock()
                    .await
                    .games
                    .lock()
                    .await
                    .get(&room_id)
                    .cloned();
                if let Some(room_arc) = room_arc {
                    let room = room_arc.lock().await;
                    let _ = room.command_tx.send(Command::MoveTroop {
                        unit_id: troop_id.clone(),
                        owner_id: player_id,
                        path: path.clone(),
                    });
                }
            }
        }
        ClientMessage::RecruitUnit {
            city_id,
            unit_type: unit_type_name,
        } => {
            let server = server_state.lock().await;
            let entry = server.player_map.lock().await.get(conn_id).cloned();
            drop(server);
            if let Some((player_id, room_id)) = entry {
                let room_arc = server_state
                    .lock()
                    .await
                    .games
                    .lock()
                    .await
                    .get(&room_id)
                    .cloned();
                if let Some(room_arc) = room_arc {
                    let room = room_arc.lock().await;
                    let _ = room.command_tx.send(Command::RecruitUnit(
                        city_id.clone(),
                        unit_type_name.clone(),
                        player_id,
                    ));
                }
            }
        }
        ClientMessage::ToggleHealing { unit_id, enabled } => {
            let server = server_state.lock().await;
            let entry = server.player_map.lock().await.get(conn_id).cloned();
            drop(server);
            if let Some((player_id, room_id)) = entry {
                let room_arc = server_state
                    .lock()
                    .await
                    .games
                    .lock()
                    .await
                    .get(&room_id)
                    .cloned();
                if let Some(room_arc) = room_arc {
                    let room = room_arc.lock().await;
                    let _ = room.command_tx.send(Command::ToggleHealing {
                        unit_id: unit_id.clone(),
                        owner_id: player_id,
                        enabled: *enabled,
                    });
                }
            }
        }
        ClientMessage::ToggleResupply { unit_id, enabled } => {
            let server = server_state.lock().await;
            let entry = server.player_map.lock().await.get(conn_id).cloned();
            drop(server);
            if let Some((player_id, room_id)) = entry {
                let room_arc = server_state
                    .lock()
                    .await
                    .games
                    .lock()
                    .await
                    .get(&room_id)
                    .cloned();
                if let Some(room_arc) = room_arc {
                    let room = room_arc.lock().await;
                    let _ = room.command_tx.send(Command::ToggleResupply {
                        unit_id: unit_id.clone(),
                        owner_id: player_id,
                        enabled: *enabled,
                    });
                }
            }
        }
        ClientMessage::UpgradeCity { city_id } => {
            let server = server_state.lock().await;
            let entry = server.player_map.lock().await.get(conn_id).cloned();
            drop(server);
            if let Some((player_id, room_id)) = entry {
                let room_arc = server_state
                    .lock()
                    .await
                    .games
                    .lock()
                    .await
                    .get(&room_id)
                    .cloned();
                if let Some(room_arc) = room_arc {
                    let room = room_arc.lock().await;
                    let _ = room
                        .command_tx
                        .send(Command::UpgradeCity(city_id.clone(), player_id));
                }
            }
        }
        ClientMessage::ToggleAdvisor { enabled } => {
            // Toggle advisor for this player's game room
            let server = server_state.lock().await;
            let room_id = server
                .player_map
                .lock()
                .await
                .get(conn_id)
                .map(|(_, r)| r.clone());
            let player_id = server
                .player_map
                .lock()
                .await
                .get(conn_id)
                .map(|(p, _)| p.clone());
            if let (Some(room_id), Some(player_id)) = (room_id, player_id) {
                let room_arc = server.games.lock().await.get(&room_id).cloned();
                drop(server);
                if let Some(room_arc) = room_arc {
                    let room = room_arc.lock().await;
                    let _ = room
                        .command_tx
                        .send(Command::ToggleAdvisor(player_id, *enabled));
                }
            }
        }
        ClientMessage::GetLeaderboard => {
            let server = server_state.lock().await;
            let leaderboards = server.storage.get_leaderboards().unwrap_or_default();
            let entries: Vec<crate::server::protocol::LeaderboardEntry> = leaderboards
                .into_iter()
                .map(|s| crate::server::protocol::LeaderboardEntry {
                    player_id: s.player_id,
                    name: s.name,
                    games_played: s.games_played,
                    games_won: s.games_won,
                    avg_casualties: s.avg_casualties,
                })
                .collect();
            let resp = ServerMessage::Leaderboard { entries };
            let encoded = encode_message_result(&resp)?;
            let _ = player_sender.send_reliable(encoded.into()).await;
        }
        ClientMessage::EmbarkUnit { unit_id } => {
            let server = server_state.lock().await;
            let (player_id, room_id) = match server.player_map.lock().await.get(conn_id).cloned() {
                Some(entry) => entry,
                None => return Ok(()),
            };
            let room_arc = server.games.lock().await.get(&room_id).cloned();
            drop(server);
            if let Some(room_arc) = room_arc {
                let room = room_arc.lock().await;
                let _ = room
                    .command_tx
                    .send(Command::EmbarkUnit(unit_id.clone(), player_id));
            }
        }
        ClientMessage::DisembarkUnit { unit_id } => {
            let server = server_state.lock().await;
            let (player_id, room_id) = match server.player_map.lock().await.get(conn_id).cloned() {
                Some(entry) => entry,
                None => return Ok(()),
            };
            let room_arc = server.games.lock().await.get(&room_id).cloned();
            drop(server);
            if let Some(room_arc) = room_arc {
                let room = room_arc.lock().await;
                let _ = room
                    .command_tx
                    .send(Command::DisembarkUnit(unit_id.clone(), player_id));
            }
        }
        _ => {}
    }
    Ok(())
}
