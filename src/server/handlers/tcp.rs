//! Raw-TCP game transport: the per-connection read loop with the Hello
//! handshake, plus the shared egress drain loops for raw TCP and the WS
//! bridge.

use axum::extract::ws::Message;
use futures::SinkExt;
use std::sync::atomic::Ordering;
use std::sync::Arc;
use tokio::sync::{mpsc, Mutex};
use tokio::time::Duration;
use uuid::Uuid;

use crate::server::framing;
use crate::server::game_server::GameServer;
use crate::server::protocol::{
    decode_client_message, encode_message_result, negotiate_features, ClientMessage, ServerMessage,
    PROTOCOL_VERSION,
};
use crate::server::room::{
    egress_bytes, egress_vec, DeltaRing, EgressMessage, PlayerSender, TcpEgress,
};

use super::lobby::handle_client_message;
use super::ws::remove_creator_lobby_on_disconnect;

/// I7: shared drain loop for a raw-TCP egress channel — resolves each
/// envelope against the keep-latest ring (skipping deltas a newer one
/// superseded) and writes to the connection. One message per wake; the
/// loop drains fully on its own when the reader catches up.
pub(crate) async fn tcp_egress_loop(
    mut writer: tokio::io::WriteHalf<tokio::net::TcpStream>,
    mut rx: mpsc::UnboundedReceiver<EgressMessage>,
    ring: Arc<DeltaRing>,
) {
    while let Some(msg) = rx.recv().await {
        if matches!(msg, EgressMessage::Close) {
            // M3/W7: close requested (encode failure) — drop the writer
            // half; the FIN ends the peer's stream.
            break;
        }
        let data = match egress_bytes(msg, &ring) {
            Some(data) => data,
            None => continue, // stale delta superseded by a newer ring entry
        };
        // WP3: length-prefixed frame (u32 BE + payload).
        if framing::write_frame(&mut writer, &data).await.is_err() {
            break;
        }
    }
}

/// I7: shared drain loop for the WebSocket egress bridge (the WS sink is
/// shared via a mutex while the read loop stays alive).
pub(crate) async fn ws_egress_loop<S>(
    sink: Arc<tokio::sync::Mutex<S>>,
    mut rx: mpsc::UnboundedReceiver<EgressMessage>,
    ring: Arc<DeltaRing>,
) where
    S: futures::Sink<Message> + Unpin,
{
    while let Some(msg) = rx.recv().await {
        if matches!(msg, EgressMessage::Close) {
            // M3/W7: close requested (encode failure) — send a proper WS
            // close frame, then drop the sink (the peer's onclose fires).
            let mut guard = sink.lock().await;
            let _ = guard.send(Message::Close(None)).await;
            break;
        }
        let data = match egress_vec(msg, &ring) {
            Some(data) => data,
            None => continue, // stale delta superseded by a newer ring entry
        };
        let mut guard = sink.lock().await;
        if guard.send(Message::Binary(data)).await.is_err() {
            break;
        }
    }
}

/// Nagle off for game traffic: deltas are small frames sent at up to 10 Hz,
/// so the kernel's Nagle coalescing delay (~up to 40 ms, or one RTT when an
/// ACK is outstanding) would ride on top of every small frame. The WS path
/// frames its own messages and is unaffected; this is the raw-TCP game
/// transport only.
pub(crate) fn configure_game_tcp_stream(stream: &tokio::net::TcpStream) {
    let _ = stream.set_nodelay(true);
}

/// WP3: how long a freshly-accepted connection may take to complete the
/// Hello handshake before the server drops it (an idle, un-handshaked
/// connection is useless and would otherwise pin reader + egress tasks).
pub(crate) const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(10);

pub(crate) async fn handle_tcp_client(
    stream: tokio::net::TcpStream,
    server_state: Arc<Mutex<GameServer>>,
) {
    let conn_id = Uuid::new_v4();
    configure_game_tcp_stream(&stream);
    let (mut reader, writer) = tokio::io::split(stream);
    // I7: channel carries tagged envelopes; the ring bounds delta memory
    let (tx, rx) = mpsc::unbounded_channel::<EgressMessage>();
    let tx_clone = tx.clone();
    let server_state_clone = server_state.clone();
    // PlayerSender for this TCP connection
    let egress = TcpEgress::new(tx_clone.clone());
    let ring = Arc::clone(&egress.ring);
    let player_sender = PlayerSender::Tcp(egress);

    // Spawn sender task
    tokio::spawn(tcp_egress_loop(writer, rx, ring));

    // Sec L2: reserve a connection slot — the raw TCP game port is a
    // second entry into the same game, so it shares the WS cap
    // (config.network.max_ws_connections). Over-cap clients get a clear
    // Error frame (best-effort, via the already-running egress loop) and
    // the connection is dropped.
    {
        let server = server_state.lock().await;
        let cap = server.config.network.max_ws_connections.max(1);
        if server.ws_connections.fetch_add(1, Ordering::Relaxed) >= cap {
            server.ws_connections.fetch_sub(1, Ordering::Relaxed);
            // M3: encode failure here means there is nothing to send — just
            // drop the connection (no partial frame is ever written).
            match encode_message_result(&ServerMessage::Error {
                message: "Server at maximum connections — try again later".to_string(),
            }) {
                Ok(frame) => {
                    let _ = player_sender.send_reliable(frame.into()).await;
                }
                Err(e) => {
                    tracing::error!(error = %e, "encode failed at TCP capacity reject — dropping connection");
                }
            }
            return;
        }
    }

    // Read loop — WP3: length-prefixed frames (u32 BE + CBOR payload); the
    // first frame MUST be Hello (HANDSHAKE_TIMEOUT budget, then the idle
    // connection is dropped).
    let mut handshaken = false;
    loop {
        let frame = if handshaken {
            framing::read_frame(&mut reader).await
        } else {
            match tokio::time::timeout(HANDSHAKE_TIMEOUT, framing::read_frame(&mut reader)).await {
                Ok(inner) => inner,
                Err(_) => {
                    tracing::warn!(
                        "TCP client did not complete the Hello handshake in time, closing"
                    );
                    break;
                }
            }
        };
        let frame = match frame {
            Ok(f) => f,
            Err(e) => {
                // Framing violated (length 0/oversized, connection mid-frame):
                // the stream can no longer be reassembled, so close.
                tracing::info!(error = %e, "TCP frame read failed, closing connection");
                break;
            }
        };
        let msg = match decode_client_message(&frame) {
            Ok(m) => m,
            Err(e) => {
                tracing::info!(error = %e, "Failed to decode ClientMessage (TCP), closing");
                let resp = ServerMessage::Error {
                    message: "Invalid message format".to_string(),
                };
                match encode_message_result(&resp) {
                    Ok(frame) => {
                        let _ = player_sender.send_reliable(frame.into()).await;
                    }
                    // M3: no partial frame — log and close.
                    Err(e) => tracing::error!(error = %e, "encode failed (TCP) — closing"),
                }
                break;
            }
        };
        if !handshaken {
            match msg {
                ClientMessage::Hello {
                    protocol_version,
                    features,
                } if protocol_version == PROTOCOL_VERSION => {
                    let negotiated = negotiate_features(features);
                    let ack = ServerMessage::HelloAck {
                        protocol_version,
                        features: negotiated,
                    };
                    match encode_message_result(&ack) {
                        Ok(frame) => {
                            let _ = player_sender.send_reliable(frame.into()).await;
                        }
                        // M3: the handshake ack could not be encoded — the
                        // connection cannot proceed; close it.
                        Err(e) => {
                            tracing::error!(error = %e, "HelloAck encode failed (TCP) — closing");
                            break;
                        }
                    }
                    server_state_clone
                        .lock()
                        .await
                        .negotiated_features
                        .lock()
                        .await
                        .insert(conn_id, negotiated);
                    handshaken = true;
                }
                other => {
                    tracing::info!(
                        ?other,
                        "First TCP message must be Hello v{PROTOCOL_VERSION}, closing"
                    );
                    let resp = ServerMessage::Error {
                        message: format!(
                            "Protocol error: first message must be Hello (version {PROTOCOL_VERSION})"
                        ),
                    };
                    match encode_message_result(&resp) {
                        Ok(frame) => {
                            let _ = player_sender.send_reliable(frame.into()).await;
                        }
                        // M3: no partial frame — log and close.
                        Err(e) => tracing::error!(error = %e, "encode failed (TCP) — closing"),
                    }
                    break;
                }
            }
            continue;
        }
        // M3: if the server reply to a client command could not be encoded,
        // a truncated frame would desync the peer — close the connection.
        if let Err(e) =
            handle_client_message(&conn_id, server_state.clone(), &msg, &player_sender).await
        {
            tracing::error!(error = %e, "server reply encode failed (TCP) — closing connection");
            break;
        }
    }

    // Cleanup on disconnect
    let owned_lobby = {
        let server = server_state_clone.lock().await;
        let mut player_map = server.player_map.lock().await;
        // M2: remember this connection's lobby before dropping the entry,
        // so a creator's unstarted lobby can be removed below.
        let owned = player_map.get(&conn_id).cloned();
        // Just remove the connection; the game room will handle player removal internally
        player_map.remove(&conn_id);
        server.negotiated_features.lock().await.remove(&conn_id);
        // Sec L2: release the connection slot reserved at accept time.
        server.ws_connections.fetch_sub(1, Ordering::Relaxed);
        owned
    };
    // M2: a disconnected creator's unstarted lobby is removed immediately —
    // without this it would retain up to ~1 MB of client-controlled custom
    // unit types until the LOBBY_IDLE_TTL sweep.
    if let Some((player_id, lobby_code)) = owned_lobby {
        remove_creator_lobby_on_disconnect(&server_state_clone, &player_id, &lobby_code).await;
    }
}
