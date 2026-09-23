//! The per-game tick loop, extracted verbatim from the inline
//! `tokio::spawn(async move { ... })` closure in `GameRoom::new`
//! (`room/mod.rs`). `TickLoop` owns exactly the locals that closure
//! captured; `spawn` wraps `run` in the same `tokio::spawn`, and the
//! caller drops the returned `JoinHandle` exactly as the old inline
//! call did (dropping a tokio handle does not abort the task).

use std::collections::HashSet;
use std::io::Write;
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
use crate::server::handlers::http::persist_game_end;
use crate::server::protocol::{
    encode_message, encode_message_with_hint, estimate_delta_size, ServerMessage,
};
use crate::server::room::spectator::SpectatorUnionCache;
use crate::server::room::types::{Command, DebugGameStats, PlayerConnection};

/// The per-game tick task's state — exactly the locals the inline
/// `tokio::spawn(async move { ... })` closure in `GameRoom::new`
/// captured. Constructed at the call site with a plain struct literal.
pub(crate) struct TickLoop {
    pub(super) game: Game,
    pub(super) players: Vec<PlayerConnection>,
    pub(super) advisors: std::collections::HashMap<String, crate::llm::advisor::Advisor>,
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
    pub(super) storage_clone: crate::storage::Storage,
    pub(super) room_id: String,
    pub(super) game_start_time: u64,
    pub(super) settings_json: String,
    pub(super) record_pid: Option<String>,
    pub(super) replay_recorder: Option<std::io::BufWriter<std::fs::File>>,
    pub(super) game_stats: Arc<std::sync::Mutex<DebugGameStats>>,
    pub(super) spectator_vision: bool,
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
            advisors,
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
            mut replay_recorder,
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
    }

    /// Wraps [`TickLoop::run`] in the same `tokio::spawn` the inline
    /// closure used. The caller drops the returned handle, exactly as the
    /// old inline `tokio::spawn(async move { ... });` call did (dropping a
    /// tokio `JoinHandle` does not abort the task).
    pub(crate) fn spawn(self) -> tokio::task::JoinHandle<()> {
        tokio::spawn(self.run())
    }
}
