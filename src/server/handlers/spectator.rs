//! Spectator WebSocket handlers.
//!
//! Handles spectator connections that watch live games without participating.

use axum::extract::ws::{CloseFrame, Message, WebSocket, WebSocketUpgrade};
use axum::extract::State;
use futures::{SinkExt, StreamExt};
use std::sync::atomic::Ordering;
use std::sync::Arc;
use tokio::sync::Mutex;

use crate::server::game_server::GameServer;
use crate::server::protocol::{encode_message, encode_message_result, ServerMessage};
use crate::server::room::{egress_vec, Command, DeltaRing, EgressMessage, PlayerSender, TcpEgress};

/// WebSocket handler for live spectator — joins the active game as a spectator
pub(crate) async fn spectator_ws_handler(
    ws: WebSocketUpgrade,
    State(state): State<Arc<Mutex<GameServer>>>,
) -> impl axum::response::IntoResponse {
    let server_state = state.clone();
    ws.on_upgrade(move |socket| handle_spectator_ws(socket, server_state))
}

pub(crate) async fn handle_spectator_ws(
    mut socket: WebSocket,
    server_state: Arc<Mutex<GameServer>>,
) {
    // Sec L2: reserve a connection slot (config.network.max_ws_connections,
    // shared with /ws). Over-cap connections are rejected with a clear
    // Error message + WS close code 1013 ("Try Again Later") before any
    // per-connection task is spawned. Every path after a successful reserve
    // releases the slot exactly once (see the releases below).
    {
        let server = server_state.lock().await;
        let cap = server.config.network.max_ws_connections.max(1);
        if server.ws_connections.fetch_add(1, Ordering::Relaxed) >= cap {
            server.ws_connections.fetch_sub(1, Ordering::Relaxed);
            let err = match encode_message_result(&ServerMessage::Error {
                message: "Server at maximum connections — try again later".to_string(),
            }) {
                Ok(e) => e,
                Err(e) => {
                    // M3: encode failure — nothing to send; just close.
                    tracing::error!(error = %e, "encode failed at spectator capacity reject — closing connection");
                    let _ = socket
                        .send(Message::Close(Some(CloseFrame {
                            code: 1011,
                            reason: "internal error".into(),
                        })))
                        .await;
                    return;
                }
            };
            let _ = socket.send(Message::Binary(err)).await;
            // axum 0.7: close() takes no frame — send an explicit Close
            // message with 1013 (Try Again Later) instead.
            let _ = socket
                .send(Message::Close(Some(CloseFrame {
                    code: 1013,
                    reason: "at capacity".into(),
                })))
                .await;
            return;
        }
    }
    let (mut ws_sink, _ws_stream) = socket.split();
    // Drop ws_stream — spectator only receives

    // Find the first active game room
    let game_id = {
        let server = server_state.lock().await;
        let x = server.games.lock().await.keys().next().cloned();
        x
    };

    let game_id = match game_id {
        Some(id) => id,
        None => {
            let err = encode_message(&ServerMessage::Error {
                message: "No active game".to_string(),
            });
            let _ = ws_sink.send(Message::Binary(err)).await;
            // Release the slot (early return — no forward task runs).
            server_state
                .lock()
                .await
                .ws_connections
                .fetch_sub(1, Ordering::Relaxed);
            return;
        }
    };

    let room_arc = {
        let server = server_state.lock().await;
        let x = server.games.lock().await.get(&game_id).cloned();
        x
    };

    let room_arc = match room_arc {
        Some(arc) => arc,
        None => {
            let err = encode_message(&ServerMessage::Error {
                message: "Game not found".to_string(),
            });
            let _ = ws_sink.send(Message::Binary(err)).await;
            // Release the slot (early return — no forward task runs).
            server_state
                .lock()
                .await
                .ws_connections
                .fetch_sub(1, Ordering::Relaxed);
            return;
        }
    };

    // I7: create the bridge channel with its keep-latest delta ring
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<EgressMessage>();
    let ring = Arc::new(DeltaRing::new(crate::server::room::DELTA_RING_DEPTH));
    let ring_bridge = Arc::clone(&ring);
    // Spawn task to forward messages from channel to WebSocket
    tokio::spawn(async move {
        let mut ws_sink = ws_sink;
        while let Some(msg) = rx.recv().await {
            let data = match egress_vec(msg, &ring) {
                Some(data) => data,
                None => continue, // stale delta superseded by a newer ring entry
            };
            if ws_sink.send(Message::Binary(data)).await.is_err() {
                break;
            }
        }
        // Sec L2: the connection is done (the room dropped the spectator's
        // sender, or the sink failed) — release the slot reserved at accept.
        server_state
            .lock()
            .await
            .ws_connections
            .fetch_sub(1, Ordering::Relaxed);
    });

    // Send the AddSpectator command via the room's command_tx
    // We need to create a PlayerSender that bridges to the channel
    let bridge = PlayerSender::Tcp(TcpEgress {
        tx,
        ring: ring_bridge,
    });
    // Send the command to register the spectator with the room
    room_arc
        .lock()
        .await
        .command_tx
        .send(Command::AddSpectator(bridge))
        .ok();
}
