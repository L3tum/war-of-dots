# ECS Refactor with hecs — Full Rewrite

## Context

`game.rs` (~450 lines) and `game_server.rs` (~2450 lines) contain nearly all game logic in a monolithic structure. The current design uses `HashMap<String, Unit>` with pervasive cloning to work around borrow checker limitations. This is inefficient for a server that should handle many parallel games.

**Goals:**
1. Split monolithic files into well-organized modules
2. Replace HashMap-of-structs with a cache-friendly ECS (hecs crate)
3. Decompose the monolithic `tick()` into independent, query-based systems
4. Eliminate unnecessary cloning by using ECS queries with `&mut` borrows
5. Optimize hot paths: O(n²) combat, per-player delta builds, BFS encirclement checks

## Approach

Use **hecs** — the most performant, actively-maintained Rust ECS crate (used internally by Bevy). Archetypal storage gives cache-friendly iteration. No implicit system scheduling — we call systems explicitly from the game tick loop, which integrates cleanly with tokio.

### ECS Design Philosophy

- **Components are pure data** — no methods, just fields
- **Systems are standalone functions** — `fn system(world: &mut hecs::World, settings: &Settings)`
- **Game struct is a thin facade** — holds the `hecs::World`, settings, and dispatches systems in `tick()`
- **No entity IDs leak to the wire protocol** — we still serialize unit data by string ID for the client

### Component Decomposition

The current `Unit` struct (~25 fields) splits into focused components:

| Component | Fields | Purpose |
|-----------|--------|---------|
| `UnitOwner` | `player_id: String` | Ownership — used for queries by player |
| `UnitTypeId` | `name: String` | Index into `GameSettings.unit_types` |
| `UnitPosition` | `x: f32, y: f32` | Position — updated by movement |
| `UnitPath` | `path: Vec<(f32, f32)>` | Waypoints — consumed by movement |
| `UnitStats` | `men: f32, organisation: f32` | Dynamic stats — modified by combat/attrition |
| `UnitEquipment` | `equipment: Vec<Equipment>, max_equipment: Vec<Equipment>` | Supply tracking |
| `UnitFlags` | `healing, attacking, encircled, resupply_enabled, on_water: bool` | Boolean flags — bit-packed ideally |
| `UnitWaterTransition` | `embarking, disembarking: bool, ticks_left: u32, boat_supply, max_boat_supply` | Water state |
| `UnitDisplay` | `symbol: String` | Wire protocol only, rarely changes |

Singleton/world-level components:

| Component | Fields | Purpose |
|-----------|--------|---------|
| `TerrainGrid` | `grid: Vec<Vec<Terrain>>, width, height` | Shared terrain lookup |
| `VisionGrids` | `grids: HashMap<String, Vec<Vec<bool>>>` | Per-player vision |
| `BorderGrids` | `grids: HashMap<String, Vec<Vec<u32>>>` | Per-player borders |
| `CityData` | `cities: Vec<City>` | All cities (cities as entities is overkill for <20 cities) |
| `EconomyFunds` | `funds: HashMap<String, u32>` | Player funds |

Players (max 6) and cities (max ~20) don't benefit from ECS entities — they stay as small collections accessed by string key.

### System Decomposition

Each system is a standalone function that queries the world for the components it needs:

| System | Query | Description |
|--------|-------|-------------|
| `EconomySystem` | `&EconomyFunds` | Generate funds every 10 ticks |
| `MovementSystem` | `&mut UnitPosition, &mut UnitPath, &UnitTypeId, &UnitOwner, &UnitFlags` | Move units along paths with terrain speed |
| `WaterTransitionSystem` | `&mut UnitWaterTransition, &mut UnitFlags, &TerrainGrid` | Handle embark/disembark timers |
| `AttritionSystem` | `&mut UnitStats, &mut UnitEquipment, &UnitTypeId, &UnitFlags` | Equipment attrition, org drain, starvation |
| `CombatSystem` | `&mut UnitStats, &mut UnitEquipment, &UnitPosition, &TerrainGrid` | Resolve adjacent enemy combat (spatial hash for O(n) instead of O(n²)) |
| `VisionSystem` | `&UnitPosition, &UnitOwner, &TerrainGrid` | Update vision grids per player |
| `BorderSystem` | `&UnitPosition, &UnitOwner` | Update border grids per player |
| `HealingSystem` | `&mut UnitStats, &mut UnitEquipment, &UnitPosition, &UnitFlags, &UnitOwner, &CityData, &EconomyFunds` | Heal units near friendly cities |
| `SupplySystem` | `&mut UnitEquipment, &UnitPosition, &UnitFlags, &UnitOwner, &CityData, &EconomyFunds` | City supply resupply |
| `EncirclementSystem` | `&mut UnitFlags, &UnitPosition, &UnitOwner, &BorderGrids, &CityData` | BFS encirclement check |
| `SurrenderSystem` | `&mut UnitStats, &UnitFlags` | Check for unit surrenders (every 30 ticks) |
| `ReinforcementSystem` | `&mut UnitStats, &UnitEquipment, &UnitTypeId` | Organisation reinforcement with supply malus |
| `CityCaptureSystem` | `&UnitPosition, &UnitOwner, &CityData` | Capture cities when units stand on them |

### Key Optimizations

1. **Spatial hash for combat** — Instead of O(n²) pairwise distance check, bucket units by grid cell and only compare units in adjacent cells.
2. **No cloning in tick** — Systems query `&mut` components directly. The `prev_unit_owners` tracking stays but is a small HashMap.
3. **Batch delta computation** — Build deltas by querying ECS once, then diff per-player. The vision/border grid diffing already exists and stays the same.
4. **Arena-allocated string IDs** — Unit entity IDs from hecs are `u32`. We keep an `Entity -> String` map for wire protocol compatibility.

## Files to Create

### Game Module (`src/game/`)

```
src/game/
  mod.rs              — re-exports (updated)
  game.rs             — Game facade: World holder, tick() dispatch, public API
  components.rs       — All hecs component structs (UnitOwner, UnitPosition, etc.)
  world_ext.rs        — hecs::World extension trait for spawn helpers
  systems/
    mod.rs            — re-exports
    economy.rs        — EconomySystem
    movement.rs       — MovementSystem
    water.rs          — WaterTransitionSystem
    attrition.rs      — AttritionSystem
    combat.rs         — CombatSystem (refactored from game::combat + spatial hash)
    vision.rs         — VisionSystem
    border.rs         — BorderSystem
    healing.rs        — HealingSystem
    supply.rs         — SupplySystem
    encirclement.rs   — EncirclementSystem
    surrender.rs      — SurrenderSystem
    reinforce.rs      — ReinforcementSystem
    city_capture.rs   — CityCaptureSystem
    spatial_hash.rs   — Spatial hash grid for O(n) combat
```

### Server Module (`src/server/`)

```
src/server/
  mod.rs              — re-exports (updated)
  game_server.rs      — GameServer struct, run(), route setup (~300 lines)
  room.rs             — GameRoom, tick loop, command handling (~600 lines)
  delta.rs            — build_delta_inner, full_state builders from ECS queries (~500 lines)
  protocol.rs         — unchanged (wire format)
  lobby.rs            — unchanged
  handlers/
    mod.rs            — re-exports
    lobby.rs          — lobby message handlers (CreateLobby, JoinLobby, StartGame, etc.)
    game.rs           — game action handlers (MoveTroop, RecruitUnit, etc.)
    spectator.rs      — spectator WS handler
    replay.rs         — replay WS handler
    http.rs           — HTTP handlers (index, maps, replays, watch, debug/stats)
```

### New Top-Level Module

```
src/spatial.rs        — Spatial hash grid (shared between game combat and potentially other systems)
src/debug.rs          — DebugStats component, DebugSystem, /debug/stats endpoint (debug-only)
```

### Debug Overlay Components (in `src/game/`)

```
src/game/
  debug_stats.rs      — DebugStats singleton component, DebugSystem (runs last each tick)
```

## Files to Modify

| File | Change |
|------|--------|
| `Cargo.toml` | Add `hecs = "0.10"` dependency and `[features] debug-overlay` flag |
| `src/main.rs` | No changes |
| `src/game/mod.rs` | Update re-exports for new modules |
| `src/game/game.rs` | Rewrite as thin Game facade wrapping hecs::World |
| `src/game/unit.rs` | Keep `UnitType`, `Terrain`, `Equipment`. Remove `Unit` struct (becomes components). |
| `src/game/economy.rs` | Keep `City`, `UnitType`, `Economy`. Remove city capture logic (→ CityCaptureSystem). |
| `src/game/player.rs` | Keep `Player` struct (small, not ECS). |
| `src/game/world.rs` | Remove (terrain/vision/borders become ECS components + systems). |
| `src/game/combat.rs` | Keep `CombatResult`, `resolve_combattor`, `check_surrender`. Remove game integration. |
| `src/server/mod.rs` | Update re-exports |
| `src/server/game_server.rs` | Strip to just GameServer struct + run() + route wiring. Add `/debug/stats` route. Move handlers. |
| `src/server/lobby.rs` | Minor: import paths |
| `src/replay/` | Update to query ECS world instead of `game.units` HashMap |
| `src/llm/agent.rs` | Update `GameState` building to query ECS world |

## Steps

### Phase 1: Foundation (ECS Skeleton)

- [ ] Add `hecs = "0.10"` to Cargo.toml
- [ ] Create `src/game/components.rs` — define all ECS component structs
- [ ] Create `src/game/systems/spatial_hash.rs` — spatial hash grid for O(n) combat lookups
- [ ] Create `src/game/world_ext.rs` — `WorldExt` trait with `spawn_unit()`, `spawn_city()` helpers
- [ ] Rewrite `src/game/game.rs` as Game facade: holds `hecs::World`, `GameSettings`, `tick`, `running`, stats. Implement `tick()` that dispatches all systems.
- [ ] Implement `Game::recruit_unit()`, `move_troops()`, `toggle_healing()`, etc. as entity insert/update operations
- [ ] Verify: `cargo check` passes with new skeleton

### Phase 2: Systems (Game Logic)

- [ ] `EconomySystem` — generate funds every 10 ticks (from `game.rs::tick`)
- [ ] `MovementSystem` — move units along paths with terrain speed (from `process_unit` movement block)
- [ ] `WaterTransitionSystem` — embark/disembark timers (from `process_unit` water block)
- [ ] `AttritionSystem` — equipment attrition, org drain, starvation (from `process_unit` attrition block)
- [ ] `CombatSystem` — resolve adjacent combat using spatial hash (from `resolve_adjacent_combat`)
- [ ] `CityCaptureSystem` — capture cities when units stand on them (from `process_unit` capture block)
- [ ] `VisionSystem` — update vision grids per player (from `update_vision`)
- [ ] `BorderSystem` — update border grids per player (from `update_borders`)
- [ ] `HealingSystem` — heal units near friendly cities (from tick healing block)
- [ ] `SupplySystem` — city supply resupply (from `apply_supply`)
- [ ] `EncirclementSystem` — BFS encirclement check (from `check_encircled`)
- [ ] `SurrenderSystem` — check surrenders every 30 ticks (from `check_surrenders`)
- [ ] `ReinforcementSystem` — org reinforcement with supply malus (from tick reinforcement block)
- [ ] Wire all systems into `Game::tick()` in correct order
- [ ] Verify: unit tests for each system, `cargo test` passes

### Phase 3: Server Reorganization

- [ ] Create `src/server/handlers/` module with sub-modules
- [ ] Extract lobby handlers → `handlers/lobby.rs`
- [ ] Extract game action handlers → `handlers/game.rs`
- [ ] Extract spectator handler → `handlers/spectator.rs`
- [ ] Extract replay handler → `handlers/replay.rs`
- [ ] Extract HTTP handlers → `handlers/http.rs`
- [ ] Create `src/server/room.rs` — `GameRoom`, tick loop, command dispatch
- [ ] Create `src/server/delta.rs` — `build_delta_inner`, `build_full_state` using ECS queries
- [ ] Strip `game_server.rs` to GameServer + run() + route wiring only
- [ ] Add `/debug/stats` route to router (gated by feature flag)
- [ ] Update `build_llm_state()` to query ECS world for LLM agent state
- [ ] Verify: `cargo check` passes, all handlers work

### Phase 4: Integration & Cleanup

- [ ] Update `src/replay/` to work with ECS world queries instead of HashMap
- [ ] Update `src/llm/` GameState building to use ECS queries
- [ ] Remove old `Unit` struct from `unit.rs`, keep `UnitType`, `Terrain`, `Equipment`
- [ ] Remove old `World` struct from `world.rs` (replaced by ECS components)
- [ ] Clean up `economy.rs` — remove city capture logic (now in system)
- [ ] Clean up `combat.rs` — keep pure functions, remove game integration
- [ ] Ensure wire protocol (protocol.rs) is unchanged — client-facing API stable
- [ ] **Debug overlay** — Create `src/debug.rs` with `/debug/stats` HTTP endpoint
- [ ] **Debug overlay** — Create `src/game/debug_stats.rs` with `DebugStats` singleton component and `DebugSystem`
- [ ] **Debug overlay** — Wire `DebugSystem` into `Game::tick()` (runs last, gated by `cfg(debug_assertions)` or feature flag)
- [ ] **Debug overlay** — Add in-browser stats overlay div to game page (shown only in debug builds)
- [ ] Add benchmarks for tick throughput: `cargo bench`
- [ ] Verify: full integration test, multiple games running concurrently

### Phase 5: Performance Verification & Debug Overlay

- [ ] Benchmark: single game tick time before vs after
- [ ] Benchmark: 10 concurrent games on one server
- [ ] Verify memory usage: ECS should use less memory than HashMap-of-Units
- [ ] Profile with `cargo flamegraph` to confirm hot paths are optimized
- [ ] **Debug overlay (debug-only build)** — Add a runtime stats overlay visible only when compiled with `--features debug-overlay` (or `cfg(debug_assertions)`):
  - **Tick timing**: per-system elapsed time (μs) and total tick time displayed in console/server logs
  - **Memory usage**: per-game memory footprint (ECS world size, component storage)
  - **Concurrent game count**: active games, total ticks/sec across all games
  - **Network stats**: messages sent/received per connection, delta sizes
  - Exposed via a `/debug/stats` HTTP endpoint (JSON) and optional in-browser overlay div on the game page
  - Implemented as a `DebugStats` singleton component in the ECS world + a `DebugSystem` that runs last each tick

## Reuse (Existing Code to Preserve)

| File | What to reuse |
|------|---------------|
| `src/game/combat.rs` | `resolve_combattor()`, `check_surrender()`, `CombatResult` — pure functions, no game state |
| `src/game/unit.rs` | `UnitType` (config), `Terrain` (enum with methods), `Equipment` (struct) |
| `src/game/economy.rs` | `City` struct, `Economy::generate_funds()`, `Economy::upgrade_city()`, `Economy::recruit_unit()` |
| `src/game/player.rs` | `Player` struct — small, not worth ECS |
| `src/server/protocol.rs` | Entire file — wire format unchanged |
| `src/server/lobby.rs` | `Lobby`, `LobbyManager` — unchanged |
| `src/config.rs` | Entire file — unchanged |
| `src/map/` | Entire module — unchanged |
| `src/storage.rs` | Entire file — unchanged |
| `src/debug.rs` | New file — debug stats endpoint, in-browser overlay HTML/JS |
| `src/game/debug_stats.rs` | New file — DebugStats component + DebugSystem |

## Verification

### Build
- `cargo check` — compiles with no errors
- `cargo clippy` — no warnings (fix any new ones)
- `cargo test` — all existing tests pass

### Functional
- Start server, join lobby, start game — full flow works
- Move units, recruit, heal, resupply, upgrade cities — all actions work
- Combat resolves correctly — units take damage, die, surrender
- Water transitions work — embark/disembark timers
- LLM agents can play — GameState query works
- Spectators can watch — delta streaming works
- Replays work — replay system queries ECS world correctly

### Performance
- Tick time for a game with 50 units: should be < 1ms (currently ~3-5ms with HashMap cloning)
- 10 concurrent games: server should handle all at 60 TPS
- Memory per game: should decrease with ECS (no HashMap overhead, no cloned snapshots)

### Wire Protocol Compatibility
- `ServerMessage` enum unchanged — clients don't need updates
- `ClientMessage` enum unchanged — clients don't need updates
- Delta format identical — same `GameDeltaMessage` structure

### Debug Overlay Verification
- `cargo check --features debug-overlay` — compiles with debug features
- `/debug/stats` endpoint returns JSON with tick timing, memory, concurrent game count
- In-browser overlay shows real-time tick timing per system
- Debug overlay is NOT present in release builds (`cargo build --release`)
- DebugSystem overhead is negligible (microsecond-level per tick)
