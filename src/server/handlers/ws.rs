//! Game WebSocket handler (`/ws`): upgrade, Hello handshake, per-connection
//! egress bridge and disconnect cleanup.

use axum::extract::ws::{CloseFrame, Message, WebSocket, WebSocketUpgrade};
use futures::StreamExt;
use std::sync::atomic::Ordering;
use std::sync::Arc;
use tokio::sync::{mpsc, Mutex};
use uuid::Uuid;

use crate::server::game_server::GameServer;
use crate::server::protocol::{
    decode_client_message, encode_message_result, negotiate_features, ClientMessage, ServerMessage,
    PROTOCOL_VERSION,
};
use crate::server::room::{EgressMessage, PlayerSender, TcpEgress};

use super::lobby::handle_client_message;
#[cfg(test)]
use super::tcp::handle_tcp_client;
use super::tcp::{ws_egress_loop, HANDSHAKE_TIMEOUT};
#[cfg(test)]
use axum::routing::get;

// WebSocket handler for game state
pub(crate) async fn ws_handler(
    ws: WebSocketUpgrade,
    server_state: axum::extract::State<Arc<Mutex<GameServer>>>,
) -> impl axum::response::IntoResponse {
    ws.on_upgrade(move |socket| handle_ws_game(socket, server_state.0))
}

// WebSocket game handler — same game logic, different transport
pub(crate) async fn handle_ws_game(mut socket: WebSocket, server_state: Arc<Mutex<GameServer>>) {
    let conn_id = Uuid::new_v4();
    // Sec L2: reserve a connection slot (config.network.max_ws_connections,
    // shared with /ws/spectator). Over-cap connections are rejected with a
    // clear Error message + WS close code 1013 ("Try Again Later") BEFORE
    // any per-connection task is spawned; the slot is released in the
    // disconnect cleanup below.
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
                    tracing::error!(error = %e, "encode failed at WS capacity reject — closing connection");
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
    // Use a single WebSocket with shared access via channels
    let (ws_sink, mut ws_stream) = socket.split();
    let ws_sink = Arc::new(Mutex::new(ws_sink));

    // I7: bridge channel carries tagged envelopes; the ring bounds delta memory
    let (tx, rx) = mpsc::unbounded_channel::<EgressMessage>();
    let egress = TcpEgress::new(tx);
    let ring = Arc::clone(&egress.ring);
    let actual_player_sender = PlayerSender::Tcp(egress);
    let server_state_clone = server_state.clone();

    // Spawn the websocket sender task
    let ws_sink_clone = Arc::clone(&ws_sink);
    tokio::spawn(ws_egress_loop(ws_sink_clone, rx, ring));

    // Read loop — receive WebSocket messages. WP3: the first binary message
    // MUST be Hello (HANDSHAKE_TIMEOUT budget). WS frames are self-framing —
    // no length prefix on this path.
    let mut handshaken = false;
    loop {
        let incoming = if handshaken {
            ws_stream.next().await
        } else {
            match tokio::time::timeout(HANDSHAKE_TIMEOUT, ws_stream.next()).await {
                Ok(m) => m,
                Err(_) => {
                    tracing::warn!(
                        "WS client did not complete the Hello handshake in time, closing"
                    );
                    break;
                }
            }
        };
        match incoming {
            Some(Ok(Message::Binary(data))) => {
                // Message size limit
                if data.len() > 1_048_576 {
                    tracing::warn!(
                        msg_len = data.len(),
                        "Message exceeds size limit (WS), closing connection"
                    );
                    break;
                }
                let msg = match decode_client_message(&data) {
                    Ok(m) => m,
                    Err(e) => {
                        tracing::info!(error = %e, msg_len = data.len(), "Failed to decode ClientMessage (WS)");
                        let resp = ServerMessage::Error {
                            message: "Invalid message format".to_string(),
                        };
                        match encode_message_result(&resp) {
                            Ok(frame) => {
                                let _ = actual_player_sender.send_reliable(frame.into()).await;
                            }
                            // M3: encode failure — a half frame is
                            // unrecoverable for the peer; close.
                            Err(e) => {
                                tracing::error!(error = %e, "encode failed (WS) — closing connection");
                                break;
                            }
                        }
                        // A malformed FIRST frame can't be the Hello handshake — fatal.
                        if !handshaken {
                            break;
                        }
                        continue; // post-handshake: keep the connection (legacy behaviour)
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
                                    let _ = actual_player_sender.send_reliable(frame.into()).await;
                                }
                                // M3: the handshake ack could not be encoded —
                                // the connection cannot proceed; close.
                                Err(e) => {
                                    tracing::error!(error = %e, "HelloAck encode failed (WS) — closing connection");
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
                                "First WS message must be Hello v{PROTOCOL_VERSION}, closing"
                            );
                            let resp = ServerMessage::Error {
                                message: format!(
                                    "Protocol error: first message must be Hello (version {PROTOCOL_VERSION})"
                                ),
                            };
                            match encode_message_result(&resp) {
                                Ok(frame) => {
                                    let _ = actual_player_sender.send_reliable(frame.into()).await;
                                }
                                // M3: no partial frame — log and close.
                                Err(e) => {
                                    tracing::error!(error = %e, "encode failed (WS) — closing connection")
                                }
                            }
                            break;
                        }
                    }
                    continue;
                }
                // M3: if the server reply to a client command could not be
                // encoded, a truncated frame would desync the peer — close
                // the connection (a half frame is unrecoverable).
                if let Err(e) = handle_client_message(
                    &conn_id,
                    server_state.clone(),
                    &msg,
                    &actual_player_sender,
                )
                .await
                {
                    tracing::error!(error = %e, "server reply encode failed (WS) — closing connection");
                    break;
                }
            }
            Some(Ok(Message::Close(_))) | None => break,
            Some(Ok(Message::Text(_)))
            | Some(Ok(Message::Ping(_)))
            | Some(Ok(Message::Pong(_))) => {
                // Ignore text, ping, pong
            }
            Some(Err(_)) => break,
        }
    }

    // Cleanup on disconnect
    let owned_lobby = {
        let server = server_state_clone.lock().await;
        let mut player_map = server.player_map.lock().await;
        // M2: remember this connection's lobby before dropping the entry,
        // so a creator's unstarted lobby can be removed below.
        let owned = player_map.get(&conn_id).cloned();
        player_map.remove(&conn_id);
        server.negotiated_features.lock().await.remove(&conn_id);
        // Sec L2: release the slot reserved at accept.
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

/// M2: on disconnect, remove the lobby if the departing connection was its
/// CREATOR. Only an unstarted lobby can still be present here — StartGame
/// consumes the lobby when the game spawns — so no live game is disturbed.
/// Lobbies the connection merely joined (as a guest) are left in place: the
/// creator is still in charge (and the idle TTL sweep is the backstop).
pub(crate) async fn remove_creator_lobby_on_disconnect(
    server_state: &Arc<Mutex<GameServer>>,
    player_id: &str,
    lobby_code: &str,
) {
    let server = server_state.lock().await;
    let mut lobby_manager = server.lobby_manager.lock().await;
    let is_creator = lobby_manager
        .get_lobby(lobby_code)
        .is_some_and(|l| l.creator == player_id);
    if is_creator {
        lobby_manager.remove_lobby(lobby_code);
        tracing::info!(
            lobby_code = %lobby_code,
            "Removed unstarted lobby: creator disconnected"
        );
    }
}

// ---------------------------------------------------------------------------
// WP3: handshake tests — the three-scenario matrix over real sockets.
// ---------------------------------------------------------------------------
#[cfg(test)]
mod wp3_handshake_tests {
    use super::*;
    use crate::config::Config;
    use crate::server::framing::{read_frame, write_frame};
    use crate::server::protocol::{
        ClientMessage, GameDeltaMessage, GameStateMessage, LobbyMessage, LobbySettings,
        PlayerState, ServerMessage, F_AOI, F_COMPACT_ENTITIES, F_GRID_RLE, PROTOCOL_VERSION,
    };
    use std::time::Duration;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::{TcpListener, TcpStream};

    /// A real GameServer with throwaway storage (unique temp dir per test so
    /// parallel tests never share a sqlite file).
    async fn make_server() -> Arc<Mutex<GameServer>> {
        let dir = std::env::temp_dir().join(format!("wod-wp3-{}", Uuid::new_v4()));
        let toml_str = format!(
            r#"
[llm]

[standard_ruleset]
enabled = true
unit_types = []

[storage]
database_path = "{db}"
replay_path = "{replay}"
"#,
            db = dir.join("db.sqlite").display(),
            replay = dir.join("replays").display(),
        );
        let cfg: Config = toml::from_str::<Config>(&toml_str).unwrap().resolve();
        Arc::new(Mutex::new(GameServer::new(cfg, 0).await.unwrap()))
    }

    fn cbor_bytes<T: serde::Serialize>(msg: &T) -> Vec<u8> {
        let mut buf = Vec::new();
        ciborium::into_writer(msg, &mut buf).expect("CBOR encode");
        buf
    }

    fn default_lobby_settings() -> LobbySettings {
        toml::from_str(
            r#"
            map = "default"
            use_standard_ruleset = true
            llm_thinking_delay = 0.0
            "#,
        )
        .expect("default lobby settings")
    }

    /// Start a server task that runs `handle_tcp_client` on the accepted
    /// connection; returns the client side of the pair.
    async fn spawn_tcp_server(
        server: Arc<Mutex<GameServer>>,
    ) -> (TcpStream, tokio::task::JoinHandle<()>) {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let handle = tokio::spawn({
            let server = server.clone();
            async move {
                let (stream, _) = listener.accept().await.unwrap();
                handle_tcp_client(stream, server).await;
            }
        });
        let client = TcpStream::connect(addr).await.unwrap();
        (client, handle)
    }

    /// Read frames until the first matching one; returns it (others are
    /// tolerated — e.g. a LobbyUpdate broadcast following LobbyCreated).
    async fn read_until_match(
        client: &mut TcpStream,
        mut is_match: impl FnMut(&ServerMessage) -> bool,
    ) -> ServerMessage {
        let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
        loop {
            let frame = tokio::time::timeout_at(deadline, read_frame(client))
                .await
                .expect("frame in time")
                .expect("connection still open");
            let msg: ServerMessage = ciborium::from_reader(&frame[..]).expect("valid server frame");
            if is_match(&msg) {
                return msg;
            }
        }
    }

    /// Scenario 1: Hello (requesting every v1 feature bit) → HelloAck with the
    /// negotiated (ANDed) set, then normal lobby traffic flows.
    #[tokio::test]
    async fn tcp_handshake_hello_then_lobby_created() {
        // The server encodes a HelloAck during this handshake: serialize with
        // the encode-fault tests (their fault switch is process-global).
        let _exclusive = crate::server::protocol::TEST_FAULT_EXCLUSIVE.lock().await;
        let server = make_server().await;
        let (mut client, handle) = spawn_tcp_server(server.clone()).await;

        let hello = ClientMessage::Hello {
            protocol_version: PROTOCOL_VERSION,
            features: F_GRID_RLE | F_COMPACT_ENTITIES | F_AOI,
        };
        write_frame(&mut client, &cbor_bytes(&hello)).await.unwrap();

        let ack =
            read_until_match(&mut client, |m| matches!(m, ServerMessage::HelloAck { .. })).await;
        match ack {
            ServerMessage::HelloAck {
                protocol_version,
                features,
            } => {
                assert_eq!(protocol_version, PROTOCOL_VERSION);
                // Nothing is supported yet: every requested bit is ANDed out.
                assert_eq!(features, 0);
            }
            other => panic!("expected HelloAck, got {other:?}"),
        }

        // The negotiated set is recorded for the connection.
        {
            let s = server.lock().await;
            let feats = s.negotiated_features.lock().await;
            assert_eq!(feats.len(), 1);
            assert_eq!(feats.values().next().copied(), Some(0));
        }

        // Lobby traffic works after the handshake.
        let create = ClientMessage::LobbyMessage(LobbyMessage::CreateLobby {
            settings: default_lobby_settings(),
            password: None,
            player_name: Some("Wp3Test".to_string()),
        });
        write_frame(&mut client, &cbor_bytes(&create))
            .await
            .unwrap();
        let created = read_until_match(&mut client, |m| {
            matches!(m, ServerMessage::LobbyCreated { .. })
        })
        .await;
        match created {
            ServerMessage::LobbyCreated { lobby_code } => assert!(!lobby_code.is_empty()),
            other => panic!("expected LobbyCreated, got {other:?}"),
        }

        // Disconnect: the negotiated-features entry is cleaned up.
        drop(client);
        let mut cleaned = false;
        for _ in 0..100 {
            if server
                .lock()
                .await
                .negotiated_features
                .lock()
                .await
                .is_empty()
            {
                cleaned = true;
                break;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        assert!(cleaned, "negotiated_features not cleaned up on disconnect");
        handle.abort();
    }

    /// Scenario: a standard-ruleset lobby — where the client payload omits
    /// `max_players` entirely, as the browser's standard mode does — must
    /// still be joinable. Regression for a real bug (caught by the live E2E
    /// suite): serde defaults the omitted field to 0, and the join full-check
    /// `players.len() >= max_players` was then immediately true (1 >= 0), so
    /// every join failed with "Lobby full". create_lobby normalizes 0 → 4.
    #[tokio::test]
    async fn tcp_standard_lobby_omitting_max_players_is_joinable() {
        let _exclusive = crate::server::protocol::TEST_FAULT_EXCLUSIVE.lock().await;
        let server = make_server().await;
        let (mut host, host_handle) = spawn_tcp_server(server.clone()).await;

        let hello = ClientMessage::Hello {
            protocol_version: PROTOCOL_VERSION,
            features: 0,
        };
        write_frame(&mut host, &cbor_bytes(&hello)).await.unwrap();
        read_until_match(&mut host, |m| matches!(m, ServerMessage::HelloAck { .. })).await;

        // default_lobby_settings() deliberately has no max_players key — the
        // exact shape the browser sends in standard mode.
        let create = ClientMessage::LobbyMessage(LobbyMessage::CreateLobby {
            settings: default_lobby_settings(),
            password: None,
            player_name: Some("Host".to_string()),
        });
        write_frame(&mut host, &cbor_bytes(&create)).await.unwrap();
        let code = match read_until_match(&mut host, |m| {
            matches!(m, ServerMessage::LobbyCreated { .. })
        })
        .await
        {
            ServerMessage::LobbyCreated { lobby_code } => lobby_code,
            other => panic!("expected LobbyCreated, got {other:?}"),
        };
        assert!(!code.is_empty());

        // A second client joins the same lobby.
        let (mut guest, guest_handle) = spawn_tcp_server(server.clone()).await;
        write_frame(&mut guest, &cbor_bytes(&hello)).await.unwrap();
        read_until_match(&mut guest, |m| matches!(m, ServerMessage::HelloAck { .. })).await;
        let join = ClientMessage::LobbyMessage(LobbyMessage::JoinLobby {
            lobby_code: code.clone(),
            password: None,
            player_name: Some("Guest".to_string()),
        });
        write_frame(&mut guest, &cbor_bytes(&join)).await.unwrap();

        // The guest gets LobbyJoined — not Error("Lobby full").
        match read_until_match(&mut guest, |m| {
            matches!(m, ServerMessage::LobbyJoined { .. })
                || matches!(m, ServerMessage::Error { .. })
        })
        .await
        {
            ServerMessage::LobbyJoined { lobby_code } => assert_eq!(lobby_code, code),
            other => panic!("expected LobbyJoined, got {other:?}"),
        }

        // The host converges on the two-player roster (LobbyUpdate broadcast).
        let update = read_until_match(&mut host, |m| match m {
            ServerMessage::LobbyUpdate { players, .. } => players.len() == 2,
            _ => false,
        })
        .await;
        match update {
            ServerMessage::LobbyUpdate {
                players, settings, ..
            } => {
                assert_eq!(players.len(), 2);
                assert_eq!(
                    settings.max_players, 4,
                    "unspecified cap must normalize to the default, not 0"
                );
            }
            other => panic!("expected LobbyUpdate, got {other:?}"),
        }

        host_handle.abort();
        guest_handle.abort();
    }

    /// Scenario 2: first frame is not a Hello → Error, then the server closes.
    #[tokio::test]
    async fn tcp_rejects_non_hello_first_message() {
        let server = make_server().await;
        let (mut client, handle) = spawn_tcp_server(server).await;

        let create = ClientMessage::LobbyMessage(LobbyMessage::CreateLobby {
            settings: default_lobby_settings(),
            password: None,
            player_name: Some("Wp3Test".to_string()),
        });
        write_frame(&mut client, &cbor_bytes(&create))
            .await
            .unwrap();

        let err = read_until_match(&mut client, |m| matches!(m, ServerMessage::Error { .. })).await;
        match err {
            ServerMessage::Error { message } => {
                assert!(message.contains("Hello"), "unexpected error: {message}");
            }
            other => panic!("expected Error, got {other:?}"),
        }

        // The handshake violation is fatal: the server closes the connection.
        let mut buf = [0u8; 8];
        let n = tokio::time::timeout(Duration::from_secs(5), client.read(&mut buf))
            .await
            .expect("close in time")
            .expect("read before EOF");
        assert_eq!(n, 0, "server must close after a handshake violation");
        handle.abort();
    }

    /// Scenario 3: Hello with an unsupported protocol version → Error + close.
    #[tokio::test]
    async fn tcp_rejects_wrong_protocol_version() {
        let server = make_server().await;
        let (mut client, handle) = spawn_tcp_server(server).await;

        let hello = ClientMessage::Hello {
            protocol_version: PROTOCOL_VERSION + 1,
            features: 0,
        };
        write_frame(&mut client, &cbor_bytes(&hello)).await.unwrap();

        let err = read_until_match(&mut client, |m| matches!(m, ServerMessage::Error { .. })).await;
        match err {
            ServerMessage::Error { message } => {
                assert!(message.contains("Hello"), "unexpected error: {message}");
            }
            other => panic!("expected Error, got {other:?}"),
        }

        let mut buf = [0u8; 8];
        let n = tokio::time::timeout(Duration::from_secs(5), client.read(&mut buf))
            .await
            .expect("close in time")
            .expect("read before EOF");
        assert_eq!(n, 0, "server must close after a version mismatch");
        handle.abort();
    }

    // ── WS path ────────────────────────────────────────────────────────────

    /// Build a masked client→server WS binary frame (clients MUST mask).
    fn mask_ws_binary(payload: &[u8]) -> Vec<u8> {
        let mut frame = vec![0x82u8]; // FIN + binary opcode
        if payload.len() < 126 {
            frame.push(payload.len() as u8 | 0x80);
        } else if payload.len() < 65536 {
            frame.push(126 | 0x80);
            frame.extend_from_slice(&(payload.len() as u16).to_be_bytes());
        } else {
            frame.push(127 | 0x80);
            frame.extend_from_slice(&(payload.len() as u64).to_be_bytes());
        }
        let mask = [0xA1u8, 0xB2, 0xC3, 0xD4];
        frame.extend_from_slice(&mask);
        for (i, b) in payload.iter().enumerate() {
            frame.push(b ^ mask[i % 4]);
        }
        frame
    }

    /// Parse one server→client WS frame from the FRONT of `data`.
    /// Returns (bytes consumed, payload); payload is empty for control
    /// frames (ping/pong/close), which the caller skips or treats as EOF.
    /// The egress loop sends one unfragmented binary frame per message, so
    /// a single frame walk is all that is needed here.
    fn take_ws_frame(data: &[u8]) -> Option<(usize, Vec<u8>)> {
        if data.len() < 2 {
            return None;
        }
        let opcode = data[0] & 0x0f;
        let mut len = (data[1] & 0x7f) as usize;
        let mut off = 2;
        if len == 126 {
            if off + 2 > data.len() {
                return None;
            }
            len = u16::from_be_bytes([data[off], data[off + 1]]) as usize;
            off += 2;
        } else if len == 127 {
            if off + 8 > data.len() {
                return None;
            }
            let bytes: [u8; 8] = data[off..off + 8].try_into().ok()?;
            len = u64::from_be_bytes(bytes) as usize;
            off += 8;
        }
        if data[1] & 0x80 != 0 {
            off += 4; // masked (tolerated; the server sends unmasked)
        }
        if off + len > data.len() {
            return None; // frame not complete yet
        }
        if opcode <= 0x2 {
            Some((off + len, data[off..off + len].to_vec()))
        } else {
            // ping/pong/close: consumed, no payload
            Some((off + len, Vec::new()))
        }
    }

    /// Read until one complete non-control WS frame is available; advances
    /// `raw` past each frame it consumes (so frames are never re-delivered).
    async fn read_ws_until(client: &mut TcpStream, raw: &mut Vec<u8>) -> Option<Vec<u8>> {
        let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
        let mut buf = [0u8; 16 * 1024];
        loop {
            if let Some((consumed, payload)) = take_ws_frame(raw) {
                raw.drain(..consumed);
                if !payload.is_empty() {
                    return Some(payload);
                }
                continue; // control frame — keep going
            }
            assert!(
                tokio::time::Instant::now() < deadline,
                "no WS frame in time"
            );
            let n = client.read(&mut buf).await.expect("ws client read");
            if n == 0 {
                return None;
            }
            raw.extend_from_slice(&buf[..n]);
        }
    }

    /// Scenario 4 (WS): binary Hello → HelloAck, then lobby traffic — the
    /// exact path the browser client takes.
    #[tokio::test]
    async fn ws_handshake_hello_then_lobby_created() {
        let _exclusive = crate::server::protocol::TEST_FAULT_EXCLUSIVE.lock().await;
        let server = make_server().await;
        let state = server.clone();
        let app = axum::Router::new().route(
            "/ws",
            get(move |ws: WebSocketUpgrade| {
                let state = state.clone();
                async move { ws.on_upgrade(move |socket| handle_ws_game(socket, state)) }
            }),
        );
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            axum::serve(listener, app).await.ok();
        });

        let mut client = TcpStream::connect(addr).await.unwrap();
        client
            .write_all(
                b"GET /ws HTTP/1.1\r\n\
                  Host: localhost\r\n\
                  Upgrade: websocket\r\n\
                  Connection: Upgrade\r\n\
                  Sec-WebSocket-Key: dGhlIHNhbXBsZSBub25jZQ==\r\n\
                  Sec-WebSocket-Version: 13\r\n\
                  \r\n",
            )
            .await
            .unwrap();
        // Consume the 101 upgrade response headers.
        let mut raw = Vec::new();
        loop {
            if let Some(pos) = crate::server::room::i7_egress_tests::find(&raw, b"\r\n\r\n") {
                raw.drain(..=pos + 3);
                break;
            }
            let mut buf = [0u8; 4096];
            let n = client.read(&mut buf).await.unwrap();
            assert!(n > 0, "upgrade response closed");
            raw.extend_from_slice(&buf[..n]);
        }

        // Hello as a masked binary frame.
        let hello = ClientMessage::Hello {
            protocol_version: PROTOCOL_VERSION,
            features: 0,
        };
        client
            .write_all(&mask_ws_binary(&cbor_bytes(&hello)))
            .await
            .unwrap();

        // HelloAck back (server frames are unmasked).
        let payload = read_ws_until(&mut client, &mut raw)
            .await
            .expect("HelloAck frame");
        let ack: ServerMessage = ciborium::from_reader(&payload[..]).expect("HelloAck cbor");
        match ack {
            ServerMessage::HelloAck {
                protocol_version,
                features,
            } => {
                assert_eq!(protocol_version, PROTOCOL_VERSION);
                assert_eq!(features, 0);
            }
            other => panic!("expected HelloAck, got {other:?}"),
        }

        // Lobby traffic after the handshake.
        let create = ClientMessage::LobbyMessage(LobbyMessage::CreateLobby {
            settings: default_lobby_settings(),
            password: None,
            player_name: Some("Wp3Ws".to_string()),
        });
        client
            .write_all(&mask_ws_binary(&cbor_bytes(&create)))
            .await
            .unwrap();
        let mut got_lobby_created = false;
        for _ in 0..8 {
            let payload = read_ws_until(&mut client, &mut raw)
                .await
                .expect("lobby frame");
            if let Ok(msg) = ciborium::from_reader::<ServerMessage, _>(&payload[..]) {
                if matches!(msg, ServerMessage::LobbyCreated { .. }) {
                    got_lobby_created = true;
                    break;
                }
            }
        }
        assert!(got_lobby_created, "LobbyCreated never arrived over WS");
        assert_eq!(
            server.lock().await.negotiated_features.lock().await.len(),
            1
        );
        drop(client);
    }

    /// Guards path-param routing in this dependency set (axum 0.7.9 +
    /// matchit 0.7.3). This matchit version only matches the `:param`
    /// syntax — `{param}` (the axum 0.8 / matchit 0.8 form) does not match
    /// and silently 404s. The live replay loop hit exactly that: the
    /// `/replays/{name}` route never matched, so `serve_replay` was never
    /// called. If a lockfile change alters matchit's path semantics, fail
    /// here in CI rather than silently breaking routes in production.
    #[tokio::test]
    async fn path_param_routes_use_colon_syntax() {
        use axum::extract::Path;
        use axum::routing::get;

        let app = axum::Router::new()
            .route("/static", get(|| async { "static-ok" }))
            .route(
                "/tp2/:name",
                get(|Path(n): Path<String>| async move { format!("hi {n}") }),
            );

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind");
        let port = listener.local_addr().unwrap().port();
        tokio::spawn(async move {
            axum::serve(listener, app).await.ok();
        });
        // Give the accept loop a head start.
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;

        let client = reqwest::Client::new();
        let static_body = client
            .get(format!("http://127.0.0.1:{port}/static"))
            .send()
            .await
            .expect("request static")
            .text()
            .await
            .expect("body static");
        assert_eq!(static_body, "static-ok", "static control route must match");

        let b = client
            .get(format!("http://127.0.0.1:{port}/tp2/nodot"))
            .send()
            .await
            .expect("request B")
            .text()
            .await
            .expect("body B");
        assert_eq!(
            b, "hi nodot",
            "`:name` route must match — this matchit version does NOT support `{{name}}`"
        );
    }

    /// Regression guard: the replay LIST page (`/replay`) must be served by
    /// the SPA index handler. Before the explicit `/replay` route existed,
    /// the request fell through to the static-file fallback and 404'd on
    /// the real server — which is why the list-UI replay path was dead even
    /// though the embedded e2e static server happened to serve it. Both
    /// slash variants must work (main.ts routes on either).
    #[tokio::test]
    async fn replay_page_served_at_replay_route() {
        let server = make_server().await;
        let app = GameServer::build_router(server);
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        tokio::spawn(async move {
            axum::serve(listener, app).await.ok();
        });
        tokio::time::sleep(Duration::from_millis(100)).await;

        let client = reqwest::Client::new();
        for path in ["/replay", "/replay/"] {
            let resp = client
                .get(format!("http://127.0.0.1:{port}{path}"))
                .send()
                .await
                .expect("request replay page");
            assert_eq!(resp.status(), 200, "{path} must serve the SPA page");
            let body = resp.text().await.expect("body");
            assert!(
                body.contains("<!DOCTYPE html>") || body.contains("<html"),
                "{path} must return index.html (SPA), got: {body:.80}"
            );
        }
    }

    /// `/replays/:name` must refuse traversal-style names (404, no file
    /// read) and serve an existing recording verbatim.
    #[tokio::test]
    async fn serve_replay_guard_and_roundtrip() {
        let server = make_server().await;

        // Seed a recording the way the game loop writes it, plus the
        // game_metadata row both replay endpoints resolve through.
        let (file_name, stored_path) = {
            let server = server.lock().await;
            let dir = server.config.storage.replay_path.clone();
            let dir_path = std::path::Path::new(&dir);
            std::fs::create_dir_all(dir_path).unwrap();
            let file_name = "roundtrip-game.jsonl".to_string();
            let body = "line-one\nline-two\n";
            std::fs::write(dir_path.join(&file_name), body).unwrap();
            let file_path = dir_path.join(&file_name);
            let stored_path = file_path.display().to_string();
            let db = server.storage.db.lock().unwrap();
            let params: Vec<sqlite::Value> = vec![
                sqlite::Value::String("roundtrip-game".to_string()),
                sqlite::Value::String(String::new()),
                sqlite::Value::String("[]".to_string()),
                sqlite::Value::Integer(1_000),
                sqlite::Value::Integer(1_100),
                sqlite::Value::String(String::new()),
                sqlite::Value::String(stored_path.clone()),
            ];
            let mut ins = db
                .prepare(
                    "INSERT OR IGNORE INTO game_metadata (id, settings, players, start_time, end_time, winner, replay_path) VALUES (?, ?, ?, ?, ?, ?, ?)",
                )
                .expect("metadata prepare");
            ins.bind(params.as_slice()).expect("metadata bind");
            ins.next().expect("metadata insert");
            (file_name, stored_path)
        };

        let app = GameServer::build_router(server);
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        tokio::spawn(async move {
            axum::serve(listener, app).await.ok();
        });
        tokio::time::sleep(Duration::from_millis(100)).await;

        let client = reqwest::Client::new();
        // The recording is served verbatim.
        let resp = client
            .get(format!("http://127.0.0.1:{port}/replays/{file_name}"))
            .send()
            .await
            .expect("request recording");
        assert_eq!(resp.status(), 200);
        assert_eq!(resp.text().await.expect("body"), "line-one\nline-two\n");
        // Traversal / bad names are refused with 404 (nothing read).
        for bad in ["a..b.jsonl", "nope.jsonl", "nope"] {
            let resp = client
                .get(format!("http://127.0.0.1:{port}/replays/{bad}"))
                .send()
                .await
                .expect("request bad name");
            assert_eq!(resp.status(), 404, "`{bad}` must be refused");
        }
        let _ = stored_path;
    }

    /// End-to-end `/ws/replay` contract: the server streams the RECORDED
    /// JSONL re-wrapped as binary CBOR `ServerMessage` frames — `FullState`
    /// first (from the `full_state` line), one `DeltaState` per `delta`
    /// line — then closes. This is exactly what the frontend's
    /// `connectReplayGame` (read-only `Connection`) consumes.
    #[tokio::test]
    async fn replay_ws_streams_recorded_frames() {
        let server = make_server().await;

        // Seed a 2-event recording with real wire payloads.
        {
            let full = GameStateMessage {
                vision_grid: None,
                border_grid: None,
                terrain: vec![vec!["grass".to_string(); 4]; 4],
                units: Vec::new(),
                cities: Vec::new(),
                player: PlayerState {
                    funds: 100,
                    units_owned: 1,
                    player_id: "player_0".to_string(),
                },
            };
            let delta = GameDeltaMessage {
                vision_grid_diff: None,
                vision_cells_changed: None,
                border_grid_diff: None,
                border_cells_changed: None,
                grid_width: None,
                units_added: Arc::new(Vec::new()),
                units_removed: Arc::new(Vec::new()),
                unit_updates: Arc::new(Vec::new()),
                city_updates: Arc::new(Vec::new()),
                player: PlayerState {
                    funds: 90,
                    units_owned: 1,
                    player_id: "player_0".to_string(),
                },
            };
            let server = server.lock().await;
            let dir = server.config.storage.replay_path.clone();
            let dir_path = std::path::Path::new(&dir);
            std::fs::create_dir_all(dir_path).unwrap();
            let file_path = dir_path.join("replay-e2e.jsonl");
            let mut jsonl = String::new();
            jsonl.push_str(&format!(
                "{{\"full_state\":{}}}\n",
                serde_json::to_string(&full).unwrap()
            ));
            jsonl.push_str(&format!(
                "{{\"delta\":{}}}\n",
                serde_json::to_string(&delta).unwrap()
            ));
            jsonl.push_str("{\"event\":\"game_end\",\"time\":1}\n");
            std::fs::write(&file_path, jsonl).unwrap();

            let db = server.storage.db.lock().unwrap();
            let params: Vec<sqlite::Value> = vec![
                sqlite::Value::String("replay-e2e".to_string()),
                sqlite::Value::String(String::new()),
                sqlite::Value::String("[]".to_string()),
                sqlite::Value::Integer(1_000),
                sqlite::Value::Integer(1_100),
                sqlite::Value::String(String::new()),
                sqlite::Value::String(file_path.display().to_string()),
            ];
            let mut ins = db
                .prepare(
                    "INSERT OR IGNORE INTO game_metadata (id, settings, players, start_time, end_time, winner, replay_path) VALUES (?, ?, ?, ?, ?, ?, ?)",
                )
                .expect("metadata prepare");
            ins.bind(params.as_slice()).expect("metadata bind");
            ins.next().expect("metadata insert");
        }

        let app = GameServer::build_router(server);
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            axum::serve(listener, app).await.ok();
        });
        tokio::time::sleep(Duration::from_millis(100)).await;

        let mut client = TcpStream::connect(addr).await.unwrap();
        client
            .write_all(
                b"GET /ws/replay?game_id=replay-e2e HTTP/1.1\r\n\
                  Host: localhost\r\n\
                  Upgrade: websocket\r\n\
                  Connection: Upgrade\r\n\
                  Sec-WebSocket-Key: dGhlIHNhbXBsZSBub25jZQ==\r\n\
                  Sec-WebSocket-Version: 13\r\n\
                  \r\n",
            )
            .await
            .unwrap();
        // Consume the 101 upgrade response headers.
        let mut raw = Vec::new();
        loop {
            if let Some(pos) = crate::server::room::i7_egress_tests::find(&raw, b"\r\n\r\n") {
                raw.drain(..=pos + 3);
                break;
            }
            let mut buf = [0u8; 4096];
            let n = tokio::time::timeout(Duration::from_secs(5), client.read(&mut buf))
                .await
                .expect("101 within 5s")
                .expect("socket open");
            if n == 0 {
                panic!("no 101 upgrade response");
            }
            raw.extend_from_slice(&buf[..n]);
        }

        // Frame 1: FullState, from the recorded full_state line.
        let payload = read_ws_until(&mut client, &mut raw)
            .await
            .expect("FullState frame");
        let msg: ServerMessage = ciborium::from_reader(&payload[..]).expect("valid cbor");
        match msg {
            ServerMessage::FullState(state) => {
                assert_eq!(
                    state.player.funds, 100,
                    "full_state line must map to FullState"
                );
                assert_eq!(state.terrain.len(), 4, "terrain comes from the recording");
            }
            other => panic!("expected FullState first, got {other:?}"),
        }

        // Frame 2: DeltaState, from the recorded delta line.
        let payload = read_ws_until(&mut client, &mut raw)
            .await
            .expect("DeltaState frame");
        let msg: ServerMessage = ciborium::from_reader(&payload[..]).expect("valid cbor");
        match msg {
            ServerMessage::DeltaState(d) => {
                assert_eq!(d.player.funds, 90, "delta line must map to DeltaState");
            }
            other => panic!("expected DeltaState second, got {other:?}"),
        }

        // Then the stream closes (the game_end marker ends it) — no further
        // data frames; control frames (e.g. close) are tolerated.
        let mut buffered = raw;
        loop {
            if let Some((consumed, payload)) = take_ws_frame(&buffered) {
                buffered.drain(..consumed);
                if !payload.is_empty() {
                    panic!("unexpected data frame after game_end: {payload:02x?}");
                }
                continue;
            }
            let mut buf = [0u8; 4096];
            let n = tokio::time::timeout(Duration::from_secs(5), client.read(&mut buf))
                .await
                .expect("EOF within 5s")
                .expect("socket read");
            if n == 0 {
                break; // server closed after the marker
            }
            buffered.extend_from_slice(&buf[..n]);
        }
    }

    /// `/ws/replay` with a malformed `game_id` must not touch the
    /// filesystem: a single binary `Error` frame, then close.
    #[tokio::test]
    async fn replay_ws_rejects_bad_game_id() {
        let server = make_server().await;
        let app = GameServer::build_router(server);
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            axum::serve(listener, app).await.ok();
        });
        tokio::time::sleep(Duration::from_millis(100)).await;

        let mut client = TcpStream::connect(addr).await.unwrap();
        client
            .write_all(
                b"GET /ws/replay?game_id=..%2F..%2Fetc%2Fpasswd HTTP/1.1\r\n\
                  Host: localhost\r\n\
                  Upgrade: websocket\r\n\
                  Connection: Upgrade\r\n\
                  Sec-WebSocket-Key: dGhlIHNhbXBsZSBub25jZQ==\r\n\
                  Sec-WebSocket-Version: 13\r\n\
                  \r\n",
            )
            .await
            .unwrap();
        let mut raw = Vec::new();
        loop {
            if let Some(pos) = crate::server::room::i7_egress_tests::find(&raw, b"\r\n\r\n") {
                raw.drain(..=pos + 3);
                break;
            }
            let mut buf = [0u8; 4096];
            let n = tokio::time::timeout(Duration::from_secs(5), client.read(&mut buf))
                .await
                .expect("101 within 5s")
                .expect("socket open");
            if n == 0 {
                panic!("no 101 upgrade response");
            }
            raw.extend_from_slice(&buf[..n]);
        }

        let payload = read_ws_until(&mut client, &mut raw)
            .await
            .expect("Error frame");
        let msg: ServerMessage = ciborium::from_reader(&payload[..]).expect("valid cbor");
        match msg {
            ServerMessage::Error { message } => {
                assert!(
                    message.contains("game_id") || message.contains("Invalid"),
                    "expected a game_id rejection, got: {message}"
                );
            }
            other => panic!("expected Error, got {other:?}"),
        }
    }

    // ── M2: unbounded lobby creation (cap / double-creation / creator-disconnect) ──

    /// make_server variant with an explicit `max_lobbies` cap.
    async fn make_server_with_lobby_cap(cap: usize) -> Arc<Mutex<GameServer>> {
        let dir = std::env::temp_dir().join(format!("wod-m2-{}", Uuid::new_v4()));
        let toml_str = format!(
            r#"
[llm]

[standard_ruleset]
enabled = true
unit_types = []

[storage]
database_path = "{db}"
replay_path = "{replay}"

[network]
max_lobbies = {cap}
"#,
            db = dir.join("db.sqlite").display(),
            replay = dir.join("replays").display(),
        );
        let cfg: Config = toml::from_str::<Config>(&toml_str).unwrap().resolve();
        Arc::new(Mutex::new(GameServer::new(cfg, 0).await.unwrap()))
    }

    /// M2a: a connection that already owns (or joined) a lobby cannot create
    /// a second one — the single-valued player_map entry would make the
    /// second lobby unreachable from the disconnect cleanup (a leak retaining
    /// up to ~1 MB of client-controlled custom unit types).
    #[tokio::test]
    async fn second_lobby_creation_from_same_connection_is_rejected() {
        let _exclusive = crate::server::protocol::TEST_FAULT_EXCLUSIVE.lock().await;
        let server = make_server().await;
        let (mut client, handle) = spawn_tcp_server(server.clone()).await;

        let hello = ClientMessage::Hello {
            protocol_version: PROTOCOL_VERSION,
            features: 0,
        };
        write_frame(&mut client, &cbor_bytes(&hello)).await.unwrap();
        read_until_match(&mut client, |m| matches!(m, ServerMessage::HelloAck { .. })).await;

        let create = ClientMessage::LobbyMessage(LobbyMessage::CreateLobby {
            settings: default_lobby_settings(),
            password: None,
            player_name: Some("M2Test".to_string()),
        });
        write_frame(&mut client, &cbor_bytes(&create))
            .await
            .unwrap();
        let created = read_until_match(&mut client, |m| {
            matches!(m, ServerMessage::LobbyCreated { .. })
        })
        .await;
        let code = match created {
            ServerMessage::LobbyCreated { lobby_code } => lobby_code,
            other => panic!("expected LobbyCreated, got {other:?}"),
        };
        assert!(!code.is_empty());

        // Second creation from the same connection → clear Error, and no
        // second lobby appears on the server.
        write_frame(&mut client, &cbor_bytes(&create))
            .await
            .unwrap();
        let err = read_until_match(&mut client, |m| matches!(m, ServerMessage::Error { .. })).await;
        match err {
            ServerMessage::Error { message } => assert!(
                message.contains("Already in a lobby"),
                "unexpected error: {message}"
            ),
            other => panic!("expected Error, got {other:?}"),
        }
        let lobby_count = server.lock().await.lobby_manager.lock().await.lobbies.len();
        assert_eq!(lobby_count, 1, "double creation must not add a lobby");
        assert!(
            server
                .lock()
                .await
                .lobby_manager
                .lock()
                .await
                .get_lobby(&code)
                .is_some(),
            "the original lobby must survive the rejected double creation"
        );

        drop(client);
        handle.abort();
    }

    /// M2b: total lobbies are capped (`config.network.max_lobbies`).
    /// Lobbies 1..=cap are created; the (cap+1)th is rejected with a clear
    /// error and the existing lobbies are unaffected.
    #[tokio::test]
    async fn lobby_creation_over_cap_is_rejected() {
        let _exclusive = crate::server::protocol::TEST_FAULT_EXCLUSIVE.lock().await;
        const CAP: usize = 2;
        let server = make_server_with_lobby_cap(CAP).await;

        let hello = ClientMessage::Hello {
            protocol_version: PROTOCOL_VERSION,
            features: 0,
        };
        let create = ClientMessage::LobbyMessage(LobbyMessage::CreateLobby {
            settings: default_lobby_settings(),
            password: None,
            player_name: Some("CapTest".to_string()),
        });

        // CAP connections each get a lobby.
        let mut clients: Vec<(TcpStream, tokio::task::JoinHandle<()>)> = Vec::new();
        for _ in 0..CAP {
            let (client, handle) = spawn_tcp_server(server.clone()).await;
            clients.push((client, handle));
        }
        for (client, _) in clients.iter_mut() {
            write_frame(client, &cbor_bytes(&hello)).await.unwrap();
            read_until_match(client, |m| matches!(m, ServerMessage::HelloAck { .. })).await;
            write_frame(client, &cbor_bytes(&create)).await.unwrap();
            let created =
                read_until_match(client, |m| matches!(m, ServerMessage::LobbyCreated { .. })).await;
            assert!(matches!(created, ServerMessage::LobbyCreated { .. }));
        }

        // The (CAP+1)th creation is rejected with a clear error.
        let (mut over, over_handle) = spawn_tcp_server(server.clone()).await;
        write_frame(&mut over, &cbor_bytes(&hello)).await.unwrap();
        read_until_match(&mut over, |m| matches!(m, ServerMessage::HelloAck { .. })).await;
        write_frame(&mut over, &cbor_bytes(&create)).await.unwrap();
        let err = read_until_match(&mut over, |m| matches!(m, ServerMessage::Error { .. })).await;
        match err {
            ServerMessage::Error { message } => assert!(
                message.contains("lobby limit"),
                "unexpected error: {message}"
            ),
            other => panic!("expected Error, got {other:?}"),
        }
        let lobby_count = server.lock().await.lobby_manager.lock().await.lobbies.len();
        assert_eq!(lobby_count, CAP, "over-cap creation must not add a lobby");

        for (_, handle) in clients {
            handle.abort();
        }
        over_handle.abort();
    }

    /// M2d: when the CREATOR of an unstarted lobby disconnects, the lobby is
    /// deleted immediately (without this it would retain up to ~1 MB of
    /// client-controlled custom unit types until the idle-TTL sweep). A GUEST
    /// disconnect leaves the lobby in place — the creator is still in charge.
    #[tokio::test]
    async fn creator_disconnect_removes_unstarted_lobby() {
        let _exclusive = crate::server::protocol::TEST_FAULT_EXCLUSIVE.lock().await;
        let server = make_server().await;

        let hello = ClientMessage::Hello {
            protocol_version: PROTOCOL_VERSION,
            features: 0,
        };

        // Creator: create the lobby.
        let (mut creator, creator_handle) = spawn_tcp_server(server.clone()).await;
        write_frame(&mut creator, &cbor_bytes(&hello))
            .await
            .unwrap();
        read_until_match(&mut creator, |m| {
            matches!(m, ServerMessage::HelloAck { .. })
        })
        .await;
        let create = ClientMessage::LobbyMessage(LobbyMessage::CreateLobby {
            settings: default_lobby_settings(),
            password: None,
            player_name: Some("M2dCreator".to_string()),
        });
        write_frame(&mut creator, &cbor_bytes(&create))
            .await
            .unwrap();
        let code = match read_until_match(&mut creator, |m| {
            matches!(m, ServerMessage::LobbyCreated { .. })
        })
        .await
        {
            ServerMessage::LobbyCreated { lobby_code } => lobby_code,
            other => panic!("expected LobbyCreated, got {other:?}"),
        };

        // Guest joins the lobby.
        let (mut guest, guest_handle) = spawn_tcp_server(server.clone()).await;
        write_frame(&mut guest, &cbor_bytes(&hello)).await.unwrap();
        read_until_match(&mut guest, |m| matches!(m, ServerMessage::HelloAck { .. })).await;
        let join = ClientMessage::LobbyMessage(LobbyMessage::JoinLobby {
            lobby_code: code.clone(),
            password: None,
            player_name: Some("M2dGuest".to_string()),
        });
        write_frame(&mut guest, &cbor_bytes(&join)).await.unwrap();
        read_until_match(&mut guest, |m| {
            matches!(m, ServerMessage::LobbyJoined { .. })
                || matches!(m, ServerMessage::LobbyUpdate { .. })
        })
        .await;

        // Guest disconnects: the lobby must SURVIVE (the creator is in
        // charge). The handler task must run to completion so its cleanup
        // executes — do NOT abort it.
        drop(guest);
        tokio::time::timeout(Duration::from_secs(5), guest_handle)
            .await
            .expect("guest handler finished in time")
            .expect("guest handler completed without panic");
        assert!(
            server
                .lock()
                .await
                .lobby_manager
                .lock()
                .await
                .get_lobby(&code)
                .is_some(),
            "guest disconnect must not delete the lobby"
        );

        // Creator disconnects: the unstarted lobby is deleted.
        drop(creator);
        let mut removed = false;
        for _ in 0..100 {
            if server
                .lock()
                .await
                .lobby_manager
                .lock()
                .await
                .get_lobby(&code)
                .is_none()
            {
                removed = true;
                break;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        assert!(
            removed,
            "creator disconnect must delete the unstarted lobby"
        );
        let _ = tokio::time::timeout(Duration::from_secs(5), creator_handle).await;
    }
}
