# Plan: Replace GameRoom Tick with Game::tick_game() + Separate LLM/Advisor Threads

## Goal

Replace the `GameRoom` tick loop's no-op `game.tick_number()` with the actual
`Game::tick_game()` system pipeline, then move LLM agent steps and advisor calls
to separate `tokio::spawn` tasks. This decouples game simulation from external
API calls, preventing LLM round-trips from blocking the main tick loop.

## Current Architecture (Baseline)

**File**: `src/server/game_server.rs` (lines ~220–600)

The `GameRoom::new()` spawns a single async task that runs the main game loop:

```
loop {
    // 1. LLM agent steps (sequential, blocks tick)
    for (agent, client) in &llm_agents {
        let state = build_llm_state(&game, &agent.id);
        let cmds = agent.step(client, &state).await;  // <-- blocks!
        for cmd in cmds {
            let _ = cmd_tx.send((agent.id.clone(), cmd, tick_count)).await;
        }
    }

    // 2. Reason log broadcast

    // 3. Advisor steps (sequential, blocks tick)
    if tick % 60 == 0 {
        for player in &players {
            let state = build_llm_state(&game, ...);
            let advice = advisor.get_advice(client, &state).await;  // <-- blocks!
            // send to player
        }
    }

    // 4. game.tick_number();  // <-- NO-OP: only increments counter, no systems run!

    // 5. Command processing from cmd_rx

    // 6. Game over check

    // 7. Delta build + broadcast

    // 8. tick.sleep(delay)
}
```

**`Game::tick_game()`** (`src/game/game.rs` line ~182): runs all 12 systems in
deterministic order via `TickContext`. It is **not called** from the game room
tick loop — only from benchmarks and tests.

**`Game::tick_number()`** (`src/game/game.rs` line ~580): only increments the
tick counter. This is what the game room calls today, meaning systems are never
run during normal gameplay.

**`build_llm_state()`** (`src/server/delta.rs` line ~262): takes `&Game`
(read-only) and returns a cloned `GameState` struct. All underlying methods
(`vision_grid_2d`, `unit_states`, `query_cities`, `all_player_funds`) accept
`&self`. No `&mut` access needed for state reads.

## Key Problems

1. **Systems never run**: `game.tick_number()` is a no-op that only increments
   the counter. The 12 game systems (movement, combat, economy, etc.) defined in
   `tick_game()` are never executed during normal gameplay.
2. **LLM blocks tick**: Agent `step()` and advisor `get_advice()` are awaited
   sequentially in the main loop, blocking all game logic.
3. **Tick rate depends on LLM latency**: The `tick.sleep()` fires after all
   LLM calls complete, so tick rate is unbounded by API response times.

## Design

### High-Level Architecture After Refactor

```
Main tick loop (tokio task, ~60 ticks/sec, owns Game):
├── Process incoming commands from cmd_rx
├── game.tick_game(&mut ctx)          // runs all 12 systems
├── Every 60th tick: broadcast GameState snapshots
├── Process advice/reason_log messages
├── Build delta + broadcast
├── Game over check
└── tick.sleep(delay)

LLM Agent task (one per agent, spawned):
├── Subscribe to state broadcast channel
├── loop:
│   ├── recv latest GameState snapshot
│   ├── agent.step(&client, &state)   // LLM API call (no locks)
│   └── send commands to cmd_tx

Advisor task (one per advisor, spawned):
├── Subscribe to state broadcast channel
├── loop:
│   ├── recv latest GameState snapshot
│   ├── advisor.get_advice(&client, &state)  // LLM API call (no locks)
│   └── send advice to main loop via advice channel
```

### State Sharing: Broadcast Channel (Lock-Free)

**No `Arc<Mutex<Game>>` needed.** The `Game` stays owned by the main tick
loop. State is shared via `tokio::sync::broadcast` channels.

- **Main loop**: After every 60th tick (~1 second), builds `GameState` snapshots
  for each player via `build_llm_state(&game, player_id)` and sends them on
  per-player broadcast channels.
- **LLM tasks**: Subscribe to their player's broadcast channel. Receive
  `GameState` snapshots passively. No locks, no shared ownership.
- **Advisor tasks**: Same — subscribe to their player's channel.

**Why every 60th tick?** LLM responses take ~10 seconds anyway. A 1-second-old
snapshot is indistinguishable from "current" from the LLM's perspective. This
reduces channel pressure from ~60 broadcasts/sec to 1 broadcast/sec per player.

**Channel type**: `tokio::sync::broadcast` with a small buffer (e.g., 2).
Subscribers that fall behind get the latest message (lag is acceptable — we
just want the most recent snapshot).

### Channel Design

| Channel | Direction | Type | Purpose |
|---------|-----------|------|---------|
| `cmd_tx` / `cmd_rx` | LLM tasks → main loop | `mpsc` | Commands to execute on Game |
| `advice_tx` / `advice_rx` | Advisor tasks → main loop | `mpsc` | Advice messages to send to players |
| `reason_tx` / `reason_rx` | LLM tasks → main loop | `mpsc` | Reason log entries to broadcast |
| `state_tx_*` / `state_rx_*` | Main loop → LLM/advisor tasks | `broadcast` | GameState snapshots (per player) |

### Delay Handling

- **LLM agents**: Each `LlmAgent` has a `thinking_delay`. The agent task sleeps
  this duration between steps using `tokio::time::sleep()`. The existing
  `last_call_time` check in `agent.step()` becomes a secondary safety net.

- **Advisors**: Configurable interval via game setting, default **1 minute**
  (`Duration::from_secs(60)`). Read from game settings at room creation.

- **State snapshots**: Broadcast every 60 ticks (~1 second). LLM/advisor tasks
  receive whichever snapshot arrives — they don't control the timing of state
  updates, only when they act on them.

## Detailed Changes

### 1. `src/server/game_server.rs` — Refactor Game Room Tick Loop

**Current**: Lines ~220–600 (the spawned async task inside `GameRoom::new()`)

**Changes**:

a. **Game stays owned by the main loop** — no `Arc` or `Mutex`.

b. **Create broadcast channels per player** (before spawning tasks):

```rust
// One broadcast channel per player for state snapshots
// Buffer of 2 is sufficient — subscribers just want the latest
let mut state_broadcasts: HashMap<String, tokio::sync::broadcast::Sender<crate::llm::agent::GameState>> = HashMap::new();
for player_id in game.players().keys().cloned() {
    let (tx, _) = tokio::sync::broadcast::channel::<crate::llm::agent::GameState>(2);
    state_broadcasts.insert(player_id, tx);
}
```

c. **Create additional channels** for advice and reason logs:

```rust
let (advice_tx, mut advice_rx) = tokio::sync::mpsc::channel::<(String, ServerMessage)>(64);
let (reason_tx, mut reason_rx) = tokio::sync::mpsc::channel::<(String, String, String)>(64);
```

d. **Spawn one LLM task per agent** (before the main loop):

```rust
let mut spawned_handles: Vec<tokio::task::JoinHandle<()>> = Vec::new();

for (mut agent, client) in llm_agents {
    let player_id = agent.id.clone();
    let agent_name = agent.name.clone();

    let state_rx = state_broadcasts.get(&player_id)
        .expect("LLM player should have broadcast channel")
        .subscribe();

    let cmd_tx_clone = cmd_tx.clone();
    let reason_tx_clone = reason_tx.clone();

    let handle = tokio::spawn(async move {
        let mut state_rx = state_rx;
        let mut latest_state: Option<crate::llm::agent::GameState> = None;

        loop {
            tokio::select! {
                // Receive state snapshots
                result = state_rx.recv() => {
                    match result {
                        Ok(state) => latest_state = Some(state),
                        Err(broadcast::error::RecvError::Lagged(mut new_rx)) => {
                            // Drain to latest
                            if let Ok(state) = new_rx.try_recv() {
                                latest_state = Some(state);
                            }
                            state_rx = new_rx;
                        }
                        Err(broadcast::error::RecvError::Closed) => break,
                    }
                }
                // Respect thinking delay, then act on latest state
                _ = tokio::time::sleep(agent.thinking_delay) => {
                    if let Some(state) = latest_state.take() {
                        match agent.step(&client, &state).await {
                            Ok(cmds) => {
                                for cmd in cmds {
                                    let _ = cmd_tx_clone.send((player_id.clone(), cmd, 0)).await;
                                }
                                // Emit recent reason log entries
                                for entry in agent.reason_log.iter().rev().take(5) {
                                    let _ = reason_tx_clone.send((
                                        agent_name.clone(),
                                        entry.action.clone(),
                                        entry.reason.clone(),
                                    )).await;
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
```

**Why `tokio::select!`**: The task has two independent async operations —
receiving state snapshots and respecting the thinking delay. `select!` lets
both progress concurrently. The task always has the latest state available
and acts on it after the thinking delay elapses. If no snapshot has arrived
yet (game just started), the task waits patiently.

e. **Spawn one advisor task per advisor-enabled player**:

```rust
let advisor_interval = Duration::from_secs(60); // Default; read from settings if available

for (player_id, _advisor) in advisors {
    let state_rx = state_broadcasts.get(&player_id)
        .expect("Advisor player should have broadcast channel")
        .subscribe();

    let advice_tx_clone = advice_tx.clone();
    let advisor_client = advisor_client_opt.clone();

    let handle = tokio::spawn(async move {
        let mut state_rx = state_rx;
        loop {
            tokio::time::sleep(advisor_interval).await;

            let state = match state_rx.recv().await {
                Ok(s) => s,
                Err(broadcast::error::RecvError::Lagged(mut new_rx)) => {
                    if let Ok(s) = new_rx.try_recv() {
                        state_rx = new_rx;
                        s
                    } else {
                        state_rx = new_rx;
                        continue;
                    }
                }
                Err(broadcast::error::RecvError::Closed) => break,
            };

            if let Some(ref client) = advisor_client {
                let mut adv = Advisor::new();
                adv.set_enabled(true);
                if let Some(advice) = adv.get_advice(client, &state).await {
                    let msg = ServerMessage::Advice { text: advice.text };
                    let _ = advice_tx_clone.send((player_id.clone(), msg)).await;
                }
            }
        }
    });
    spawned_handles.push(handle);
}
```

f. **Simplified main tick loop**:

```rust
let mut last_delta_tick: Option<u32> = None;
let mut tick_count: u32 = 0;
let mut players = player_states;
let mut last_state = /* ... existing delta state tracking ... */;

loop {
    // --- Process commands from LLM agents & players (non-blocking) ---
    while let Ok((player_id, cmd, _tick)) = cmd_rx.try_recv() {
        match cmd {
            Command::MoveTroop(troop_id, path) => {
                game.move_troop(&troop_id, &path, &player_id);
            }
            Command::RecruitUnit(city_id, unit_type_name, owner) => {
                if let Some(unit_type) = game.settings().unit_types.iter()
                    .find(|u| u.name == unit_type_name).cloned()
                {
                    let _ = game.recruit_unit(&city_id, &unit_type, &owner);
                }
            }
            // ... other command variants (same as existing) ...
        }
    }

    // --- Process advice messages (non-blocking) ---
    while let Ok((player_id, msg)) = advice_rx.try_recv() {
        let encoded = encode_message(&msg);
        for player in &players {
            if player.id().map_or(false, |id| *id == player_id) {
                let _ = player.tx().send(encoded.clone()).await;
            }
        }
    }

    // --- Process reason log messages (non-blocking) ---
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

    // --- Run game systems ---
    {
        let mut ctx = TickContext::default();
        game.tick_game(&mut ctx);
        tick_count += 1;
    }

    // --- Broadcast state snapshots every 60 ticks (~1 sec) ---
    if tick_count % 60 == 0 {
        for (player_id, tx) in &state_broadcasts {
            let state = build_llm_state(&game, player_id);
            let _ = tx.send(state); // Drop error if no subscribers
        }
    }

    // --- Check game over ---
    let winner_id_opt = determine_winner(&game);
    let player_ids: Vec<String> = game.players().keys().cloned().collect();
    let active_count = /* ... existing logic ... */;

    if active_count <= 1 {
        // Game over logic — record storage, broadcast GameOver
        // Abort spawned tasks
        for handle in spawned_handles {
            handle.abort();
        }
        break;
    }

    // --- Build & broadcast delta ---
    {
        let current_tick = game.tick();
        if last_delta_tick != Some(current_tick) {
            let delta = build_delta(&game, &mut last_state, current_tick);
            let encoded = encode_message(&delta);
            for player in &players {
                let _ = player.tx().send(encoded.clone()).await;
            }
            last_delta_tick = Some(current_tick);
        }
    }

    // --- Handle disconnects ---
    // ... existing logic ...

    // --- Tick at fixed rate ---
    tick.sleep().await;
}
```

### 2. `src/llm/agent.rs` — No Changes

The `step()` signature remains `async fn step(&mut self, client, state) -> Result<Vec<Command>>`.
The `reason_log` is read by the spawned task after each step. No API changes needed.

### 3. `src/llm/advisor.rs` — No Changes

The `get_advice()` signature remains unchanged. The spawned task handles the
interval timing.

### 4. `src/game/game.rs` — No Changes

`tick_game()` already exists and is complete. `tick_number()` can remain for
backward compatibility (benchmarks/tests).

### 5. `src/server/delta.rs` — No Changes

`build_llm_state()` takes `&Game` and returns a cloned `GameState`. Works as-is.

## File Change Summary

| File | Change |
|------|--------|
| `src/server/game_server.rs` | Major: spawn LLM/advisor tasks with broadcast channels, simplify main loop, call `tick_game()` |
| `src/llm/agent.rs` | None |
| `src/llm/advisor.rs` | None |
| `src/game/game.rs` | None |
| `src/server/delta.rs` | None |

## Risks & Mitigations

1. **Stale state**: Snapshots are up to 1 second old (60 ticks at 60 TPS).
   **Mitigation**: LLM responses take ~10 seconds, so 1-second staleness is
   negligible. Strategic decisions don't require frame-perfect data.

2. **Broadcast channel lag**: If the main loop sends faster than subscribers
   receive, `Lagged` errors occur. **Mitigation**: Buffer size of 2 with
   `Lagged` handling (re-subscribe, skip step). State snapshots are cheap to
   regenerate.

3. **Advisor state persistence**: Each advisor step creates a new `Advisor`
   instance, losing `last_suggestion`. **Mitigation**: Store `last_suggestion`
   in the task's local state. Only matters for deduplication of advice text —
   not critical.

4. **Task cleanup on game end**: Spawned tasks need to stop. **Mitigation**:
   Track `JoinHandle`s in a `Vec`. Call `handle.abort()` on game end. Drop
   broadcast senders to close subscriber channels.

5. **Command tick number**: LLM commands sent via `cmd_tx` include a tick
   number, but LLM tasks don't know the current tick. **Mitigation**: Send
   tick number alongside state in the broadcast, or accept `tick=0` and let
   the main loop apply commands at the current tick. The tick number is used
   for replay/delta tracking — off-by-one is acceptable for LLM commands.

## Execution Order

1. Refactor `game_server.rs` tick loop:
   - Create broadcast channels per player
   - Spawn LLM agent tasks (subscribe to state channel, use `select!`)
   - Spawn advisor tasks (subscribe to state channel)
   - Simplify main loop: process commands → `tick_game()` → broadcast snapshots every 60 ticks → delta → sleep
2. Track `JoinHandle`s for cleanup on game end
3. Test with benchmark: verify tick rate is stable (~60 TPS) regardless of LLM latency
4. Test with LLM agents: verify commands flow correctly and state snapshots arrive
5. Test game over: verify tasks are cleaned up properly
