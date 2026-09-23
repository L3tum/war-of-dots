//! Server: HTTP + raw TCP for game state
//!
//! This file holds the `GameServer` struct + impl (router assembly,
//! listener wiring, idle-lobby sweep) and the router-level hardening tests.
//! The room machinery lives in `room/` and the request handlers in
//! `handlers/`.

use std::collections::HashMap;
use std::sync::atomic::AtomicUsize;
use std::sync::Arc;
use tokio::sync::Mutex;
use uuid::Uuid;

use axum::routing::get;

use crate::config::Config;
use crate::map::map_registry;
use crate::server::handlers::http::{
    index_handler, list_maps_handler, serve_static, watch_handler,
};
use crate::server::handlers::replay::{gated_replay_ws_handler, list_replays, serve_replay};
use crate::server::handlers::spectator::spectator_ws_handler;
use crate::server::handlers::tcp::handle_tcp_client;
use crate::server::handlers::ws::ws_handler;
use crate::server::lobby::{LobbyManager, LOBBY_IDLE_TTL};

// Re-export types from room module so existing `crate::server::game_server::X`
// paths keep working (canonical home: `crate::server::room`).
pub use crate::server::room::{
    egress_bytes, egress_vec, Command, DebugGameStats, DeltaRing, EgressMessage, GameRoom,
    LlmPlayerConfig, PlayerConnection, PlayerSender, TcpEgress,
};

#[cfg(test)]
use crate::game::economy::UnitType;
#[cfg(test)]
use crate::game::game::GameSettings;
#[cfg(test)]
use crate::game::unit::Equipment;
#[cfg(test)]
use crate::map::map_config::load_default_map;
#[cfg(test)]
use crate::server::protocol::{ClientMessage, ServerMessage, PROTOCOL_VERSION};
#[cfg(test)]
use std::sync::atomic::Ordering;
#[cfg(test)]
use tokio::time::Duration;

pub struct GameServer {
    pub(crate) port: u16,
    /// Interface to bind listeners on. Defaults to 0.0.0.0 (all interfaces);
    /// the CLI --bind flag overrides it (e.g. 127.0.0.1 for the E2E harness so
    /// the test server is never LAN-reachable).
    pub(crate) bind: String,
    pub(crate) config: Config,
    pub(crate) lobby_manager: Mutex<LobbyManager>,
    pub(crate) games: Mutex<HashMap<String, Arc<Mutex<GameRoom>>>>,
    /// Maps a connection UUID to (player_id, game_room_id)
    pub(crate) player_map: Mutex<HashMap<Uuid, (String, String)>>,
    /// WP3: negotiated feature bits per connection (Hello ∩ server support).
    /// Inserted at handshake, removed on disconnect. Future work packages
    /// (grid RLE, compact entities, AoI) read this to pick the wire format.
    pub(crate) negotiated_features: Mutex<HashMap<Uuid, u32>>,
    /// Sec L2: live WebSocket connection count (`/ws` + `/ws/spectator`).
    /// Reserved at accept (see `handle_ws_game` / `handle_spectator_ws`),
    /// released when the connection ends. Enforces
    /// `config.network.max_ws_connections`.
    pub(crate) ws_connections: AtomicUsize,
    /// Sec L1: request-time gate for the /debug/* HTTP endpoints, set from
    /// `config.debug.endpoints` in `new()` (see `debug_endpoints_enabled`).
    pub(crate) debug_endpoints: bool,
    /// Shared storage for statistics and leaderboards
    pub(crate) storage: crate::storage::Storage,
}

#[allow(dead_code)]
impl GameServer {
    pub async fn new(config: Config, port: u16) -> anyhow::Result<Self> {
        let storage = crate::storage::Storage::new(
            &config.storage.database_path,
            &config.storage.replay_path,
        )?;
        map_registry::init();
        let debug_endpoints = config.debug.endpoints;
        Ok(GameServer {
            port,
            bind: "0.0.0.0".to_string(),
            config,
            lobby_manager: Mutex::new(LobbyManager::new()),
            games: Mutex::new(HashMap::new()),
            player_map: Mutex::new(HashMap::new()),
            negotiated_features: Mutex::new(HashMap::new()),
            ws_connections: AtomicUsize::new(0),
            debug_endpoints,
            storage,
        })
    }

    /// Override the listener interface (CLI `--bind`). Default is 0.0.0.0
    /// (all interfaces); the E2E harness uses 127.0.0.1 so the test server
    /// is never LAN-reachable.
    pub fn set_bind(&mut self, bind: impl Into<String>) {
        self.bind = bind.into();
    }

    /// Sec L1: request-time gate for the /debug/* HTTP endpoints — the
    /// single source of truth the debug handlers re-check at request time.
    /// Both halves must hold:
    /// - the same cfg condition `build_router` uses to register the routes
    ///   (debug builds / `debug-overlay` feature), and
    /// - the `debug.endpoints` config flag (runtime; lets an operator
    ///   disable the endpoints in a debug build, and lets a test prove the
    ///   gate holds even when the routes are registered).
    ///
    /// An exposed route (misconfigured proxy, feature-flag slip, a test app
    /// registering the route unconditionally) must 404 here — in particular
    /// the CPU-heavy benchmark must never run in a non-debug deployment.
    pub(crate) fn debug_endpoints_enabled(&self) -> bool {
        cfg!(any(debug_assertions, feature = "debug-overlay")) && self.debug_endpoints
    }

    /// Assemble the production HTTP route table (SPA pages + game WS +
    /// replay endpoints). A method so tests can serve the EXACT same routes
    /// on an ephemeral port (see the `replay_*` tests) instead of a copy
    /// that could drift from production.
    pub(crate) fn build_router(state: Arc<Mutex<GameServer>>) -> axum::Router {
        // `mut` is only needed in debug_assertions / debug-overlay builds (the
        // cfg'd block below reassigns `router`); plain release never mutates it.
        #[allow(unused_mut)]
        let mut router = axum::Router::new()
            .route("/", get(index_handler))
            .route("/watch", get(watch_handler))
            .route("/ws", get(ws_handler))
            // SPA route for the replay list viewer page (the same
            // index.html; main.ts routes on `location.pathname` —
            // without this, /replay fell through to serve_static and
            // 404'd on the real server). Both slash variants: the list page
            // links `/replay?game_id=…`, but `/replay/` must not 404 either.
            .route("/replay", get(index_handler))
            .route("/replay/", get(index_handler))
            .route("/ws/replay", get(gated_replay_ws_handler))
            .route("/ws/spectator", get(spectator_ws_handler))
            .route("/replays", get(list_replays))
            .route("/replays/:name", get(serve_replay))
            .route("/maps", get(list_maps_handler))
            .fallback(serve_static);

        #[cfg(any(debug_assertions, feature = "debug-overlay"))]
        {
            // Route-level gate (compile time). The handlers re-check
            // `debug_endpoints_enabled` at request time — a route that is
            // ever exposed must not execute debug logic (Sec L1).
            router = router.route("/debug/stats", get(crate::debug::debug_stats_handler));
            // The benchmark route (and handler) only exist when the
            // benchmark module is compiled in.
            #[cfg(feature = "microbenchmarks")]
            {
                router = router.route(
                    "/debug/benchmark",
                    get(crate::debug::debug_benchmark_handler),
                );
            }
        }

        let cors = tower_http::cors::CorsLayer::new()
            .allow_origin(tower_http::cors::Any)
            .allow_methods(tower_http::cors::Any)
            .allow_headers(tower_http::cors::Any);

        router.layer(cors).with_state(state)
    }

    pub async fn run(self) -> anyhow::Result<()> {
        let port = self.port;
        let bind = self.bind.clone();
        let state = Arc::new(Mutex::new(self));
        let state_ws = state.clone();

        let app = Self::build_router(state.clone());

        let listener_http = tokio::net::TcpListener::bind(format!("{}:{}", bind, port)).await?;
        // M5: a failed bind here used to surface as a bare io::Error with no
        // context. Make it loud and consistent with the HTTP bind above (both
        // abort startup): a server whose raw-TCP game port is dead would run
        // with half its client paths missing, which is a misconfiguration, not
        // a degraded mode worth continuing in.
        let listener_ws =
            match tokio::net::TcpListener::bind(format!("{}:{}", bind, port + 1)).await {
                Ok(listener) => listener,
                Err(e) => {
                    tracing::error!(
                        http_port = port,
                        tcp_game_port = port + 1,
                        error = %e,
                        "Failed to bind the TCP game listener on {}:{} (config port is {}; the \
                         TCP game port is always config port + 1). Consequence if we continued: \
                         raw-TCP clients could not join, while WebSocket clients on port {} would \
                         still work. Aborting — free port {} or start with a different `port`.",
                        bind,
                        port + 1,
                        port,
                        port,
                        port + 1
                    );
                    return Err(anyhow::anyhow!(
                        "bind TCP game listener on {}:{} (config port {} + 1): {}",
                        bind,
                        port + 1,
                        port,
                        e
                    ));
                }
            };

        tokio::spawn(async move {
            axum::serve(listener_http, app).await.ok();
        });

        // M2: periodic sweep of idle lobbies (LOBBY_IDLE_TTL = 30 min). A
        // lobby with no join activity for that long — including lobbies
        // whose creator vanished without a clean disconnect (crash, network
        // cut) — is removed, bounding the memory footprint of
        // client-controlled custom unit types (up to ~1 MB per lobby). The
        // creator-disconnect cleanup in the read loops is the fast path;
        // this sweep is the backstop.
        {
            let sweep_state = state.clone();
            tokio::spawn(async move {
                let mut interval = tokio::time::interval(LOBBY_IDLE_TTL / 2);
                interval.tick().await; // the first tick completes immediately
                loop {
                    interval.tick().await;
                    let server = sweep_state.lock().await;
                    let mut lobby_manager = server.lobby_manager.lock().await;
                    let reaped = lobby_manager.reap_idle_lobbies(LOBBY_IDLE_TTL);
                    if !reaped.is_empty() {
                        tracing::info!(
                            reaped = ?reaped,
                            ttl_secs = LOBBY_IDLE_TTL.as_secs(),
                            "Reaped idle lobbies (no join activity within TTL)"
                        );
                    }
                }
            });
        }

        tracing::info!(
            "HTTP server on {}:{}, TCP game on {}:{}",
            bind,
            port,
            bind,
            port + 1
        );

        // D-3: Graceful shutdown on Ctrl+C
        let shutdown_signal = tokio::signal::ctrl_c();
        tokio::pin!(shutdown_signal);

        loop {
            tokio::select! {
                _ = &mut shutdown_signal => {
                    tracing::info!("Received shutdown signal, stopping server...");
                    break;
                }
                result = listener_ws.accept() => {
                    match result {
                        Ok((stream, _)) => {
                            let server = state_ws.clone();
                            tokio::spawn(handle_tcp_client(stream, server));
                        }
                        Err(e) => {
                            tracing::error!("Accept error: {}", e);
                            break;
                        }
                    }
                }
            }
        }
        tracing::info!("Server stopped");
        Ok(())
    }
}

// ────────────────────────────────────────────────────────────────────────
// Sec L1/L2 hardening: debug endpoint gate + connection/spectator caps.
// ────────────────────────────────────────────────────────────────────────
#[cfg(test)]
mod security_hardening_tests {
    use super::*;
    use crate::config::{CityUpgradeSettings, Config};
    use crate::game::game::InfluenceSettings;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::{TcpListener, TcpStream};

    /// A real GameServer with throwaway storage (unique temp dir per test so
    /// parallel tests never share a sqlite file). The optional `network` / `debug`
    /// sections are the knobs under test.
    async fn make_server_with(
        max_ws_connections: Option<usize>,
        max_spectators_per_game: Option<usize>,
        debug_endpoints: Option<bool>,
    ) -> Arc<Mutex<GameServer>> {
        let dir = std::env::temp_dir().join(format!("wod-sec-{}", Uuid::new_v4()));
        let mut toml_str = format!(
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
        if max_ws_connections.is_some() || max_spectators_per_game.is_some() {
            toml_str.push_str("\n[network]");
            if let Some(v) = max_ws_connections {
                toml_str.push_str(&format!("\nmax_ws_connections = {v}"));
            }
            if let Some(v) = max_spectators_per_game {
                toml_str.push_str(&format!("\nmax_spectators_per_game = {v}"));
            }
        }
        if let Some(v) = debug_endpoints {
            toml_str.push_str(&format!("\n[debug]\nendpoints = {v}"));
        }
        let cfg: Config = toml::from_str::<Config>(&toml_str).unwrap().resolve();
        Arc::new(Mutex::new(GameServer::new(cfg, 0).await.unwrap()))
    }

    /// Serve the PRODUCTION router (build_router) on an ephemeral port — the
    /// same routes production runs, so in debug builds /debug/* are registered
    /// and the in-handler gate is what keeps them inert.
    async fn serve_router(state: Arc<Mutex<GameServer>>) -> String {
        let app = GameServer::build_router(state);
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            axum::serve(listener, app).await.ok();
        });
        tokio::time::sleep(Duration::from_millis(50)).await;
        format!("http://127.0.0.1:{}", addr.port())
    }

    // ── FIX 1: /debug/* request-time gate ──────────────────────────────

    /// Sec L1: in this (debug) build `build_router` registers /debug/stats
    /// (and /debug/benchmark too, with microbenchmarks) — the routes EXIST.
    /// With `debug.endpoints = false` the in-handler gate must 404 them:
    /// a registered route must not execute debug logic (the benchmark is an
    /// unauthenticated CPU-DoS lever).
    #[tokio::test]
    async fn debug_benchmark_404_when_config_gate_closed_despite_registered_route() {
        let state = make_server_with(None, None, Some(false)).await;
        let base = serve_router(state).await;
        let client = reqwest::Client::new();
        let paths: &[&str] = if cfg!(feature = "microbenchmarks") {
            &["/debug/stats", "/debug/benchmark"]
        } else {
            &["/debug/stats"]
        };
        for path in paths {
            let resp = client
                .get(format!("{base}{path}"))
                .send()
                .await
                .expect("request");
            assert_eq!(
                resp.status(),
                reqwest::StatusCode::NOT_FOUND,
                "{path} must 404 with the config gate closed, even though the route is registered"
            );
        }
    }

    /// Sec L1: with `debug.endpoints = true` (and a debug build) the same
    /// routes must work — the gate opens. (Only compiled with microbenchmarks,
    /// when the /debug/benchmark route exists at all.)
    #[cfg(feature = "microbenchmarks")]
    #[tokio::test]
    async fn debug_benchmark_runs_when_config_gate_open() {
        let state = make_server_with(None, None, Some(true)).await;
        let base = serve_router(state).await;
        let client = reqwest::Client::new();
        let resp = client
            .get(format!("{base}/debug/benchmark?units_per_city=1&ticks=10"))
            .send()
            .await
            .expect("request");
        assert_eq!(
            resp.status(),
            reqwest::StatusCode::OK,
            "benchmark must run when debug is enabled"
        );
        let body: serde_json::Value = resp.json().await.expect("json body");
        assert!(body.is_object(), "benchmark summary must be a JSON object");

        let resp = client
            .get(format!("{base}/debug/stats"))
            .send()
            .await
            .expect("request");
        assert_eq!(
            resp.status(),
            reqwest::StatusCode::OK,
            "/debug/stats must work"
        );
    }

    // ── FIX 2: WS connection cap ───────────────────────────────────────

    /// Raw WS client: HTTP upgrade handshake, returning the stream plus any
    /// bytes already buffered past the 101 response (same pattern as the WP3
    /// handshake tests).
    async fn ws_connect(addr: &str, path: &str) -> (TcpStream, Vec<u8>) {
        let mut client = TcpStream::connect(addr).await.unwrap();
        client
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
            .unwrap();
        let mut raw = Vec::new();
        let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
        loop {
            if let Some(pos) = raw.windows(4).position(|w| w == b"\r\n\r\n") {
                raw.drain(..=pos + 3);
                return (client, raw);
            }
            assert!(
                tokio::time::Instant::now() < deadline,
                "101 upgrade response in time"
            );
            let mut buf = [0u8; 4096];
            let n = client.read(&mut buf).await.expect("socket open");
            if n == 0 {
                panic!("no 101 upgrade response — server closed the connection");
            }
            raw.extend_from_slice(&buf[..n]);
        }
    }

    /// One client→server masked binary frame.
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

    fn cbor_bytes<T: serde::Serialize>(msg: &T) -> Vec<u8> {
        let mut buf = Vec::new();
        ciborium::into_writer(msg, &mut buf).expect("CBOR encode");
        buf
    }

    /// Parse one server→client WS frame from the FRONT of `data`.
    /// Returns (bytes consumed, opcode, payload); control frames carry their
    /// payload (close code + reason) so callers can inspect it.
    fn take_ws_frame(data: &[u8]) -> Option<(usize, u8, Vec<u8>)> {
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
            off += 4;
        }
        if off + len > data.len() {
            return None;
        }
        Some((off + len, opcode, data[off..off + len].to_vec()))
    }

    /// Read the next complete WS frame (any opcode) with a deadline.
    async fn read_ws_frame(client: &mut TcpStream, raw: &mut Vec<u8>) -> Option<(u8, Vec<u8>)> {
        let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
        let mut buf = [0u8; 16 * 1024];
        loop {
            if let Some((consumed, opcode, payload)) = take_ws_frame(raw) {
                raw.drain(..consumed);
                return Some((opcode, payload));
            }
            assert!(tokio::time::Instant::now() < deadline, "WS frame in time");
            let n = client.read(&mut buf).await.expect("ws client read");
            if n == 0 {
                return None;
            }
            raw.extend_from_slice(&buf[..n]);
        }
    }

    /// Sec L2: with max_ws_connections = 2, two connections are accepted, the
    /// third is rejected with a clear Error + Close(1013) and the counter
    /// stays at 2; after one connection closes the counter drops to 1 and a
    /// new connection is accepted again.
    #[tokio::test]
    async fn ws_connection_cap_rejects_over_capacity_and_releases_on_close() {
        let _exclusive = crate::server::protocol::TEST_FAULT_EXCLUSIVE.lock().await;
        const CAP: usize = 2;
        let state = make_server_with(Some(CAP), None, None).await;
        let base = serve_router(state.clone()).await;
        let addr = base.trim_start_matches("http://").to_string();

        // Two connections are accepted (upgrade + Hello → HelloAck).
        let mut conns: Vec<(TcpStream, Vec<u8>)> = Vec::new();
        for _ in 0..CAP {
            let (mut client, mut raw) = ws_connect(&addr, "/ws").await;
            let hello = ClientMessage::Hello {
                protocol_version: PROTOCOL_VERSION,
                features: 0,
            };
            client
                .write_all(&mask_ws_binary(&cbor_bytes(&hello)))
                .await
                .unwrap();
            let (opcode, payload) = read_ws_frame(&mut client, &mut raw)
                .await
                .expect("HelloAck frame");
            assert_eq!(opcode, 0x2, "expected binary frame, got opcode {opcode}");
            let msg: ServerMessage = ciborium::from_reader(&payload[..]).expect("valid cbor");
            assert!(
                matches!(msg, ServerMessage::HelloAck { .. }),
                "expected HelloAck, got {msg:?}"
            );
            conns.push((client, raw));
        }
        assert_eq!(
            state.lock().await.ws_connections.load(Ordering::Relaxed),
            CAP,
            "both connections must hold a slot"
        );

        // Third connection: the upgrade succeeds (the cap is enforced inside
        // the handler), then it receives a clear Error followed by
        // Close(1013 "Try Again Later"), and no slot is consumed.
        let (mut client, mut raw) = ws_connect(&addr, "/ws").await;
        let (opcode, payload) = read_ws_frame(&mut client, &mut raw)
            .await
            .expect("over-cap frame");
        assert_eq!(opcode, 0x2, "expected binary Error frame");
        let msg: ServerMessage = ciborium::from_reader(&payload[..]).expect("valid cbor");
        match msg {
            ServerMessage::Error { message } => {
                assert!(
                    message.contains("maximum connections"),
                    "unexpected error message: {message}"
                );
            }
            other => panic!("expected Error, got {other:?}"),
        }
        let (opcode, payload) = read_ws_frame(&mut client, &mut raw)
            .await
            .expect("close frame");
        assert_eq!(
            opcode, 0x8,
            "expected Close control frame, got opcode {opcode}"
        );
        assert!(payload.len() >= 2, "close frame must carry a code");
        assert_eq!(
            u16::from_be_bytes([payload[0], payload[1]]),
            1013,
            "close code must be 1013 (Try Again Later)"
        );
        assert_eq!(
            state.lock().await.ws_connections.load(Ordering::Relaxed),
            CAP,
            "the rejected connection must not consume a slot"
        );

        // Close one connection → its slot is released → the counter drops.
        drop(conns.remove(0).0);
        let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
        loop {
            if state.lock().await.ws_connections.load(Ordering::Relaxed) == CAP - 1 {
                break;
            }
            assert!(
                tokio::time::Instant::now() < deadline,
                "slot released after close"
            );
            tokio::time::sleep(Duration::from_millis(50)).await;
        }

        // A new connection is now accepted — the released slot is reusable.
        let (mut client, mut raw) = ws_connect(&addr, "/ws").await;
        let hello = ClientMessage::Hello {
            protocol_version: PROTOCOL_VERSION,
            features: 0,
        };
        client
            .write_all(&mask_ws_binary(&cbor_bytes(&hello)))
            .await
            .unwrap();
        let (opcode, payload) = read_ws_frame(&mut client, &mut raw)
            .await
            .expect("HelloAck frame");
        assert_eq!(opcode, 0x2);
        let msg: ServerMessage = ciborium::from_reader(&payload[..]).expect("valid cbor");
        assert!(
            matches!(msg, ServerMessage::HelloAck { .. }),
            "expected HelloAck after slot release, got {msg:?}"
        );
    }

    // ── M3/W7: encode failure closes the connection (no partial bytes) ──

    /// M3/W7 caller-path contract: when the server→client CBOR encode fails
    /// mid-stream, the WS read loop must CLOSE the connection instead of
    /// shipping a truncated frame. With the test fault armed, the first
    /// server frame on this connection (the HelloAck reply) fails to encode:
    /// the client must observe EOF (or a WS close) WITHOUT ever receiving a
    /// decodable `ServerMessage` — a half frame is unrecoverable for the
    /// peer, so the drop-and-close is the only safe behavior.
    ///
    /// The fault is global but scoped to `HelloAck` encodes; this test
    /// holds it only from Hello-send to first-read (milliseconds), so the
    /// blast radius is a stray HelloAck on a concurrent test's fresh
    /// connection during that window (same trade-off the protocol-level
    /// fault tests accept).
    #[tokio::test]
    async fn ws_encode_failure_closes_connection_without_partial_bytes() {
        // The fault hook is process-global: serialize with the protocol-level
        // fault tests for the whole body (we hold the fault across a network
        // round-trip, so the window is far wider than their microsecond one).
        let _exclusive = crate::server::protocol::TEST_FAULT_EXCLUSIVE.lock().await;
        let state = make_server_with(None, None, None).await;
        let base = serve_router(state).await;
        let addr = base.trim_start_matches("http://").to_string();

        let (mut client, mut raw) = ws_connect(&addr, "/ws").await;

        // Arm the fault, THEN send Hello: the server's HelloAck encode hits
        // the fault. Disarm as soon as the first read completes — the close
        // arrives within one round-trip.
        let fault = crate::server::protocol::TEST_ENCODE_FAULT.lock().await;
        let hello = ClientMessage::Hello {
            protocol_version: PROTOCOL_VERSION,
            features: 0,
        };
        client
            .write_all(&mask_ws_binary(&cbor_bytes(&hello)))
            .await
            .unwrap();

        // Read until EOF or a frame appears — whichever comes first.
        let first =
            tokio::time::timeout(Duration::from_secs(5), read_ws_frame(&mut client, &mut raw))
                .await
                .expect("server answered (close or frame) in time");
        drop(fault);

        match first {
            // Expected: the socket closed with no payload at all — no
            // partial CBOR bytes ever reached the peer.
            None => {}
            Some((opcode, payload)) => {
                // Tolerate a clean WS close frame; reject ANY binary frame —
                // a decodable one means the contract is violated (a
                // "successful" encode despite the fault), and an
                // undecodable one is exactly the partial-frame desync the
                // fix exists to prevent.
                assert_eq!(
                    opcode, 0x8,
                    "a failed encode must not ship a {opcode:02x}-opcode frame of {payload:?}"
                );
                assert!(
                    payload.len() >= 2,
                    "close frame carries the 16-bit status code"
                );
            }
        }
        // After the close, further reads see EOF (the connection is gone —
        // not half-open, not silently kept).
        let mut buf = [0u8; 1];
        let eof = tokio::time::timeout(Duration::from_secs(5), client.read(&mut buf))
            .await
            .expect("EOF read in time")
            .expect("socket readable until EOF");
        assert_eq!(eof, 0, "connection must be closed after a failed encode");
    }

    // ── FIX 2: per-game spectator cap ─────────────────────────────────

    /// Minimal game settings (same shape as the delta.rs test helper).
    fn test_game_settings() -> GameSettings {
        let unit_type = UnitType {
            name: "infantry".to_string(),
            symbol: "I".to_string(),
            men: 100,
            equipment: vec![Equipment {
                name: "rifle".to_string(),
                count: 100,
                max_count: 100,
                cost_per_item: 2.5,
            }],
            soft_attack: 5.0,
            soft_defense: 5.5,
            hard_attack: 3.0,
            hard_defense: 2.0,
            recruitment_cost: 200,
            max_organisation: 100.0,
            reinforcement_rate: 5.0,
            speed: 1.0,
            base_attrition: 0.5,
            fighting_attrition: 1.0,
            supply_buffer_percent: 10,
            supply_rate: 1,
            max_supply: 200,
        };
        GameSettings {
            starting_funds: 1000,
            initial_manpower: 10_000,
            funds_per_city: 1.0,
            recruitment_rate: 1.0,
            game_speed: 1.0,
            use_standard_ruleset: true,
            unit_types: vec![unit_type],
            city_upgrade_settings: CityUpgradeSettings::default(),
            water_speed: 0.5,
            influence: InfluenceSettings::default(),
        }
    }

    /// Wait for the spectator's FullState greeting on its egress channel
    /// (deltas are tolerated and skipped).
    async fn wait_for_full_state(
        rx: &mut tokio::sync::mpsc::UnboundedReceiver<EgressMessage>,
        label: &str,
    ) {
        let deadline = tokio::time::Instant::now() + Duration::from_secs(15);
        loop {
            assert!(
                tokio::time::Instant::now() < deadline,
                "{label}: FullState in time"
            );
            match tokio::time::timeout_at(deadline, rx.recv())
                .await
                .expect("deadline checked above")
            {
                Some(EgressMessage::Reliable(data)) => {
                    let msg: ServerMessage = ciborium::from_reader(&data[..]).expect("valid cbor");
                    if matches!(msg, ServerMessage::FullState { .. }) {
                        return;
                    }
                }
                Some(EgressMessage::Delta(_)) => {} // keep-latest ring ref — skip
                Some(EgressMessage::Close) => {}    // tolerate: close request (encode-failure path)
                None => panic!("{label}: egress channel closed before FullState"),
            }
        }
    }

    /// Sec L2: with max_spectators_per_game = 2, spectators 1-2 receive the
    /// FullState greeting; the third is rejected with a clear Error and NO
    /// FullState (and earlier spectators keep their stream).
    #[tokio::test]
    async fn spectator_cap_rejects_over_capacity_spectators() {
        const CAP: usize = 2;
        let state = make_server_with(None, Some(CAP), None).await;
        let (config, storage) = {
            let s = state.lock().await;
            (s.config.clone(), s.storage.clone())
        };

        // One controlled player with no live connection: the room needs a
        // real game player (player_0, created by Game::new) so city
        // ownership and the spectator vision grid resolve.
        let players = vec![PlayerConnection::Player {
            id: "player_0".to_string(),
            tx: PlayerSender::None,
        }];
        let room = GameRoom::new(
            "game1".to_string(),
            load_default_map(),
            "default".to_string(),
            test_game_settings(),
            players,
            vec![],
            "room1".to_string(),
            state.clone(),
            config,
            storage,
            true,
        );

        // Spectators 1 and 2 join: each receives the FullState greeting.
        let mut spec_rxs: Vec<tokio::sync::mpsc::UnboundedReceiver<EgressMessage>> = Vec::new();
        for i in 0..CAP {
            let (tx, rx) = tokio::sync::mpsc::unbounded_channel::<EgressMessage>();
            room.command_tx
                .send(Command::AddSpectator(PlayerSender::Tcp(TcpEgress::new(tx))))
                .expect("command accepted");
            let mut rx = rx;
            wait_for_full_state(&mut rx, &format!("spectator {i}")).await;
            spec_rxs.push(rx);
        }

        // Spectator CAP+1 is rejected: a clear Error, and never a FullState.
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<EgressMessage>();
        room.command_tx
            .send(Command::AddSpectator(PlayerSender::Tcp(TcpEgress::new(tx))))
            .expect("command accepted");
        let deadline = tokio::time::Instant::now() + Duration::from_secs(15);
        loop {
            assert!(
                tokio::time::Instant::now() < deadline,
                "over-cap spectator: answer in time"
            );
            match tokio::time::timeout_at(deadline, rx.recv())
                .await
                .expect("deadline checked above")
            {
                Some(EgressMessage::Reliable(data)) => {
                    let msg: ServerMessage = ciborium::from_reader(&data[..]).expect("valid cbor");
                    match msg {
                        ServerMessage::Error { message } => {
                            assert!(
                                message.contains("Spectator limit"),
                                "unexpected error: {message}"
                            );
                            break;
                        }
                        ServerMessage::FullState { .. } => {
                            panic!("over-cap spectator must NOT receive FullState");
                        }
                        other => panic!("unexpected message for over-cap spectator: {other:?}"),
                    }
                }
                Some(EgressMessage::Delta(_)) => {} // tolerate
                Some(EgressMessage::Close) => {}    // tolerate (rejection path never closes)
                None => panic!("over-cap spectator: channel closed"),
            }
        }

        // The earlier spectators are unaffected: the room still holds their
        // senders (if the rejection path had dropped them, the receiver side
        // would report the channel closed).
        for (i, rx) in spec_rxs.iter().enumerate() {
            assert!(
                !rx.is_closed(),
                "accepted spectator {i}'s egress channel must still be open"
            );
        }

        // Drop the room → the command channel closes → the tick task exits.
        drop(room);
    }
}
