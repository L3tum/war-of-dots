//! Game room: `GameRoom`'s constructor (game setup, replay-recorder init,
//! initial full-state send, LLM client/agent creation, LLM agent task
//! spawns, advisor task spawns, and the tick-loop spawn — the loop body
//! itself lives in `tick.rs`) plus the spectator-cap helper the tick task
//! uses.
//!
//! `GameRoom::new` is moved verbatim from `game_server.rs`. The `GameRoom`
//! struct itself lives in `types.rs` (moved there verbatim from the old
//! `room.rs`) so this file's bare `Mutex` can mean the tokio mutex the
//! constructor takes while the struct's `stats` field keeps its std mutex.

use std::sync::Arc;
use tokio::sync::Mutex;

use crate::config::Config;
use crate::game::game::{Game, GameSettings};
use crate::map::map::Map;
use crate::server::game_server::GameServer;
use crate::server::protocol::{encode_message, ServerMessage};

pub mod commands;
pub mod llm_tasks;
pub mod lobby_flow;
pub(crate) mod settings;
pub mod setup;
mod spectator;
pub mod start;
mod state;
pub mod tick;
mod types;

pub use types::*;

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
    pub(crate) fn new(
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
        // Sec L2: per-game spectator cap (config.network.max_spectators_per_game),
        // enforced in the tick task — the sole owner of `players` (see
        // `add_spectator_capped`).
        let spectator_cap = config.network.max_spectators_per_game;

        // Setup phase (game creation, city assignment, starting recruits,
        // replay-recorder init, initial full-state send, LLM client/agent
        // creation, channel creation) — moved to `setup::prepare_game`.
        let prepared = setup::prepare_game(
            map,
            map_name,
            settings,
            players,
            llm_configs,
            &room_id,
            config,
            storage,
        );

        // --- Spawn LLM agent tasks (one per agent) ---
        let (mut spawned_handles, reason_rx) = llm_tasks::spawn_llm_agents(
            &prepared.state_broadcasts,
            &prepared.cmd_tx,
            prepared.llm_pairs,
        );

        // --- Spawn advisor tasks (one per enabled advisor) ---
        let advice_rx = llm_tasks::spawn_advisors(
            &mut spawned_handles,
            &prepared.state_broadcasts,
            &prepared.advisors,
            prepared.advisor_client_opt,
        );

        // Shared per-game debug stats, updated by the tick loop and read by /debug/stats.
        let game_stats = Arc::new(std::sync::Mutex::new(DebugGameStats::default()));
        // A second Arc so the loop-use and the final GameRoom placement don't conflict
        // (the coroutine would otherwise move `game_stats` and reject the later use).
        let game_stats_room = game_stats.clone();

        tick::TickLoop {
            game: prepared.game,
            players: prepared.players,
            advisors: prepared.advisors,
            server_state,
            state_broadcasts: prepared.state_broadcasts,
            spawned_handles,
            advice_rx,
            reason_rx,
            cmd_rx: prepared.cmd_rx,
            spectator_cap,
            interval_ms: prepared.interval_ms,
            storage_clone: prepared.storage_clone,
            room_id,
            game_start_time: prepared.game_start_time,
            settings_json: prepared.settings_json,
            record_pid: prepared.record_pid,
            replay_recorder: prepared.replay_recorder,
            game_stats,
            spectator_vision,
        }
        .spawn();

        GameRoom {
            game_id,
            command_tx: prepared.cmd_tx,
            storage: prepared.storage,
            spectator_vision,
            stats: game_stats_room,
        }
    }
}
