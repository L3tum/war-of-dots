//! Server: HTTP + raw TCP for game state

use sqlite::Value;
use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::sync::{mpsc, Mutex};
use tokio::time::Duration;
use uuid::Uuid;

use axum::extract::ws::{Message, WebSocket};
use axum::extract::{State, WebSocketUpgrade};
use axum::response::Html;
use axum::routing::get;
use futures::{SinkExt, StreamExt};
use include_dir::{include_dir, Dir};

use crate::config::{Config, EquipmentConfig, UnitTypeConfig as ConfigUnitType};
use crate::game::economy::UnitType;
use crate::game::game::{Game, GameSettings};
use crate::game::unit::Equipment;

use crate::map::map::Map;
use crate::map::map_config::load_default_map;
use crate::map::map_registry;
use crate::replay::replay::ReplayEvent;
use crate::server::delta::{run_network_phase, NetworkBaselines, NetworkPhaseResult};
use crate::server::lobby::LobbyManager;
use crate::server::protocol::{
    decode_client_message, encode_message, CityState, ClientMessage, GameStateMessage,
    LobbyMessage, LobbySettings, PlayerState, ServerMessage, UnitState,
};

// Re-export types from room module for use throughout game_server
pub use crate::server::room::{
    Command, DebugGameStats, GameRoom, LlmPlayerConfig, PlayerConnection, PlayerSender,
};

// Re-export delta functions
use crate::server::delta::{build_delta_inner, build_llm_state, compute_shared_deltas};

static FRONTEND: Dir = include_dir!("$CARGO_MANIFEST_DIR/frontend/dist");

pub struct GameServer {
    pub(crate) port: u16,
    pub(crate) config: Config,
    pub(crate) lobby_manager: Mutex<LobbyManager>,
    pub(crate) games: Mutex<HashMap<String, Arc<Mutex<GameRoom>>>>,
    /// Maps a connection UUID to (player_id, game_room_id)
    pub(crate) player_map: Mutex<HashMap<Uuid, (String, String)>>,
    /// Shared storage for statistics and leaderboards
    pub(crate) storage: crate::storage::Storage,
}

#[allow(dead_code)]
impl GameServer {
    pub async fn new(config: Config, port: u16) -> anyhow::Result<Self> {
        let storage = crate::storage::Storage::new(&config.storage.database_path)?;
        map_registry::init();
        Ok(GameServer {
            port,
            config,
            lobby_manager: Mutex::new(LobbyManager::new()),
            games: Mutex::new(HashMap::new()),
            player_map: Mutex::new(HashMap::new()),
            storage,
        })
    }

    pub async fn run(self) -> anyhow::Result<()> {
        let port = self.port;
        let state = Arc::new(Mutex::new(self));
        let state_ws = state.clone();

        let app = {
            let mut router = axum::Router::new()
                .route("/", get(index_handler))
                .route("/watch", get(watch_handler))
                .route("/ws", get(ws_handler))
                .route("/ws/replay", get(replay_ws_handler))
                .route("/ws/spectator", get(spectator_ws_handler))
                .route("/replays", get(list_replays))
                .route("/maps", get(list_maps_handler))
                .fallback(serve_static);

            #[cfg(any(debug_assertions, feature = "debug-overlay"))]
            {
                router = router
                    .route("/debug/stats", get(crate::debug::debug_stats_handler))
                    .route("/debug/tick", get(crate::debug::debug_tick_handler))
                    .route(
                        "/debug/benchmark",
                        get(crate::debug::debug_benchmark_handler),
                    );
            }

            let cors = tower_http::cors::CorsLayer::new()
                .allow_origin(tower_http::cors::Any)
                .allow_methods(tower_http::cors::Any)
                .allow_headers(tower_http::cors::Any);

            router.layer(cors).with_state(state.clone())
        };

        let listener_http = tokio::net::TcpListener::bind(format!("0.0.0.0:{}", port)).await?;
        let listener_ws = tokio::net::TcpListener::bind(format!("0.0.0.0:{}", port + 1)).await?;

        tokio::spawn(async move {
            axum::serve(listener_http, app).await.ok();
        });

        tracing::info!(
            "HTTP server on port {}, TCP game on port {}",
            port,
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

impl GameRoom {
    #[allow(clippy::too_many_arguments)]
    fn new(
        game_id: String,
        map: Map,
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
        for unit in game.unit_states() {
            *prev_owners.entry(unit.owner.clone()).or_insert(0) += 1;
        }
        // prev_unit_owners removed in ECS refactor = prev_owners;

        let (cmd_tx, mut cmd_rx) = mpsc::unbounded_channel();
        let game_speed = game.settings().game_speed;
        let interval_ms = (1000.0 / game_speed).ceil() as u64;

        for player in &players {
            let player_id = player.id().cloned().unwrap_or_default();
            let initial_state = game.full_state_with_player_id(&player_id);
            let initial_msg = ServerMessage::FullState(initial_state);
            let encoded = encode_message(&initial_msg);
            let _ = player.tx().blocking_send(encoded);
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
                                        log::warn!("LLM agent {} step failed: {}", agent_name, e);
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

        tokio::spawn(async move {
            let mut game = game;
            let mut baselines: NetworkBaselines = {
                let mut b = NetworkBaselines::default();
                b.prev_units = game
                    .unit_states()
                    .into_iter()
                    .map(|u| (u.id.clone(), u))
                    .collect();
                b.prev_cities = game
                    .query_cities()
                    .iter()
                    .map(|c| (c.id.clone(), c.into()))
                    .collect();
                b.prev_funds = game.all_player_funds();
                b.prev_border = game.territory_grid_flat();
                for pid in game.players().keys() {
                    if let Some(v) = game.vision_grid_flat(pid) {
                        b.prev_vision.insert(pid.clone(), v);
                    }
                }
                b
            };
            let mut players = players;
            let mut advisors = advisors;
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
                                    let _ = player.tx().send(encoded.clone()).await;
                                }
                            }
                        }

                        // --- Process reason log entries (non-blocking) ---
                        while let Ok((name, action, reason)) = reason_rx.try_recv() {
                            let encoded = encode_message(&ServerMessage::LlmReasonLog {
                                player_name: name,
                                action,
                                reason,
                            });
                            for player in &players {
                                let _ = player.tx().send(encoded.clone()).await;
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
                                    let all_pids: Vec<String> = game.players().keys().cloned().collect();
                                    let vision_grid = game.union_vision_grids(&all_pids);
                                    let border_grid = all_pids.first().and_then(|p| game.border_grid_2d(p));
                                    let full_state = game.full_state_with_vision(vision_grid, border_grid);
                                    let encoded = encode_message(&ServerMessage::FullState(full_state));
                                    let _ = sender.send(encoded).await;
                                    players.push(PlayerConnection::Spectator { tx: sender });
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

                        // Snapshot game state once per tick, shared by the
                        // active-player check and the per-player delta build.
                        let current_units = game.unit_states_direct();
                        let current_cities: Vec<CityState> =
                            game.query_cities().iter().map(|c| c.into()).collect();

                        // Update live debug stats (read by /debug/stats)
                        {
                            let mut gs = game_stats.lock().unwrap_or_else(|e| e.into_inner());
                            gs.tick_count = tick_count;
                            gs.last_tick_ms = _tick_start.elapsed().as_secs_f64() * 1000.0;
                            gs.entity_count = game.unit_count() + game.query_cities().len();
                        }

                        // --- Broadcast state snapshots every 60 ticks (~1 sec) ---
                        if tick_count.is_multiple_of(60) {
                            for (player_id, tx) in &state_broadcasts {
                                let state = build_llm_state(&game, player_id);
                                let _ = tx.send(state);
                            }
                        }

                        // --- Check game over ---
                        let winner_id_opt = game.determine_winner();
                        let player_ids: Vec<String> = game.players().keys().cloned().collect();
                        let total_players = player_ids.len();
                        let active_count = player_ids.iter().filter(|pid| {
                            let has_units = current_units.iter().any(|u| u.owner.as_str() == pid.as_str());
                            let has_cities = current_cities.iter().any(|c| c.owner.as_deref() == Some(pid.as_str()));
                            has_units || has_cities
                        }).count();
                        if active_count == 0 {
                            let end_time = std::time::SystemTime::now()
                                .duration_since(std::time::UNIX_EPOCH)
                                .map(|d| d.as_secs())
                                .unwrap_or(0);
                            storage_clone.record_game_end(
                                &room_id,
                                end_time,
                                &[""],
                                &player_ids.iter().map(|s| s.as_str()).collect::<Vec<_>>(),
                                game.player_stats(),
                                game.settings().use_standard_ruleset,
                            );
                            let msg = ServerMessage::GameOver {
                                winner: "Draw".to_string(),
                                game_id: room_id.clone(),
                                message: "⚖️ Draw — all players eliminated".to_string(),
                            };
                            let encoded = encode_message(&msg);
                            for player in &players {
                                let _ = player.tx().send(encoded.clone()).await;
                            }
                            for handle in spawned_handles {
                                handle.abort();
                            }
                            break;
                        }
                        if total_players > 1 && active_count == 1 {
                            let winner_id = winner_id_opt.unwrap_or_default();
                            let winner_name = game.players().get(&winner_id)
                                .map(|p| p.name.clone())
                                .unwrap_or_else(|| winner_id.clone());
                            let end_time = std::time::SystemTime::now()
                                .duration_since(std::time::UNIX_EPOCH)
                                .map(|d| d.as_secs())
                                .unwrap_or(0);
                            storage_clone.record_game_end(
                                &room_id,
                                end_time,
                                &[winner_id.as_str()],
                                &player_ids.iter().map(|s| s.as_str()).collect::<Vec<_>>(),
                                game.player_stats(),
                                game.settings().use_standard_ruleset,
                            );
                            let msg = ServerMessage::GameOver {
                                winner: winner_name,
                                game_id: room_id.clone(),
                                message: "Game over — last player standing wins".to_string(),
                            };
                            let encoded = encode_message(&msg);
                            for player in &players {
                                let _ = player.tx().send(encoded.clone()).await;
                            }
                            for handle in spawned_handles {
                                handle.abort();
                            }
                            break;
                        }

                        // --- Network phase: per-player delta build + encode ---
                        let regular_pids: Vec<String> = players
                            .iter()
                            .filter_map(|p| {
                                if let PlayerConnection::Player { id, .. } = p {
                                    Some(id.clone())
                                } else {
                                    None
                                }
                            })
                            .collect();
                        let phase: NetworkPhaseResult =
                            run_network_phase(&game, &mut baselines, &regular_pids, &current_units, &current_cities);
                        let mut send_map: HashMap<String, Vec<u8>> = phase.encoded.into_iter().collect();

                        // Spectators get the union vision grid (handled separately
                        // since their vision differs from per-player vision).
                        if spectator_vision {
                            let all_pids: Vec<String> = game.players().keys().cloned().collect();
                            let spec_vision: Option<Arc<Vec<u8>>> =
                                game.union_vision_grid_flat(&all_pids).map(Arc::new);
                            let spec_border: Option<Arc<Vec<u8>>> = game.territory_grid_flat();
                            let spec_pid = String::new();
                            let prev_spec_vision: Option<&Arc<Vec<u8>>> =
                                baselines.prev_vision.get(&spec_pid);
                            let cur_unit_ids: HashSet<&str> =
                                current_units.iter().map(|u| u.id.as_str()).collect();
                            let all_funds = game.all_player_funds();
                            let grid_width = game.terrain_width();

                            let shared = compute_shared_deltas(
                                &baselines.prev_units,
                                &baselines.prev_cities,
                                &current_units,
                                &current_cities,
                                &cur_unit_ids,
                                baselines.prev_border.as_ref(),
                                &spec_border,
                            );
                            if let Some(delta) = build_delta_inner(
                                &baselines.prev_units,
                                &baselines.prev_funds,
                                prev_spec_vision,
                                &current_units,
                                &all_funds,
                                &spec_vision,
                                grid_width,
                                &spec_pid,
                                &shared,
                            ) {
                                send_map.insert(spec_pid.clone(), encode_message(&delta));
                            }
                            if let Some(v) = &spec_vision {
                                baselines.prev_vision.insert(spec_pid, Arc::clone(v));
                            }
                        }

                        // Send encoded deltas; keep alive connections
                        let mut alive = Vec::new();
                        for player in players {
                            let pid = player.id().cloned().unwrap_or_default();
                            if let Some(encoded) = send_map.get(&pid) {
                                if player.tx().send(encoded.clone()).await.is_ok() {
                                    alive.push(player);
                                }
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
                            let end_time = std::time::SystemTime::now()
                                .duration_since(std::time::UNIX_EPOCH)
                                .map(|d| d.as_secs())
                                .unwrap_or(0);
                            storage_clone.record_game_end(
                                &room_id,
                                end_time,
                                &[winner_id.as_deref().unwrap_or("")],
                                &player_ids,
                                game.player_stats(),
                                game.settings().use_standard_ruleset,
                            );
                            let msg = ServerMessage::GameOver {
                                winner: "Abandoned".to_string(),
                                game_id: room_id.clone(),
                                message: "Game abandoned — no human players remaining".to_string(),
                            };
                            let encoded = encode_message(&msg);
                            for player in &players {
                                let _ = player.tx().send(encoded.clone()).await;
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
                            let end_time = std::time::SystemTime::now()
                                .duration_since(std::time::UNIX_EPOCH)
                                .map(|d| d.as_secs())
                                .unwrap_or(0);
                            storage_clone.record_game_end(
                                &room_id,
                                end_time,
                                &[winner_id.as_deref().unwrap_or("")],
                                &player_ids,
                                game.player_stats(),
                                game.settings().use_standard_ruleset,
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
                                let all_pids: Vec<String> = game.players().keys().cloned().collect();
                                let vision_grid = game.union_vision_grids(&all_pids);
                                let border_grid = all_pids.first().and_then(|p| game.border_grid_2d(p));
                                let full_state = game.full_state_with_vision(vision_grid, border_grid);
                                let encoded = encode_message(&ServerMessage::FullState(full_state));
                                let _ = sender.send(encoded).await;
                                players.push(PlayerConnection::Spectator { tx: sender });
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
        let units: Vec<UnitState> = self.unit_states().to_vec();
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

#[derive(serde::Serialize)]
pub(crate) struct ReplayInfo {
    game_id: String,
    winner: String,
    players: Vec<String>,
    start_time: i64,
    end_time: i64,
    duration: i64,
}

/// WebSocket handler for replay spectator
pub(crate) async fn replay_ws_handler(
    ws: WebSocketUpgrade,
    State(state): State<Arc<Mutex<GameServer>>>,
    axum::extract::Query(params): axum::extract::Query<std::collections::HashMap<String, String>>,
) -> impl axum::response::IntoResponse {
    let game_id = params.get("game_id").cloned();
    let server_state = state.clone();
    ws.on_upgrade(move |socket| handle_replay_ws(socket, server_state, game_id))
}

/// WebSocket handler for live spectator — joins the active game as a spectator
pub(crate) async fn spectator_ws_handler(
    ws: WebSocketUpgrade,
    State(state): State<Arc<Mutex<GameServer>>>,
) -> impl axum::response::IntoResponse {
    let server_state = state.clone();
    ws.on_upgrade(move |socket| handle_spectator_ws(socket, server_state))
}

pub(crate) async fn handle_spectator_ws(socket: WebSocket, server_state: Arc<Mutex<GameServer>>) {
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
            return;
        }
    };

    // Create a bridge channel: room sends to WS via this sender
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    // Spawn task to forward messages from channel to WebSocket
    tokio::spawn(async move {
        let mut ws_sink = ws_sink;
        while let Some(data) = rx.recv().await {
            if ws_sink.send(Message::Binary(data)).await.is_err() {
                break;
            }
        }
    });

    // Send the AddSpectator command via the room's command_tx
    // We need to create a PlayerSender that bridges to the channel
    let bridge = PlayerSender::Tcp(tx.clone());
    // Send the command to register the spectator with the room
    room_arc
        .lock()
        .await
        .command_tx
        .send(Command::AddSpectator(bridge))
        .ok();
}

pub(crate) async fn handle_replay_ws(
    socket: WebSocket,
    server_state: Arc<Mutex<GameServer>>,
    game_id: Option<String>,
) {
    // Run the entire replay logic on a blocking thread
    let rt = tokio::runtime::Handle::current();
    tokio::task::spawn_blocking(move || {
        rt.block_on(async {
            let (mut ws_sink, ws_stream) = socket.split();
            drop(ws_stream);

            let server = server_state.lock().await;
            let storage_path = server.storage.path.clone();
            drop(server);

            // Open a new DB connection for this task
            // If no game_id provided, send error and exit
            let game_id_str = match game_id {
                Some(id) => id,
                None => {
                    let err = encode_message(&ServerMessage::Error {
                        message: "No game_id provided".to_string(),
                    });
                    let _ = ws_sink.send(Message::Binary(err)).await;
                    return;
                }
            };

            // Open database connection
            let db = match sqlite::Connection::open(&storage_path) {
                Ok(d) => d,
                Err(e) => {
                    let err = encode_message(&ServerMessage::Error {
                        message: format!("Database error: {}", e),
                    });
                    let _ = ws_sink.send(Message::Binary(err)).await;
                    return;
                }
            };

            // Use parameterized query to prevent SQL injection
            let mut stmt = match db.prepare("SELECT replay_path FROM game_metadata WHERE id = ?") {
                Ok(s) => s,
                Err(e) => {
                    let err = encode_message(&ServerMessage::Error {
                        message: format!("Database query error: {}", e),
                    });
                    let _ = ws_sink.send(Message::Binary(err)).await;
                    return;
                }
            };
            let params: Vec<Value> = vec![Value::String(game_id_str.clone())];
            if let Err(e) = stmt.bind(params.as_slice()) {
                tracing::error!(%e, game_id = %game_id_str, "Failed to bind game_id");
                let err = encode_message(&ServerMessage::Error {
                    message: "Database binding error".to_string(),
                });
                let _ = ws_sink.send(Message::Binary(err)).await;
                return;
            }
            let replay_path: String = match stmt.next() {
                Ok(sqlite::State::Row) => {
                    let path: String = stmt.read(0).unwrap_or_default();
                    if path.is_empty() {
                        let err = encode_message(&ServerMessage::Error {
                            message: "Replay path not found".to_string(),
                        });
                        let _ = ws_sink.send(Message::Binary(err)).await;
                        return;
                    }
                    path
                }
                _ => {
                    let err = encode_message(&ServerMessage::Error {
                        message: "Replay not found".to_string(),
                    });
                    let _ = ws_sink.send(Message::Binary(err)).await;
                    return;
                }
            };

            // Load the replay file and reconstruct the game
            let map = crate::map::map_config::load_default_map();
            let num_replay_players = map.cities.len();
            let mut replay_player = match crate::replay::replay::ReplayPlayer::from_file(
                &replay_path,
                Game::new(
                    map,
                    crate::game::game::GameSettings {
                        starting_funds: 100,
                        initial_manpower: 2000,
                        funds_per_city: 10.0,
                        recruitment_rate: 10.0,
                        game_speed: 60.0,
                        use_standard_ruleset: true,
                        unit_types: vec![],
                        city_upgrade_settings: crate::config::CityUpgradeSettings::default(),
                        water_speed: 1.2,
                        influence: crate::game::game::InfluenceSettings::default(),
                    },
                    num_replay_players,
                ),
            ) {
                Ok(rp) => rp,
                Err(_) => return,
            };

            // Send initial full state
            let initial_msg = ServerMessage::FullState(replay_player.full_state());
            let encoded = encode_message(&initial_msg);
            if ws_sink.send(Message::Binary(encoded)).await.is_err() {
                return;
            }

            // Build prev state for delta comparison
            let mut prev_units: HashMap<String, UnitState> = replay_player
                .game
                .unit_states()
                .to_vec()
                .into_iter()
                .map(|u| {
                    let state: UnitState = u.clone();
                    (u.id.clone(), state)
                })
                .collect();
            let mut prev_cities: HashMap<String, CityState> = replay_player
                .game
                .query_cities()
                .iter()
                .map(|c| {
                    let state: CityState = CityState::from(c);
                    (c.id.clone(), state)
                })
                .collect();
            let mut prev_funds = replay_player.all_player_funds().clone();
            let mut prev_vision_grid: Option<Arc<Vec<u8>>> = None;
            let mut prev_border_grid: Option<Arc<Vec<u8>>> = None;
            let grid_width = replay_player.game.terrain_width();

            // Loop: consume events and build deltas
            let mut events: Vec<ReplayEvent> = Vec::new();
            while let Some(event) = replay_player.next() {
                events.push(event.clone());
            }
            for event in events {
                replay_player.apply_event(&event);
                // Flat grids (no 2-D allocation in replay path either)
                let all_pids: Vec<String> = replay_player.game.players().keys().cloned().collect();
                let current_vision: Option<Arc<Vec<u8>>> = replay_player
                    .game
                    .union_vision_grid_flat(&all_pids)
                    .map(Arc::new);
                let current_border: Option<Arc<Vec<u8>>> = replay_player.game.territory_grid_flat();
                let current_units = replay_player.game.unit_states();
                let current_cities: Vec<CityState> = replay_player
                    .game
                    .query_cities()
                    .iter()
                    .map(|c| c.into())
                    .collect();
                let all_funds = replay_player.game.all_player_funds();
                let current_unit_ids: HashSet<&str> =
                    current_units.iter().map(|u| u.id.as_str()).collect();
                let shared = compute_shared_deltas(
                    &prev_units,
                    &prev_cities,
                    &current_units,
                    &current_cities,
                    &current_unit_ids,
                    prev_border_grid.as_ref(),
                    &current_border,
                );
                if let Some(delta) = build_delta_inner(
                    &prev_units,
                    &prev_funds,
                    prev_vision_grid.as_ref(),
                    &current_units,
                    &all_funds,
                    &current_vision,
                    grid_width,
                    "",
                    &shared,
                ) {
                    // Commit the prev state only on non-idle delta
                    prev_units = current_units
                        .iter()
                        .map(|u| (u.id.clone(), u.clone()))
                        .collect();
                    prev_cities = current_cities
                        .iter()
                        .map(|c| (c.id.clone(), c.clone()))
                        .collect();
                    prev_funds = all_funds;
                    prev_vision_grid = current_vision;
                    prev_border_grid = current_border;

                    let encoded = encode_message(&delta);
                    if ws_sink.send(Message::Binary(encoded)).await.is_err() {
                        return;
                    }
                }
                // Wait for next tick (game speed)
                let interval_ms = (1000.0f64 / 60.0).ceil() as u64;
                tokio::time::sleep(std::time::Duration::from_millis(interval_ms)).await;
            }

            // Send final full state
            let final_msg = ServerMessage::FullState(replay_player.full_state());
            let encoded = encode_message(&final_msg);
            let _ = ws_sink.send(Message::Binary(encoded)).await;
        });
    });
}

// WebSocket handler for game state
pub(crate) async fn ws_handler(
    ws: WebSocketUpgrade,
    server_state: axum::extract::State<Arc<Mutex<GameServer>>>,
) -> impl axum::response::IntoResponse {
    ws.on_upgrade(move |socket| handle_ws_game(socket, server_state.0))
}

pub(crate) async fn handle_tcp_client(
    stream: tokio::net::TcpStream,
    server_state: Arc<Mutex<GameServer>>,
) {
    let conn_id = Uuid::new_v4();
    let (reader, writer) = tokio::io::split(stream);
    let (tx, rx) = mpsc::unbounded_channel::<Vec<u8>>();
    let tx_clone = tx.clone();
    let server_state_clone = server_state.clone();
    // PlayerSender for this TCP connection
    let player_sender = PlayerSender::Tcp(tx_clone.clone());

    // Spawn sender task
    tokio::spawn(async move {
        let mut writer = writer;
        let mut rx = rx;
        while let Some(data) = rx.recv().await {
            if writer.write_all(&data).await.is_err() {
                break;
            }
        }
    });

    // Read loop
    let mut reader = reader;
    let mut buf = Vec::new();
    loop {
        match reader.read(&mut buf).await {
            Ok(0) => break,
            Ok(_) => {
                // Message size limit
                if buf.len() > 1_048_576 {
                    tracing::warn!(
                        msg_len = buf.len(),
                        "Message exceeds size limit (TCP), closing connection"
                    );
                    break;
                }
                match decode_client_message(&buf) {
                    Ok(msg) => {
                        handle_client_message(&conn_id, server_state.clone(), &msg, &player_sender)
                            .await;
                        // Message fully consumed — reset the buffer. The console client writes
                        // one complete CBOR message per write, so a single message either
                        // arrives within one read or is fully reassembled before this point.
                        buf.clear();
                    }
                    Err(e) => {
                        // Do NOT clear on decode failure: a CBOR message can be split across
                        // multiple TCP reads. Keep the undecoded bytes so the next read can
                        // reassemble the partial message instead of discarding it.
                        // Buffer growth is bounded by the size-limit check above (1MB), so a
                        // client that sends malformed data will eventually be disconnected.
                        tracing::info!(
                            error = %e,
                            msg_len = buf.len(),
                            "Failed to decode ClientMessage (TCP), buffering for reassembly"
                        );
                    }
                }
            }
            Err(_) => break,
        }
    }

    // Cleanup on disconnect
    {
        let server = server_state_clone.lock().await;
        let mut player_map = server.player_map.lock().await;
        // Just remove the connection; the game room will handle player removal internally
        player_map.remove(&conn_id);
    }
}

// WebSocket game handler — same game logic, different transport
pub(crate) async fn handle_ws_game(socket: WebSocket, server_state: Arc<Mutex<GameServer>>) {
    let conn_id = Uuid::new_v4();
    // Use a single WebSocket with shared access via channels
    let (ws_sink, mut ws_stream) = socket.split();
    let ws_sink = Arc::new(Mutex::new(ws_sink));

    // Create a bridge channel to send data to the websocket
    let (tx, mut rx) = mpsc::unbounded_channel::<Vec<u8>>();
    let actual_player_sender = PlayerSender::Tcp(tx);
    let server_state_clone = server_state.clone();

    // Spawn the websocket sender task
    let ws_sink_clone = Arc::clone(&ws_sink);
    tokio::spawn(async move {
        while let Some(data) = rx.recv().await {
            let mut sink = ws_sink_clone.lock().await;
            if sink.send(Message::Binary(data)).await.is_err() {
                break;
            }
        }
    });

    // Read loop — receive WebSocket messages
    loop {
        match ws_stream.next().await {
            Some(Ok(Message::Binary(data))) => {
                // Message size limit
                if data.len() > 1_048_576 {
                    tracing::warn!(
                        msg_len = data.len(),
                        "Message exceeds size limit (WS), closing connection"
                    );
                    break;
                }
                match decode_client_message(&data) {
                    Ok(msg) => {
                        handle_client_message(
                            &conn_id,
                            server_state.clone(),
                            &msg,
                            &actual_player_sender,
                        )
                        .await;
                    }
                    Err(e) => {
                        tracing::info!(error = %e, msg_len = data.len(), "Failed to decode ClientMessage (WS)");
                        let resp = ServerMessage::Error {
                            message: "Invalid message format".to_string(),
                        };
                        let encoded = encode_message(&resp);
                        let _ = actual_player_sender.send(encoded).await;
                    }
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
    {
        let server = server_state_clone.lock().await;
        let mut player_map = server.player_map.lock().await;
        player_map.remove(&conn_id);
    }
}

pub(crate) async fn handle_client_message(
    conn_id: &Uuid,
    server_state: Arc<Mutex<GameServer>>,
    msg: &ClientMessage,
    player_sender: &PlayerSender,
) {
    match msg {
        ClientMessage::LobbyMessage(lobby_msg) => {
            match lobby_msg {
                LobbyMessage::CreateLobby {
                    settings,
                    password,
                    player_name,
                } => {
                    let player_id = format!("player_{}", &Uuid::new_v4().to_string()[..8]);
                    let player_name = player_name.clone().unwrap_or_else(|| "Player".to_string());
                    let code;
                    // Clone the player_sender for the lobby
                    let lobby_sender = match player_sender {
                        PlayerSender::Tcp(tx) => PlayerSender::Tcp(tx.clone()),
                        PlayerSender::Websocket(_) => {
                            // For WebSocket, create a channel to bridge
                            let (tx, mut rx) = mpsc::unbounded_channel();
                            let original_sender = player_sender.clone();
                            // Spawn a task to relay messages
                            tokio::spawn(async move {
                                while let Some(data) = rx.recv().await {
                                    let _ = original_sender.send(data).await;
                                }
                            });
                            PlayerSender::Tcp(tx)
                        }
                        PlayerSender::None => {
                            // No connection — create a throwaway channel
                            let (tx, _rx) = mpsc::unbounded_channel();
                            PlayerSender::Tcp(tx)
                        }
                    };
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
                    let encoded = encode_message(&resp);
                    let _ = player_sender.send(encoded).await;

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
                            let update_encoded = encode_message(&update);
                            for p in &lobby.players {
                                let _ = p.connection.send(update_encoded.clone()).await;
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
                    let lobby_sender = match player_sender {
                        PlayerSender::Tcp(tx) => PlayerSender::Tcp(tx.clone()),
                        PlayerSender::Websocket(_) => {
                            let (tx, mut rx) = mpsc::unbounded_channel();
                            let original_sender = player_sender.clone();
                            tokio::spawn(async move {
                                while let Some(data) = rx.recv().await {
                                    let _ = original_sender.send(data).await;
                                }
                            });
                            PlayerSender::Tcp(tx)
                        }
                        PlayerSender::None => {
                            // No connection — create a throwaway channel
                            let (tx, _rx) = mpsc::unbounded_channel();
                            PlayerSender::Tcp(tx)
                        }
                    };
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
                            let join_encoded = encode_message(&join_resp);
                            let _ = player_sender.send(join_encoded).await;

                            // Broadcast LobbyUpdate to all players in the lobby
                            let server = server_state.lock().await;
                            let lobby_manager = server.lobby_manager.lock().await;
                            if let Some(l) = lobby_manager.get_lobby(&lobby_code_inner) {
                                let encoded = encode_message(&ServerMessage::LobbyUpdate {
                                    players: players.clone(),
                                    settings: settings.clone(),
                                });
                                for p in &l.players {
                                    let _ = p.connection.send(encoded.clone()).await;
                                }
                            }
                        }
                        Err(e) => {
                            let resp = ServerMessage::Error { message: e };
                            let encoded = encode_message(&resp);
                            let _ = player_sender.send(encoded).await;
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
                            let encoded = encode_message(&resp);
                            let _ = player_sender.send(encoded).await;
                            return;
                        }
                        let result = {
                            let server = server_state.lock().await;
                            let mut lobby_manager = server.lobby_manager.lock().await;
                            lobby_manager.set_slot_llm(&lobby_code, *slot, llm_provider.clone())
                        };
                        if let Err(e) = result {
                            let resp = ServerMessage::Error { message: e };
                            let encoded = encode_message(&resp);
                            let _ = player_sender.send(encoded).await;
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
                            let encoded = encode_message(&resp);
                            let _ = player_sender.send(encoded).await;
                        }
                    } else {
                        let resp = ServerMessage::Error {
                            message: "Not in a lobby".to_string(),
                        };
                        let encoded = encode_message(&resp);
                        let _ = player_sender.send(encoded).await;
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
                            let encoded = encode_message(&resp);
                            let _ = player_sender.send(encoded).await;
                            return;
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
                            let encoded = encode_message(&resp);
                            let _ = player_sender.send(encoded).await;
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
                            let encoded = encode_message(&resp);
                            let _ = player_sender.send(encoded).await;
                        }
                    } else {
                        let resp = ServerMessage::Error {
                            message: "Not in a lobby".to_string(),
                        };
                        let encoded = encode_message(&resp);
                        let _ = player_sender.send(encoded).await;
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
                            let encoded = encode_message(&resp);
                            let _ = player_sender.send(encoded).await;
                            return;
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
                            let encoded = encode_message(&resp);
                            let _ = player_sender.send(encoded).await;
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
                            let encoded = encode_message(&resp);
                            let _ = player_sender.send(encoded).await;
                        }
                    } else {
                        let resp = ServerMessage::Error {
                            message: "Not in a lobby".to_string(),
                        };
                        let encoded = encode_message(&resp);
                        let _ = player_sender.send(encoded).await;
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
                        let encoded = encode_message(&resp);
                        let _ = player_sender.send(encoded).await;
                        return;
                    };
                    let is_creator = {
                        let server = server_state.lock().await;
                        let player_map = server.player_map.lock().await;
                        let Some((player_id, _)) = player_map.get(conn_id) else {
                            return;
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
                        let encoded = encode_message(&resp);
                        let _ = player_sender.send(encoded).await;
                        return;
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
                                let encoded = encode_message(&resp);
                                let _ = player_sender.send(encoded).await;
                                return;
                            }
                        };
                        if !lobby.players.is_empty() {
                            // G-3: Validate custom ruleset settings
                            if !lobby.settings.use_standard_ruleset {
                                if let Some(err) = validate_custom_settings(&lobby.settings) {
                                    let resp = ServerMessage::Error { message: err };
                                    let encoded = encode_message(&resp);
                                    let _ = player_sender.send(encoded).await;
                                    return;
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
                            return;
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
                        let encoded = encode_message(&resp);
                        let _ = player_sender.send(encoded).await;
                        return;
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
                                return;
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
                    let game_settings_clone = game_settings;
                    let human_players_clone = human_players;
                    let llm_configs_clone = llm_configs;
                    tokio::spawn(async move {
                        let server_inner = server_state_inner.lock().await;
                        let mut games = server_inner.games.lock().await;
                        let room = GameRoom::new(
                            lobby_code_clone.clone(),
                            map_clone,
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
                    // Update player map for this connection
                    {
                        let server = server_state.lock().await;
                        let mut player_map = server.player_map.lock().await;
                        player_map.insert(*conn_id, ("player_0".to_string(), lobby_code.clone()));
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
                    let clear_encoded = encode_message(&clear_resp);
                    let lobby_update_resp = ServerMessage::LobbyUpdate {
                        players: all_players.clone(),
                        settings: settings.clone(),
                    };
                    let lobby_encoded = encode_message(&lobby_update_resp);
                    for sender in &lobby_player_senders {
                        let _ = sender.send(clear_encoded.clone()).await;
                        let _ = sender.send(lobby_encoded.clone()).await;
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
            let encoded = encode_message(&resp);
            let _ = player_sender.send(encoded).await;
        }
        ClientMessage::EmbarkUnit { unit_id } => {
            let server = server_state.lock().await;
            let (player_id, room_id) = match server.player_map.lock().await.get(conn_id).cloned() {
                Some(entry) => entry,
                None => return,
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
                None => return,
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
    if settings.water_speed < 0.1 || settings.water_speed > 10.0 {
        return Some("water_speed must be in range 0.1-10.0".into());
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
