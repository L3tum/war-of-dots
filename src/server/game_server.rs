//! Server: HTTP + raw TCP for game state

use bytes::Bytes;
use sqlite::Value;
use std::collections::{HashMap, HashSet};
use std::io::Write as _;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use tokio::sync::{mpsc, Mutex};
use tokio::time::Duration;
use uuid::Uuid;

use axum::extract::ws::{CloseFrame, Message, WebSocket};
use axum::extract::{Query, State, WebSocketUpgrade};
use axum::response::{Html, IntoResponse};
use axum::routing::get;
use futures::{SinkExt, StreamExt};
use include_dir::{include_dir, Dir};

use crate::config::{Config, EquipmentConfig, UnitTypeConfig as ConfigUnitType};
use crate::game::economy::UnitType;
use crate::game::game::{Game, GameSettings};
// H5: engine-neutral network baselines (engine module, plain engine types).
use crate::game::unit::Equipment;
use crate::game::NetworkBaselines;

use crate::map::map::Map;
use crate::map::map_config::load_default_map;
use crate::map::map_registry;
use crate::server::delta::{
    init_unit_baselines, run_network_phase, update_unit_baselines, wire_unit_states,
    NetworkPhaseResult, TickDiffs, UnitBaselineStore,
};
use crate::server::framing;
use crate::server::lobby::{LobbyManager, LOBBY_IDLE_TTL};
use crate::server::protocol::{
    decode_client_message, encode_message, encode_message_result, encode_message_with_hint,
    estimate_delta_size, negotiate_features, CborEncodeError, CityState, ClientMessage,
    GameStateMessage, LobbyMessage, LobbySettings, PlayerState, ServerMessage, UnitState,
    PROTOCOL_VERSION,
};

// Re-export types from room module for use throughout game_server
pub use crate::server::room::{
    egress_bytes, egress_vec, Command, DebugGameStats, DeltaRing, EgressMessage, GameRoom,
    LlmPlayerConfig, PlayerConnection, PlayerSender, TcpEgress,
};

// Re-export delta functions
use crate::server::delta::{build_delta_inner, build_llm_state};

static FRONTEND: Dir = include_dir!("$CARGO_MANIFEST_DIR/frontend/dist");

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

/// Sec L2: spectator join with the per-game cap (config
/// `network.max_spectators_per_game`). The check and the `players.push`
/// both happen in the tick task — the sole owner of `players` and the sole
/// consumer of the command channel — so concurrent joins cannot race past
/// the cap. Over-cap joins receive a clear Error message (their dropped
/// sender then ends the spectator's forward task and closes the
/// connection); earlier spectators are unaffected.
async fn add_spectator_capped(
    cap: usize,
    players: &mut Vec<PlayerConnection>,
    game: &Game,
    sender: PlayerSender,
) {
    let cap = cap.max(1);
    let spec_count = players
        .iter()
        .filter(|p| matches!(p, PlayerConnection::Spectator { .. }))
        .count();
    if spec_count >= cap {
        let err = encode_message(&ServerMessage::Error {
            message: format!("Spectator limit reached ({cap})"),
        });
        let _ = sender.send_reliable(err.into()).await;
        return;
    }
    let all_pids: Vec<String> = game.players().keys().cloned().collect();
    let vision_grid = game.union_vision_grids(&all_pids);
    let border_grid = all_pids.first().and_then(|p| game.border_grid_2d(p));
    let full_state = game.full_state_with_vision(vision_grid, border_grid);
    let encoded = encode_message(&ServerMessage::FullState(full_state));
    let _ = sender.send_reliable(encoded.into()).await;
    players.push(PlayerConnection::Spectator { tx: sender });
}

impl GameRoom {
    #[allow(clippy::too_many_arguments)]
    fn new(
        game_id: String,
        map: Map,
        // Map name as chosen in the lobby (e.g. "default", a custom-map id)
        // — persisted into `game_metadata.settings` at game end so the
        // replay list can show which map the game ran on.
        map_name: String,
        settings: GameSettings,
        players: Vec<PlayerConnection>,
        llm_configs: Vec<LlmPlayerConfig>,
        room_id: String,
        server_state: Arc<Mutex<GameServer>>,
        config: Config,
        storage: crate::storage::Storage,
        spectator_vision: bool,
    ) -> Self {
        let starting_funds = settings.starting_funds;

        // Real wall-clock game start (UNIX seconds). `record_game_end`
        // persists it so the replay list's `end_time - start_time` duration
        // is the actual game length (the old start_time=0 sentinel rendered
        // as ~54-year games).
        let game_start_time: u64 = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);

        // Build the list of controlled player IDs (human or LLM) BEFORE creating the game
        let controlled_player_ids: Vec<String> = players
            .iter()
            .filter_map(|p| p.id().cloned())
            .chain(llm_configs.iter().map(|c| c.player_id.clone()))
            .collect::<std::collections::HashSet<String>>()
            .into_iter()
            .collect();
        let num_players = controlled_player_ids.len().max(1);

        let mut game = Game::new(map, settings, num_players);
        game.start();

        // Assign each city to the corresponding player (round-robin among controlled players)
        let cities_snapshot: Vec<String> =
            game.query_cities().iter().map(|c| c.id.clone()).collect();
        for (i, city_id) in cities_snapshot.iter().enumerate() {
            let pid = &controlled_player_ids[i % controlled_player_ids.len()];
            game.assign_city_owner(city_id, pid);
            game.all_player_funds()
                .entry(pid.clone())
                .or_insert(starting_funds);
        }
        // Recruit a starting force of 3 units for each controlled player (W8)
        let unit_type_opt = game.settings().unit_types.first().cloned();
        if let Some(unit_type) = unit_type_opt {
            let cities_with_pos: Vec<(String, (f32, f32))> = game
                .query_cities()
                .iter()
                .map(|c| (c.id.clone(), c.position))
                .collect();
            for (i, player_id) in controlled_player_ids.iter().enumerate() {
                if let Some((city_id, (cx, cy))) = cities_snapshot
                    .get(i)
                    .and_then(|id| cities_with_pos.iter().find(|(cid, _)| cid == id))
                    .cloned()
                {
                    // First unit: standard gated recruit at city position
                    let result = game.recruit_unit(&city_id, &unit_type, player_id);
                    if result.is_none() {
                        tracing::warn!(
                            player = %player_id,
                            city = %city_id,
                            "starting recruit failed: funds/manpower/occupancy gate"
                        );
                    }
                    // Additional units via recruit_unit_at (bypasses occupancy gate)
                    for offset in [(2.0f32, 0.0f32), (-2.0f32, 0.0f32)] {
                        let pos = (cx + offset.0, cy + offset.1);
                        if game.recruit_unit_at(&unit_type, player_id, pos).is_none() {
                            tracing::debug!(
                                player = %player_id,
                                city = %city_id,
                                x = pos.0,
                                y = pos.1,
                                "additional starting recruit failed"
                            );
                        }
                    }
                }
            }
        }
        // Initialize prev_unit_owners for stats tracking
        let mut prev_owners = std::collections::HashMap::new();
        for unit in game.query_all_units() {
            *prev_owners.entry(unit.owner.clone()).or_insert(0) += 1;
        }
        // prev_unit_owners removed in ECS refactor = prev_owners;

        let (cmd_tx, mut cmd_rx) = mpsc::unbounded_channel();
        let game_speed = game.settings().game_speed;
        let interval_ms = (1000.0 / game_speed).ceil() as u64;

        // --- Replay recording (best-effort) ---
        // Every game is written to `{replay_path}/{room_id}.jsonl` in exactly
        // the shape the frontend's ReplayPlayer consumes: one
        // `{"full_state": ...}` line, then one `{"delta": ...}` line per tick
        // that produces a delta for the recording viewpoint, and a final
        // `{"event":"game_end"}` marker (written when the loop exits). The
        // viewpoint is the first game player: per-pid deltas are
        // vision-scoped, so the replay shows that player's view of the game —
        // the same stream their client received over the wire. Recording is
        // deliberately non-fatal: a disk failure degrades to "no replay",
        // never a broken game.
        // Viewpoint = the first human connection: `run_network_phase` only
        // builds deltas for human pids (`regular_pids`), so a pure-LLM
        // viewpoint would yield an empty file. Pure-LLM games (no human
        // connections) simply aren't recorded.
        let record_pid: Option<String> = players.iter().find_map(|p| p.id().cloned());
        let mut replay_recorder: Option<std::io::BufWriter<std::fs::File>> = None;
        if record_pid.is_some() {
            let dir = config.storage.replay_path.clone();
            let _ = std::fs::create_dir_all(&dir);
            match std::fs::File::create(
                std::path::Path::new(&dir).join(format!("{}.jsonl", room_id)),
            ) {
                Ok(f) => replay_recorder = Some(std::io::BufWriter::new(f)),
                Err(e) => tracing::warn!("replay: cannot create file for game {}: {}", room_id, e),
            }
        }

        // Settings payload for the game_metadata row written at game end:
        // the ACTUAL in-play settings + map name (record_game_end persists
        // it verbatim; the replay list UI shows it).
        let settings_json = game_metadata_settings_json(&game, &map_name);

        for player in &players {
            let player_id = player.id().cloned().unwrap_or_default();
            let initial_state = game.full_state_with_player_id(&player_id);
            // Replay: log the viewpoint's initial full state (the same
            // payload about to be sent on the wire), flushed immediately so
            // the file is playable even if the process dies mid-game.
            if record_pid.as_deref() == Some(player_id.as_str()) {
                if let Some(rec) = replay_recorder.as_mut() {
                    if let Ok(v) = serde_json::to_value(&initial_state) {
                        let _ = writeln!(rec, "{{\"full_state\":{}}}", v);
                        let _ = rec.flush();
                    }
                }
            }
            let initial_msg = ServerMessage::FullState(initial_state);
            let encoded = encode_message(&initial_msg);
            let _ = player.tx().blocking_send_reliable(encoded.into());
        }

        // Create LLM agents from configs
        let mut llm_pairs: Vec<(crate::llm::agent::LlmAgent, crate::llm::clients::LlmClient)> =
            Vec::new();

        for llm_config in llm_configs {
            let client = match crate::llm::client_factory::create_llm_client(
                &llm_config.provider,
                &llm_config.model,
                llm_config.api_key.as_deref(),
                llm_config.base_url.as_deref(),
            ) {
                Ok(client) => client,
                Err(e) => {
                    eprintln!(
                        "Failed to create LLM client for {}: {}",
                        llm_config.player_name, e
                    );
                    continue;
                }
            };
            let agent = crate::llm::agent::LlmAgent::new(
                &llm_config.player_id,
                &llm_config.player_name,
                crate::llm::agent::LlmProvider {
                    name: llm_config.provider.clone(),
                    model: llm_config.model.clone(),
                    api_key: None,
                    base_url: llm_config.base_url,
                },
                llm_config.thinking_delay,
            );
            llm_pairs.push((agent, client));
        }

        // Create advisors for human players (disabled by default)
        let advisor_client_opt = if let Some(provider_name) = config.llm.advisor_provider.as_ref() {
            if let Some(provider_config) = config
                .llm
                .providers
                .iter()
                .find(|p| p.name == *provider_name)
            {
                match crate::llm::client_factory::create_llm_client(
                    &provider_config.provider,
                    &provider_config.model,
                    provider_config.api_key.as_deref(),
                    provider_config.base_url.as_deref(),
                ) {
                    Ok(client) => Some(client),
                    Err(e) => {
                        eprintln!("Failed to create advisor client: {}", e);
                        None
                    }
                }
            } else {
                None
            }
        } else {
            None
        };

        let mut advisors: std::collections::HashMap<String, crate::llm::advisor::Advisor> =
            std::collections::HashMap::new();
        if let Some(ref _advisor_client) = advisor_client_opt {
            for player in &players {
                let advisor = crate::llm::advisor::Advisor::new();
                if let Some(id) = player.id() {
                    advisors.insert(id.clone(), advisor);
                }
            }
            // Set all advisors disabled initially
            for advisor in advisors.values_mut() {
                advisor.set_enabled(false);
            }
        }

        let storage_clone = storage.clone();

        // --- Broadcast channels per player for state snapshots (lock-free) ---
        let mut state_broadcasts: std::collections::HashMap<
            String,
            tokio::sync::broadcast::Sender<crate::llm::agent::GameState>,
        > = std::collections::HashMap::new();
        for player_id in game.players().keys().cloned() {
            let (tx, _) = tokio::sync::broadcast::channel::<crate::llm::agent::GameState>(2);
            state_broadcasts.insert(player_id, tx);
        }

        // --- Spawn LLM agent tasks (one per agent) ---
        let mut spawned_handles: Vec<tokio::task::JoinHandle<()>> = Vec::new();
        // Reason log channel: LLM tasks → main loop → all players
        let (reason_tx, mut reason_rx) = tokio::sync::mpsc::channel::<(String, String, String)>(64);

        for (mut agent, client) in llm_pairs {
            let player_id = agent.id.clone();
            let agent_name = agent.name.clone();

            let state_rx = match state_broadcasts.get(&player_id) {
                Some(tx) => tx.subscribe(),
                None => continue,
            };

            let cmd_tx_clone = cmd_tx.clone();
            let reason_tx_clone = reason_tx.clone();

            let handle = tokio::spawn(async move {
                let mut state_rx = state_rx;
                let mut latest_state: Option<crate::llm::agent::GameState> = None;

                loop {
                    tokio::select! {
                        result = state_rx.recv() => {
                            match result {
                                Ok(state) => {
                                    latest_state = Some(state);
                                }
                                Err(tokio::sync::broadcast::error::RecvError::Lagged(_skipped)) => {
                                    // Missed some snapshots — receiver still usable
                                    // Just wait for the next one
                                }
                                Err(tokio::sync::broadcast::error::RecvError::Closed) => {
                                    break;
                                }
                            }
                        }
                        _ = tokio::time::sleep(agent.thinking_delay) => {
                            if let Some(state) = latest_state.take() {
                                match agent.step(&client, &state).await {
                                    Ok(cmds) => {
                                        for cmd in cmds {
                                            let _ = cmd_tx_clone.send(cmd);
                                        }
                                        // Broadcast recent reason log entries
                                        for entry in agent.reason_log.iter().rev().take(5) {
                                            let _ = reason_tx_clone.send((
                                                agent_name.clone(),
                                                entry.action.clone(),
                                                entry.reason.clone(),
                                            )).await;
                                        }
                                        // Trim reason log to prevent unbounded growth
                                        if agent.reason_log.len() > 100 {
                                            agent.reason_log.drain(..agent.reason_log.len() - 100);
                                        }
                                    }
                                    Err(e) => {
                                        tracing::warn!("LLM agent {} step failed: {}", agent_name, e);
                                    }
                                }
                            }
                        }
                    }
                }
            });
            spawned_handles.push(handle);
        }

        // --- Spawn advisor tasks (one per enabled advisor) ---
        let advisor_client_opt_spawn = advisor_client_opt.clone();
        let advisor_interval = std::time::Duration::from_secs(60);
        // Advice channel: advisor tasks → main loop → player
        let (advice_tx, mut advice_rx) = tokio::sync::mpsc::channel::<(String, ServerMessage)>(64);

        for player_id in advisors.keys() {
            let state_rx = match state_broadcasts.get(player_id) {
                Some(tx) => tx.subscribe(),
                None => continue,
            };

            let advisor_client = advisor_client_opt_spawn.clone();
            let advice_tx_clone = advice_tx.clone();
            let pid = player_id.clone();

            let handle = tokio::spawn(async move {
                let mut state_rx = state_rx;
                loop {
                    tokio::time::sleep(advisor_interval).await;

                    let state = match state_rx.recv().await {
                        Ok(s) => s,
                        Err(tokio::sync::broadcast::error::RecvError::Lagged(_skipped)) => {
                            continue;
                        }
                        Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
                    };

                    if let Some(ref client) = advisor_client {
                        let mut adv = crate::llm::advisor::Advisor::new();
                        adv.set_enabled(true);
                        if let Some(advice) = adv.get_advice(client, &state).await {
                            let msg = ServerMessage::Advice { text: advice.text };
                            let _ = advice_tx_clone.send((pid.clone(), msg)).await;
                        }
                    }
                }
            });
            spawned_handles.push(handle);
        }

        // Shared per-game debug stats, updated by the tick loop and read by /debug/stats.
        let game_stats = Arc::new(std::sync::Mutex::new(DebugGameStats::default()));
        // A second Arc so the loop-use and the final GameRoom placement don't conflict
        // (the coroutine would otherwise move `game_stats` and reject the later use).
        let game_stats_room = game_stats.clone();
        // Sec L2: per-game spectator cap (config.network.max_spectators_per_game),
        // enforced in the tick task — the sole owner of `players` (see
        // `add_spectator_capped`).
        let spectator_cap = config.network.max_spectators_per_game;

        tokio::spawn(async move {
            let mut game = game;
            // H5: engine-neutral baselines live in the ENGINE module (plain
            // engine types, single commit step); the wire-typed unit baseline
            // store stays server-side (Arch H2) and is seeded alongside.
            let mut baselines = NetworkBaselines::from_snapshot(
                game.query_cities(),
                game.all_player_funds(),
                {
                    let mut m = std::collections::HashMap::new();
                    for pid in game.players().keys() {
                        if let Some(v) = game.vision_grid_flat(pid) {
                            m.insert(pid.clone(), v);
                        }
                    }
                    m
                },
                game.territory_grid_flat(),
            );
            // I1: seed the entity-indexed unit baseline store ONCE per game
            // (replaces the per-tick `prev_units` HashMap rebuild).
            let mut unit_baselines = UnitBaselineStore::default();
            init_unit_baselines(&game, &mut unit_baselines);
            let mut players = players;
            let mut advisors = advisors;
            // I8: lazy cache for the spectator union vision grid — recomputed
            // only when the pid list changes or any per-player vision grid Arc
            // changes (the influence double-buffer flips ALL published Arcs on
            // every dirty run, so cache hits land exactly on grid-clean ticks).
            let mut spec_union_cache: Option<SpectatorUnionCache> = None;
            let server_state = server_state;
            let state_broadcasts = state_broadcasts;
            let spawned_handles = spawned_handles;

            let mut interval = tokio::time::interval(Duration::from_millis(interval_ms));
            let mut tick_count: u64 = 0;
            loop {
                tokio::select! {
                    _ = interval.tick() => {
                        tick_count += 1;
                        let _tick_start = std::time::Instant::now();

                        // --- Process advice messages (non-blocking) ---
                        while let Ok((pid, msg)) = advice_rx.try_recv() {
                            let encoded = encode_message(&msg);
                            for player in &players {
                                if player.id().is_some_and(|id| *id == pid) {
                                    let _ = player.tx().send_reliable(encoded.clone().into()).await;
                                }
                            }
                        }

                        // --- Process reason log entries (non-blocking) ---
                        while let Ok((name, action, reason)) = reason_rx.try_recv() {
                            let encoded = Bytes::from(encode_message(&ServerMessage::LlmReasonLog {
                                player_name: name,
                                action,
                                reason,
                            }));
                            for player in &players {
                                let _ = player.tx().send_reliable(encoded.clone()).await;
                            }
                        }

                        // --- Process commands from LLM agents & players (non-blocking) ---
                        while let Ok(cmd) = cmd_rx.try_recv() {
                            match cmd {
                                Command::MoveTroop { unit_id, owner_id, path } => {
                                    // Ownership is enforced inside game.move_troops (single check).
                                    game.move_troops(&unit_id, &owner_id, path);
                                }
                                Command::RecruitUnit(city_id, unit_type_name, owner) => {
                                    if let Some(unit_type) = game.settings().unit_types.iter().find(|u| u.name == unit_type_name).cloned() {
                                        let _ = game.recruit_unit(&city_id, &unit_type, &owner);
                                    }
                                }
                                Command::ToggleHealing { unit_id, owner_id, enabled } => {
                                    game.toggle_healing(&unit_id, &owner_id, enabled);
                                }
                                Command::ToggleResupply { unit_id, owner_id, enabled } => {
                                    game.toggle_resupply(&unit_id, &owner_id, enabled);
                                }
                                Command::UpgradeCity(city_id, owner) => {
                                    let _ = game.upgrade_city(&city_id, &owner);
                                }
                                Command::ToggleAdvisor(pid, enabled) => {
                                    if let Some(advisor) = advisors.get_mut(&pid) {
                                        advisor.set_enabled(enabled);
                                    }
                                }
                                Command::AddSpectator(sender) => {
                                    // Sec L2: per-game spectator cap (the tick
                                    // task is the sole owner of `players`).
                                    add_spectator_capped(spectator_cap, &mut players, &game, sender)
                                        .await;
                                }
                                Command::EmbarkUnit(unit_id, pid) => {
                                    game.embark_unit(&unit_id, &pid);
                                }
                                Command::DisembarkUnit(unit_id, pid) => {
                                    game.disembark_unit(&unit_id, &pid);
                                }
                            }
                        }

                        // --- Run game systems ---
                        game.tick_game();

                        // I1: single in-place ECS pass replacing the per-tick
                        // full unit snapshot — the server-side
                        // `update_unit_baselines` (Arch H2 seam: the engine
                        // only yields plain `UnitView`s) diffs every live
                        // unit against the PRE-COMMIT baseline, emits only
                        // changed units into `unit_diff`, and commits the
                        // baseline store. `unit_diff` feeds both the
                        // active-player check (per-owner counts) and the
                        // per-player delta build below.
                        let unit_diff = update_unit_baselines(&game, &mut unit_baselines);
                        // H5: plain engine city snapshot (engine-owned data —
                        // the server converts to wire types only inside the
                        // diff/commit).
                        let current_cities: Vec<crate::game::economy::City> = game.query_cities();

                        // Update live debug stats (read by /debug/stats)
                        {
                            let mut gs = game_stats.lock().unwrap_or_else(|e| e.into_inner());
                            gs.tick_count = tick_count;
                            gs.last_tick_ms = _tick_start.elapsed().as_secs_f64() * 1000.0;
                            gs.entity_count = game.unit_count() + game.city_count();
                        }

                        // --- Broadcast state snapshots every 60 ticks (~1 sec) ---
                        if tick_count.is_multiple_of(60) {
                            for (player_id, tx) in &state_broadcasts {
                                // Perf #1: no subscribers means the build is
                                // pure waste — a full clone of every unit's
                                // strings/vecs plus ~80k grid cells for a
                                // channel nobody reads (humans have no LLM
                                // agent/advisor attached). LLM agents and
                                // advisors subscribe at game start, so a
                                // late-attaching receiver still gets a fresh
                                // snapshot within one second of subscribing.
                                if tx.receiver_count() == 0 {
                                    continue;
                                }
                                let state = build_llm_state(&game, player_id);
                                let _ = tx.send(state);
                            }
                        }

                        // --- Check game over ---
                        // I5: build the per-owner unit count map ONCE (it is
                        // also passed to run_network_phase, which previously
                        // recomputed it) plus a city-owner set, and reduce
                        // the active check from O(P×N) string compares to
                        // O(P). The predicate is identical to the old scan —
                        // and identical to determine_winner's own
                        // `players_with_power` predicate (game.rs) — so when
                        // active_count == 1 the winner IS the sole active
                        // player and the per-tick determine_winner() call
                        // (P archetype + P city scans) is skipped entirely.
                        // I1: the per-owner counts come straight from the
                        // in-place pass (`unit_diff`) instead of a second
                        // O(N) `count_units_by_owner` scan — and borrowed,
                        // not cloned (Perf #4: the per-tick `HashMap` clone
                        // was pure waste — the active check only reads it,
                        // and `unit_diff` is moved into the network phase
                        // below, where the map lives on as `SharedDeltas`).
                        let units_by_owner = &unit_diff.current_owner_counts;
                        let city_owners: HashSet<&str> = current_cities
                            .iter()
                            .filter_map(|c| c.owner.as_deref())
                            .collect();
                        // M4: borrowed pids — `Vec<String>` cloned every tick
                        // (N string allocations) is unnecessary: the game map
                        // outlives this tick, so `&str` borrows are free.
                        let player_ids: Vec<&str> =
                            game.players().keys().map(|s| s.as_str()).collect();
                        let total_players = player_ids.len();
                        let active_count = player_ids.iter().filter(|pid| {
                            units_by_owner.get(**pid).copied().unwrap_or(0) > 0
                                || city_owners.contains(**pid)
                        }).count();
                        // I5: the winner, when the game ends, is the sole
                        // active player (predicate-identical to the old
                        // determine_winner call for this case).
                        let sole_active = (active_count == 1).then(|| {
                            player_ids
                                .iter()
                                .find(|pid| {
                                    units_by_owner
                                        .get(**pid)
                                        .copied()
                                        .unwrap_or(0)
                                        > 0
                                        || city_owners.contains(**pid)
                                })
                                .copied()
                        }).flatten();
                        if active_count == 0 {
                            persist_game_end(
                                &storage_clone,
                                &room_id,
                                game_start_time,
                                &settings_json,
                                &[""],
                                &player_ids,
                                &game,
                            );
                            let msg = ServerMessage::GameOver {
                                winner: "Draw".to_string(),
                                game_id: room_id.clone(),
                                message: "⚖️ Draw — all players eliminated".to_string(),
                            };
                            let encoded = Bytes::from(encode_message(&msg));
                            for player in &players {
                                let _ = player.tx().send_reliable(encoded.clone()).await;
                            }
                            for handle in spawned_handles {
                                handle.abort();
                            }
                            break;
                        }
                        if total_players > 1 && active_count == 1 {
                            let winner_id = sole_active.unwrap_or_default();
                            let winner_name = game.players().get(winner_id)
                                .map(|p| p.name.clone())
                                .unwrap_or_else(|| winner_id.to_string());
                            persist_game_end(
                                &storage_clone,
                                &room_id,
                                game_start_time,
                                &settings_json,
                                &[winner_id],
                                &player_ids,
                                &game,
                            );
                            let msg = ServerMessage::GameOver {
                                winner: winner_name,
                                game_id: room_id.clone(),
                                message: "Game over — last player standing wins".to_string(),
                            };
                            let encoded = Bytes::from(encode_message(&msg));
                            for player in &players {
                                let _ = player.tx().send_reliable(encoded.clone()).await;
                            }
                            for handle in spawned_handles {
                                handle.abort();
                            }
                            break;
                        }

                        // --- Network phase: per-player delta build + encode ---
                        // W4: borrowed pids — no per-tick String clones for the id list.
                        let regular_pids: Vec<&str> = players
                            .iter()
                            .filter_map(|p| {
                                if let PlayerConnection::Player { id, .. } = p {
                                    Some(id.as_str())
                                } else {
                                    None
                                }
                            })
                            .collect();

                        // H3: the per-tick shared diff inputs flow as one
                        // structured value (unit diff + city snapshot +
                        // border grid) instead of parallel parameters.
                        let tick_diffs = TickDiffs {
                            units: unit_diff,
                            cities: current_cities,
                            border_grid: game.territory_grid_flat(),
                        };
                        let mut phase: NetworkPhaseResult = run_network_phase(
                            &game,
                            &mut baselines,
                            &regular_pids,
                            tick_diffs,
                            true, // W9 splice path: on — A/B confirmed ~1.5% off the network phase (byte-for-byte verified encoder)
                            record_pid.as_deref(),
                        );

                        // Replay: log the recording viewpoint's delta for this
                        // tick (idle ticks build no delta — nothing was sent
                        // on the wire either, so nothing is logged).
                        // Perf #2: NO per-tick flush — the BufWriter buffers in
                        // userspace and we flush on a cadence below (~1 s,
                        // aligned with the state-snapshot ticks), at game end,
                        // and on drop. Crash durability: the last <1 s of
                        // deltas may be lost; the replay viewer tolerates a
                        // missing terminal marker, and the JSONL shape is
                        // unchanged (frontend replay paths are byte-pinned).
                        let recorded_delta = phase.recorded_delta.take();
                        if let Some(rec) = replay_recorder.as_mut() {
                            if let Some(d) = &recorded_delta {
                                if let Ok(v) = serde_json::to_value(d) {
                                    let _ = writeln!(rec, "{{\"delta\":{}}}", v);
                                }
                            }
                            // Perf #2: cadence flush (every 60 ticks ≈ 1 s).
                            if tick_count.is_multiple_of(60) {
                                let _ = rec.flush();
                            }
                        }

                        // W4: outbox by move — the send loop takes ownership of each
                        // payload, so a tick doesn't memcpy the (up to 200 KB) encoded
                        // message per player, and no per-tick HashMap/String-key clones
                        // are needed. The spectator entry (if any) is pushed in before
                        // the send loop below.
                        let mut outbox: Vec<(String, Vec<u8>)> = phase.encoded;

                        // W5: the spectator reuses the shared delta phase computed by
                        // run_network_phase instead of recomputing the whole shared
                        // diff for the synthetic "" spectator every tick (no fresh
                        // HashSet of unit ids, no fresh all_player_funds(), no extra
                        // 40 KB union-grid Arc in the common no-spectator case).
                        // - The union vision grid is computed only when at least one
                        //   spectator connection actually exists.
                        // - Per-owner unit counts come from `phase.shared` (W8),
                        //   so the spectator no longer needs the unit snapshot
                        //   that was moved into run_network_phase.
                        // - Reusing `phase.shared` (pre-commit baseline) means the
                        //   spectator sees the tick's real unit/city updates — the
                        //   deliberate W1 behavior; post-commit prev_funds == all_funds.
                        // - The "" pid has no funds baseline entry, so the idle-skip
                        //   only fires on truly idle ticks (W5 fix) instead of the
                        //   spectator receiving a message every tick.
                        if spectator_vision
                            && players.iter().any(|p| matches!(p, PlayerConnection::Spectator { .. }))
                        {
                            let all_pids: Vec<String> = game.players().keys().cloned().collect();
                            // I8: per-pid vision grid Arcs are the cache keys —
                            // `Arc::ptr_eq` misses on any grid-dirty run (all
                            // published Arcs flip together), so the 1 MB union
                            // OR only runs when it actually changed.
                            let source_arcs: Vec<Option<Arc<Vec<u8>>>> = all_pids
                                .iter()
                                .map(|pid| game.vision_grid_flat(pid))
                                .collect();
                            let spec_vision = SpectatorUnionCache::update(
                                &mut spec_union_cache,
                                &all_pids,
                                &source_arcs,
                                || game.union_vision_grid_flat(&all_pids),
                            );
                            let spec_pid = String::new();
                            let prev_spec_vision: Option<&Arc<Vec<u8>>> =
                                baselines.prev_vision.get(&spec_pid);
                            if let Some((delta, _)) = build_delta_inner(
                                &baselines.prev_funds,
                                prev_spec_vision,
                                &baselines.prev_funds,
                                &spec_vision,
                                phase.grid_width,
                                &spec_pid,
                                &phase.shared,
                            ) {
                                // N5: pre-size the CBOR buffer (the old path used bare encode_message).
                                let dref = match &delta {
                                    ServerMessage::DeltaState(d) => d,
                                    _ => unreachable!(),
                                };
                                match encode_message_with_hint(&delta, estimate_delta_size(dref)) {
                                    Ok(encoded) => {
                                        if let Some(v) = &spec_vision {
                                            baselines.commit_vision(spec_pid.clone(), Arc::clone(v));
                                        }
                                        outbox.push((spec_pid, encoded));
                                    }
                                    Err(e) => {
                                        // M3/W7: the spectator's delta encode failed —
                                        // drop the frame (never send the truncated
                                        // bytes) and close the connection.
                                        tracing::error!(error = %e, "spectator delta encode failed — closing connection");
                                        for p in &players {
                                            if matches!(p, PlayerConnection::Spectator { .. }) {
                                                p.tx().close();
                                            }
                                        }
                                    }
                                }
                            } else if let Some(v) = &spec_vision {
                                baselines.commit_vision(spec_pid, Arc::clone(v));
                            }
                        }

                        // Send encoded deltas; keep alive connections.
                        // W4: take-by-move via a linear scan (players ≤ 16); the
                        // is_ok() check is the sole drop condition (send failed →
                        // the player is dropped), same semantics as before.
                        // M3/W7: players whose delta encode FAILED this tick are
                        // not in the outbox (the frame was dropped, never sent);
                        // the shared unit/city/funds baselines advanced without
                        // delivery, so their stream is unrecoverable — close the
                        // connection and stop sending to them.
                        let encode_failed: std::collections::HashSet<&str> = phase
                            .encode_failures
                            .iter()
                            .map(|p| p.as_str())
                            .collect();
                        if !encode_failed.is_empty() {
                            for player in &players {
                                if let Some(pid) = player.id() {
                                    if encode_failed.contains(pid.as_str()) {
                                        player.tx().close();
                                    }
                                }
                            }
                            for pid in &phase.encode_failures {
                                tracing::error!(
                                    player_id = %pid,
                                    "delta encode failure — frame dropped, connection closed"
                                );
                            }
                        }
                        let mut alive = Vec::new();
                        for player in players {
                            let pid = player.id().cloned().unwrap_or_default();
                            if encode_failed.contains(pid.as_str()) {
                                // M3/W7: encode failed this tick — connection closed
                                // above; drop the player from the tick loop.
                                continue;
                            }
                            if let Some(pos) = outbox.iter().position(|(p, _)| p.as_str() == pid) {
                                let (_, bytes) = outbox.swap_remove(pos);
                                // I7: tick deltas go through the bounded keep-latest
                                // ring; send failure (channel closed) still drops the player.
                                if player.tx().send_delta(bytes).await.is_ok() {
                                    alive.push(player);
                                }
                                // else: connection died -> player dropped
                            } else {
                                // Nothing changed for this player; keep the connection alive.
                                alive.push(player);
                            }
                        }
                        players = alive;

                        // End the game when the last human player leaves, even if
                        // LLM agents or spectators remain. Spectators do not count
                        // as active presence, so they alone do not keep a game alive.
                        let human_count = players
                            .iter()
                            .filter(|p| matches!(p, PlayerConnection::Player { .. }))
                            .count();
                        if human_count == 0 {
                            let winner_id = game.determine_winner();
                            let player_ids: Vec<&str> = game.players().keys().map(|s| s.as_str()).collect();
                            persist_game_end(
                                &storage_clone,
                                &room_id,
                                game_start_time,
                                &settings_json,
                                &[winner_id.as_deref().unwrap_or("")],
                                &player_ids,
                                &game,
                            );
                            let msg = ServerMessage::GameOver {
                                winner: "Abandoned".to_string(),
                                game_id: room_id.clone(),
                                message: "Game abandoned — no human players remaining".to_string(),
                            };
                            let encoded = Bytes::from(encode_message(&msg));
                            for player in &players {
                                let _ = player.tx().send_reliable(encoded.clone()).await;
                            }
                            {
                                let server = server_state.lock().await;
                                let mut games = server.games.lock().await;
                                games.remove(&room_id);
                            }
                            for handle in spawned_handles {
                                handle.abort();
                            }
                            break;
                        }

                        if players.is_empty() {
                            let winner_id = game.determine_winner();
                            let player_ids: Vec<&str> = game.players().keys().map(|s| s.as_str()).collect();
                            persist_game_end(
                                &storage_clone,
                                &room_id,
                                game_start_time,
                                &settings_json,
                                &[winner_id.as_deref().unwrap_or("")],
                                &player_ids,
                                &game,
                            );
                            let server = server_state.lock().await;
                            let mut games = server.games.lock().await;
                            games.remove(&room_id);
                            for handle in spawned_handles {
                                handle.abort();
                            }
                            break;
                        }
                    }
                    // Blocking command recv for non-tick-driven commands (player connections)
                    cmd = cmd_rx.recv() => {
                        match cmd {
                            Some(Command::MoveTroop { unit_id, owner_id, path }) => {
                                // Ownership is enforced inside game.move_troops (single check).
                                game.move_troops(&unit_id, &owner_id, path);
                            }
                            Some(Command::RecruitUnit(city_id, unit_type_name, owner)) => {
                                if let Some(unit_type) = game.settings().unit_types.iter().find(|u| u.name == unit_type_name).cloned() {
                                    let _ = game.recruit_unit(&city_id, &unit_type, &owner);
                                }
                            }
                            Some(Command::ToggleHealing { unit_id, owner_id, enabled }) => {
                                game.toggle_healing(&unit_id, &owner_id, enabled);
                            }
                            Some(Command::ToggleResupply { unit_id, owner_id, enabled }) => {
                                game.toggle_resupply(&unit_id, &owner_id, enabled);
                            }
                            Some(Command::UpgradeCity(city_id, owner)) => {
                                let _ = game.upgrade_city(&city_id, &owner);
                            }
                            Some(Command::ToggleAdvisor(pid, enabled)) => {
                                if let Some(advisor) = advisors.get_mut(&pid) {
                                    advisor.set_enabled(enabled);
                                }
                            }
                            Some(Command::AddSpectator(sender)) => {
                                // Sec L2: per-game spectator cap (the tick
                                // task is the sole owner of `players`).
                                add_spectator_capped(spectator_cap, &mut players, &game, sender)
                                    .await;
                            }
                            Some(Command::EmbarkUnit(unit_id, pid)) => {
                                game.embark_unit(&unit_id, &pid);
                            }
                            Some(Command::DisembarkUnit(unit_id, pid)) => {
                                game.disembark_unit(&unit_id, &pid);
                            }
                            None => break,
                        }
                    }
                }
            }

            // Replay: terminal marker (best-effort — the game is over either
            // way; the loop has already aborted all spawned tasks).
            if let Some(rec) = replay_recorder.as_mut() {
                let end_time = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|d| d.as_secs())
                    .unwrap_or(0);
                let _ = writeln!(rec, "{{\"event\":\"game_end\",\"time\":{}}}", end_time);
                let _ = rec.flush();
            }
        });

        GameRoom {
            game_id,
            command_tx: cmd_tx,
            storage,
            spectator_vision,
            stats: game_stats_room,
        }
    }
}

impl From<&crate::game::economy::City> for CityState {
    fn from(city: &crate::game::economy::City) -> Self {
        CityState {
            id: city.id.clone(),
            position: city.position,
            owner: city.owner.clone(),
            manpower: 0, // Global pool, not per-city
            level: city.level,
        }
    }
}

/// Determine the winner of the game — the last player with units/cities
#[allow(dead_code)]
impl Game {
    fn full_state(&self) -> GameStateMessage {
        // Use the first player's vision/border for now (backward compatibility)
        self.full_state_for(self.players().keys().next().map(|s| s.as_str()))
    }

    fn full_state_for(&self, player_id: Option<&str>) -> GameStateMessage {
        let (vision_grid, border_grid) = player_id
            .map(|pid| (self.vision_grid_2d(pid), self.border_grid_2d(pid)))
            .unwrap_or((None, None));
        self.build_full_state(vision_grid, border_grid, player_id)
    }

    fn full_state_with_player_id(&self, player_id: &str) -> GameStateMessage {
        let (vision_grid, border_grid) = (
            self.vision_grid_2d(player_id),
            self.border_grid_2d(player_id),
        );
        self.build_full_state(vision_grid, border_grid, Some(player_id))
    }

    fn full_state_with_vision(
        &self,
        vision_grid: Option<Vec<Vec<bool>>>,
        border_grid: Option<Vec<Vec<u32>>>,
    ) -> GameStateMessage {
        self.build_full_state(vision_grid, border_grid, None)
    }

    fn build_full_state(
        &self,
        vision_grid: Option<Vec<Vec<bool>>>,
        border_grid: Option<Vec<Vec<u32>>>,
        player_id: Option<&str>,
    ) -> GameStateMessage {
        // Perf #5: `wire_unit_states` already builds a fresh Vec — the old
        // `.to_vec()` here made a gratuitous second full copy (every unit's
        // strings + equipment Vecs) on every full-state build.
        let units: Vec<UnitState> = wire_unit_states(self);
        let cities: Vec<CityState> = self.query_cities().iter().map(|c| c.into()).collect();
        let terrain = self.terrain_grid_2d();
        // Per-player funds/units for the viewing player, matching the delta system.
        // For a supervisor view (player_id == None) fall back to the aggregate totals.
        let player_funds: u32 = match player_id {
            Some(pid) => self.player_funds(pid),
            None => self.all_player_funds().values().sum(),
        };
        let units_owned: u32 = match player_id {
            Some(pid) => units.iter().filter(|u| u.owner == pid).count() as u32,
            None => units.len() as u32,
        };
        GameStateMessage {
            vision_grid,
            border_grid,
            terrain,
            units,
            cities,
            player: PlayerState {
                funds: player_funds,
                units_owned,
                player_id: player_id.unwrap_or("").to_string(),
            },
        }
    }
}

/// Serve the index.html page
pub(crate) async fn index_handler() -> Html<String> {
    let index_html = FRONTEND
        .get_file("index.html")
        .unwrap()
        .contents_utf8()
        .unwrap();
    Html(index_html.to_string())
}

/// Spectator watch page — auto-connects to the current game as a spectator
pub(crate) async fn watch_handler() -> Html<String> {
    Html(r#"
<!DOCTYPE html>
<html>
<head>
  <meta charset="UTF-8">
  <title>War of Dots — Spectator</title>
  <style>
    body { background: #1a1a2e; font-family: sans-serif; color: #fff; text-align: center; }
    .panel { background: rgba(0,0,0,0.8); padding: 32px; border-radius: 8px; margin: 50px auto; max-width: 500px; }
    button { padding: 12px 24px; font-size: 16px; margin: 8px; cursor: pointer; }
  </style>
</head>
<body>
  <div class="panel">
    <h1>📼 Spectator Mode</h1>
    <p>Connect to watch the current game as a spectator.</p>
    <p>Requires the game to have <strong>spectator_vision</strong> enabled.</p>
    <button id="connect-btn">Connect as Spectator</button>
    <br><br>
    <a href="/">Return to Lobby</a>
  </div>
  <script>
    const connectBtn = document.getElementById('connect-btn');
    connectBtn.addEventListener('click', () => {
      const protocol = location.protocol === 'https:' ? 'wss:' : 'ws:';
      const ws = new WebSocket(`${protocol}//${location.host}/ws/spectator`);
      ws.onopen = () => {
        // Redirect to replay viewer with WebSocket connection (complex)
        // For simplicity, just show a message
        alert('Connected as spectator. The game stream is being sent.');
        // In a full implementation, you'd display the game here using the renderer.
        // For now, we just redirect to the game page and let the spectator overlay show.
      };
      ws.onclose = () => alert('Disconnected.');
    });
  </script>
</body>
</html>
"#.to_string())
}

pub(crate) async fn serve_static(
    req: axum::http::Request<axum::body::Body>,
) -> Result<axum::response::Response, axum::http::StatusCode> {
    let path_str = req.uri().path().trim_start_matches('/').to_string();
    if path_str.is_empty() {
        return Err(axum::http::StatusCode::NOT_FOUND);
    }
    let file = match FRONTEND.get_file(&path_str) {
        Some(file) => file,
        None => return Err(axum::http::StatusCode::NOT_FOUND),
    };
    let content_type = match file.path().extension().and_then(|e| e.to_str()) {
        Some("js") => "application/javascript",
        Some("mjs") => "application/javascript",
        Some("html") => "text/html; charset=utf-8",
        Some("css") => "text/css; charset=utf-8",
        Some("json") => "application/json",
        Some("map") => "application/json",
        Some("wasm") => "application/wasm",
        Some("png") => "image/png",
        Some("svg") => "image/svg+xml",
        Some("ico") => "image/x-icon",
        Some("woff") => "font/woff",
        Some("woff2") => "font/woff2",
        Some("ttf") => "font/ttf",
        _ => "application/octet-stream",
    };
    let content = file.contents().to_vec();
    Ok(axum::response::Response::builder()
        .header(axum::http::header::CONTENT_TYPE, content_type)
        .body(axum::body::Body::from(content))
        .unwrap())
}

/// List all available maps
pub(crate) async fn list_maps_handler() -> axum::Json<Vec<String>> {
    axum::Json(map_registry::list_maps())
}

/// Serialize the in-play settings + map name for the `game_metadata.settings`
/// column (persisted by `record_game_end` at game end). `GameSettings` is
/// not `Serialize` (nor is `UnitType`), so project the fields the replay
/// list UI cares about here rather than deriving across modules.
fn game_metadata_settings_json(game: &Game, map_name: &str) -> String {
    let s = game.settings();
    serde_json::json!({
        "map": map_name,
        "settings": {
            "starting_funds": s.starting_funds,
            "initial_manpower": s.initial_manpower,
            "funds_per_city": s.funds_per_city,
            "recruitment_rate": s.recruitment_rate,
            "game_speed": s.game_speed,
            "use_standard_ruleset": s.use_standard_ruleset,
            "unit_types": s.unit_types.iter().map(|u| u.name.clone()).collect::<Vec<_>>(),
            "water_speed": s.water_speed,
        },
    })
    .to_string()
}

/// Persist the game-end record (wall-clock start/end + the in-play settings
/// JSON) when a game terminates. All four end paths — draw, last player
/// standing, abandoned (no humans left), and room empty — share this.
fn persist_game_end(
    storage: &crate::storage::Storage,
    room_id: &str,
    game_start_time: u64,
    settings_json: &str,
    winners: &[&str],
    player_ids: &[&str],
    game: &Game,
) {
    let end_time = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    storage.record_game_end(
        room_id,
        game_start_time,
        end_time,
        settings_json,
        winners,
        player_ids,
        game.player_stats(),
        game.settings().use_standard_ruleset,
    );
}

/// List all available replays
pub(crate) async fn list_replays(
    State(state): State<Arc<Mutex<GameServer>>>,
) -> axum::Json<Vec<ReplayInfo>> {
    let server = state.lock().await;
    let storage = server.storage.clone();
    let db = match storage.db.lock() {
        Ok(d) => d,
        Err(_) => return axum::Json(Vec::new()),
    };
    let mut stmt = match db.prepare(
        "SELECT id, players, winner, start_time, end_time FROM game_metadata ORDER BY end_time DESC"
    ) {
        Ok(s) => s,
        Err(_) => return axum::Json(Vec::new()),
    };
    let mut replays = Vec::new();
    while let Ok(sqlite::State::Row) = stmt.next() {
        let id: String = stmt.read(0).unwrap_or_default();
        let players_str: String = stmt.read(1).unwrap_or_default();
        let winner: String = stmt.read(2).unwrap_or_default();
        let start_time: i64 = stmt.read(3).unwrap_or(0);
        let end_time: i64 = stmt.read(4).unwrap_or(0);
        let players: Vec<String> = serde_json::from_str(&players_str).unwrap_or_default();
        let duration = if start_time < end_time {
            end_time - start_time
        } else {
            0
        };
        replays.push(ReplayInfo {
            game_id: id,
            winner,
            players,
            start_time,
            end_time,
            duration,
        });
    }
    axum::Json(replays)
}

/// Serve a recorded replay JSONL from the runtime replay directory.
///
/// The embedded `FRONTEND` static fallback (include_dir) only knows about
/// files that existed at compile time, so runtime-recorded games need this
/// dedicated route. The frontend's `ReplayPlayer` fetches `replays/{file}`
/// and this is what resolves it for live-recorded games.
///
/// NOTE: this axum/matchit version (0.7.9 / 0.7.3) uses the `:param` path
/// syntax — the `{param}` form does not match. See
/// `path_param_routes_use_colon_syntax` for the regression guard.
///
/// Security: the shared basename-only rule
/// (`crate::server::handlers::replay::is_safe_replay_file_name`) — no path
/// separators, no `..` sequences, no null bytes, `.jsonl` suffix — otherwise
/// the route could read arbitrary files (`/replays/../../config.toml`).
pub(crate) async fn serve_replay(
    State(state): State<Arc<Mutex<GameServer>>>,
    axum::extract::Path(name): axum::extract::Path<String>,
) -> Result<axum::response::Response, axum::http::StatusCode> {
    if !crate::server::handlers::replay::is_safe_replay_file_name(&name) {
        return Err(axum::http::StatusCode::NOT_FOUND);
    }
    let dir = state.lock().await.config.storage.replay_path.clone();
    let content = std::fs::read(std::path::Path::new(&dir).join(&name))
        .map_err(|_| axum::http::StatusCode::NOT_FOUND)?;
    Ok(axum::response::Response::builder()
        .header(axum::http::header::CONTENT_TYPE, "application/x-ndjson")
        .body(axum::body::Body::from(content))
        .unwrap())
}

#[derive(serde::Serialize)]
pub(crate) struct ReplayInfo {
    game_id: String,
    winner: String,
    players: Vec<String>,
    start_time: i64,
    end_time: i64,
    duration: i64,
}

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

// WebSocket handler for game state
pub(crate) async fn ws_handler(
    ws: WebSocketUpgrade,
    server_state: axum::extract::State<Arc<Mutex<GameServer>>>,
) -> impl axum::response::IntoResponse {
    ws.on_upgrade(move |socket| handle_ws_game(socket, server_state.0))
}

/// Sec L2: route-level cap gate for the self-terminating read-only
/// `/ws/replay` stream. Replay connections are NOT counted in
/// `ws_connections` (they stream a recording once and close), but no new
/// WebSocket connection of any kind is accepted while the cap is reached —
/// so while `/ws` + `/ws/spectator` are at capacity this handler rejects
/// with HTTP 503 (no upgrade) instead of opening yet another socket.
pub(crate) async fn gated_replay_ws_handler(
    ws: WebSocketUpgrade,
    State(server_state): State<Arc<Mutex<GameServer>>>,
    params: Query<std::collections::HashMap<String, String>>,
) -> axum::response::Response {
    let at_capacity = {
        let server = server_state.lock().await;
        let cap = server.config.network.max_ws_connections.max(1);
        server.ws_connections.load(Ordering::Relaxed) >= cap
    };
    if at_capacity {
        return (
            axum::http::StatusCode::SERVICE_UNAVAILABLE,
            "Server at maximum connections — try again later",
        )
            .into_response();
    }
    crate::server::handlers::replay::replay_ws_handler(ws, State(server_state), params)
        .await
        .into_response()
}

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

/// I7: build the lobby-facing sender from a connection sender. TCP senders
/// share the same channel + keep-latest ring; a WebSocket closure gets a
/// bridged channel with its own ring; `None` gets a throwaway channel whose
/// receiver is dropped (sends fail and are ignored at every call site).
fn lobby_bridge_sender(player_sender: &PlayerSender) -> PlayerSender {
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
                                message:
                                    "Already in a lobby — start it or leave before creating another"
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
                            let players: Vec<super::protocol::LobbyPlayer> = lobby
                                .players
                                .iter()
                                .map(|p| super::protocol::LobbyPlayer {
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
                        (String, Vec<super::protocol::LobbyPlayer>, LobbySettings),
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
                                let players: Vec<super::protocol::LobbyPlayer> = lobby
                                    .players
                                    .iter()
                                    .map(|p| super::protocol::LobbyPlayer {
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
                                player_map.insert(
                                    *conn_id,
                                    (player_id.clone(), lobby_code_inner.clone()),
                                );
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
                                let encoded = Bytes::from(encode_message_result(
                                    &ServerMessage::LobbyUpdate {
                                        players: players.clone(),
                                        settings: settings.clone(),
                                    },
                                )?);
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
                                message: "Only the lobby creator can configure LLM slots"
                                    .to_string(),
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
                                let players: Vec<super::protocol::LobbyPlayer> = lobby
                                    .players
                                    .iter()
                                    .map(|p| super::protocol::LobbyPlayer {
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
                                message: "Only the lobby creator can configure LLM slots"
                                    .to_string(),
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
                            let (api_key, base_url) =
                                match server.config.llm.providers.iter().find(|c| {
                                    c.name == provider_name || c.provider == provider_name
                                }) {
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
                                    lobby_manager.set_slot_llm(
                                        &lobby_code,
                                        *slot,
                                        provider_name.clone(),
                                    )
                                };
                            }
                            // Broadcast LobbyUpdate
                            let (players, settings) = {
                                let server = server_state.lock().await;
                                let lobby_manager = server.lobby_manager.lock().await;
                                let lobby = lobby_manager.get_lobby(&lobby_code).unwrap();
                                let players: Vec<super::protocol::LobbyPlayer> = lobby
                                    .players
                                    .iter()
                                    .map(|p| super::protocol::LobbyPlayer {
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
                                message: "Only the lobby creator can configure LLM slots"
                                    .to_string(),
                            };
                            let encoded = encode_message_result(&resp)?;
                            let _ = player_sender.send_reliable(encoded.into()).await;
                            return Ok(());
                        }
                        let result = {
                            let server = server_state.lock().await;
                            let mut lobby_manager = server.lobby_manager.lock().await;
                            lobby_manager.add_llm_player(
                                &lobby_code,
                                name.clone(),
                                llm_provider.clone(),
                            )
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
                                let players: Vec<super::protocol::LobbyPlayer> = lobby
                                    .players
                                    .iter()
                                    .map(|p| super::protocol::LobbyPlayer {
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
                            if !lobby.settings.use_standard_ruleset
                                || !lobby.settings.custom_unit_types.is_empty()
                            {
                                if let Some(err) = validate_custom_settings(&lobby.settings) {
                                    let resp = ServerMessage::Error { message: err };
                                    let encoded = encode_message_result(&resp)?;
                                    let _ = player_sender.send_reliable(encoded.into()).await;
                                    return Ok(());
                                }
                            }

                            let unit_types = if lobby.settings.use_standard_ruleset
                                && server.config.standard_ruleset.enabled
                            {
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
                            let map =
                                map_registry::get(&settings.map).unwrap_or_else(load_default_map);
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
                                    let (model, api_key, base_url) = if let Some(config) =
                                        lobby.llm_agent_configs.get(&(i as u32))
                                    {
                                        (
                                            config.model.clone(),
                                            config.api_key.clone(),
                                            config.base_url.clone(),
                                        )
                                    } else {
                                        // Fall back to first matching global config provider
                                        let provider_config =
                                            server.config.llm.providers.iter().find(|c| {
                                                c.provider == llm_provider || c.name == llm_provider
                                            });
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
                            let name_json =
                                serde_json::to_string(&name).unwrap_or(format!("\"{}\"", name));
                            let mut stmt = match db.prepare(
                                "INSERT OR REPLACE INTO player_names (player_id, name) VALUES (?, ?)",
                            ) {
                                Ok(s) => s,
                                Err(e) => {
                                    tracing::error!(%e, "Failed to prepare player_names insert");
                                    continue;
                                }
                            };
                            let params: Vec<Value> =
                                vec![Value::String(player_id), Value::String(name_json)];
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
                            let mut stmt = match db.prepare(
                                "INSERT OR REPLACE INTO player_names (player_id, name) VALUES (?, ?)",
                            ) {
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
                            if let Some(slot) =
                                lobby_players.iter().position(|(id, _)| id == &entry.0)
                            {
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
                            Some(lobby) => {
                                lobby.players.iter().map(|p| p.connection.clone()).collect()
                            }
                            None => Vec::new(),
                        }
                    };
                    let all_players: Vec<super::protocol::LobbyPlayer> = {
                        let server = server_state.lock().await;
                        let lobby_manager = server.lobby_manager.lock().await;
                        match lobby_manager.get_lobby(&lobby_code) {
                            Some(lobby) => lobby
                                .players
                                .iter()
                                .map(|p| super::protocol::LobbyPlayer {
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
                }
            }
        }
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
            let entries: Vec<super::protocol::LeaderboardEntry> = leaderboards
                .into_iter()
                .map(|s| super::protocol::LeaderboardEntry {
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
    }
    Ok(())
}

/// Parse equipment string like "rifles:100:0.5" into an Equipment struct
/// Supports legacy format "name:count" and new format "name:count:cost_per_item"
#[allow(dead_code)]
fn parse_equipment(s: &str) -> crate::game::unit::Equipment {
    let parts: Vec<&str> = s.split(':').collect();
    let name = parts[0].to_string();
    let count: u32 = parts.get(1).and_then(|c| c.parse().ok()).unwrap_or(100);
    let cost_per_item: f32 = parts.get(2).and_then(|c| c.parse().ok()).unwrap_or(0.0);
    crate::game::unit::Equipment {
        name,
        count,
        max_count: count,
        cost_per_item,
    }
}

/// Convert standard ruleset config unit types to game unit types
fn convert_unit_types(unit_types: &[ConfigUnitType]) -> Vec<UnitType> {
    unit_types
        .iter()
        .map(|ut| {
            let equipment: Vec<Equipment> = ut
                .equipment
                .iter()
                .map(|s| {
                    // Parse "name:count:cost_per_item" format
                    let ecfg = EquipmentConfig::from_str(s);
                    Equipment {
                        name: ecfg.name,
                        count: ecfg.count,
                        max_count: ecfg.count,
                        cost_per_item: ecfg.cost_per_item,
                    }
                })
                .collect();
            // Derive recruitment cost from equipment if not specified
            let recruitment_cost = ut.recruitment_cost.unwrap_or_else(|| {
                // Base manpower cost (1 fund per 10 men) + equipment costs
                let base_manpower_cost = ut.men / 10;
                let equipment_cost: u32 = ut
                    .equipment
                    .iter()
                    .map(|s| {
                        let ecfg = EquipmentConfig::from_str(s);
                        (ecfg.count as f32 * ecfg.cost_per_item).ceil() as u32
                    })
                    .sum();
                base_manpower_cost + equipment_cost
            });
            UnitType {
                name: ut.name.clone(),
                symbol: ut.symbol.clone(),
                men: ut.men,
                equipment,
                soft_attack: ut.soft_attack,
                soft_defense: ut.soft_defense,
                hard_attack: ut.hard_attack,
                hard_defense: ut.hard_defense,
                speed: ut.speed,
                max_organisation: ut.max_organisation,
                reinforcement_rate: ut.reinforcement_rate,
                base_attrition: ut.base_attrition,
                fighting_attrition: ut.fighting_attrition,
                recruitment_cost,
                supply_buffer_percent: ut.supply_buffer_percent,
                supply_rate: 5,
                max_supply: crate::config::derive_max_supply(ut.men),
            }
        })
        .collect()
}

/// Convert custom unit types from lobby settings to game unit types
fn convert_custom_unit_types(unit_types: &[super::protocol::CustomUnitType]) -> Vec<UnitType> {
    unit_types
        .iter()
        .map(|ut| UnitType {
            name: ut.name.clone(),
            symbol: ut.symbol.clone(),
            men: ut.men,
            equipment: ut
                .equipment
                .iter()
                .map(|(name, count)| Equipment {
                    name: name.clone(),
                    count: *count,
                    max_count: *count,
                    cost_per_item: 0.0, // Custom types don't have per-item cost
                })
                .collect(),
            soft_attack: ut.soft_attack,
            soft_defense: ut.soft_defense,
            hard_attack: ut.hard_attack,
            hard_defense: ut.hard_defense,
            speed: ut.speed,
            max_organisation: ut.max_organisation,
            reinforcement_rate: ut.reinforcement_rate,
            base_attrition: 0.5,
            fighting_attrition: 1.0,
            recruitment_cost: ut.recruitment_cost,
            supply_buffer_percent: 10, // Default
            supply_rate: 5,
            max_supply: ut.men * 2, // men * CARRY_WEIGHT
        })
        .collect()
}

fn build_default_unit_types() -> Vec<UnitType> {
    crate::game::economy::default_unit_types()
}

/// G-3: Validate custom ruleset settings to prevent unreasonable values.
/// Returns an error message if validation fails, None if valid.
fn validate_custom_settings(settings: &LobbySettings) -> Option<String> {
    if settings.starting_funds == 0 {
        return Some("starting_funds must be > 0".into());
    }
    if settings.starting_funds > 100_000 {
        return Some("starting_funds must be <= 100000".into());
    }
    if settings.funds_per_city < 0.0 || settings.funds_per_city > 10000.0 {
        return Some("funds_per_city must be in range 0-10000".into());
    }
    if settings.game_speed < 0.1 || settings.game_speed > 10.0 {
        return Some("game_speed must be in range 0.1-10.0".into());
    }
    if settings.max_players == 0 || settings.max_players > 16 {
        return Some("max_players must be in range 1-16".into());
    }
    if settings.recruitment_rate < 0.0 || settings.recruitment_rate > 100.0 {
        return Some("recruitment_rate must be in range 0-100".into());
    }
    // NaN fails both range comparisons, so finiteness is checked explicitly.
    if !settings.water_speed.is_finite()
        || settings.water_speed < 0.1
        || settings.water_speed > 10.0
    {
        return Some("water_speed must be finite and in range 0.1-10.0".into());
    }
    // Validate custom unit types (fully client-controlled). Degenerate types
    // are game-breaking or DoS vectors: speed 0 yields 0/0 = NaN on the first
    // movement tick, and men 0 / recruitment_cost 0 yields an unbounded free
    // army. `men` is capped because it is the army-size field.
    for (i, ut) in settings.custom_unit_types.iter().enumerate() {
        if ut.men == 0 {
            return Some(format!(
                "custom unit type {i} ({}): men must be >= 1",
                ut.name
            ));
        }
        if ut.men > 100_000 {
            return Some(format!(
                "custom unit type {i} ({}): men must be <= 100000",
                ut.name
            ));
        }
        if ut.recruitment_cost == 0 {
            return Some(format!(
                "custom unit type {i} ({}): recruitment_cost must be > 0",
                ut.name
            ));
        }
        if !ut.speed.is_finite() || ut.speed <= 0.0 {
            return Some(format!(
                "custom unit type {i} ({}): speed must be finite and > 0",
                ut.name
            ));
        }
        if ut.speed > 100.0 {
            return Some(format!(
                "custom unit type {i} ({}): speed must be <= 100",
                ut.name
            ));
        }
        // Item 7b: finiteness + sane bounds for the remaining client floats.
        // NaN/inf here would poison combat resolution and organisation/regen
        // math for the entire game (a single NaN unit corrupts every future
        // battle it fights). Negative combat stats and a non-positive
        // max_organisation are degenerate; the 1e6 cap keeps squared combat
        // products far below f32 overflow.
        for (label, value) in [
            ("soft_attack", ut.soft_attack),
            ("soft_defense", ut.soft_defense),
            ("hard_attack", ut.hard_attack),
            ("hard_defense", ut.hard_defense),
        ] {
            if !value.is_finite() || !(0.0..=1_000_000.0).contains(&value) {
                return Some(format!(
                    "custom unit type {i} ({}): {label} must be finite and in range 0-1000000",
                    ut.name
                ));
            }
        }
        if !ut.max_organisation.is_finite()
            || ut.max_organisation <= 0.0
            || ut.max_organisation > 1_000_000.0
        {
            return Some(format!(
                "custom unit type {i} ({}): max_organisation must be finite and in range (0, 1000000]",
                ut.name
            ));
        }
        if !ut.reinforcement_rate.is_finite()
            || ut.reinforcement_rate < 0.0
            || ut.reinforcement_rate > 1_000_000.0
        {
            return Some(format!(
                "custom unit type {i} ({}): reinforcement_rate must be finite and in range 0-1000000",
                ut.name
            ));
        }
    }
    // Validate city upgrade settings
    let cs = &settings.city_upgrade_settings;
    if cs.max_level > 50 {
        return Some("city max_level must be <= 50".into());
    }
    if cs.base_upgrade_cost > 100_000 {
        return Some("base_upgrade_cost must be <= 100000".into());
    }
    None
}

#[cfg(test)]
mod validate_custom_settings_tests {
    use super::validate_custom_settings;
    use crate::config::CityUpgradeSettings;
    use crate::server::protocol::{CustomUnitType, LobbySettings};

    /// Baseline custom ruleset that passes every lobby-level check, so each
    /// test can isolate one unit-type (or water_speed) violation.
    fn base_settings() -> LobbySettings {
        // The type default (max_level 100) would itself trip the <= 50 rule,
        // so override it inline.
        let city = CityUpgradeSettings {
            max_level: 10,
            ..CityUpgradeSettings::default()
        };
        LobbySettings {
            map: "default".to_string(),
            starting_funds: 1000,
            initial_manpower: 10_000,
            funds_per_city: 1.0,
            game_speed: 1.0,
            use_standard_ruleset: false,
            llm_thinking_delay: 3.0,
            max_players: 4,
            custom_unit_types: vec![],
            spectator_vision: false,
            recruitment_rate: 10.0,
            water_speed: 1.2,
            city_upgrade_settings: city,
        }
    }

    fn unit_type(speed: f32, men: u32, recruitment_cost: u32) -> CustomUnitType {
        CustomUnitType {
            name: "infantry".to_string(),
            symbol: "I".to_string(),
            men,
            equipment: vec![],
            soft_attack: 5.0,
            soft_defense: 5.0,
            hard_attack: 3.0,
            hard_defense: 2.0,
            speed,
            max_organisation: 100.0,
            reinforcement_rate: 1.0,
            recruitment_cost,
        }
    }

    #[test]
    fn valid_custom_unit_type_passes() {
        let mut s = base_settings();
        s.custom_unit_types = vec![unit_type(1.0, 1000, 20)];
        assert_eq!(validate_custom_settings(&s), None);
    }

    #[test]
    fn zero_or_non_finite_speed_is_rejected() {
        for speed in [0.0, f32::NAN, f32::INFINITY] {
            let mut s = base_settings();
            s.custom_unit_types = vec![unit_type(speed, 1000, 20)];
            let err = validate_custom_settings(&s)
                .unwrap_or_else(|| panic!("speed {speed} must be rejected"));
            assert!(err.contains("speed"), "unexpected error: {err}");
        }
    }

    #[test]
    fn zero_men_is_rejected() {
        let mut s = base_settings();
        s.custom_unit_types = vec![unit_type(1.0, 0, 20)];
        let err = validate_custom_settings(&s).expect("men 0 must be rejected");
        assert!(err.contains("men"), "unexpected error: {err}");
    }

    #[test]
    fn oversized_men_is_rejected() {
        let mut s = base_settings();
        s.custom_unit_types = vec![unit_type(1.0, 100_001, 20)];
        let err = validate_custom_settings(&s).expect("men > 100000 must be rejected");
        assert!(err.contains("men"), "unexpected error: {err}");
    }

    #[test]
    fn free_recruitment_is_rejected() {
        // The reported DoS: recruitment_cost 0 + men 0 (unbounded free army).
        // Both degenerate fields are rejected individually.
        let mut s = base_settings();
        s.custom_unit_types = vec![unit_type(1.0, 0, 0)];
        let err = validate_custom_settings(&s).expect("men 0 + cost 0 must be rejected");
        assert!(err.contains("men"), "unexpected error: {err}");

        s.custom_unit_types = vec![unit_type(1.0, 1000, 0)];
        let err = validate_custom_settings(&s).expect("cost 0 with men > 0 must be rejected");
        assert!(err.contains("recruitment_cost"), "unexpected error: {err}");
    }

    #[test]
    fn nan_water_speed_is_rejected() {
        let mut s = base_settings();
        s.water_speed = f32::NAN; // NaN passes a bare range check
        let err = validate_custom_settings(&s).expect("NaN water_speed must be rejected");
        assert!(err.contains("water_speed"), "unexpected error: {err}");
    }

    // ------------------------------------------------------------------
    // Item 7b: finiteness/bounds for the combat + regen floats
    // ------------------------------------------------------------------

    fn unit_type_f(
        soft_attack: f32,
        soft_defense: f32,
        hard_attack: f32,
        hard_defense: f32,
        max_organisation: f32,
        reinforcement_rate: f32,
    ) -> CustomUnitType {
        CustomUnitType {
            name: "infantry".to_string(),
            symbol: "I".to_string(),
            men: 1000,
            equipment: vec![],
            soft_attack,
            soft_defense,
            hard_attack,
            hard_defense,
            speed: 1.0,
            max_organisation,
            reinforcement_rate,
            recruitment_cost: 20,
        }
    }

    #[test]
    fn nan_combat_stats_are_rejected() {
        // A single NaN unit would poison every future battle it fights.
        for (label, ut) in [
            (
                "soft_attack",
                unit_type_f(f32::NAN, 5.0, 3.0, 2.0, 100.0, 1.0),
            ),
            (
                "soft_defense",
                unit_type_f(5.0, f32::NAN, 3.0, 2.0, 100.0, 1.0),
            ),
            (
                "hard_attack",
                unit_type_f(5.0, 5.0, f32::NAN, 2.0, 100.0, 1.0),
            ),
            (
                "hard_defense",
                unit_type_f(5.0, 5.0, 3.0, f32::NAN, 100.0, 1.0),
            ),
        ] {
            let mut s = base_settings();
            s.custom_unit_types = vec![ut];
            let Some(err) = validate_custom_settings(&s) else {
                panic!("NaN {label} must be rejected");
            };
            assert!(err.contains(label), "unexpected error for {label}: {err}");
        }
    }

    #[test]
    fn inf_combat_stats_are_rejected() {
        for (label, ut) in [
            (
                "soft_attack",
                unit_type_f(f32::INFINITY, 5.0, 3.0, 2.0, 100.0, 1.0),
            ),
            (
                "hard_defense",
                unit_type_f(5.0, 5.0, 3.0, f32::INFINITY, 100.0, 1.0),
            ),
        ] {
            let mut s = base_settings();
            s.custom_unit_types = vec![ut];
            let Some(err) = validate_custom_settings(&s) else {
                panic!("inf {label} must be rejected");
            };
            assert!(err.contains(label), "unexpected error for {label}: {err}");
        }
    }

    #[test]
    fn negative_combat_stats_are_rejected() {
        for (label, ut) in [
            ("soft_attack", unit_type_f(-1.0, 5.0, 3.0, 2.0, 100.0, 1.0)),
            ("soft_defense", unit_type_f(5.0, -1.0, 3.0, 2.0, 100.0, 1.0)),
            ("hard_attack", unit_type_f(5.0, 5.0, -1.0, 2.0, 100.0, 1.0)),
            ("hard_defense", unit_type_f(5.0, 5.0, 3.0, -1.0, 100.0, 1.0)),
        ] {
            let mut s = base_settings();
            s.custom_unit_types = vec![ut];
            let Some(err) = validate_custom_settings(&s) else {
                panic!("negative {label} must be rejected");
            };
            assert!(err.contains(label), "unexpected error for {label}: {err}");
        }
    }

    #[test]
    fn degenerate_organisation_and_regen_are_rejected() {
        // max_organisation <= 0: a unit with no organisation bar (broken HP).
        // NaN/inf regen: organisation math corruption / unbounded healing.
        let cases = [
            (
                "max_organisation zero",
                unit_type_f(5.0, 5.0, 3.0, 2.0, 0.0, 1.0),
            ),
            (
                "max_organisation NaN",
                unit_type_f(5.0, 5.0, 3.0, 2.0, f32::NAN, 1.0),
            ),
            (
                "reinforcement_rate negative",
                unit_type_f(5.0, 5.0, 3.0, 2.0, 100.0, -1.0),
            ),
            (
                "reinforcement_rate NaN",
                unit_type_f(5.0, 5.0, 3.0, 2.0, 100.0, f32::NAN),
            ),
            (
                "soft_attack over cap",
                unit_type_f(1_000_001.0, 5.0, 3.0, 2.0, 100.0, 1.0),
            ),
        ];
        for (label, ut) in cases {
            let mut s = base_settings();
            s.custom_unit_types = vec![ut];
            assert!(
                validate_custom_settings(&s).is_some(),
                "{label} must be rejected"
            );
        }
    }

    #[test]
    fn finite_bounded_combat_stats_pass() {
        // Boundary values the validator must ACCEPT (no over-rejection):
        // zero is legal for combat stats and regen, the 1e6 caps are
        // inclusive.
        let mut s = base_settings();
        s.custom_unit_types = vec![unit_type_f(
            0.0,         // soft_attack
            0.0,         // soft_defense
            0.0,         // hard_attack
            0.0,         // hard_defense
            1_000_000.0, // max_organisation (inclusive upper bound)
            0.0,         // reinforcement_rate
        )];
        assert_eq!(validate_custom_settings(&s), None);
    }

    // ------------------------------------------------------------------
    // Item 7a: the validator is flag-agnostic — custom types are validated
    // when PRESENT, even with use_standard_ruleset = true. The StartGame
    // call site relies on exactly this: it invokes validation whenever
    // custom_unit_types is non-empty, regardless of the flag.
    // ------------------------------------------------------------------

    #[test]
    fn custom_types_validated_even_with_standard_flag() {
        let mut s = base_settings();
        s.use_standard_ruleset = true; // the bypass the call site now closes
        s.custom_unit_types = vec![unit_type(0.0, 1000, 20)]; // speed 0 = NaN on move
        let err = validate_custom_settings(&s)
            .expect("custom types must be validated even with the standard flag");
        assert!(err.contains("speed"), "unexpected error: {err}");

        // And a well-formed custom type with the flag set still passes.
        let mut s = base_settings();
        s.use_standard_ruleset = true;
        s.custom_unit_types = vec![unit_type(1.0, 1000, 20)];
        assert_eq!(validate_custom_settings(&s), None);
    }
}

// ---------------------------------------------------------------------------
// I8: lazy spectator union vision grid cache
// ---------------------------------------------------------------------------

/// I8: cached union of every player's vision grid, for the spectator network
/// path. `union_vision_grid_flat` allocates a fresh ~1 MB `Vec` + P×1 MB OR
/// on every call; this cache recomputes it only when the pid list or any
/// source per-player vision grid `Arc` changes.
///
/// Correctness rests on the influence double-buffer invariant
/// (`components.rs`): published vision-grid `Arc`s are never mutated in
/// place — a dirty run allocates fresh grids and flips ALL published `Arc`
/// refs at once. So `Arc::ptr_eq` across the source list misses on every
/// grid-dirty run and hits exactly on grid-clean ticks.
struct SpectatorUnionCache {
    pids: Vec<String>,
    /// Per-pid vision grid `Arc` (or `None` for a pid with no grid), same
    /// order as `pids`. These are the cache keys.
    sources: Vec<Option<Arc<Vec<u8>>>>,
    /// Cached union, in the same flat layout as `union_vision_grid_flat`.
    union: Arc<Vec<u8>>,
}

impl SpectatorUnionCache {
    /// Returns the union grid for this tick: the cached `Arc` on a hit
    /// (pid list unchanged and every source `Arc` `Arc::ptr_eq`-equal), or
    /// `recompute()`'s result wrapped in a fresh cache entry on a miss.
    ///
    /// `sources` must be one entry per `pids` entry, in the same order — the
    /// per-pid `vision_grid_flat` results that `union_vision_grid_flat` ORs
    /// together.
    fn update(
        cache: &mut Option<Self>,
        pids: &[String],
        sources: &[Option<Arc<Vec<u8>>>],
        recompute: impl FnOnce() -> Option<Vec<u8>>,
    ) -> Option<Arc<Vec<u8>>> {
        let hit = cache.as_ref().is_some_and(|c| {
            c.pids == pids
                && c.sources.len() == sources.len()
                && c.sources
                    .iter()
                    .zip(sources.iter())
                    .all(|(a, b)| match (a, b) {
                        (None, None) => true,
                        (Some(x), Some(y)) => Arc::ptr_eq(x, y),
                        _ => false,
                    })
        });
        if hit {
            return Some(Arc::clone(&cache.as_ref().unwrap().union));
        }
        match recompute() {
            Some(union) => {
                let entry = Self {
                    pids: pids.to_vec(),
                    sources: sources.to_vec(),
                    union: Arc::new(union),
                };
                let out = Arc::clone(&entry.union);
                *cache = Some(entry);
                Some(out)
            }
            None => {
                *cache = None;
                None
            }
        }
    }
}

#[cfg(test)]
mod spectator_union_cache_tests {
    use super::SpectatorUnionCache;
    use std::sync::Arc;

    fn pid(s: &str) -> String {
        s.to_string()
    }

    /// Acceptance gate: cache hit when pids are identical and every source
    /// Arc is the same allocation — `recompute` must NOT run.
    #[test]
    fn cache_hit_on_identical_arcs_and_pids() {
        let grid_a: Arc<Vec<u8>> = Arc::new(vec![0, 1, 1, 0]);
        let grid_b: Arc<Vec<u8>> = Arc::new(vec![1, 0, 0, 1]);
        let pids = vec![pid("a"), pid("b")];
        let sources = vec![Some(Arc::clone(&grid_a)), Some(Arc::clone(&grid_b))];
        let mut cache: Option<SpectatorUnionCache> = None;

        let mut recomputes = 0;
        let make = |recomputes: &mut i32| -> Option<Vec<u8>> {
            *recomputes += 1;
            Some(vec![1, 1, 1, 1]) // a | b
        };

        let first =
            SpectatorUnionCache::update(&mut cache, &pids, &sources, || make(&mut recomputes));
        assert!(first.is_some());
        assert_eq!(recomputes, 1);

        // Same pids + same Arc allocations (fresh clones of the same Arcs).
        let sources2 = vec![Some(Arc::clone(&grid_a)), Some(Arc::clone(&grid_b))];
        let second =
            SpectatorUnionCache::update(&mut cache, &pids, &sources2, || make(&mut recomputes));
        assert!(Arc::ptr_eq(
            first.as_ref().unwrap(),
            second.as_ref().unwrap()
        ));
        assert_eq!(recomputes, 1, "hit must not recompute");
        assert_eq!(second.unwrap().as_slice(), &[1, 1, 1, 1]);
    }

    /// Acceptance gate: recompute when a source grid is a different
    /// allocation (grid-dirty run flips all published Arcs).
    #[test]
    fn recompute_on_source_arc_miss() {
        let pids = vec![pid("a"), pid("b")];
        let old_a: Arc<Vec<u8>> = Arc::new(vec![0, 1, 1, 0]);
        let old_b: Arc<Vec<u8>> = Arc::new(vec![1, 0, 0, 1]);
        let mut cache = Option::<SpectatorUnionCache>::None;

        let mut recomputes = 0;
        let make = |recomputes: &mut i32| -> Option<Vec<u8>> {
            *recomputes += 1;
            Some(vec![*recomputes as u8, 0, 0, 0]) // tag with the call count
        };

        let s_old = vec![Some(Arc::clone(&old_a)), Some(Arc::clone(&old_b))];
        let first =
            SpectatorUnionCache::update(&mut cache, &pids, &s_old, || make(&mut recomputes));
        assert_eq!(recomputes, 1);

        // New allocations (as a dirty run would produce): same VALUES, new Arcs.
        let new_a: Arc<Vec<u8>> = Arc::new(vec![0, 1, 1, 0]);
        let s_new = vec![Some(Arc::clone(&new_a)), Some(Arc::clone(&old_b))];
        assert!(!Arc::ptr_eq(&old_a, &new_a));
        let second =
            SpectatorUnionCache::update(&mut cache, &pids, &s_new, || make(&mut recomputes));
        assert_eq!(recomputes, 2, "any Arc miss must recompute");
        assert!(!Arc::ptr_eq(
            first.as_ref().unwrap(),
            second.as_ref().unwrap()
        ));

        // Cache now holds the NEW sources: identical inputs hit again.
        let third =
            SpectatorUnionCache::update(&mut cache, &pids, &s_new, || make(&mut recomputes));
        assert_eq!(recomputes, 2);
        assert!(Arc::ptr_eq(
            second.as_ref().unwrap(),
            third.as_ref().unwrap()
        ));
    }

    /// Acceptance gate: recompute on pid join (and leave) — the pid list is
    /// part of the key.
    #[test]
    fn recompute_on_pid_join_or_leave() {
        let grid_a: Arc<Vec<u8>> = Arc::new(vec![1, 0, 0, 0]);
        let grid_b: Arc<Vec<u8>> = Arc::new(vec![0, 1, 0, 0]);
        let pids_a = vec![pid("a")];
        let pids_ab = vec![pid("a"), pid("b")];
        let mut cache: Option<SpectatorUnionCache> = None;

        let mut recomputes = 0;
        let make = |recomputes: &mut i32| -> Option<Vec<u8>> {
            *recomputes += 1;
            Some(vec![*recomputes as u8; 4])
        };

        let _ =
            SpectatorUnionCache::update(&mut cache, &pids_a, &[Some(Arc::clone(&grid_a))], || {
                make(&mut recomputes)
            });
        assert_eq!(recomputes, 1);

        // b joins: even though a's Arc is unchanged, the union must refresh.
        let _ = SpectatorUnionCache::update(
            &mut cache,
            &pids_ab,
            &[Some(Arc::clone(&grid_a)), Some(Arc::clone(&grid_b))],
            || make(&mut recomputes),
        );
        assert_eq!(recomputes, 2, "pid join must recompute");

        // b leaves again.
        let _ =
            SpectatorUnionCache::update(&mut cache, &pids_a, &[Some(Arc::clone(&grid_a))], || {
                make(&mut recomputes)
            });
        assert_eq!(recomputes, 3, "pid leave must recompute");

        // Hit again on the (now cached) single-pid state.
        let _ =
            SpectatorUnionCache::update(&mut cache, &pids_a, &[Some(Arc::clone(&grid_a))], || {
                make(&mut recomputes)
            });
        assert_eq!(recomputes, 3);
    }

    /// A pid with no grid (`None` source) is part of the key: None → Some and
    /// Some → None must both recompute.
    #[test]
    fn none_sources_are_part_of_the_key() {
        let grid_a: Arc<Vec<u8>> = Arc::new(vec![1, 1, 0, 0]);
        let pids = vec![pid("a"), pid("b")];
        let mut cache: Option<SpectatorUnionCache> = None;

        let mut recomputes = 0;
        let make = |recomputes: &mut i32| -> Option<Vec<u8>> {
            *recomputes += 1;
            Some(vec![*recomputes as u8; 4])
        };

        let none_b = vec![Some(Arc::clone(&grid_a)), None];
        let _ = SpectatorUnionCache::update(&mut cache, &pids, &none_b, || make(&mut recomputes));
        assert_eq!(recomputes, 1);

        // b gains a grid.
        let grid_b: Arc<Vec<u8>> = Arc::new(vec![0, 0, 1, 1]);
        let some_b = vec![Some(Arc::clone(&grid_a)), Some(Arc::clone(&grid_b))];
        let _ = SpectatorUnionCache::update(&mut cache, &pids, &some_b, || make(&mut recomputes));
        assert_eq!(recomputes, 2, "None -> Some must recompute");

        // b loses it again.
        let _ = SpectatorUnionCache::update(&mut cache, &pids, &none_b, || make(&mut recomputes));
        assert_eq!(recomputes, 3, "Some -> None must recompute");

        // Hit again.
        let _ = SpectatorUnionCache::update(&mut cache, &pids, &none_b, || make(&mut recomputes));
        assert_eq!(recomputes, 3);
    }

    /// Recompute returning `None` (no world data / no players) clears the
    /// cache and returns `None`.
    #[test]
    fn recompute_none_clears_cache() {
        let grid_a: Arc<Vec<u8>> = Arc::new(vec![1, 0, 0, 0]);
        let pids = vec![pid("a")];
        let mut cache: Option<SpectatorUnionCache> = None;
        let _ =
            SpectatorUnionCache::update(&mut cache, &pids, &[Some(Arc::clone(&grid_a))], || {
                Some(vec![1; 4])
            });
        assert!(cache.is_some());

        // Empty pid list forces a miss; recompute -> None must clear the cache.
        let out = SpectatorUnionCache::update(&mut cache, &[], &[], || None);
        assert!(out.is_none());
        assert!(cache.is_none(), "recompute None must clear the cache");
    }
}

// ---------------------------------------------------------------------------
// I7: typed bounded keep-latest egress — unit + slow-client soak tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod i7_egress_tests {
    use super::*;
    use crate::server::room::{DeltaRing, DELTA_RING_DEPTH};
    use std::time::Duration;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    const DELTA_SIZE: usize = 64 * 1024;
    const N_DELTAS: u64 = 512;

    type PendingEgress =
        Arc<std::sync::Mutex<Option<(mpsc::UnboundedReceiver<EgressMessage>, Arc<DeltaRing>)>>>;

    /// Wait for the test's hand-off of (channel, ring), then run the exact
    /// drain loop the server spawns for the path under test.
    async fn wait_release(
        pending: &PendingEgress,
    ) -> (mpsc::UnboundedReceiver<EgressMessage>, Arc<DeltaRing>) {
        loop {
            if let Some(v) = pending.lock().unwrap().take() {
                return v;
            }
            tokio::time::sleep(Duration::from_millis(1)).await;
        }
    }

    async fn gated_tcp_egress_loop(
        pending: PendingEgress,
        writer: tokio::io::WriteHalf<tokio::net::TcpStream>,
    ) {
        let (rx, ring) = wait_release(&pending).await;
        tcp_egress_loop(writer, rx, ring).await;
    }

    async fn gated_ws_egress_loop<S>(pending: PendingEgress, sink: Arc<tokio::sync::Mutex<S>>)
    where
        S: futures::Sink<Message> + Unpin,
    {
        let (rx, ring) = wait_release(&pending).await;
        ws_egress_loop(sink, rx, ring).await;
    }

    /// One tick's delta payload: zeroed 64 KB with a unique marker up front
    /// (the egress path is byte-transparent).
    fn delta_bytes(i: u64) -> Vec<u8> {
        let mut buf = vec![0u8; DELTA_SIZE];
        let marker = format!("I7DELTA-{i:05}").into_bytes();
        buf[..marker.len()].copy_from_slice(&marker);
        buf
    }

    pub(crate) fn find(bytes: &[u8], needle: &[u8]) -> Option<usize> {
        bytes.windows(needle.len()).position(|w| w == needle)
    }

    /// Indexes of the deltas whose markers appear in `received`, in wire order.
    fn collect_delivered(received: &[u8]) -> Vec<u64> {
        const PREFIX: &[u8] = b"I7DELTA-";
        received
            .windows(PREFIX.len() + 5)
            .filter(|w| &w[..PREFIX.len()] == PREFIX)
            .filter_map(|w| std::str::from_utf8(&w[PREFIX.len()..]).ok())
            .filter_map(|s| s.parse::<u64>().ok())
            .collect()
    }

    /// Reassemble server→client frame payloads (unmasked; continuation
    /// frames appended, ping/pong skipped, close ends the stream).
    pub(crate) fn parse_ws_frames(data: &[u8]) -> Option<Vec<u8>> {
        let mut out = Vec::new();
        let mut i = 0;
        while i + 2 <= data.len() {
            let opcode = data[i] & 0x0f;
            let mut len = (data[i + 1] & 0x7f) as usize;
            let mut off = i + 2;
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
            if data[i + 1] & 0x80 != 0 {
                off += 4; // masking key (client→server only; tolerate it)
            }
            if off + len > data.len() {
                return None;
            }
            match opcode {
                0x0..=0x2 => out.extend_from_slice(&data[off..off + len]),
                0x8 => return Some(out),
                _ => {} // ping/pong/reserved: skip
            }
            i = off + len;
        }
        Some(out)
    }

    /// Push a stall's worth of traffic: N tick deltas, then two reliable
    /// messages (Error, GameOver). Returns (last delta, error, game over).
    /// The drain task must be gated (not yet started): every pushed delta
    /// then lands in the ring and evicts the oldest past depth, so the
    /// bounded-memory invariant is assertable deterministically.
    async fn push_soak_traffic(
        sender: &PlayerSender,
        ring: &Arc<DeltaRing>,
    ) -> (Vec<u8>, Vec<u8>, Vec<u8>) {
        let mut last_delta = Vec::new();
        for i in 0..N_DELTAS {
            let bytes = delta_bytes(i);
            if i + 1 == N_DELTAS {
                last_delta = bytes.clone();
            }
            sender.send_delta(bytes).await.unwrap();
        }
        // The drain task is gated and the client reader is stalled: the ring
        // holds exactly its depth — N_DELTAS deltas were pushed, but only the
        // latest DELTA_RING_DEPTH payloads remain in memory.
        assert_eq!(ring.len(), DELTA_RING_DEPTH);
        assert_eq!(ring.latest_seq(), Some(N_DELTAS - 1));
        assert!(ring.byte_size() <= DELTA_RING_DEPTH * DELTA_SIZE);

        let error_msg = encode_message(&ServerMessage::Error {
            message: "i7-soak-error".to_string(),
        });
        let game_over = encode_message(&ServerMessage::GameOver {
            winner: "i7".to_string(),
            game_id: "soak".to_string(),
            message: "i7 soak game over".to_string(),
        });
        // Reliable: always queued, never dropped.
        sender
            .send_reliable(error_msg.clone().into())
            .await
            .unwrap();
        sender
            .send_reliable(game_over.clone().into())
            .await
            .unwrap();
        (last_delta, error_msg, game_over)
    }

    /// Read the client until the GameOver payload is in the buffer (no EOF
    /// assumption — the drain task is what we are testing).
    async fn read_until(client: &mut tokio::net::TcpStream, target: &[u8], into: &mut Vec<u8>) {
        let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
        let mut buf = [0u8; 64 * 1024];
        let mut last_checked = 0usize;
        loop {
            if find(&into[last_checked..], target).is_some() {
                return;
            }
            assert!(
                tokio::time::Instant::now() < deadline,
                "target not delivered in time"
            );
            let n = client.read(&mut buf).await.expect("client read failed");
            assert!(n > 0, "connection closed before target delivered");
            into.extend_from_slice(&buf[..n]);
            last_checked = into.len().saturating_sub(target.len());
        }
    }

    fn assert_resumed(received: &[u8], last_delta: &[u8], error_msg: &[u8], game_over: &[u8]) {
        let delivered = collect_delivered(received);
        // The stall outgrew the ring: stale deltas were dropped (that is what
        // bounds memory), so strictly fewer than N_DELTAS arrived.
        assert!(
            delivered.len() < N_DELTAS as usize,
            "stall must drop stale deltas (got {} of {})",
            delivered.len(),
            N_DELTAS
        );
        // The client resumes with the LATEST delta, in order.
        assert_eq!(
            delivered.last().copied(),
            Some(N_DELTAS - 1),
            "client must resume with the latest delta"
        );
        assert!(
            delivered.windows(2).all(|w| w[0] < w[1]),
            "in-order delivery"
        );

        // Reliable messages were never dropped and arrived after the stall
        // cleared, in order, after the latest delta.
        let err_pos = find(received, error_msg).expect("reliable Error must be delivered");
        let over_pos = find(received, game_over).expect("reliable GameOver must be delivered");
        assert!(over_pos > err_pos, "reliable preserves order");
        // The reliable error starts at or after the end of the latest delta's
        // bytes (they are adjacent in the stall-clear drain).
        let last_end =
            find(received, last_delta).expect("latest delta must be delivered") + last_delta.len();
        assert!(
            err_pos >= last_end,
            "reliable delivered after the stall clears (after the latest delta)"
        );
    }

    #[test]
    fn delta_ring_keep_latest_bounded() {
        let ring = DeltaRing::new(3);
        assert_eq!(ring.len(), 0);
        assert!(ring.is_empty());
        let a = ring.push(vec![1u8; 10]);
        let b = ring.push(vec![2u8; 10]);
        let c = ring.push(vec![3u8; 10]);
        let d = ring.push(vec![4u8; 10]);
        assert_eq!((a, b, c, d), (0u64, 1, 2, 3));
        // Keep-latest: once past depth, the oldest entry is evicted.
        assert_eq!(ring.len(), 3);
        assert_eq!(ring.byte_size(), 30);
        assert_eq!(ring.latest_seq(), Some(d));
        assert!(ring.take(a).is_none(), "evicted seq must be gone");
        assert_eq!(ring.take(b), Some(vec![2u8; 10]));
        assert_eq!(ring.take(c), Some(vec![3u8; 10]));
        assert_eq!(ring.take(d), Some(vec![4u8; 10]));
        assert!(ring.is_empty());
    }

    #[test]
    fn egress_bytes_skips_evicted_deltas() {
        let ring = DeltaRing::new(DELTA_RING_DEPTH);
        let first = ring.push(vec![9u8; 8]);
        let last = (0..N_DELTAS).fold(first, |_, _| ring.push(vec![9u8; 8]));
        assert!(egress_bytes(EgressMessage::Delta(first), &ring).is_none());
        assert_eq!(
            egress_bytes(EgressMessage::Delta(last), &ring),
            Some(vec![9u8; 8].into())
        );
        assert_eq!(
            egress_bytes(EgressMessage::Reliable(vec![7u8].into()), &ring),
            Some(vec![7u8].into())
        );
    }

    /// Nagle must be off on the raw-TCP game transport: small delta frames at
    /// up to 10 Hz would otherwise eat the kernel's ~40 ms coalescing delay.
    /// (Invisible to the headless benchmark, which models no socket — this is
    /// the acceptance gate.)
    #[tokio::test]
    async fn tcp_game_stream_disables_nagle() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let _client = tokio::net::TcpStream::connect(addr).await.unwrap();
        let (stream, _) = listener.accept().await.unwrap();
        configure_game_tcp_stream(&stream);
        assert!(
            stream.nodelay().unwrap(),
            "game TCP stream must disable Nagle"
        );
    }

    /// A broadcast frame must reach every player as a REF of one allocation,
    /// not a per-player copy: `send_reliable(Bytes)` clones are refcount
    /// bumps, so every queued envelope shares the same underlying bytes.
    #[tokio::test]
    async fn reliable_broadcast_shares_one_allocation() {
        let frame = Bytes::from(encode_message(&ServerMessage::ClearSpectator));
        let mut chunk_ptrs = Vec::new();
        for _ in 0..3 {
            let (tx, mut rx) = mpsc::unbounded_channel::<EgressMessage>();
            let sender = PlayerSender::Tcp(TcpEgress::new(tx));
            sender.send_reliable(frame.clone()).await.unwrap();
            let EgressMessage::Reliable(received) = rx.try_recv().unwrap() else {
                panic!("expected a Reliable envelope");
            };
            chunk_ptrs.push(received.as_ptr());
        }
        assert!(
            chunk_ptrs.iter().all(|p| *p == chunk_ptrs[0]),
            "broadcast clones must share the frame's allocation, got {chunk_ptrs:?}"
        );
    }

    /// Slow-client soak over the TCP path: with a stalled reader, delta
    /// memory stays bounded at the ring depth, reliable GameOver/Error
    /// messages are delivered after the stall clears, and the client resumes
    /// with the latest delta. The drain task (the exact `tcp_egress_loop`
    /// `handle_tcp_client` spawns) is gated behind a hand-off so the stall is
    /// deterministic: all 512 deltas are pushed while the drain is idle, so
    /// ring eviction runs to completion before the invariant is checked.
    /// Invisible to the headless stress harness (send = 0 ms there), so this
    /// test is the acceptance gate.
    #[tokio::test]
    async fn tcp_slow_client_soak_bounded_and_recovers() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let mut client = tokio::net::TcpStream::connect(addr).await.unwrap();
        let (stream, _) = listener.accept().await.unwrap();
        let (_stalled_reader, writer) = tokio::io::split(stream);

        let (tx, rx) = mpsc::unbounded_channel::<EgressMessage>();
        let egress = TcpEgress::new(tx);
        let ring = Arc::clone(&egress.ring);
        let sender = PlayerSender::Tcp(egress);

        // Stalled: the client never reads, and the drain task waits for the
        // hand-off, so the ring takes the full stall.
        let pending: PendingEgress = Arc::new(std::sync::Mutex::new(None));
        let pending_drain = Arc::clone(&pending);
        tokio::spawn(async move {
            gated_tcp_egress_loop(pending_drain, writer).await;
        });

        let (last_delta, error_msg, game_over) = push_soak_traffic(&sender, &ring).await;
        drop(sender);

        // Stall clears: hand the channel + ring to the drain, then read until
        // the reliable GameOver arrives.
        *pending.lock().unwrap() = Some((rx, ring.clone()));
        let mut received = Vec::new();
        read_until(&mut client, &game_over, &mut received).await;
        assert_resumed(&received, &last_delta, &error_msg, &game_over);
    }

    /// Slow-client soak over the WS bridge path — the same drain loop
    /// handle_ws_game spawns, against a real axum WebSocket upgrade. The
    /// drain is gated behind the test's hand-off, mirroring the TCP path.
    #[tokio::test]
    async fn ws_slow_client_soak_bounded_and_recovers() {
        let (tx, rx) = mpsc::unbounded_channel::<EgressMessage>();
        let egress = TcpEgress::new(tx);
        let ring = Arc::clone(&egress.ring);
        let sender = PlayerSender::Tcp(egress);

        // Starts empty: the gated drain waits for the test's hand-off.
        let pending: PendingEgress = Arc::new(std::sync::Mutex::new(None));
        let app = axum::Router::new()
            .route("/ws", get(test_ws_upgrade))
            .with_state(pending.clone());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            axum::serve(listener, app).await.ok();
        });

        let mut client = tokio::net::TcpStream::connect(addr).await.unwrap();
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
        // Stalled: the client never reads. Push a stall's worth of traffic.
        let (last_delta, error_msg, game_over) = push_soak_traffic(&sender, &ring).await;
        drop(sender);

        // Stall clears: hand the channel + ring to the gated drain, then read
        // until the reliable GameOver arrives.
        *pending.lock().unwrap() = Some((rx, ring.clone()));
        let mut raw = Vec::new();
        read_until(&mut client, &game_over, &mut raw).await;
        let header_end = find(&raw, b"\r\n\r\n").expect("101 upgrade header") + 4;
        let payload = parse_ws_frames(&raw[header_end..]).expect("ws frames parse");
        assert_resumed(&payload, &last_delta, &error_msg, &game_over);
    }

    async fn test_ws_upgrade(
        ws: WebSocketUpgrade,
        State(pending): State<PendingEgress>,
    ) -> impl axum::response::IntoResponse {
        ws.on_upgrade(move |socket| async move {
            let (ws_sink, _ws_stream) = socket.split();
            let sink = Arc::new(tokio::sync::Mutex::new(ws_sink));
            // Gated: waits for the test's hand-off, then runs exactly the
            // drain task handle_ws_game spawns.
            tokio::spawn(gated_ws_egress_loop(pending, sink));
        })
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
            if let Some(pos) = super::i7_egress_tests::find(&raw, b"\r\n\r\n") {
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
            if let Some(pos) = super::i7_egress_tests::find(&raw, b"\r\n\r\n") {
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
            if let Some(pos) = super::i7_egress_tests::find(&raw, b"\r\n\r\n") {
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
