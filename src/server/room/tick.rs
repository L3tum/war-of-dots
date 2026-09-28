//! The per-game tick loop, extracted verbatim from the inline
//! `tokio::spawn(async move { ... })` closure in `GameRoom::new`
//! (`room/mod.rs`). `TickLoop` owns exactly the locals that closure
//! captured; `spawn` wraps `run` in the same `tokio::spawn`, and the
//! caller drops the returned `JoinHandle` exactly as the old inline
//! call did (dropping a tokio handle does not abort the task).

use std::collections::HashSet;
use std::io::Write;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use bytes::Bytes;
use tokio::sync::{mpsc, Mutex};
use tokio::time::Duration;

use super::add_spectator_capped;
use crate::game::game::Game;
use crate::game::NetworkBaselines;
use crate::server::delta::{
    build_delta_inner, build_llm_state, init_unit_baselines, run_network_phase,
    update_unit_baselines, NetworkPhaseResult, TickDiffs, UnitBaselineStore,
};
use crate::server::game_server::GameServer;
use crate::server::protocol::{
    encode_message, encode_message_with_hint, estimate_delta_size, ServerMessage,
};
use crate::server::room::spectator::SpectatorUnionCache;
use crate::server::room::types::{Command, DebugGameStats, PlayerConnection, PlayerSender};
use crate::storage::{persist_game_end, Storage};

/// The per-game tick task's state — exactly the locals the inline
/// `tokio::spawn(async move { ... })` closure in `GameRoom::new`
/// captured. Constructed at the call site with a plain struct literal.
pub(crate) struct TickLoop {
    pub(super) game: Game,
    pub(super) players: Vec<PlayerConnection>,
    /// HIGH #1: per-player advisor enable flags, shared (by `Arc`) with
    /// every spawned advisor task. `ToggleAdvisor` writes the flag;
    /// each advisor task reads it before every 60 s LLM call. Disabled
    /// by default (setup.rs). Replaces the dead `HashMap<String, Advisor>`
    /// — the spawned tasks never read it, so the toggle did nothing.
    pub(super) advisor_enabled: std::collections::HashMap<String, Arc<AtomicBool>>,
    pub(super) server_state: Arc<Mutex<GameServer>>,
    pub(super) state_broadcasts: std::collections::HashMap<
        String,
        tokio::sync::broadcast::Sender<crate::llm::agent::GameState>,
    >,
    pub(super) spawned_handles: Vec<tokio::task::JoinHandle<()>>,
    pub(super) advice_rx: mpsc::Receiver<(String, ServerMessage)>,
    pub(super) reason_rx: mpsc::Receiver<(String, String, String)>,
    pub(super) cmd_rx: mpsc::UnboundedReceiver<Command>,
    pub(super) spectator_cap: usize,
    pub(super) interval_ms: u64,
    pub(super) storage_clone: Storage,
    pub(super) room_id: String,
    pub(super) game_start_time: u64,
    pub(super) settings_json: String,
    pub(super) record_pid: Option<String>,
    /// Arch #13: line-oriented sender to the dedicated replay writer task
    /// ([`replay_writer_task`]) — the tick task no longer touches the
    /// `BufWriter<File>` itself. Channel ordering = file ordering.
    pub(super) replay_tx: Option<mpsc::UnboundedSender<String>>,
    pub(super) game_stats: Arc<std::sync::Mutex<DebugGameStats>>,
    pub(super) spectator_vision: bool,
}

/// Arch #5: the single game-end path shared by ALL four terminal branches
/// (draw, last player standing, abandoned, room empty). Each branch calls
/// this and then `break`s.
///
/// `remove_room: true` everywhere (HIGH #2): finished rooms must leave
/// `server.games`. The old Draw / last-player-standing branches skipped the
/// removal, so dead rooms sat in the map forever and the spectator handler
/// could pick one — its `AddSpectator` send then failed silently (`.ok()`)
/// and the spectator WebSocket stayed open with no room behind it.
///
/// Persists the game-end record through the SYNC `persist_game_end` (there is
/// no async variant of it; `crate::storage`'s module docs keep the tick task
/// on the sync path — it already runs on a worker thread, and game end is a
/// once-per-game event, not a per-tick one), broadcasts `GameOver` to every
/// remaining connection (a no-op when the room is empty), removes the room
/// from `server.games`, and aborts the spawned LLM agent/advisor handles.
#[allow(clippy::too_many_arguments)]
async fn finish_game(
    players: &mut [PlayerConnection],
    spawned_handles: Vec<tokio::task::JoinHandle<()>>,
    server_state: &Arc<Mutex<GameServer>>,
    storage: &Storage,
    room_id: &str,
    game_start_time: u64,
    settings_json: &str,
    winners: &[&str],
    player_ids: &[&str],
    game: &Game,
    winner: &str,
    message: &str,
    remove_room: bool,
) {
    persist_game_end(
        storage,
        room_id,
        game_start_time,
        settings_json,
        winners,
        player_ids,
        game,
    );
    if !players.is_empty() {
        let msg = ServerMessage::GameOver {
            winner: winner.to_string(),
            game_id: room_id.to_string(),
            message: message.to_string(),
        };
        let encoded = Bytes::from(encode_message(&msg));
        for player in players.iter() {
            let _ = player.tx().send_reliable(encoded.clone()).await;
        }
    }
    if remove_room {
        let server = server_state.lock().await;
        let mut games = server.games.lock().await;
        games.remove(room_id);
    }
    for handle in spawned_handles {
        handle.abort();
    }
}

/// Arch #4: the split between what a [`Command`] can apply against the game
/// object synchronously and what needs tick-local context (the advisor flag
/// map, the spectator cap + `players` ownership).
enum CommandOutcome {
    /// Fully applied synchronously against `game` — nothing more to do.
    Applied,
    /// Needs the tick task's `advisor_enabled` flag map (HIGH #1).
    ToggleAdvisor { player_id: String, enabled: bool },
    /// Needs the tick task's `players` (its sole owner) + the spectator cap.
    AddSpectator(PlayerSender),
}

/// Arch #4: single synchronous half of the command dispatch, shared by
/// BOTH `select!` arms (the per-tick `try_recv` drain and the blocking
/// `recv` arm). Game-object commands are applied here; everything else is
/// returned as a [`CommandOutcome`] for [`dispatch_outcome`].
fn apply_command(cmd: Command, game: &mut Game) -> CommandOutcome {
    match cmd {
        Command::MoveTroop {
            unit_id,
            owner_id,
            path,
        } => {
            // Ownership is enforced inside game.move_troops (single check).
            game.move_troops(&unit_id, &owner_id, path);
            CommandOutcome::Applied
        }
        Command::RecruitUnit {
            city_id,
            unit_type: unit_type_name,
            owner,
        } => {
            if let Some(unit_type) = game
                .settings()
                .unit_types
                .iter()
                .find(|u| u.name == unit_type_name)
                .cloned()
            {
                let _ = game.recruit_unit(&city_id, &unit_type, &owner);
            }
            CommandOutcome::Applied
        }
        Command::ToggleHealing {
            unit_id,
            owner_id,
            enabled,
        } => {
            game.toggle_healing(&unit_id, &owner_id, enabled);
            CommandOutcome::Applied
        }
        Command::ToggleResupply {
            unit_id,
            owner_id,
            enabled,
        } => {
            game.toggle_resupply(&unit_id, &owner_id, enabled);
            CommandOutcome::Applied
        }
        Command::UpgradeCity { city_id, owner } => {
            let _ = game.upgrade_city(&city_id, &owner);
            CommandOutcome::Applied
        }
        Command::EmbarkUnit { unit_id, player_id } => {
            game.embark_unit(&unit_id, &player_id);
            CommandOutcome::Applied
        }
        Command::DisembarkUnit { unit_id, player_id } => {
            game.disembark_unit(&unit_id, &player_id);
            CommandOutcome::Applied
        }
        // Tick-local: the advisor flag map lives in the tick task.
        Command::ToggleAdvisor { player_id, enabled } => {
            CommandOutcome::ToggleAdvisor { player_id, enabled }
        }
        // Tick-local: `players` is owned by the tick task (Sec L2 cap).
        Command::AddSpectator(sender) => CommandOutcome::AddSpectator(sender),
    }
}

/// Arch #4: tick-local follow-up for the [`CommandOutcome`]s that
/// [`apply_command`] cannot apply against the game object. Both `select!`
/// arms call `apply_command` + this, so the dispatch is one code path with
/// identical behavior in both arms.
async fn dispatch_outcome(
    outcome: CommandOutcome,
    players: &mut Vec<PlayerConnection>,
    game: &Game,
    advisor_enabled: &std::collections::HashMap<String, Arc<AtomicBool>>,
    spectator_cap: usize,
) {
    match outcome {
        CommandOutcome::Applied => {}
        CommandOutcome::ToggleAdvisor { player_id, enabled } => {
            // HIGH #1: the shared flag — each spawned advisor task reads
            // this exact `Arc` before every 60 s iteration. Unknown pids
            // are a no-op (same as the old `advisors.get_mut` miss).
            if let Some(flag) = advisor_enabled.get(&player_id) {
                flag.store(enabled, Ordering::Relaxed);
            }
        }
        CommandOutcome::AddSpectator(sender) => {
            // Sec L2: per-game spectator cap (the tick
            // task is the sole owner of `players`).
            add_spectator_capped(spectator_cap, players, game, sender).await;
        }
    }
}

/// Arch #13: the dedicated replay writer task — keeps the JSONL file IO
/// (BufWriter + flushes) off the async tick path. The tick task owns
/// [`TickLoop::replay_tx`] (line-oriented); this task owns the
/// `BufWriter<File>`, writes each line as it arrives, and flushes on its
/// own ~1 s cadence (replacing the old in-tick every-60-ticks flush).
/// The terminal `game_end` marker arrives as the LAST line before the tick
/// task drops its sender, so channel ordering guarantees it is written
/// last; on channel close the task flushes and exits.
///
/// Deliberately NOT in `spawned_handles`: the game-end path aborts those
/// handles, and this task must survive them to write + flush the terminal
/// marker. Its `JoinHandle` is dropped at spawn (dropping a tokio handle
/// does not abort the task — same as today's handles).
pub(crate) async fn replay_writer_task(
    path: std::path::PathBuf,
    mut rx: mpsc::UnboundedReceiver<String>,
) {
    // Best-effort: a disk failure degrades to "no replay", never a broken
    // game (the tick task's sends then just fail and are ignored).
    let mut rec = match std::fs::File::create(&path) {
        Ok(f) => std::io::BufWriter::new(f),
        Err(e) => {
            tracing::warn!(path = %path.display(), %e, "replay: cannot create file");
            return;
        }
    };
    let mut flush_interval = tokio::time::interval(Duration::from_secs(1));
    flush_interval.tick().await; // the first tick fires immediately — skip it
    let mut header_flushed = false;
    loop {
        tokio::select! {
            line = rx.recv() => match line {
                Some(line) => {
                    let _ = writeln!(rec, "{line}");
                    if !header_flushed {
                        // Flush the header (full_state) immediately so the
                        // file is playable even if the process dies
                        // mid-game (the old setup-side flush guarantee).
                        let _ = rec.flush();
                        header_flushed = true;
                    }
                }
                None => {
                    // Channel closed — the tick task is gone and every line
                    // (including the terminal game_end marker) is written;
                    // final flush, then exit.
                    let _ = rec.flush();
                    return;
                }
            },
            _ = flush_interval.tick() => {
                let _ = rec.flush();
            }
        }
    }
}

impl TickLoop {
    /// The tick loop — the former inline closure in `GameRoom::new`,
    /// verbatim. The leading `let mut X = X;` / `let X = X;` statements
    /// are kept: they move each field into a local exactly as the old
    /// closure's captured locals were moved.
    pub(crate) async fn run(self) {
        let TickLoop {
            game,
            players,
            advisor_enabled,
            server_state,
            state_broadcasts,
            spawned_handles,
            mut advice_rx,
            mut reason_rx,
            mut cmd_rx,
            spectator_cap,
            interval_ms,
            storage_clone,
            room_id,
            game_start_time,
            settings_json,
            record_pid,
            replay_tx,
            game_stats,
            spectator_vision,
        } = self;

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
        // Perf #2: city snapshot cache — the per-tick `query_cities()` full
        // clone (C city String allocations EVERY tick, idle or not) is
        // replaced by a generation-checked `Arc` cache: the `Arc<Vec<City>>`
        // is rebuilt only when `Game::city_generation` moves (a city was
        // captured, upgraded, or re-assigned), and an unchanged tick
        // reuses the cached Arc (one refcount bump, zero content clone).
        // The baseline commit aliases the same Arc (shared allocation), so
        // the old per-tick commit re-clone is gone as well.
        let mut city_cache: Option<Arc<Vec<crate::game::economy::City>>> = None;
        let mut city_cache_gen: u64 = 0;
        // Perf #3: persistent per-tick funds scratch map. Keys are the
        // stable player-id set (seeded once — the player set is fixed at
        // game start; a defensive insert-if-missing below keeps this
        // correct even if that ever changes); values are refilled IN PLACE
        // every tick, so the per-tick `all_player_funds()` allocation
        // (fresh HashMap bucket table + P key-String clones) is gone.
        // `NetworkBaselines::commit` swaps this scratch with `prev_funds`
        // (drain-back), so it is refilled next tick, never reallocated.
        let mut funds_scratch: std::collections::HashMap<String, u32> = game
            .players()
            .keys()
            .map(|k| (k.clone(), game.player_funds(k)))
            .collect();
        let mut players = players;
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
                    // Arch #4: one dispatch path shared with the blocking
                    // `recv` arm below — identical behavior in both.
                    while let Ok(cmd) = cmd_rx.try_recv() {
                        let outcome = apply_command(cmd, &mut game);
                        dispatch_outcome(
                            outcome,
                            &mut players,
                            &game,
                            &advisor_enabled,
                            spectator_cap,
                        )
                        .await;
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
                    // Perf #2: generation-checked cache — if no city
                    // mutation bumped `city_generation` since the cache was
                    // built, reuse the cached `Arc<Vec<City>>` (refcount
                    // bump only); else rebuild from `query_cities`. The
                    // shared Arc is moved into `TickDiffs` below (the cache
                    // keeps its own ref) and committed back into the
                    // baseline, which now aliases the same allocation.
                    let city_gen = game.city_generation();
                    let current_cities: Arc<Vec<crate::game::economy::City>> =
                        match &city_cache {
                            Some(cached) if city_cache_gen == city_gen => Arc::clone(cached),
                            _ => {
                                let fresh = Arc::new(game.query_cities());
                                city_cache = Some(Arc::clone(&fresh));
                                city_cache_gen = city_gen;
                                fresh
                            }
                        };

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
                        // Arch #5: unified game-end (HIGH #2: room removed
                        // from server.games like every other end branch).
                        finish_game(
                            &mut players,
                            spawned_handles,
                            &server_state,
                            &storage_clone,
                            &room_id,
                            game_start_time,
                            &settings_json,
                            &[""],
                            &player_ids,
                            &game,
                            "Draw",
                            "⚖️ Draw — all players eliminated",
                            true,
                        )
                        .await;
                        break;
                    }
                    if total_players > 1 && active_count == 1 {
                        let winner_id = sole_active.unwrap_or_default();
                        let winner_name = game.players().get(winner_id)
                            .map(|p| p.name.clone())
                            .unwrap_or_else(|| winner_id.to_string());
                        // Arch #5: unified game-end (HIGH #2: room removed
                        // from server.games like every other end branch).
                        finish_game(
                            &mut players,
                            spawned_handles,
                            &server_state,
                            &storage_clone,
                            &room_id,
                            game_start_time,
                            &settings_json,
                            &[winner_id],
                            &player_ids,
                            &game,
                            &winner_name,
                            "Game over — last player standing wins",
                            true,
                        )
                        .await;
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

                    // Perf #3: refill the persistent funds scratch IN PLACE
                    // (values only — the key Strings survive the `commit`
                    // swap, so a steady-state tick performs zero funds-map
                    // allocation). The old `all_player_funds()` call inside
                    // `run_network_phase` built a fresh HashMap + P key
                    // clones every tick for this exact data.
                    for pid in game.players().keys() {
                        match funds_scratch.get_mut(pid) {
                            Some(v) => *v = game.player_funds(pid),
                            // Defensive: a player id missing from the
                            // scratch (the player set is fixed at game
                            // start, so this never fires in steady state).
                            None => {
                                funds_scratch
                                    .insert(pid.clone(), game.player_funds(pid));
                            }
                        }
                    }

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
                        &mut funds_scratch,
                        true, // W9 splice path: on — A/B confirmed ~1.5% off the network phase (byte-for-byte verified encoder)
                        record_pid.as_deref(),
                    );

                    // Replay: log the recording viewpoint's delta for this
                    // tick (idle ticks build no delta — nothing was sent
                    // on the wire either, so nothing is logged).
                    // Arch #13: line-oriented send to the dedicated writer
                    // task — no in-tick BufWriter, no in-tick flush (the
                    // writer flushes on its own ~1 s cadence). The send is
                    // a cheap short-line allocation, bounded by tick count.
                    let recorded_delta = phase.recorded_delta.take();
                    if let (Some(d), Some(tx)) = (&recorded_delta, replay_tx.as_ref()) {
                        if let Ok(v) = serde_json::to_value(d) {
                            let _ = tx.send(format!("{{\"delta\":{}}}", v));
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
                        // Arch #5: unified game-end (this branch already
                        // removed the room; the behavior is unchanged).
                        let winner_id = game.determine_winner();
                        let player_ids: Vec<&str> = game.players().keys().map(|s| s.as_str()).collect();
                        finish_game(
                            &mut players,
                            spawned_handles,
                            &server_state,
                            &storage_clone,
                            &room_id,
                            game_start_time,
                            &settings_json,
                            &[winner_id.as_deref().unwrap_or("")],
                            &player_ids,
                            &game,
                            "Abandoned",
                            "Game abandoned — no human players remaining",
                            true,
                        )
                        .await;
                        break;
                    }

                    if players.is_empty() {
                        // Arch #5: unified game-end (this branch already
                        // removed the room; no GameOver is sent — there is
                        // no one left in the room, so `finish_game`'s send
                        // loop is a no-op here).
                        let winner_id = game.determine_winner();
                        let player_ids: Vec<&str> = game.players().keys().map(|s| s.as_str()).collect();
                        finish_game(
                            &mut players,
                            spawned_handles,
                            &server_state,
                            &storage_clone,
                            &room_id,
                            game_start_time,
                            &settings_json,
                            &[winner_id.as_deref().unwrap_or("")],
                            &player_ids,
                            &game,
                            "Abandoned",
                            "Game abandoned — no human players remaining",
                            true,
                        )
                        .await;
                        break;
                    }
                }
                // Blocking command recv for non-tick-driven commands (player connections)
                cmd = cmd_rx.recv() => {
                    match cmd {
                        // Arch #4: same dispatch as the tick arm above.
                        Some(cmd) => {
                            let outcome = apply_command(cmd, &mut game);
                            dispatch_outcome(
                                outcome,
                                &mut players,
                                &game,
                                &advisor_enabled,
                                spectator_cap,
                            )
                            .await;
                        }
                        None => break,
                    }
                }
            }
        }

        // Replay: terminal marker (Arch #13) — sent as the LAST line before
        // the sender is dropped, so the channel preserves ordering and the
        // writer task (NOT in spawned_handles, so it survives the game-end
        // aborts above) writes it after the spawned handles are aborted,
        // then flushes on channel close. Best-effort — the game is over
        // either way.
        if let Some(tx) = replay_tx.as_ref() {
            let end_time = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_secs())
                .unwrap_or(0);
            let _ = tx.send(format!("{{\"event\":\"game_end\",\"time\":{}}}", end_time));
        }
        // `replay_tx` is dropped here (end of `run`), closing the channel:
        // the writer task sees `None`, flushes, and exits.
    }

    /// Wraps [`TickLoop::run`] in the same `tokio::spawn` the inline
    /// closure used. The caller drops the returned handle, exactly as the
    /// old inline `tokio::spawn(async move { ... });` call did (dropping a
    /// tokio `JoinHandle` does not abort the task).
    pub(crate) fn spawn(self) -> tokio::task::JoinHandle<()> {
        tokio::spawn(self.run())
    }
}
