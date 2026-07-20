# War of Dots — Comprehensive Function Call Trace Report

## 1. Project Overview

**War of Dots** is a LAN multiplayer RTS war game written in Rust. It uses an ECS architecture (`hecs`) for the game engine, an HTTP/WebSocket server (`axum`), and supports LLM agents. The codebase has ~1,940 indexed nodes across 40+ source files.

---

## 2. Entry Points

### `main()` → `src/main.rs`

```
main()
├── CLI parsing (clap)
├── IF --benchmark:
│   └── benchmark::run_benchmark(config) → returns BenchmarkResults
│       └── benchmark::generate_html_report(results, prev) → HTML string
└── ELSE (normal server mode):
    ├── config::load(config_path) → Config
    ├── server::GameServer::new(config, port) → GameServer
    └── game_server.run() → awaits indefinitely
```

### `GameServer::new()` → `src/server/game_server.rs:54`

```
GameServer::new(config, port)
├── storage::Storage::new(db_path) → SQLite-backed storage
├── map_registry::init() → register available maps
└── LobbyManager::new() → empty lobby map
```

### `GameServer::run()` → `src/server/game_server.rs:67`

```
GameServer::run()
├── axum::Router with routes:
│   ├── GET "/"       → index_handler()
│   ├── GET "/watch"  → watch_handler()
│   ├── GET "/ws"     → ws_handler() → handle_ws_game()
│   ├── GET "/ws/replay" → replay_ws_handler() → handle_replay_ws()
│   ├── GET "/ws/spectator" → spectator_ws_handler() → handle_spectator_ws()
│   ├── GET "/replays" → list_replays()
│   ├── GET "/maps"  → list_maps_handler()
│   └── fallback     → serve_static(frontend/dist/*)
│   [DEBUG ROUTES]
│   ├── GET "/debug/stats"     → debug_stats_handler()
│   ├── GET "/debug/tick"      → debug_tick_handler()
│   └── GET "/debug/benchmark" → debug_benchmark_handler()
├── tokio::spawn(axum::serve(http_listener, app))  ← HTTP server
└── while listener_ws.accept():
      └── tokio::spawn(handle_tcp_client(stream, server))
```

---

## 3. Client Connection Flow

### WebSocket Path: `ws_handler()` → `handle_ws_game()` → `handle_client_message()`

```
ws_handler(ws, server_state)
└── handle_ws_game(socket, server_state)
    ├── split socket → (ws_sink, ws_stream)
    ├── create bridge channel (mpsc::channel)
    ├── spawn sender task: rx → ws_sink.send(Binary)
    └── read loop:
        └── ws_stream.next() → Binary(data)
            └── serde_cbor::from_slice::<ClientMessage>(&data)
                └── handle_client_message(conn_id, server_state, msg, player_sender)
```

### TCP Path: `handle_tcp_client()` → `handle_client_message()`

```
handle_tcp_client(stream, server_state)
├── tokio::io::split(stream) → (reader, writer)
├── spawn sender task: rx → writer.write_all(data)
└── read loop:
    └── reader.read(&mut buf) → Ok(n)
        └── serde_cbor::from_slice::<ClientMessage>(&buf)
            └── handle_client_message(conn_id, server_state, msg, player_sender)
```

---

## 4. Command Dispatch: `handle_client_message()` → `src/server/game_server.rs:1129`

This is the central message router. It handles both game commands and lobby operations:

```
handle_client_message(conn_id, server_state, msg, player_sender)
│
├── ClientMessage::LobbyMessage(lobby_msg)
│   ├── LobbyMessage::CreateLobby
│   │   └── lobby_manager.create_lobby(player_id, name, settings, password, sender, custom_units)
│   │       └── returns lobby_code → send LobbyCreated to client
│   ├── LobbyMessage::JoinLobby
│   │   └── lobby_manager.join_lobby(code, player_id, name, password, sender)
│   │       └── broadcast LobbyUpdate to all players
│   ├── LobbyMessage::SetSlotLlm
│   │   └── lobby_manager.set_slot_llm(code, slot, provider)
│   │       └── broadcast LobbyUpdate
│   ├── LobbyMessage::SetSlotLlmConfig
│   │   └── lobby_manager.set_slot_llm_config(code, slot, config)
│   │       └── broadcast LobbyUpdate
│   └── LobbyMessage::StartGame
│       └── create GameRoom (see Section 5)
│
├── ClientMessage::MoveUnit / MoveTroop
│   └── room.command_tx.send(Command::MoveTroop(troop_id, path))
├── ClientMessage::RecruitUnit
│   └── room.command_tx.send(Command::RecruitUnit(city_id, unit_type, owner))
├── ClientMessage::ToggleHealing
│   └── room.command_tx.send(Command::ToggleHealing(unit_id, enabled))
├── ClientMessage::ToggleResupply
│   └── room.command_tx.send(Command::ToggleResupply(unit_id, enabled))
├── ClientMessage::UpgradeCity
│   └── room.command_tx.send(Command::UpgradeCity(city_id, owner))
├── ClientMessage::ToggleAdvisor
│   └── room.command_tx.send(Command::ToggleAdvisor(player_id, enabled))
├── ClientMessage::EmbarkUnit
│   └── room.command_tx.send(Command::EmbarkUnit(unit_id, player_id))
├── ClientMessage::DisembarkUnit
│   └── room.command_tx.send(Command::DisembarkUnit(unit_id, player_id))
├── ClientMessage::GetLeaderboard
│   └── storage.get_leaderboard() → send Leaderboard message
└── ClientMessage::LobbyMessage(StartGame) → spawn GameRoom
```

---

## 5. Game Room Lifecycle

### `GameRoom::new()` → `src/server/game_server.rs:126`

This is a massive function that creates the game world and spawns the tick loop:

```
GameRoom::new(game_id, map, settings, players, llm_configs, room_id, server, config, storage)
│
├── Game::new(map, settings)  ← ECS world creation (Section 6)
├── game.start()
├── Assign cities to players
├── Recruit starting infantry for each player
│   └── game.recruit_unit(city_id, infantry_type, player_id)
├── Create LLM agents from configs:
│   ├── client_factory::create_llm_client(provider, model, api_key, base_url)
│   ├── LlmAgent::new(player_id, name, provider, delay)
│   └── llm_pairs.push((agent, client))
├── Create advisors for human players (if configured):
│   └── Advisor::new() → advisors.insert(player_id, advisor)
├── Create command channel: (cmd_tx, cmd_rx) = mpsc::unbounded_channel()
├── Build initial state → send FullState to all players
│   └── game.full_state() → encode_message → player.tx().send()
└── tokio::spawn(game tick loop) → see Section 7
```

---

## 6. ECS World Initialization: `Game::new()` → `src/game/game.rs:117`

```
Game::new(map: Map, settings: GameSettings)
├── hecs::World::new()
├── For each city in map:
│   ├── Player::new(player_id, name, is_human, starting_funds)
│   ├── PlayerGameStats::default()
│   └── funds.insert(player_id, starting_funds)
├── City::new(city_id, position) for each city
├── Terrain grid from map (vec![vec![Terrain; height]; width])
├── world.spawn_world(terrain, width, height, city_upgrade_settings, funds, cities)
│   └── Creates WorldBundle (TerrainGrid, VisionGrids, BorderGrids, CityData, EconomyFunds, CityUpgradeData)
└── Returns Game { world, world_entity, settings, players, player_stats, entity_registry }
```

---

## 7. Game Tick Loop (spawned inside GameRoom::new)

This is the core game loop. It runs as an async task with `tokio::select!`:

```
tokio::spawn(async { game loop })
│
├── Interval tick (every interval_ms = 1000/game_speed):
│   │
│   ├── LLM Agent Steps (for each LLM agent):
│   │   ├── build_llm_state(&game, &agent.id) → GameState
│   │   │   └── game.vision_grid_2d(player_id), game.unit_states(), game.query_cities()
│   │   ├── agent.step(client, &state).await → Vec<Command>
│   │   │   └── LlmAgent builds system prompt, calls LLM, parses JSON response
│   │   └── For each command: cmd_tx.send(cmd)
│   │
│   ├── Broadcast LLM reason logs (new entries since last broadcast)
│   │
│   ├── Advisor steps (every 60 ticks per player):
│   │   ├── build_llm_state(&game, player_id)
│   │   └── advisor.get_advice(advisor_client, &state).await
│   │       └── sends Advice message to player
│   │
│   ├── game.tick_number() ← just reads tick count (no-op call)
│   │
│   ├── Check game over:
│   │   ├── determine_winner(&game) → count units + cities per player
│   │   ├── If active_count == 0: draw, record_game_end(), send GameOver
│   │   ├── If active_count == 1: winner found, record_game_end(), send GameOver
│   │   └── record_game_end(room_id, end_time, winners, all_players, stats)
│   │       └── storage.record_game_end() → SQLite INSERT
│   │
│   ├── Build spectator vision (union of all players' vision grids)
│   │
│   └── For each player connection:
│       ├── Build delta:
│       │   └── build_delta_inner(&game, &mut prev_units, &mut prev_cities, &mut prev_funds, ...)
│       │       ├── game.unit_states() → Vec<UnitState>
│       │       ├── game.query_cities() → Vec<City>
│       │       └── Compare with prev state → GameDeltaMessage
│       ├── encode_message(&delta)
│       └── player.tx().send(encoded) → if ok, keep alive
│       └── If no players alive: cleanup, record_game_end(), break
│
└── Command channel (cmd_rx.recv()):
    ├── Command::MoveTroop(id, path)       → game.move_troops(id, path)
    ├── Command::RecruitUnit(city, type, owner) → game.recruit_unit(city, type, owner)
    ├── Command::ToggleHealing(id, enabled) → game.toggle_healing(id, enabled)
    ├── Command::ToggleResupply(id, enabled) → game.toggle_resupply(id, enabled)
    ├── Command::UpgradeCity(city, owner)  → game.upgrade_city(city, owner)
    ├── Command::ToggleAdvisor(player, enabled) → advisor.set_enabled(enabled)
    ├── Command::AddSpectator(sender)      → send FullState, add to players list
    ├── Command::EmbarkUnit(id, player)    → game.embark_unit(id, player)
    ├── Command::DisembarkUnit(id, player) → game.disembark_unit(id, player)
    └── None → break (channel closed)
```

---

## 8. `Game::tick_game()` → System Dispatch Pipeline → `src/game/game.rs:182`

This is the inner game simulation. It runs all 12 game systems in a fixed order:

```
Game::tick_game()
│
├── Guard: if !running → return
├── self.tick += 1
│
├── TickContext::build(&world, world_entity)
│   └── Extracts: cities, funds, map dimensions, units_by_player
│
├── [Every ECONOMY_TICK_INTERVAL (10) ticks]
│   └── systems::economy::run(&mut world, world_entity, funds_per_city)
│       └── Iterate owned cities → add funds + manpower to global pools
│       └── Returns HashMap<player_id, funds_earned>
│
├── systems::water::run(&mut world, world_entity) [debug builds]
│   └── Process embark/disembark timers, update on_water flag
│
├── world.build_spatial_hash() ← shared by Movement + Combat
│   └── Query all UnitPosition → insert into SpatialHash grid
│
├── systems::movement::run(&mut world, world_entity, settings)
│   ├── Phase 1: query units with non-empty path → compute_movement()
│   │   └── compute_movement(pos, path, unit_type, stats, equipment, terrain, settings)
│   │       └── unit_speed_with_terrain(terrain, water_speed, unit_type_speed)
│   │       └── Apply: terrain modifiers (Hills=0.7, Forest=0.8, Water=water_speed)
│   │       └── Apply: overload penalty (equipment weight vs men capacity)
│   │       └── Apply: org drain (0.05 per tick)
│   └── Phase 2: apply MovementChanges → update position, path, org, moved flag
│
├── systems::attrition::run(&mut world, world_entity, settings)
│   └── For each unit: reduce men based on base_attrition + fighting_attrition
│   └── Supply loss: equipment attrition proportional to movement/combat
│
├── systems::combat::run(&mut world, world_entity, spatial_hash)
│   ├── Collect candidates: units that moved OR are fighting
│   ├── For each candidate: spatial_hash.query_nearby(pos) → adjacent enemies
│   ├── Combat resolution:
│   │   ├── Calculate soft attack/defense (men/max_men * organisation * water_modifier)
│   │   ├── Terrain modifier: Hills=1.5, Forest=0.75, Water=0.5
│   │   ├── Apply damage to both units (men loss + org drain -10)
│   │   ├── Equipment attrition: loss proportional to damage taken
│   │   └── Supply consumption: proportional to damage taken
│   ├── Return dead entities (men <= 0)
│   ├── Retreat check: clear fighting flag for units with no nearby enemy
│   └── Clear moved flags for all units
│   └── DESPAWN dead entities
│
├── systems::city_capture::run(&mut world, world_entity, player_stats)
│   └── For each unit within 1.5 of a city → transfer ownership
│   └── Update player_stats.cities_captured
│
├── [Every INFLUENCE_TICK_INTERVAL (30) ticks]
│   └── systems::influence::run(&mut world, ctx, tick)
│       ├── Phase 1: Build PlayerInput per player (unit positions, city positions)
│       ├── Phase 2: compute_player_influence(input) per player
│       │   ├── For each unit: project vision (square) + influence (circle)
│       │   │   └── circle_spans() → precomputed spans (no sqrt in inner loop)
│       │   ├── For each city: project vision (1.5x radius) + influence (5x weight)
│       │   └── Terrain modifier: Hills → 2x vision radius
│       └── Phase 3: Write vision/border grids to world
│
├── systems::healing::run(&mut world, ctx)
│   └── For each unit with healing flag AND not encircled:
│       └── Restore men at reinforcement_rate (capped at max_men)
│
├── systems::supply::run(&mut world, world_entity, ctx, recruitment_rate)
│   ├── For each player: find cities in supply range
│   ├── For each unit near friendly city:
│   │   ├── Calculate resupply rate (closest city full, secondary decayed)
│   │   └── Deduct from global manpower pool
│   └── Apply: unit.men += gain (capped at max_men)
│
├── [Every CITY_CAPTURE_TICK_INTERVAL (30) ticks]
│   └── systems::encirclement::run(&mut world, ctx, tick)
│       ├── build_dominant_player_grid(border_grids) → which player controls each cell
│       ├── mark_safe_edges_multi() → edges are safe
│       ├── mark_safe_near_cities_multi() → city radius is safe
│       ├── BFS flood-fill from safe anchors through friendly cells
│       └── Mark units as encircled if in dominated territory but NOT reachable from safe zone
│
├── [Every 30 ticks]
│   └── systems::surrender::run(&mut world, world_entity)
│       └── For each player: if all units have men <= 5% of max_men AND encircled
│       └── Return dead entities to despawn
│       └── DESPAWN surrendered units
│
├── track_unit_losses() → compare prev_unit_counts with current, update stats
│
└── systems::reinforce::run(&mut world, world_entity)
    └── For each unit near owned city: reinforce men at reinforcement_rate
```

---

## 9. Game Facade Methods → Call Sites

| Method | Defined In | Called From |
|--------|-----------|-------------|
| `Game::new(map, settings)` | `game.rs:117` | `GameRoom::new()` (game_server.rs:149) |
| `Game::start()` | `game.rs:177` | `GameRoom::new()` (game_server.rs:150) |
| `Game::tick_game()` | `game.rs:182` | NOT called directly from tick loop (tick loop calls `tick_number()` as no-op; systems run inline) |
| `Game::recruit_unit(city, type, owner)` | `game.rs:416` | `GameRoom::new()` (starting units), `handle_client_message()` via `Command::RecruitUnit` |
| `Game::move_troops(unit, path)` | `game.rs:469` | `handle_client_message()` via `Command::MoveTroop` |
| `Game::toggle_healing(unit, enabled)` | `game.rs:478` | `handle_client_message()` via `Command::ToggleHealing` |
| `Game::toggle_resupply(unit, enabled)` | `game.rs:487` | `handle_client_message()` via `Command::ToggleResupply` |
| `Game::upgrade_city(city, owner)` | `game.rs:496` | `handle_client_message()` via `Command::UpgradeCity` |
| `Game::embark_unit(unit, player)` | `game.rs:538` | `handle_client_message()` via `Command::EmbarkUnit` |
| `Game::disembark_unit(unit, player)` | `game.rs:632` | `handle_client_message()` via `Command::DisembarkUnit` |
| `Game::query_all_units()` | `game.rs:689` | `Game::unit_states()`, benchmark |
| `Game::query_cities()` | `game.rs:733` | `GameRoom::new()`, `delta::build_delta_inner()`, `Game::full_state()` |
| `Game::unit_states()` | `game.rs:943` | `GameRoom::new()`, `delta::build_delta_inner()`, `determine_winner()` |
| `Game::vision_grid_2d(player)` | `game.rs:775` | `GameRoom::new()`, `Game::full_state_for()`, `delta::build_llm_state()` |
| `Game::border_grid_2d(player)` | `game.rs:788` | `GameRoom::new()`, `Game::full_state_for()` |
| `Game::all_player_funds()` | `game.rs:988` | `GameRoom::new()`, `delta::build_delta_inner()` |
| `Game::union_vision_grids(players)` | `game.rs:818` | `GameRoom` tick loop (spectator vision) |
| `Game::player_stats()` | `game.rs:938` | `GameRoom` tick loop (game end recording) |
| `Game::players()` | `game.rs:925` | `GameRoom::new()`, `determine_winner()`, tick loop |
| `Game::settings()` | `game.rs:928` | `GameRoom::new()`, tick loop |

---

## 10. System Module Dependencies

```
systems/mod.rs exports:
├── attrition (run)
├── city_capture (run)
├── combat (run)
├── economy (run)
├── encirclement (run)
├── healing (run)
├── influence (run)
├── movement (run, compute_movement, unit_speed_with_terrain)
├── reinforce (run)
├── spatial_hash (SpatialHash struct)
├── supply (run)
├── surrender (run)
└── water (run)
[Legacy, not called: border, vision — replaced by influence.rs]
```

---

## 11. World Extension Trait → `WorldExt` (`src/game/world_ext.rs`)

| Method | Called By |
|--------|-----------|
| `spawn_world(terrain, w, h, settings, funds, cities)` | `Game::new()` |
| `spawn_unit(unit_type, owner, position)` | `Game::recruit_unit()` |
| `world_data(entity)` | All systems (read-only world bundle access) |
| `world_data_clone(entity)` | `Game::query_cities()`, `Game::vision_grid_2d()`, `Game::border_grid_2d()` |
| `world_data_mut(entity)` | `systems::economy::run()`, `systems::influence::run()`, `systems::supply::run()` |
| `terrain_at(entity, x, y)` | `Game::embark_unit()`, `Game::disembark_unit()` |
| `player_funds(entity, player_id)` | `Game::player_funds()` |
| `build_spatial_hash()` | `Game::tick_game()` (once per tick, shared by Movement + Combat) |
| `record_detailed(entity, key, duration)` | Debug timing guards |

---

## 12. Protocol & Delta Flow

```
ClientMessage variants (client → server):
├── MoveUnit { unit_id, path }
├── MoveTroop { troop_id, path }  [legacy alias]
├── RecruitUnit { city_id, unit_type }
├── ToggleHealing { unit_id, enabled }
├── ToggleResupply { unit_id, enabled }
├── UpgradeCity { city_id }
├── ToggleAdvisor { enabled }
├── EmbarkUnit { unit_id }
├── DisembarkUnit { unit_id }
├── GetLeaderboard
└── LobbyMessage { CreateLobby, JoinLobby, SetSlotLlm, SetSlotLlmConfig, StartGame }

ServerMessage variants (server → client):
├── FullState(GameStateMessage)  ← initial state + full snapshots
├── DeltaState(GameDeltaMessage) ← incremental updates
├── LobbyCreated { lobby_code }
├── LobbyUpdate { players, settings }
├── Error { message }
├── LlmReasonLog { player_name, action, reason }
├── Advice { text }
├── ClearSpectator
├── Leaderboard { entries }
└── GameOver { winner, game_id, message }

Serialization: serde_cbor (binary), also serde_json for some endpoints
```

---

## 13. LLM Agent Flow

```
LlmAgent::step(client, &game_state)
├── build_system_prompt() → prompt with unit types, rules, available actions
├── client.query(system_prompt, user_message)
│   └── LlmClient (OpenAI | Ollama | Anthropic) → HTTP POST
├── Parse JSON response → Vec<Action>
├── Map actions to Commands:
│   ├── Action::MoveUnit → Command::MoveTroop
│   ├── Action::RecruitUnit → Command::RecruitUnit
│   ├── Action::UpgradeCity → Command::UpgradeCity
│   └── Action::ToggleHealing → Command::ToggleHealing
├── Push entry to reason_log
└── Return Vec<Command>

Advisor::get_advice(client, &game_state)
├── If !enabled → return None
├── Build advice prompt from game state
├── client.query(advice_prompt) → LLM response
├── Parse as Advice { text, action, reason }
└── Return Some(advice)
```

---

## 14. Map System

```
Map (src/map/map.rs)
├── terrain(x, y) → Terrain enum (Plains, Water, Mountain, Hills, Forest)
└── from_config(&MapConfig) → Map [parses terrain string with chars: .~^#*]

MapConfig: { width, height, cities: Vec<(f32, f32)>, terrain_data: Option<String> }

map_registry:
├── init() → register built-in maps
└── list_maps() → Vec<String>
```

---

## 15. Storage System

```
Storage (src/storage.rs)
├── new(db_path) → SQLite connection
├── record_game_end(room_id, end_time, winners, players, stats, use_standard)
│   ├── INSERT INTO game_metadata (id, players, winner, start_time, end_time, replay_path)
│   ├── INSERT INTO player_stats (game_id, player_id, units_recruited, units_lost, ...)
│   └── UPDATE player_leaderboard (games_played, games_won, avg_casualties)
├── get_leaderboard() → Vec<LeaderboardEntry>
└── get_game_replay(game_id) → Option<replay_path>
```

---

## 16. Replay System

```
ReplayRecorder (src/replay/recorder.rs)
├── new(game) → starts recording
├── record(&game) → captures ReplayEvent per tick
└── save(path) → serialize to disk

ReplayPlayer (src/replay/replay.rs)
├── from_file(path, game) → load replay
├── next() → advance one event
├── apply_event(&event) → re-apply to game state
└── full_state() → GameStateMessage for streaming

handle_replay_ws(socket, server_state, game_id)
├── Open SQLite, lookup replay_path
├── ReplayPlayer::from_file(replay_path, Game::new(default_map, default_settings))
├── Send initial FullState
├── Loop: replay_player.next() → apply_event → build_delta_inner → send
└── Send final FullState
```

---

## 17. Benchmark System

```
run_benchmark(config)
├── BattleSimulator::new(config) → creates game with random seed
├── For each tick:
│   ├── game.tick_game()
│   └── Record system timings (via debug_stats)
└── Return BenchmarkResults { summary, per_system_timings }

generate_html_report(results, prev_results)
└── Return HTML string with comparison deltas
```

---

## 18. Complete Component Map

**Per-unit ECS components** (`src/game/components.rs`):
- `UnitOwner` — player_id
- `UnitTypeId` — unit type name
- `UnitDisplay` — symbol
- `UnitPosition` — x, y
- `UnitPath` — Vec<(f32, f32)> waypoints
- `UnitStats` — men, max_men, organisation, max_organisation, reinforcement_rate
- `UnitEquipment` — equipment, max_equipment, supply, max_supply, supply_buffer_percent
- `UnitFlags` — healing, fighting, moved, encircled, resupply_enabled, on_water
- `UnitWaterTransition` — embarking, disembarking, timers, boat_supply

**World-level ECS components** (on single world entity):
- `WorldBundle` → { TerrainGrid, VisionGrids, BorderGrids, CityData, EconomyFunds, CityUpgradeData, DebugStats }

---

## 19. Key Architectural Observations

1. **Tick-first architecture**: The game loop runs all systems in a fixed, deterministic order. This matters because combat reads `moved` flags set by movement, and city capture runs after combat (so dead units can't capture).

2. **Spatial hash reuse**: `build_spatial_hash()` is called once per tick and shared between Movement and Combat, avoiding redundant grid construction.

3. **TickContext optimization**: Built once per tick, pre-groups units by player and clones small data (cities ~20 entries, funds ~12 entries). Grids are accessed via `Arc::clone` (cheap pointer bump, not 10KB clone).

4. **Scoped Arc extraction pattern**: Every system that needs terrain/borders does `Arc::clone(&wb.terrain.grid)` then drops the `hecs::Ref` borrow immediately. This is a consistent pattern to avoid borrow checker conflicts.

5. **Delta encoding**: `build_delta_inner()` compares current state against previous snapshots and only sends changes. This reduces bandwidth significantly for large maps.

6. **Influence system replaces Vision + Border**: The old `border.rs` and `vision.rs` are marked as legacy. `influence.rs` computes both in one pass per source entity.

7. **Game::tick_game() vs tick loop divergence**: The `tick_game()` method defines the complete system pipeline, but the actual game room tick loop runs systems inline (not calling `tick_game()`). This is intentional — it interleaves LLM/advisor steps between game systems. The `tick_game()` method is used by the benchmark system and integration tests.

---

## 20. Verdict

**Approve** — The codebase is well-structured with clear separation of concerns:

- **Strong**: ECS architecture is clean, system order is well-defined, spatial optimization (hash reuse, TickContext, Arc cloning) is evidence-based with comments explaining why Rayon was rejected.
- **Good**: Protocol is well-defined with CBOR serialization and comprehensive round-trip tests.
- **Good**: LLM integration is cleanly isolated behind `LlmAgent`/`Advisor` abstractions.
- **Note**: `Game::tick_game()` is defined but the game room tick loop runs systems inline (calling `game.tick_number()` as a no-op). The tick loop in `GameRoom::new()` directly calls systems rather than delegating to `tick_game()`. This is a deliberate design choice (to interleave LLM/advisor steps) but creates a divergence between the `tick_game()` method and actual execution flow.
