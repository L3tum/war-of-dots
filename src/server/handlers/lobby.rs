//! Lobby + in-game message dispatch.
//!
//! `lobby_bridge_sender` builds the lobby-facing egress bridge;
//! `handle_client_message` is the central message router — a thin
//! dispatcher over `room::lobby_flow` (lobby arms: create/join/LLM-slot/
//! start) and `room::commands` (game-command arms: recruit/move/toggles/
//! upgrade/advisor/leaderboard/embark) — shared by the TCP and WebSocket
//! transports.

use std::sync::Arc;
use tokio::sync::{mpsc, Mutex};
use uuid::Uuid;

use crate::server::game_server::GameServer;
use crate::server::protocol::{
    encode_message_result, CborEncodeError, ClientMessage, ServerMessage,
};
use crate::server::room::{egress_bytes, DeltaRing, EgressMessage, PlayerSender, TcpEgress};

/// I7: build the lobby-facing sender from a connection sender. TCP senders
/// share the same channel + keep-latest ring; a WebSocket closure gets a
/// bridged channel with its own ring; `None` gets a throwaway channel whose
/// receiver is dropped (sends fail and are ignored at every call site).
pub(crate) fn lobby_bridge_sender(player_sender: &PlayerSender) -> PlayerSender {
    match player_sender {
        PlayerSender::Tcp(e) => PlayerSender::Tcp(e.clone()),
        PlayerSender::Websocket(_) => {
            let (tx, mut rx) = mpsc::unbounded_channel::<EgressMessage>();
            let ring = Arc::new(DeltaRing::new(crate::server::room::DELTA_RING_DEPTH));
            let egress = TcpEgress {
                tx,
                ring: Arc::clone(&ring),
            };
            let original_sender = player_sender.clone();
            tokio::spawn(async move {
                while let Some(msg) = rx.recv().await {
                    if matches!(msg, EgressMessage::Close) {
                        // M3/W7: forward the close request to the real
                        // connection (the bridge has no payload to resolve).
                        original_sender.close();
                        continue;
                    }
                    let data = match egress_bytes(msg, &ring) {
                        Some(data) => data,
                        None => continue,
                    };
                    let _ = original_sender.send_reliable(data).await;
                }
            });
            PlayerSender::Tcp(egress)
        }
        PlayerSender::None => {
            // No connection — create a throwaway channel
            let (tx, _rx) = mpsc::unbounded_channel::<EgressMessage>();
            PlayerSender::Tcp(TcpEgress::new(tx))
        }
    }
}

/// M3: fallible — returns `Err` when a server reply cannot be CBOR-encoded.
/// A truncated frame would desync the peer, so callers close the connection
/// on `Err` (the read loops do exactly that).
pub(crate) async fn handle_client_message(
    conn_id: &Uuid,
    server_state: Arc<Mutex<GameServer>>,
    msg: &ClientMessage,
    player_sender: &PlayerSender,
) -> Result<(), CborEncodeError> {
    match msg {
        // WP3: Hello is only valid as the connection's FIRST message (the
        // read loops gate it before dispatch); a repeated Hello is a
        // protocol violation.
        ClientMessage::Hello { .. } => {
            let resp = ServerMessage::Error {
                message: "Protocol error: Hello is only allowed as the first message".to_string(),
            };
            let encoded = encode_message_result(&resp)?;
            let _ = player_sender.send_reliable(encoded.into()).await;
        }
        ClientMessage::LobbyMessage(lobby_msg) => {
            // Extracted verbatim into `crate::server::room::lobby_flow`.
            crate::server::room::lobby_flow::handle_lobby_message(
                conn_id,
                server_state,
                lobby_msg,
                player_sender,
            )
            .await?;
        }
        msg => {
            // Game-command arms extracted verbatim into
            // `crate::server::room::commands`.
            crate::server::room::commands::handle_game_command(
                conn_id,
                server_state,
                msg,
                player_sender,
            )
            .await?;
        }
    }
    Ok(())
}
