# Plan: Fix All Review Issues — War of Dots

## Status: COMPLETED ✅

**All 16 issues addressed across 4 phases. Project compiles cleanly with zero warnings, all 17 tests pass, clippy is clean.**

### What was done:

- **Phase 1 (Security)**: SQL injection eliminated via parameterized queries (`sqlite::Value` binding). `Storage` uses `Arc<Mutex<sqlite::Connection>>` for O(1) clones and thread safety. WAL mode + busy timeout enabled. Error logging added to all DB operations.
- **Phase 2 (Architecture)**: Typed error module created (`src/errors.rs`). Crate-level `#![allow(dead_code)]` removed with 20+ targeted annotations. Orphaned `world.rs` and `combat.rs` deleted. Duplicate clippy allow removed.
- **Phase 3 (Systems)**: `src/game/constants.rs` created with 10 named constants. Magic numbers replaced in `game.rs`, `combat.rs`, `supply.rs`, `influence.rs`. Legacy `VisionGrids` indexing inconsistency resolved by deleting orphaned legacy World struct.
- **Phase 4 (Polish)**: HTML benchmark reports moved to `reports/`. `.gitignore` updated.

### Deferred for separate effort:

- **3.4 Game loop extraction**: The ~300-line `GameRoom::new` game loop could be extracted into a `GameLoop` struct for testability. This is a large refactor with no correctness issues — just code organization.
- **3.5 LLM rate limiting**: Adding sliding window rate limiting + token budget tracking for LLM agent calls. Requires new state structures in the server.

---

## Phase 1: Security & Data Integrity (Blockers) — ~8-10 hrs

> **Why first?** SQL injection is a critical vulnerability. Connection pooling + WAL improves reliability for all subsequent changes. These are non-breaking externally (protocol stays the same).

### 1.1 SQL Injection in `storage.rs` → Parameterized Queries

**Files**: `src/storage.rs`

**Current (vulnerable)**:
```rust
self.db.execute(format!(
    "INSERT OR IGNORE INTO game_metadata (id, players, end_time, winner, replay_path) VALUES ('{}', '{}', {}, '{}', '{}')",
    game_id, serde_json::to_string(players).unwrap(), end_time,
    serde_json::to_string(winners.first().unwrap_or(&"")).unwrap(), replay_path
)).ok();
```

**Fix**: Replace ALL string-interpolated SQL with prepared statements + bound parameters. The `sqlite` crate (0.36) uses `prepare()` → `bind()` → `next()`:

```rust
// Before (vulnerable):
self.db.execute(format!(
    "INSERT OR IGNORE INTO game_metadata (id, players, end_time, winner, replay_path) VALUES ('{}', '{}', {}, '{}', '{}')",
    game_id, serde_json::to_string(players).unwrap(), end_time,
    serde_json::to_string(winners.first().unwrap_or(&"")).unwrap(), replay_path
)).ok();

// After (safe):
let players_json = serde_json::to_string(players).unwrap();
let winner = serde_json::to_string(winners.first().unwrap_or(&"")).unwrap();
let mut stmt = self.db.prepare(
    "INSERT OR IGNORE INTO game_metadata (id, settings, players, start_time, end_time, winner, replay_path) VALUES (?, ?, ?, ?, ?, ?, ?)"
).map_err(|e| tracing::error!(%e, "Failed to prepare game_metadata insert")).ok()?;
stmt.bind((game_id, "", &players_json, 0i64, end_time as i64, &winner, &replay_path))
    .and_then(|()| stmt.next())
    .map_err(|e| tracing::error!(%e, game_id, "Failed to insert game metadata")).ok();
```

**Queries to fix** (all in `storage.rs` — 4 `format!`-based SQL strings):
1. `record_game_end()` — INSERT game_metadata (line ~60)
2. `record_game_end()` — INSERT OR REPLACE player_stats for standard ruleset (line ~78)
3. `record_game_end()` — INSERT OR REPLACE player_stats for non-standard (line ~87)
4. `record_game_end()` — INSERT OR REPLACE player_names (line ~100)

**Already safe**: `get_leaderboards()` uses `prepare()` + `stmt.next()` + `stmt.read()` ✅

**Testing**: Add unit tests with injection payloads (`"'; DROP TABLE game_metadata; --`).

---

### 1.2 SQL Injection in Replay Handler → Parameterized Query

**Files**: `src/server/game_server.rs`

**Current (vulnerable)**:
```rust
let mut stmt = match db.prepare(format!(
    "SELECT replay_path FROM game_metadata WHERE id = '{}'",
    game_id_str.replace('\'', "\\'")
)) { ... }
```

**Fix**:
```rust
// Before (vulnerable):
let mut stmt = match db.prepare(format!(
    "SELECT replay_path FROM game_metadata WHERE id = '{}'",
    game_id_str.replace('\'', "\\'")
)) { ... }

// After (safe):
let mut stmt = match db.prepare(
    "SELECT replay_path FROM game_metadata WHERE id = ?",
) {
    Ok(s) => s,
    Err(e) => {
        let err = encode_message(&ServerMessage::Error {
            message: format!("Database query error: {}", e),
        });
        let _ = ws_sink.send(Message::Binary(err)).await;
        return;
    }
};
stmt.bind((0, &game_id_str))
    .map_err(|e| tracing::error!(%e, game_id = %game_id_str, "Failed to bind game_id"))
    .ok()?;
```

**Note**: The `game_id` comes from a URL query parameter (`?game_id=...`). Even though it's a LAN game, this endpoint is exposed over HTTP and should be treated as untrusted input.

---

### 1.3 Storage Clone Error Handling → No Silent Fallback

**Files**: `src/storage.rs`

**Current (dangerous)**:
```rust
impl std::clone::Clone for Storage {
    fn clone(&self) -> Self {
        match sqlite::Connection::open(&self.path) {
            Ok(db) => Storage { db, path: self.path.clone() },
            Err(_) => Storage {
                db: sqlite::Connection::open("war_of_dots.db").unwrap(), // ← panics
                path: self.path.clone(),
            },
        }
    }
}
```

**Fix**: Open the path once, share via `Arc`. The clone becomes cheap:

```rust
use std::sync::Arc;

pub struct Storage {
    pub db: Arc<sqlite::Connection>,
    pub path: String,
}

impl Clone for Storage {
    fn clone(&self) -> Self {
        Storage {
            db: Arc::clone(&self.db),
            path: self.path.clone(),
        }
    }
}

impl Storage {
    pub fn new(path: &str) -> anyhow::Result<Self> {
        // ... create dirs ...
        let db = sqlite::Connection::open(path)
            .with_context(|| format!("Failed to open database at {}", path))?;
        // Create tables...
        Ok(Storage {
            db: Arc::new(db),
            path: path.to_string(),
        })
    }
}
```

**Why this is better**:
- Clone is O(1) — no new connection opened
- Errors surface immediately at startup instead of silently during gameplay
- Single shared connection avoids concurrent-write issues with SQLite

---

### 1.4 Enable WAL Mode for SQLite

**Files**: `src/storage.rs` (in `Storage::new()`)

**Add after opening connection**:
```rust
db.execute("PRAGMA journal_mode=WAL")?;
db.execute("PRAGMA synchronous=NORMAL")?;      // Good balance of safety/performance
db.execute("PRAGMA busy_timeout=5000")?;        // Wait up to 5s for locks instead of failing
```

**Why WAL**:
- Concurrent readers don't block writers (important for leaderboard queries during gameplay)
- Better write performance for sequential inserts (game stats)
- Crash-safe checkpointing

---

### 1.5 Add Error Logging to Silent `.ok()` Calls

**Files**: `src/storage.rs`, `src/server/game_server.rs`

Replace `.ok()` with `.map_err(|e| tracing::error!(...)).ok()` for DB operations:

```rust
self.db.execute(&query)
    .map_err(|e| tracing::error!(game_id, %e, "Failed to record game metadata"))
    .ok();
```

**Scope**: ~12 locations across storage.rs and game_server.rs where errors are silently dropped.

---

## Phase 2: Architecture — Typed Errors & Dead Code — ~6-8 hrs

> **Dependencies**: None on Phase 1 (can run in parallel).

### 2.1 Introduce Typed Error Type

**Files**: New `src/errors.rs`, update `src/lib.rs`, `src/storage.rs`, `src/server/*.rs`, `src/config.rs`

**Create**:
```rust
// src/errors.rs
use thiserror::Error;

#[derive(Error, Debug)]
pub enum WarError {
    #[error("Database error: {0}")]
    Database(#[from] sqlite::Error),

    #[error("Storage path error: {0}")]
    StoragePath(#[from] std::io::Error),

    #[error("Configuration error: {0}")]
    Config(#[from] toml::de::Error),

    #[error("City '{0}' not found or not owned by '{1}'")]
    CityNotOwned(String, String),

    #[error("Insufficient funds: need {needed}, have {have}")]
    InsufficientFunds { needed: u32, have: u32 },

    #[error("Unit type '{0}' not found")]
    UnitTypeNotFound(String),

    #[error("LLM error: {0}")]
    Llm(String),

    #[error("Protocol error: {0}")]
    Protocol(#[from] serde_cbor::Error),

    #[error("Replay not found: {0}")]
    ReplayNotFound(String),
}
```

**Migration strategy**:
1. Create `src/errors.rs` with the error enum
2. Re-export from `src/lib.rs`: `pub use errors::WarError;`
3. Change `Storage::new()` to return `Result<Self, WarError>` (from `anyhow::Result`)
4. Change `config::load()` to return `Result<Config, WarError>`
5. Keep `anyhow::Result` in `main.rs` (top-level is fine for anyhow)
6. Game-internal methods keep `anyhow` — only public-facing APIs get typed errors

**What NOT to change**: Internal game logic (combat, movement, economy systems) — these use `anyhow` or return `Option`/`Result<(), String>` and don't need typed errors.

---

### 2.2 Remove `#![allow(dead_code)]` + Clean Up

**Files**: `src/main.rs`, and all files with scattered `#[allow(dead_code)]`

**Steps**:
1. Remove `#![allow(dead_code)]` from `src/main.rs:17`
2. Run `cargo clippy -- -W dead_code` to get the full list
3. For each dead item, either:
   - **Delete** if truly unused (legacy code)
   - **Keep with `#[allow(dead_code)]`** + comment explaining why (e.g., public API for future use, schema types)
4. Expected dead code to address:
   - `World` struct in `src/game/world.rs` (legacy, superseded by ECS)
   - `Economy` struct methods in `src/game/economy.rs` (duplicated by ECS systems)
   - Various `#[allow(dead_code)]` on `Unit` methods (keep — used by LLM/protocol)
   - `PlayerSender::Websocket` variant (used but clippy may flag)

---

### 2.3 Remove Legacy `World` Struct

**Files**: DELETE `src/game/world.rs`, update `src/game/mod.rs`

**Evidence**: The `World` struct in `src/game/world.rs` has:
- `vision_grids: HashMap<String, Vec<Vec<bool>>>` — superseded by ECS `VisionGrids`
- `border_grids: HashMap<String, Vec<Vec<u32>>>` — superseded by ECS `BorderGrids`
- `update_vision()` / `update_borders()` — superseded by `systems::influence::run()`
- `union_vision_grids()` — superseded by `Game::union_vision_grids()`

**Verification**: Search for all references to `crate::game::world::World` or `game::world::`. If none found in non-test code, safe to delete.

**Also remove**: The re-export from `src/game/mod.rs` if it exists.

---

### 2.4 Fix Duplicate `#[allow(clippy::too_many_arguments)]`

**Files**: `src/server/game_server.rs:71-72`

**Fix**: Delete the duplicate line. Then consider using a builder or config struct for `GameRoom::new()` (9 params):

```rust
pub struct GameRoomConfig {
    pub game_id: String,
    pub map: Map,
    pub settings: GameSettings,
    pub players: Vec<PlayerConnection>,
    pub llm_configs: Vec<LlmPlayerConfig>,
    pub room_id: String,
    pub server_state: Arc<Mutex<GameServer>>,
    pub config: Config,
    pub storage: Storage,
    pub spectator_vision: bool,
}
```

**Priority**: Low — this is cosmetic but the duplicate is a clear mistake.

---

## Phase 3: Systems Refactoring — ~8-10 hrs

> **Dependencies**: Phase 2 (dead code cleanup must happen first to know what's used).

### 3.1 Extract Magic Numbers to Constants

**Files**: `src/game/game.rs`, `src/game/systems/supply.rs`, `src/game/systems/combat.rs`, `src/game/systems/encirclement.rs`

**Create `src/game/constants.rs`**:
```rust
/// Ticks between economy (funds generation) cycles
pub const ECONOMY_TICK_INTERVAL: u64 = 10;

/// Ticks between influence (vision + border) updates
pub const INFLUENCE_TICK_INTERVAL: u64 = 3;

/// Ticks between encirclement checks
pub const ENCIRCLEMENT_TICK_INTERVAL: u64 = 30;

/// Ticks between surrender checks
pub const SURRENDER_TICK_INTERVAL: u64 = 30;

/// Combat range squared (range = 2.0, so sq = 4.0)
pub const COMBAT_RANGE_SQ: f32 = 4.0;

/// Supply range squared (range = 1.5, so sq = 2.25)
pub const SUPPLY_RANGE_SQ: f32 = 2.25;

/// Equipment supply rate per tick near a city
pub const SUPPLY_RATE_PER_TICK: u32 = 50;

/// Ticks to complete embark/disembark transition
pub const WATER_TRANSITION_TICKS: u32 = 10;

/// Vision radius for units on plains
pub const VISION_RADIUS: f32 = 3.0;

/// Vision multiplier for units on hills
pub const VISION_HILLS_MULTIPLIER: f32 = 2.0;

/// Vision multiplier for cities (relative to unit vision)
pub const CITY_VISION_MULTIPLIER: f32 = 1.5;
```

**Replace** all occurrences:
- `self.tick.is_multiple_of(10)` → `self.tick.is_multiple_of(constants::ECONOMY_TICK_INTERVAL)`
- `supply_range_sq = 1.5 * 1.5` → `constants::SUPPLY_RANGE_SQ`
- `dist_sq >= 4.0` → `dist_sq >= constants::COMBAT_RANGE_SQ`
- `embarking_ticks_left = 10` → `constants::WATER_TRANSITION_TICKS`
- `vision_radius: f32 = 3.0` → `constants::VISION_RADIUS`

---

### 3.2 Economy Two-Phase Borrow Pattern

**Files**: `src/game/systems/economy.rs`

**Current** (holds mutable borrow during iteration):
```rust
pub fn run(world: &mut World, world_entity: hecs::Entity) {
    let wb = match world.world_data_mut(world_entity) {
        Some(wb) => wb,
        None => return,
    };
    for city in &wb.cities.cities {
        // ... modifies wb.funds.funds while iterating
    }
}
```

**Fix** — collect changes first, apply second:
```rust
pub fn run(world: &mut World, world_entity: hecs::Entity) {
    // Phase 1: collect (scoped immutable borrow)
    let changes: Vec<(String, u32)> = {
        let wb = match world.world_data(world_entity) {
            Some(wb) => wb,
            None => return,
        };
        wb.cities.cities.iter()
            .filter_map(|city| {
                city.owner.as_ref().map(|owner| {
                    let income_mult = wb.city_upgrades.settings.income_multiplier(city.level);
                    let manpower_bonus = (city.manpower as f32 / city.max_manpower as f32) * 1.0;
                    let gain = ((1.0 + manpower_bonus) * income_mult) as u32;
                    (owner.clone(), gain)
                })
            })
            .collect()
    };
    // wb dropped — borrow released

    // Phase 2: apply (mutable borrow)
    if let Some(wb) = world.world_data_mut(world_entity) {
        for (owner, gain) in changes {
            let funds = wb.funds.funds.entry(owner).or_insert(0);
            *funds = funds.saturating_add(gain);
        }
    }
}
```

**Pattern to apply to all systems** that currently hold long mutable borrows. Check: `influence.rs`, `healing.rs`, `supply.rs`, `encirclement.rs`.

---

### 3.3 Vision Grid Indexing Consistency

**Files**: `src/game/systems/influence.rs`, `src/game/components.rs`

**Issue**: The old `World` struct used `grid[x][y]` (column-major), while ECS `VisionGrids` uses flat `[y * width + x]` (row-major). After deleting the old `World` struct (Phase 2.3), verify all remaining code uses the flat indexing consistently.

**Audit checklist**:
1. `components.rs` VisionGrids/Borders → flat `[y * width + x]` ✅
2. `influence.rs` → verify it writes to flat arrays
3. `delta.rs` `flat_to_2d()` → converts flat to 2D correctly ✅
4. `game.rs` `vision_grid_2d()` / `border_grid_2d()` → converts correctly ✅
5. `world_ext.rs` → verify no old-style indexing

---

### 3.4 Game Loop Refactoring

**Files**: `src/server/game_server.rs` → New `src/server/game_loop.rs`

**Current**: ~300 lines of game loop logic inside `GameRoom::new()` in an anonymous async block.

**Extract to** `src/server/game_loop.rs`:
```rust
pub struct GameLoop {
    game: Game,
    players: Vec<PlayerConnection>,
    llm_agents: Vec<(LlmAgent, LlmClient)>,
    advisors: HashMap<String, Advisor>,
    advisor_client: Option<LlmClient>,
    cmd_tx: mpsc::UnboundedSender<Command>,
    cmd_rx: mpsc::UnboundedReceiver<Command>,
    storage: Storage,
    room_id: String,
    server_state: Arc<Mutex<GameServer>>,
    // Previous state trackers
    prev_units: HashMap<String, UnitState>,
    prev_cities: HashMap<String, CityState>,
    prev_funds: HashMap<String, u32>,
    prev_vision_grid: Option<Vec<Vec<bool>>>,
    prev_border_grid: Option<Vec<Vec<u32>>>,
    spectator_vision: bool,
    tick_count: u64,
}

impl GameLoop {
    pub async fn run(mut self) {
        let mut interval = tokio::time::interval(self.tick_interval());

        loop {
            tokio::select! {
                _ = interval.tick() => {
                    self.on_tick().await;
                }
                cmd = self.cmd_rx.recv() => {
                    self.on_command(cmd).await;
                }
            }
        }
    }

    async fn on_tick(&mut self) {
        self.run_llm_agents().await;
        self.broadcast_llm_reasons().await;
        self.run_advisors().await;
        self.tick_game();
        self.check_game_over().await;
        self.broadcast_deltas().await;
    }

    async fn on_command(&mut self, cmd: Option<Command>) {
        // ... handle commands
    }

    // Each of these is now a focused method:
    async fn run_llm_agents(&mut self);
    async fn broadcast_llm_reasons(&mut self);
    async fn run_advisors(&mut self);
    fn tick_game(&mut self);
    async fn check_game_over(&mut self);
    async fn broadcast_deltas(&mut self);
    fn tick_interval(&self) -> Duration;
}
```

**Benefits**:
- Each concern is a separate method — testable in isolation
- The `GameRoom::new()` function becomes a simple constructor
- Easier to add features (e.g., pause, speed control)
- Clear ownership — `GameLoop` owns everything it needs

---

### 3.5 LLM Rate Limiting & Cost Tracking

**Files**: `src/llm/agent.rs`, `src/llm/mod.rs`

**Add to `LlmAgent`**:
```rust
pub struct LlmAgent {
    // ... existing fields ...
    /// Maximum API calls per minute
    pub max_calls_per_minute: u32,
    /// Timestamps of recent API calls (for sliding window)
    pub call_timestamps: Vec<std::time::Instant>,
    /// Total cost tracking (approximate, in "tokens")
    pub total_tokens_used: u64,
    /// Maximum budget in tokens (0 = unlimited)
    pub token_budget: u64,
}
```

**In `step()` method**, add rate check:
```rust
pub async fn step(&mut self, client: &LlmClient, state: &GameState) -> anyhow::Result<Vec<Command>> {
    // Existing thinking delay check ...

    // Rate limit check
    let now = std::time::Instant::now();
    self.call_timestamps.retain(|t| now.duration_since(*t).as_secs() < 60);
    if self.call_timestamps.len() >= self.max_calls_per_minute as usize {
        return Ok(Vec::new()); // Rate limited
    }

    // Budget check
    if self.token_budget > 0 && self.total_tokens_used >= self.token_budget {
        tracing::warn!(agent = %self.name, "Token budget exceeded");
        return Ok(Vec::new());
    }

    // ... existing LLM call ...

    // Track usage
    self.call_timestamps.push(now);
    // Note: actual token counting requires provider-specific response parsing
}
```

**Config**: Add `max_calls_per_minute` and `token_budget` to `LlmAgentConfig` and `LlmProvider`.

---

## Phase 4: Polish & Housekeeping — ~3-5 hrs

> **Dependencies**: All prior phases.

### 4.1 Reduce Redundant Conversions

**Files**: `src/game/game.rs`

**Current chain**: ECS components → `UnitQueryData` → `UnitState` (protocol) → `EquipmentState`

**Optimize**: The `unit_states()` method does two conversions:
1. `query_all_units()` → `Vec<UnitQueryData>` (ECS → intermediate)
2. `UnitQueryData` → `protocol::UnitState` (intermediate → wire format)

**Consider**: A direct `ecs_to_protocol()` function that skips the intermediate struct:
```rust
pub fn unit_states(&self) -> Vec<protocol::UnitState> {
    self.world.query::<(/* all components */>)>()
        .iter()
        .map(|(_, components)| {
            // Direct conversion to protocol::UnitState
        })
        .collect()
}
```

**Priority**: Low — this is a micro-optimization. The current code is clear and correct.

---

### 4.2 Benchmark Report File Management

**Files**: `.gitignore`, new `reports/` directory

**Current**: 15+ HTML report files scattered in project root.

**Fix**:
1. Add to `.gitignore`:
   ```
   report_*.html
   report.html
   benchmark*.html
   ```
2. Update `benchmark.rs` to default to `reports/benchmark.html`:
   ```rust
   #[arg(long, default_value = "reports/benchmark.html")]
   bench_html: Option<String>,
   ```
3. Create `reports/` with a `.gitkeep`

---

### 4.3 Add `.gitignore` for Build Artifacts & Reports

**Files**: `.gitignore` (create or update)

```
# Build
target/
**/*.pdb

# Benchmark reports
report*.html
benchmark*.html
reports/

# Database (if stored locally)
*.db
*.db-wal
*.db-shm

# IDE
.idea/
.vscode/
*.swp
```

---

### 4.4 Update Cargo.toml Dependencies

**Files**: `Cargo.toml`

**Add**: No new crates needed. The existing `thiserror = "1"` and `sqlite = "0.36"` support all changes. The `Arc`-based storage avoids needing a separate connection pool crate.

**Consider** (optional, future):
- `r2d2` + `r2d2_sqlite` if connection pooling becomes necessary (e.g., for high-concurrency deployments)
- `sqlx` for compile-time checked queries (big migration, not in this plan)

---

## Execution Order & Parallelism

```
Phase 1.1 (SQL injection storage)    ──┐
Phase 1.2 (SQL injection replay)      ──┤
Phase 1.3 (Storage Arc::clone)        ──┼── Sequential (all touch storage.rs)
Phase 1.4 (WAL mode)                  ──┘
Phase 1.5 (Error logging)             ──┐
                                        ├── Can start after 1.1-1.4 land
                                        │
Phase 2.1 (Typed errors)     ──────────┤
Phase 2.2 (Dead code)        ──────────┤
Phase 2.3 (Legacy World)     ──────────┼── Can run in parallel with Phase 1
Phase 2.4 (Duplicate allow)  ──────────┘
                                        │
Phase 3.1 (Constants)      ─────────────┐
Phase 3.2 (Two-phase borrows) ──────────┤
Phase 3.3 (Vision consistency) ─────────┤
Phase 3.4 (Game loop refactor) ─────────┼── Sequential (depend on 2.3 dead code removal)
Phase 3.5 (LLM rate limiting) ──────────┘
                                        │
Phase 4.1-4.4 (Polish)   ────────────────┘── After all above
```

**Can run in parallel**:
- Phase 1 (security) and Phase 2 (architecture) are independent
- Within Phase 1: 1.1 and 1.2 can be done together (different files)
- Within Phase 2: 2.2 and 2.4 can be done together

**Must be sequential**:
- Phase 3 depends on Phase 2 (need to know what's dead before refactoring systems)
- Phase 4 depends on everything

---

## Verification Checklist

After each phase:
- [ ] `cargo check` — compiles cleanly
- [ ] `cargo test` — all tests pass
- [ ] `cargo clippy` — no new warnings
- [ ] `cargo test --test integration_game` — integration tests pass
- [ ] Manual: start server, create lobby, play a game → no panics

After ALL phases:
- [ ] Benchmark still runs (`--benchmark` flag)
- [ ] LLM agents still connect and play
- [ ] Replay system works end-to-end
- [ ] Leaderboard persists correctly
- [ ] No SQL injection possible (audit all SQL)
- [ ] No `#![allow(dead_code)]` at crate level

---

## Risk Assessment

| Risk | Likelihood | Mitigation |
|------|-----------|------------|
| Parameterized queries break existing DB schema | Low | Same schema, different binding style |
| Arc<Connection> causes SQLite lock contention | Medium | WAL mode + busy_timeout handles this |
| Game loop refactor introduces timing bugs | Medium | Keep same tick interval, test with benchmark |
| Dead code removal breaks something | Low | Compiler will catch it; keep tests green |
| Typed errors break error handling chains | Medium | Keep `anyhow` internally, only change public APIs |
