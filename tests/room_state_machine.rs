//! Integration tests for the room state machine — driven entirely through
//! the PUBLIC wire surface against a real `GameServer` (raw-TCP game port
//! for lobby/game traffic, HTTP + WebSocket for the spectator path),
//! following the pattern of the in-crate `handlers/ws.rs` handler tests.
//!
//! Covers the room lifecycle paths that had no tests:
//! - `start_game_by_non_creator_is_rejected` — authorization + lobby survival.
//! - `llm_slot_flow_starts_bot_and_streams_reasons` — AddLlmPlayer /
//!   SetSlotLlmConfig / start: the LLM player enters `game.players()`,
//!   receives state broadcasts, and its tool calls surface as
//!   `LlmReasonLog` frames for human players.
//! - `abandoned_game_removes_room_and_rejects_late_spectator` — a finished
//!   game (all humans disconnected) leaves `server.games`; a late
//!   `AddSpectator` must get a clean "No active game" Error, never attach
//!   to a dead room.

use std::sync::Arc;
use std::time::Duration;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use war_of_dots::config::Config;
use war_of_dots::server::framing::{read_frame, write_frame};
use war_of_dots::server::game_server::GameServer;
use war_of_dots::server::protocol::{
    ClientMessage, LobbyMessage, LobbySettings, ServerMessage, PROTOCOL_VERSION,
};

// ────────────────────────────────────────────────────────────────────────
// Harness: server boot + wire clients
// ────────────────────────────────────────────────────────────────────────

/// Find a free TCP port such that `port` and `port + 1` are both bindable
/// (the server binds the HTTP port and the raw-TCP game port at +1).
async fn free_port_pair() -> (u16, u16) {
    for _ in 0..32 {
        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind probe listener");
        let port = listener.local_addr().expect("local_addr").port();
        drop(listener);
        if TcpListener::bind(("127.0.0.1", port + 1)).await.is_ok() {
            return (port, port + 1);
        }
    }
    panic!("could not find a free (port, port+1) pair");
}

/// Boot a real `GameServer` on a free port pair with temp-dir storage.
/// `llm_provider_url`: when set, registers an `ollama` provider (name
/// `mock-ollama`) pointing at the mock LLM server. The server task runs
/// until the test process exits (its accept loop blocks on ctrl_c, which
/// the runtime drop cancels).
pub async fn boot_server(llm_provider_url: Option<&str>) -> (u16, u16, std::path::PathBuf) {
    let (port, tcp_port) = free_port_pair().await;
    let dir =
        std::env::temp_dir().join(format!("wods-roomsm-{}", uuid::Uuid::new_v4().as_simple()));
    std::fs::create_dir_all(&dir).expect("create temp storage dir");

    let llm_section = llm_provider_url.map_or_else(
        || "[llm]\n".to_string(),
        |url| {
            format!(
                r#"[llm]
[[llm.providers]]
name = "mock-ollama"
provider = "ollama"
model = "test-model"
base_url = "{url}"
"#
            )
        },
    );
    let toml_str = format!(
        r#"{llm_section}
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
    let mut server = GameServer::new(cfg, port)
        .await
        .expect("server boots (temp storage)");
    server.set_bind("127.0.0.1");
    tokio::spawn(async move {
        server.run().await.ok();
    });
    // Wait until both listeners are accepting (the run task binds them
    // asynchronously after the spawn).
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    loop {
        let tcp_ok = TcpStream::connect(("127.0.0.1", tcp_port)).await.is_ok();
        let http_ok = TcpStream::connect(("127.0.0.1", port)).await.is_ok();
        if tcp_ok && http_ok {
            break;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "server listeners did not come up in time"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    (port, tcp_port, dir)
}

/// Lobby settings the wire client sends (the standard-ruleset baseline —
/// the same minimal TOML the console client uses; serde defaults fill in
/// the rest, matching what a UI user gets).
pub fn default_lobby_settings() -> LobbySettings {
    toml::from_str(
        r#"
        map = "default"
        use_standard_ruleset = true
        llm_thinking_delay = 0.0
        "#,
    )
    .expect("default lobby settings parse")
}

/// One raw-TCP game client: Hello handshake done, ready for lobby/game
/// frames (the exact transport `handle_tcp_client` serves on port + 1).
/// `pub` (with `pub` connect/send/recv) so `tests/debug_probe.rs` can
/// reuse this harness via `#[path = "room_state_machine.rs"] mod roomsm;`.
pub struct TcpClient {
    stream: TcpStream,
}

impl TcpClient {
    pub async fn connect(tcp_port: u16) -> Self {
        let stream = TcpStream::connect(("127.0.0.1", tcp_port))
            .await
            .expect("connect to game port");
        let mut client = TcpClient { stream };
        client.hello().await;
        client
    }

    /// WP3: the Hello handshake — Hello must be the first frame, and the
    /// client waits for HelloAck before sending anything else.
    async fn hello(&mut self) {
        self.send(&ClientMessage::Hello {
            protocol_version: PROTOCOL_VERSION,
            features: 0,
        })
        .await;
        match self.recv().await {
            ServerMessage::HelloAck {
                protocol_version, ..
            } => assert_eq!(protocol_version, PROTOCOL_VERSION, "hello ack version"),
            other => panic!("expected HelloAck first, got {other:?}"),
        }
    }

    pub async fn send(&mut self, msg: &ClientMessage) {
        let mut bytes = Vec::new();
        ciborium::into_writer(msg, &mut bytes).expect("encode ClientMessage");
        write_frame(&mut self.stream, &bytes)
            .await
            .expect("write frame");
    }

    /// Read one frame and decode it (no timeout — callers wrap in
    /// `tokio::time::timeout` when a deadline matters).
    pub async fn recv(&mut self) -> ServerMessage {
        let frame = read_frame(&mut self.stream).await.expect("read frame");
        ciborium::from_reader::<ServerMessage, _>(&frame[..]).expect("decode ServerMessage")
    }

    /// Read frames until `match` returns `Some`, skipping noise
    /// (`DeltaState`, lobby chatter). Fails the test on deadline.
    async fn recv_until<T, F>(&mut self, what: &str, deadline_s: u64, mut matcher: F) -> T
    where
        F: FnMut(&ServerMessage) -> Option<T>,
    {
        let deadline = tokio::time::Instant::now() + Duration::from_secs(deadline_s);
        loop {
            let frame = tokio::time::timeout_at(deadline, self.recv())
                .await
                .unwrap_or_else(|_| panic!("timed out waiting for {what}"));
            if let Some(found) = matcher(&frame) {
                return found;
            }
        }
    }
}

/// `GET /debug/stats` → `active_games` count (the route is registered in
/// debug builds; `debug.endpoints` defaults to true there).
async fn active_game_count(http_port: u16) -> usize {
    let url = format!("http://127.0.0.1:{http_port}/debug/stats");
    let body: serde_json::Value = reqwest::get(&url)
        .await
        .expect("GET /debug/stats")
        .json()
        .await
        .expect("debug stats JSON");
    body["active_games"].as_u64().expect("active_games field") as usize
}

// ────────────────────────────────────────────────────────────────────────
// WS client (spectator path) — minimal binary-frame reader over raw TCP,
// mirroring the hand-rolled handshake in the in-crate ws.rs tests.
// ────────────────────────────────────────────────────────────────────────

/// Open a WebSocket to `path` and return the stream positioned after the
/// 101 upgrade headers.
async fn ws_connect(http_port: u16, path: &str) -> TcpStream {
    let mut stream = TcpStream::connect(("127.0.0.1", http_port))
        .await
        .expect("connect to http port");
    stream
        .write_all(
            format!(
                "GET {path} HTTP/1.1\r\n\
                 Host: localhost\r\n\
                 Upgrade: websocket\r\n\
                 Connection: Upgrade\r\n\
                 Sec-WebSocket-Key: dGhlIHNhbXBsZSBub25jZQ==\r\n\
                 Sec-WebSocket-Version: 13\r\n\
                 \r\n"
            )
            .as_bytes(),
        )
        .await
        .expect("send upgrade request");
    // Read the 101 response headers, byte by byte (the byte-wise read
    // consumes exactly through \r\n\r\n, so any data frame bytes that
    // arrived in the same packet remain in the stream buffer for the
    // frame reader).
    let mut buf = Vec::new();
    let mut byte = [0u8; 1];
    while !buf.ends_with(b"\r\n\r\n") {
        let n = stream.read(&mut byte).await.expect("read upgrade response");
        assert!(n == 1, "upgrade response read closed");
        buf.extend_from_slice(&byte);
        assert!(buf.len() < 16_384, "upgrade response too large");
    }
    assert!(
        buf.starts_with(b"HTTP/1.1 101"),
        "expected 101 switching protocols, got: {}",
        String::from_utf8_lossy(&buf)
    );
    stream
}

/// Parse one unmasked server→client WS frame from `data`; `None` when the
/// data does not yet contain a complete frame. Returns (opcode, payload).
fn parse_ws_frame(data: &[u8]) -> Option<(u8, Vec<u8>)> {
    if data.len() < 2 {
        return None;
    }
    let opcode = data[0] & 0x0f;
    let len = (data[1] & 0x7f) as usize;
    let mut off = 2;
    let len = if len == 126 {
        if data.len() < 4 {
            return None;
        }
        off = 4;
        u16::from_be_bytes([data[2], data[3]]) as usize
    } else if len == 127 {
        if data.len() < 10 {
            return None;
        }
        off = 10;
        u64::from_be_bytes(data[2..10].try_into().unwrap()) as usize
    } else {
        len
    };
    if data.len() < off + len {
        return None;
    }
    Some((opcode, data[off..off + len].to_vec()))
}

/// Connect a spectator and read the first binary ServerMessage the server
/// sends (the handler's first act on both success and failure paths).
async fn spectator_first_message(http_port: u16) -> ServerMessage {
    let mut stream = ws_connect(http_port, "/ws/spectator").await;
    let mut buf = Vec::new();
    let mut tmp = [0u8; 4096];
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    loop {
        if let Some((opcode, payload)) = parse_ws_frame(&buf) {
            if opcode == 0x2 {
                return ciborium::from_reader::<ServerMessage, _>(&payload[..])
                    .expect("decode spectator ServerMessage");
            }
            if opcode == 0x8 {
                panic!("server closed the WS before sending a message");
            }
            // ping/pong: skip the frame (unmasked server→client frames)
            buf.drain(..2 + payload.len());
            continue;
        }
        let n = tokio::time::timeout_at(deadline, stream.read(&mut tmp))
            .await
            .expect("spectator read deadline")
            .expect("spectator read");
        if n == 0 {
            panic!("spectator connection closed before a message");
        }
        buf.extend_from_slice(&tmp[..n]);
    }
}

// ────────────────────────────────────────────────────────────────────────
// 1a: StartGame authorization
// ────────────────────────────────────────────────────────────────────────

/// A non-creator `StartGame` is rejected with a clear Error and leaves the
/// lobby untouched: the creator can still start it afterwards, and the
/// lobby is consumed by the successful start (a further StartGame — from
/// any seat — is rejected again and creates no second room).
#[tokio::test]
async fn start_game_by_non_creator_is_rejected() {
    let (http_port, tcp_port, _dir) = boot_server(None).await;

    let mut creator = TcpClient::connect(tcp_port).await;
    creator
        .send(&ClientMessage::LobbyMessage(LobbyMessage::CreateLobby {
            settings: default_lobby_settings(),
            password: None,
            player_name: Some("Creator".into()),
        }))
        .await;
    let lobby_code = creator
        .recv_until("LobbyCreated", 10, |m| match m {
            ServerMessage::LobbyCreated { lobby_code } => Some(lobby_code.clone()),
            _ => None,
        })
        .await;

    // The member joins the (creator's) lobby — join is open, start is not.
    // The creator stays connected: a creator disconnect removes the lobby.
    let mut member = TcpClient::connect(tcp_port).await;
    member
        .send(&ClientMessage::LobbyMessage(LobbyMessage::JoinLobby {
            lobby_code: lobby_code.clone(),
            password: None,
            player_name: Some("Member".into()),
        }))
        .await;
    member
        .recv_until("LobbyJoined", 10, |m| match m {
            ServerMessage::LobbyJoined { .. } => Some(()),
            ServerMessage::Error { message } => panic!("join failed: {message}"),
            _ => None,
        })
        .await;

    // The member tries to start: rejected with the exact authorization
    // Error — no room, no lobby consumption.
    member
        .send(&ClientMessage::LobbyMessage(LobbyMessage::StartGame))
        .await;
    let err = member
        .recv_until("StartGame rejection", 10, |m| match m {
            ServerMessage::Error { message } => Some(message.clone()),
            _ => None,
        })
        .await;
    assert_eq!(
        err, "Only the lobby creator can start the game",
        "non-creator StartGame must be rejected with the authorization error"
    );

    // The lobby survived the rejected start: the creator (still seated)
    // starts it for real and receives the game's FullState.
    creator
        .send(&ClientMessage::LobbyMessage(LobbyMessage::StartGame))
        .await;
    creator
        .recv_until("FullState", 15, |m| match m {
            ServerMessage::FullState(_) => Some(()),
            ServerMessage::Error { message } => panic!("creator start failed: {message}"),
            _ => None,
        })
        .await;

    // The lobby was consumed by the successful start: a further StartGame
    // is rejected (the is-creator gate reads the gone lobby as
    // "not the creator") and — crucially — creates no second room.
    member
        .send(&ClientMessage::LobbyMessage(LobbyMessage::StartGame))
        .await;
    let err = member
        .recv_until("second StartGame rejection", 10, |m| match m {
            ServerMessage::Error { message } => Some(message.clone()),
            _ => None,
        })
        .await;
    assert_eq!(
        err, "Only the lobby creator can start the game",
        "a StartGame after the lobby was consumed must be rejected"
    );
    assert_eq!(
        active_game_count(http_port).await,
        1,
        "the rejected start must not create a second room"
    );

    // Cleanup: drop the only human connection so the room abandons.
    drop(member);
    drop(creator);
}

// ────────────────────────────────────────────────────────────────────────
// 1b: LLM slot flow (AddLlmPlayer + SetSlotLlmConfig + start)
// ──────────────────────────────────────────

/// Mock LLM server: records every `/api/chat` request body and answers
/// with a fixed `recruit_unit` tool call (with a reason).
struct MockLlm {
    url: String,
    requests: Arc<std::sync::Mutex<Vec<serde_json::Value>>>,
}

async fn start_mock_llm() -> MockLlm {
    let requests = Arc::new(std::sync::Mutex::new(Vec::<serde_json::Value>::new()));
    let seen = Arc::clone(&requests);
    let app = axum::Router::new().route(
        "/api/chat",
        axum::routing::post(move |body: axum::Json<serde_json::Value>| {
            let seen = Arc::clone(&seen);
            async move {
                seen.lock().unwrap().push(body.0);
                axum::Json(serde_json::json!({
                    "message": {
                        "content": "",
                        "tool_calls": [{
                            "id": "tc_1",
                            "type": "function",
                            "function": {
                                "name": "recruit_unit",
                                "arguments": {
                                    "city_id": "city_1",
                                    "unit_type": "infantry",
                                    "reason": "build up strength"
                                }
                            }
                        }]
                    }
                }))
            }
        }),
    );
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind mock llm");
    let port = listener.local_addr().expect("local_addr").port();
    tokio::spawn(async move {
        axum::serve(listener, app).await.ok();
    });
    tokio::time::sleep(Duration::from_millis(100)).await;
    MockLlm {
        url: format!("http://127.0.0.1:{port}"),
        requests,
    }
}

/// The LLM slot flow end-to-end: an LLM player added in the lobby enters
/// `game.players()` on start, receives per-player state broadcasts from
/// the tick loop, its tool calls are mapped to room commands, and the
/// reason log streams to human players as `LlmReasonLog`.
#[tokio::test]
async fn llm_slot_flow_starts_bot_and_streams_reasons() {
    let mock = start_mock_llm().await;
    let (_http_port, tcp_port, _dir) = boot_server(Some(&mock.url)).await;

    let mut creator = TcpClient::connect(tcp_port).await;

    // Create the lobby, then add an LLM player (provider type "ollama" —
    // the string the client chooses is the PROVIDER TYPE, resolved
    // server-side against configured providers by name).
    creator
        .send(&ClientMessage::LobbyMessage(LobbyMessage::CreateLobby {
            settings: default_lobby_settings(),
            password: None,
            player_name: Some("Creator".into()),
        }))
        .await;
    let _lobby_code = creator
        .recv_until("LobbyCreated", 10, |m| match m {
            ServerMessage::LobbyCreated { lobby_code } => Some(lobby_code.clone()),
            _ => None,
        })
        .await;

    creator
        .send(&ClientMessage::LobbyMessage(LobbyMessage::AddLlmPlayer {
            name: "Bot".into(),
            llm_provider: "ollama".into(),
        }))
        .await;
    // The lobby update must show the LLM slot: a second, non-human player.
    // (Lobby-era ids are `player_{uuid8}`; the in-game slot id "player_1"
    // is minted at start and asserted later on the state broadcast.)
    let players = creator
        .recv_until("LobbyUpdate with LLM slot", 10, |m| match m {
            ServerMessage::LobbyUpdate { players, .. } => {
                players.iter().any(|p| !p.is_human).then(|| players.clone())
            }
            _ => None,
        })
        .await;
    assert_eq!(players.len(), 2, "creator + one LLM slot");
    let bot = players
        .iter()
        .find(|p| !p.is_human)
        .expect("the LLM player must be in the lobby");
    let human = players
        .iter()
        .find(|p| p.is_human)
        .expect("the creator must be in the lobby");
    assert_eq!(bot.name, "Bot");
    assert_ne!(bot.id, human.id, "the LLM slot is a distinct player");
    assert_eq!(
        bot.llm_provider.as_deref(),
        Some("ollama"),
        "LLM slot keeps its provider type"
    );

    // Set the slot's model — provider/key/base_url resolve server-side
    // from the configured provider (no secrets cross the wire).
    creator
        .send(&ClientMessage::LobbyMessage(
            LobbyMessage::SetSlotLlmConfig {
                slot: 1,
                model: "test-model".into(),
            },
        ))
        .await;

    // Start the game.
    creator
        .send(&ClientMessage::LobbyMessage(LobbyMessage::StartGame))
        .await;
    creator
        .recv_until("FullState", 15, |m| match m {
            ServerMessage::FullState(_) => Some(()),
            ServerMessage::Error { message } => panic!("start failed: {message}"),
            _ => None,
        })
        .await;

    // The tick loop broadcasts a per-player state snapshot every 60 ticks
    // (at the standard 60 ticks/s that is ~1 s); the agent steps on each
    // snapshot and calls the mock provider.
    let deadline = tokio::time::Instant::now() + Duration::from_secs(20);
    let req = loop {
        if let Some(r) = mock.requests.lock().unwrap().first() {
            break r.clone();
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "the LLM player must receive a state broadcast and call the provider"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    };

    // The request must be the agent's: system prompt first, the LLM
    // player's own state second (player_id "player_1" proves the bot is in
    // game.players() with a subscribed state channel), and the game tools
    // attached.
    let messages = req["messages"].as_array().expect("messages array");
    assert_eq!(messages[0]["role"], "system");
    let state_json = messages[1]["content"].as_str().expect("state JSON string");
    let state: serde_json::Value =
        serde_json::from_str(state_json).expect("user message is the state JSON");
    assert_eq!(
        state["player_id"], "player_1",
        "the LLM player's own state must be broadcast to its agent"
    );
    assert!(!req["tools"].as_array().expect("tools array").is_empty());

    // The bot's tool call (with reason) must surface to the human player
    // as an LlmReasonLog frame — the full agent → command-mapping →
    // reason-broadcast path.
    let reason_log = creator
        .recv_until("LlmReasonLog", 15, |m| match m {
            ServerMessage::LlmReasonLog {
                player_name,
                action,
                reason,
            } => Some((player_name.clone(), action.clone(), reason.clone())),
            _ => None,
        })
        .await;
    let (name, action, reason) = reason_log;
    assert_eq!(name, "Bot");
    assert!(
        action.contains("recruit_unit"),
        "reason log names the tool call, got: {action}"
    );
    assert_eq!(reason, "build up strength");

    drop(creator);
}

// ────────────────────────────────────────────────────────────────────────
// 1c: finished game → room removed → late spectator rejected
// ────────────────────────────────────────────────────────────────────────

/// When the last human disconnects, the tick loop ends the game
/// ("Abandoned") and the room leaves `server.games`. A spectator
/// connecting afterwards must get a clean `Error("No active game")` —
/// a late `AddSpectator` must never attach to a dead room.
#[tokio::test]
async fn abandoned_game_removes_room_and_rejects_late_spectator() {
    let (http_port, tcp_port, _dir) = boot_server(None).await;

    let mut creator = TcpClient::connect(tcp_port).await;
    creator
        .send(&ClientMessage::LobbyMessage(LobbyMessage::CreateLobby {
            settings: default_lobby_settings(),
            password: None,
            player_name: Some("Creator".into()),
        }))
        .await;
    let _code = creator
        .recv_until("LobbyCreated", 10, |m| match m {
            ServerMessage::LobbyCreated { lobby_code } => Some(lobby_code.clone()),
            _ => None,
        })
        .await;
    creator
        .send(&ClientMessage::LobbyMessage(LobbyMessage::StartGame))
        .await;
    creator
        .recv_until("FullState", 15, |m| match m {
            ServerMessage::FullState(_) => Some(()),
            ServerMessage::Error { message } => panic!("start failed: {message}"),
            _ => None,
        })
        .await;

    // The room is live.
    assert_eq!(
        active_game_count(http_port).await,
        1,
        "the started game must be in server.games"
    );

    // The only human disconnects: the tick loop detects the dead egress
    // channel within a few ticks and ends the game as "Abandoned".
    drop(creator);

    // Wait for the room to leave server.games.
    let deadline = tokio::time::Instant::now() + Duration::from_secs(15);
    loop {
        if active_game_count(http_port).await == 0 {
            break;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "the abandoned game must be removed from server.games"
        );
        tokio::time::sleep(Duration::from_millis(100)).await;
    }

    // A late spectator must be rejected cleanly — not attached, not
    // served a stale FullState.
    match spectator_first_message(http_port).await {
        ServerMessage::Error { message } => assert_eq!(message, "No active game"),
        other => panic!(
            "a late spectator must get a clean Error, not {other:?} \
             (attaching to a dead room would leak a connection)"
        ),
    }
}
