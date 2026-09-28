//! Setup phase of `GameRoom::new`, extracted verbatim from `mod.rs`:
//! game creation (Game::new + start), city assignment, starting recruits,
//! replay-recorder init (the line-oriented sender to the dedicated writer
//! task in `tick.rs`), `settings_json`, the initial FullState send to each
//! player, LLM client/agent creation (`llm_pairs`), plus channel creation
//! and a few scalars. `prepare_game` bundles everything
//! `llm_tasks::spawn_llm_agents`, `llm_tasks::spawn_advisors` and the
//! `tick::TickLoop` literal consume into a [`PreparedRoom`].

use std::collections::HashMap;
use std::sync::atomic::AtomicBool;
use std::sync::Arc;
use tokio::sync::mpsc;

use super::tick::replay_writer_task;
use super::{Command, LlmPlayerConfig, PlayerConnection};
use crate::config::Config;
use crate::game::game::{Game, GameSettings};
use crate::llm::agent::{GameState, LlmAgent};
use crate::llm::clients::LlmClient;
use crate::map::map::Map;
use crate::server::handlers::http::game_metadata_settings_json;
use crate::server::protocol::{encode_message, ServerMessage};
use crate::storage::Storage;

/// Everything the `GameRoom::new` setup phase produces that the LLM spawns
/// and the tick loop consume. The `GameRoom::new` orchestrator reads each
/// field from this bundle by name.
pub(crate) struct PreparedRoom {
    pub(super) game: Game,
    pub(super) players: Vec<PlayerConnection>,
    /// Arch #13: line-oriented sender to the dedicated replay writer task
    /// (`tick::replay_writer_task`), which owns the `BufWriter<File>`.
    /// `None` when the game is not recorded (pure-LLM, no human viewpoint).
    pub(super) replay_tx: Option<mpsc::UnboundedSender<String>>,
    pub(super) record_pid: Option<String>,
    pub(super) settings_json: String,
    pub(super) llm_pairs: Vec<(LlmAgent, LlmClient)>,
    pub(super) cmd_tx: mpsc::UnboundedSender<Command>,
    pub(super) cmd_rx: mpsc::UnboundedReceiver<Command>,
    pub(super) state_broadcasts: HashMap<String, tokio::sync::broadcast::Sender<GameState>>,
    /// HIGH #1: per-player advisor enable flags (shared by `Arc` with the
    /// tick loop and each spawned advisor task). Disabled by default.
    pub(super) advisor_enabled: HashMap<String, Arc<AtomicBool>>,
    pub(super) advisor_client_opt: Option<LlmClient>,
    pub(super) interval_ms: u64,
    pub(super) game_start_time: u64,
    /// The original `storage` (goes to `GameRoom`) plus its clone
    /// (goes to the tick loop) — mirrors the two locals in the old
    /// inline code.
    pub(super) storage: Storage,
    pub(super) storage_clone: Storage,
}

/// The setup phase of `GameRoom::new`, moved verbatim from `mod.rs`
/// (same statements, ordering, comments and logging; only mechanical
/// parameter names/imports/visibility). Infallible, matching the old
/// inline code: every fallible step degrades with a warn/eprintln,
/// never an Err.
#[allow(clippy::too_many_arguments)]
pub(crate) fn prepare_game(
    map: Map,
    map_name: String,
    settings: GameSettings,
    players: Vec<PlayerConnection>,
    llm_configs: Vec<LlmPlayerConfig>,
    room_id: &str,
    config: Config,
    storage: Storage,
) -> PreparedRoom {
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
    let cities_snapshot: Vec<String> = game.query_cities().iter().map(|c| c.id.clone()).collect();
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

    let (cmd_tx, cmd_rx) = mpsc::unbounded_channel();
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
    // Arch #13: the file IO lives in a dedicated writer task
    // (`tick::replay_writer_task`): the tick task only sends line-oriented
    // strings over `replay_tx` (channel order = file order: full_state,
    // per-tick deltas, terminal game_end marker — the marker is sent as
    // the last line before the sender is dropped). The writer owns the
    // BufWriter and flushes on its own ~1 s cadence. Recording is
    // deliberately non-fatal: a disk failure degrades to "no replay",
    // never a broken game. The writer task is deliberately NOT tracked in
    // `spawned_handles` — it must survive the game-end aborts to flush the
    // terminal marker.
    let record_pid: Option<String> = players.iter().find_map(|p| p.id().cloned());
    let mut replay_tx: Option<mpsc::UnboundedSender<String>> = None;
    if record_pid.is_some() {
        let dir = config.storage.replay_path.clone();
        let _ = std::fs::create_dir_all(&dir);
        let path = std::path::Path::new(&dir).join(format!("{}.jsonl", room_id));
        let (tx, rx) = mpsc::unbounded_channel::<String>();
        // Drop the JoinHandle — dropping a tokio handle does not abort the
        // task (the writer outlives the tick loop on purpose).
        tokio::spawn(replay_writer_task(path, rx));
        replay_tx = Some(tx);
    }

    // Settings payload for the game_metadata row written at game end:
    // the ACTUAL in-play settings + map name (record_game_end persists
    // it verbatim; the replay list UI shows it).
    let settings_json = game_metadata_settings_json(&game, &map_name);

    for player in &players {
        let player_id = player.id().cloned().unwrap_or_default();
        let initial_state = game.full_state_with_player_id(&player_id);
        // Replay: log the viewpoint's initial full state (the same
        // payload about to be sent on the wire). The writer task flushes
        // the header line immediately, so the file is playable even if the
        // process dies mid-game.
        if record_pid.as_deref() == Some(player_id.as_str()) {
            if let Some(tx) = replay_tx.as_ref() {
                if let Ok(v) = serde_json::to_value(&initial_state) {
                    let _ = tx.send(format!("{{\"full_state\":{}}}", v));
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

    // Create the advisor LLM client (only when configured) — the shared
    // per-player enable flags are built from it just below.
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

    // HIGH #1: per-player advisor enable FLAGS (one `Arc<AtomicBool>` per
    // human player, disabled by default) — shared with the tick loop
    // (Command::ToggleAdvisor writes it) and each spawned advisor task
    // (reads it before every 60 s LLM call). Replaces the old
    // `HashMap<String, Advisor>`, which was dead state: `spawn_advisors`
    // built a fresh `Advisor::new()` with `set_enabled(true)` every
    // iteration, so the tick loop's toggle mutated a copy nobody read —
    // the toggle did nothing, and the provider was called every 60 s per
    // human player even when "disabled". The `Advisor` itself now lives
    // inside the spawned task (created once, outside the loop).
    let mut advisor_enabled: HashMap<String, Arc<AtomicBool>> = HashMap::new();
    if advisor_client_opt.is_some() {
        for player in &players {
            if let Some(id) = player.id() {
                // Intended default: DISABLED (no LLM spend until toggled on).
                advisor_enabled.insert(id.clone(), Arc::new(AtomicBool::new(false)));
            }
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

    PreparedRoom {
        game,
        players,
        replay_tx,
        record_pid,
        settings_json,
        llm_pairs,
        cmd_tx,
        cmd_rx,
        state_broadcasts,
        advisor_enabled,
        advisor_client_opt,
        interval_ms,
        game_start_time,
        storage,
        storage_clone,
    }
}
