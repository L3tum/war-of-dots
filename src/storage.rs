//! SQLite storage for game metadata, leaderboards
//!
//! **Sync core / async wrappers split (Arch #12):** the on-disk schema and
//! all SQL live in the sync methods below (`record_game_end`,
//! `get_leaderboards`, `list_game_metadata`, `find_replay_path`,
//! `upsert_player_names`, and the free function `persist_game_end`). They
//! guard the single `sqlite::Connection` with a short-lived `std::sync::Mutex`
//! and are safe to call from any sync context — in particular the tick task
//! (see `persist_game_end`), which runs on a worker thread and must not pay
//! the `spawn_blocking` hop on its end-of-game path.
//!
//! The `*_async` wrappers are for async contexts (axum handlers, the lobby
//! start flow). They route the identical blocking work through
//! `tokio::task::spawn_blocking` so the SQLite mutex is only ever held on a
//! blocking-pool thread, never on an async runtime worker. `Storage` is
//! `Clone`-via-`Arc`, so it moves into the blocking task cheaply; the
//! connection itself stays single-owned (one `Connection` behind one
//! `Mutex`), which is what SQLite's serialized threading model wants.

use sqlite::{State, Value};
use std::collections::HashMap;
use std::fs;
use std::sync::{Arc, Mutex};

/// Re-export player stats from game module
pub use crate::game::game::PlayerGameStats;

use crate::game::game::Game;

pub struct Storage {
    pub db: Arc<Mutex<sqlite::Connection>>,
    pub path: String,
    /// Directory where replay JSONL files are written (mirrors
    /// `config.storage.replay_path`). Kept here so `record_game_end` stores
    /// the real on-disk location in `game_metadata.replay_path` instead of a
    /// cwd-relative guess.
    pub replay_path: String,
}

impl Clone for Storage {
    fn clone(&self) -> Self {
        Storage {
            db: Arc::clone(&self.db),
            path: self.path.clone(),
            replay_path: self.replay_path.clone(),
        }
    }
}

/// Stage at which a parameterized statement failed (see `Storage::execute`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ExecFailure {
    Lock,
    Prepare,
    Bind,
    Execute,
}

impl Storage {
    pub fn new(path: &str, replay_path: &str) -> Result<Self, crate::errors::StorageError> {
        fs::create_dir_all(
            std::path::Path::new(path)
                .parent()
                .unwrap_or(std::path::Path::new(".")),
        )
        .map_err(|source| crate::errors::StorageError::Open {
            path: path.to_string(),
            source: Box::new(source),
        })?;
        let db =
            sqlite::Connection::open(path).map_err(|source| crate::errors::StorageError::Open {
                path: path.to_string(),
                source: Box::new(source),
            })?;
        db.execute("CREATE TABLE IF NOT EXISTS game_metadata (id TEXT PRIMARY KEY, settings TEXT, players TEXT, start_time INTEGER, end_time INTEGER, winner TEXT, replay_path TEXT)").map_err(|source| crate::errors::StorageError::Execute(Box::new(source)))?;
        db.execute("CREATE TABLE IF NOT EXISTS player_stats (player_id TEXT PRIMARY KEY, total_casualties INTEGER DEFAULT 0, standard_ruleset_games INTEGER DEFAULT 0, standard_ruleset_wins INTEGER DEFAULT 0, standard_ruleset_casualties INTEGER DEFAULT 0)").map_err(|source| crate::errors::StorageError::Execute(Box::new(source)))?;
        db.execute(
            "CREATE TABLE IF NOT EXISTS player_names (player_id TEXT PRIMARY KEY, name TEXT)",
        )
        .map_err(|source| crate::errors::StorageError::Execute(Box::new(source)))?;
        // Enable WAL mode for better concurrent access
        db.execute("PRAGMA journal_mode=WAL")
            .map_err(|source| crate::errors::StorageError::Execute(Box::new(source)))?;
        db.execute("PRAGMA synchronous=NORMAL")
            .map_err(|source| crate::errors::StorageError::Execute(Box::new(source)))?;
        db.execute("PRAGMA busy_timeout=5000")
            .map_err(|source| crate::errors::StorageError::Execute(Box::new(source)))?;
        Ok(Storage {
            db: Arc::new(Mutex::new(db)),
            path: path.to_string(),
            replay_path: replay_path.to_string(),
        })
    }

    /// Run one parameterized statement to completion (prepare → bind →
    /// next), logging (with `context`) on any failure instead of panicking.
    /// The storage layer is best-effort: a failed write must not take down
    /// the game loop.
    fn execute(&self, context: &str, sql: &str, params: &[Value]) -> Result<(), ExecFailure> {
        let db = match self.db.lock() {
            Ok(db) => db,
            Err(e) => {
                tracing::error!(%e, %context, "Failed to acquire DB lock");
                return Err(ExecFailure::Lock);
            }
        };
        let mut stmt = match db.prepare(sql) {
            Ok(stmt) => stmt,
            Err(e) => {
                tracing::error!(%e, %context, "Failed to prepare statement");
                return Err(ExecFailure::Prepare);
            }
        };
        if let Err(e) = stmt.bind(params) {
            tracing::error!(%e, %context, "Failed to bind params");
            return Err(ExecFailure::Bind);
        }
        if let Err(e) = stmt.next() {
            tracing::error!(%e, %context, "Statement execution failed");
            return Err(ExecFailure::Execute);
        }
        Ok(())
    }

    /// Persist game-end metadata.
    ///
    /// `start_time` and `end_time` are UNIX seconds. The replay list UI
    /// computes duration as `end_time - start_time`, so the caller must pass
    /// the REAL wall-clock start (captured when the game began) — the old
    /// `start_time = 0` sentinel rendered as ~54-year durations.
    /// `settings_json` is the actual in-play settings + map name
    /// (serialized by the caller — see `game_metadata_settings_json` in
    /// game_server.rs), stored verbatim in the `settings` column.
    /// Kept flat (not a struct) — 9 args, one over clippy's limit, by design.
    #[allow(clippy::too_many_arguments)]
    pub fn record_game_end(
        &self,
        game_id: &str,
        start_time: u64,
        end_time: u64,
        settings_json: &str,
        winners: &[&str],
        players: &[&str],
        player_stats: &HashMap<String, PlayerGameStats>,
        is_standard_ruleset: bool,
    ) {
        let replay_path = format!("{}/{}.jsonl", self.replay_path, game_id);
        let players_json = serde_json::to_string(players).unwrap();
        let winner = serde_json::to_string(winners.first().unwrap_or(&"")).unwrap();

        // Record game metadata (parameterized query — no SQL injection)
        // Lock/prepare/bind failure means the DB is unusable for this write —
        // bail (the per-player writes below would fail the same way). An
        // execution failure is logged in `execute` and we fall through, as
        // before.
        let metadata_err = self.execute(
            &format!("game_metadata:{game_id}"),
            "INSERT OR IGNORE INTO game_metadata (id, settings, players, start_time, end_time, winner, replay_path) VALUES (?, ?, ?, ?, ?, ?, ?)",
            &[
                Value::String(game_id.to_string()),
                Value::String(settings_json.to_string()),
                Value::String(players_json),
                Value::Integer(start_time as i64),
                Value::Integer(end_time as i64),
                Value::String(winner),
                Value::String(replay_path),
            ],
        );
        if matches!(
            metadata_err,
            Err(ExecFailure::Lock | ExecFailure::Prepare | ExecFailure::Bind)
        ) {
            return;
        }

        // Record per-player statistics (parameterized queries — no SQL injection)
        for (player_id, stats) in player_stats {
            let casualties = stats.units_lost as i64;
            let games_won = if winners.contains(&player_id.as_str()) {
                1i64
            } else {
                0i64
            };

            let stats_ctx = format!("player_stats:{player_id}");
            let ok = if is_standard_ruleset {
                self.execute(
                    &stats_ctx,
                    "INSERT OR REPLACE INTO player_stats (player_id, standard_ruleset_games, standard_ruleset_wins, standard_ruleset_casualties, total_casualties) \
                     VALUES (?, COALESCE((SELECT standard_ruleset_games FROM player_stats WHERE player_id=?), 0) + 1, \
                     COALESCE((SELECT standard_ruleset_wins FROM player_stats WHERE player_id=?), 0) + ?, \
                     COALESCE((SELECT standard_ruleset_casualties FROM player_stats WHERE player_id=?), 0) + ?, \
                     COALESCE((SELECT total_casualties FROM player_stats WHERE player_id=?), 0) + ?)",
                    &[
                        Value::String(player_id.to_string()),
                        Value::String(player_id.to_string()),
                        Value::String(player_id.to_string()),
                        Value::Integer(games_won),
                        Value::String(player_id.to_string()),
                        Value::Integer(casualties),
                        Value::String(player_id.to_string()),
                        Value::Integer(casualties),
                    ],
                )
                .is_ok()
            } else {
                self.execute(
                    &stats_ctx,
                    "INSERT OR REPLACE INTO player_stats (player_id, total_casualties) \
                     VALUES (?, COALESCE((SELECT total_casualties FROM player_stats WHERE player_id=?), 0) + ?)",
                    &[
                        Value::String(player_id.to_string()),
                        Value::String(player_id.to_string()),
                        Value::Integer(casualties),
                    ],
                )
                .is_ok()
            };
            if !ok {
                continue;
            }

            // Record player name
            let actual_name = format!("Player {}", player_id);
            let name_str = actual_name
                .strip_prefix("Player player_")
                .map(|s| s.to_string())
                .unwrap_or_else(|| actual_name.clone());
            let name_json = serde_json::to_string(&name_str).unwrap_or(format!("\"{}\"", name_str));

            let name_params: Vec<Value> = vec![
                Value::String(player_id.to_string()),
                Value::String(name_json),
            ];
            if self
                .execute(
                    &format!("player_names:{player_id}"),
                    "INSERT OR REPLACE INTO player_names (player_id, name) VALUES (?, ?)",
                    name_params.as_slice(),
                )
                .is_err()
            {
                continue;
            }
        }
    }

    pub fn get_leaderboards(&self) -> Result<Vec<PlayerStat>, String> {
        let db = self.db.lock().map_err(|e| e.to_string())?;
        let mut stmt = db.prepare(
            "SELECT p.player_id, p.name, s.standard_ruleset_games, s.standard_ruleset_wins, s.standard_ruleset_casualties \
             FROM player_names p JOIN player_stats s ON p.player_id = s.player_id \
             WHERE s.standard_ruleset_games > 0 \
             ORDER BY s.standard_ruleset_wins DESC"
        ).map_err(|e| e.to_string())?;
        let mut stats = Vec::new();
        while let Ok(State::Row) = stmt.next() {
            let player_id: String = stmt.read(0).unwrap_or_default();
            let name: String = stmt.read(1).unwrap_or_default();
            // Column order must match the SELECT above:
            // (2) standard_ruleset_games, (3) standard_ruleset_wins,
            // (4) standard_ruleset_casualties.
            let standard_games: i64 = stmt.read(2).unwrap_or(0);
            let standard_wins: i64 = stmt.read(3).unwrap_or(0);
            let standard_casualties: i64 = stmt.read(4).unwrap_or(0);
            stats.push(PlayerStat {
                player_id,
                name,
                games_played: standard_games,
                games_won: standard_wins,
                avg_casualties: standard_casualties as f32 / standard_games.max(1) as f32,
            });
        }
        Ok(stats)
    }

    /// All game-end rows, newest first (`end_time DESC`) — the sync core
    /// behind `list_replays` (handlers/replay.rs). Degrades to an empty
    /// list (with a log) on a poisoned/failed DB, matching the handler's
    /// pre-refactor behavior.
    pub fn list_game_metadata(&self) -> Vec<GameMetadataRecord> {
        let db = match self.db.lock() {
            Ok(db) => db,
            Err(e) => {
                tracing::error!(%e, "Failed to acquire DB lock for game_metadata list");
                return Vec::new();
            }
        };
        let mut stmt = match db.prepare(
            "SELECT id, players, winner, start_time, end_time FROM game_metadata ORDER BY end_time DESC",
        ) {
            Ok(s) => s,
            Err(e) => {
                tracing::error!(%e, "Failed to prepare game_metadata list query");
                return Vec::new();
            }
        };
        let mut records = Vec::new();
        while let Ok(State::Row) = stmt.next() {
            records.push(GameMetadataRecord {
                id: stmt.read(0).unwrap_or_default(),
                players_json: stmt.read(1).unwrap_or_default(),
                winner: stmt.read(2).unwrap_or_default(),
                start_time: stmt.read(3).unwrap_or(0),
                end_time: stmt.read(4).unwrap_or(0),
            });
        }
        records
    }

    /// Look up the persisted replay path for `game_id` (sync core behind the
    /// `/ws/replay` resolver). Returns `None` for missing rows, empty paths,
    /// or a failed DB — the handler maps `None` to "Replay not found".
    pub fn find_replay_path(&self, game_id: &str) -> Option<String> {
        let db = match self.db.lock() {
            Ok(db) => db,
            Err(e) => {
                tracing::error!(%e, "Failed to acquire DB lock for replay lookup");
                return None;
            }
        };
        let mut stmt = match db.prepare("SELECT replay_path FROM game_metadata WHERE id = ?") {
            Ok(s) => s,
            Err(e) => {
                tracing::error!(%e, game_id = %game_id, "Failed to prepare replay lookup");
                return None;
            }
        };
        let params = [Value::String(game_id.to_string())];
        if stmt.bind(params.as_slice()).is_err() {
            tracing::error!(game_id = %game_id, "Failed to bind game_id");
            return None;
        }
        if matches!(stmt.next(), Ok(State::Row)) {
            stmt.read(0).ok().filter(|p: &String| !p.is_empty())
        } else {
            None
        }
    }

    /// Insert-or-replace player display names (`player_names` table).
    ///
    /// Sync core for the start-flow name registration (room/start.rs): the
    /// initial lock failure is the only fatal one (mirrors the pre-refactor
    /// early `return` in `start_game`); per-row prepare/bind/execute
    /// failures are logged and skipped — a bad name row must not abort a
    /// game start.
    pub fn upsert_player_names(&self, names: &[(String, String)]) -> Result<(), String> {
        let db = match self.db.lock() {
            Ok(db) => db,
            Err(e) => return Err(e.to_string()),
        };
        for (player_id, name) in names {
            let name_json = serde_json::to_string(name).unwrap_or(format!("\"{}\"", name));
            let mut stmt = match db
                .prepare("INSERT OR REPLACE INTO player_names (player_id, name) VALUES (?, ?)")
            {
                Ok(s) => s,
                Err(e) => {
                    tracing::error!(%e, player_id = %player_id, "Failed to prepare player_names insert");
                    continue;
                }
            };
            let params: Vec<Value> =
                vec![Value::String(player_id.clone()), Value::String(name_json)];
            if let Err(e) = stmt.bind(params.as_slice()) {
                tracing::error!(%e, player_id = %player_id, "Failed to bind player_names params");
                continue;
            }
            if let Err(e) = stmt.next() {
                tracing::error!(%e, player_id = %player_id, "Failed to insert player_names");
            }
        }
        Ok(())
    }
}

/// One `game_metadata` row as the replay list needs it. `players_json` is
/// the verbatim `players` column (a JSON string array) — the list handler
/// deserializes it, keeping the JSON shape in one place.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GameMetadataRecord {
    pub id: String,
    pub players_json: String,
    pub winner: String,
    pub start_time: i64,
    pub end_time: i64,
}

// ---------------------------------------------------------------------------
// Async wrappers (Arch #12): identical blocking work, off the runtime.
// ---------------------------------------------------------------------------

impl Storage {
    /// Async entry point for `get_leaderboards` (axum handlers, lobby UI).
    /// Runs the sync core on the blocking pool so the `std::sync::Mutex`
    /// around the single connection is never held by an async worker.
    pub async fn get_leaderboards_async(&self) -> Result<Vec<PlayerStat>, String> {
        let storage = self.clone();
        tokio::task::spawn_blocking(move || storage.get_leaderboards())
            .await
            .map_err(|e| format!("leaderboards task panicked: {e}"))?
    }

    /// Async entry point for `record_game_end` (same split as above; the tick
    /// task keeps calling the sync `record_game_end` via `persist_game_end`
    /// — it already runs on a worker thread).
    #[allow(clippy::too_many_arguments)]
    pub async fn record_game_end_async(
        &self,
        game_id: &str,
        start_time: u64,
        end_time: u64,
        settings_json: &str,
        winners: &[&str],
        players: &[&str],
        player_stats: &HashMap<String, PlayerGameStats>,
        is_standard_ruleset: bool,
    ) {
        let storage = self.clone();
        let game_id = game_id.to_string();
        let settings_json = settings_json.to_string();
        let winners: Vec<String> = winners.iter().map(|s| s.to_string()).collect();
        let players: Vec<String> = players.iter().map(|s| s.to_string()).collect();
        let player_stats = player_stats.clone();
        tokio::task::spawn_blocking(move || {
            let winners_ref: Vec<&str> = winners.iter().map(|s| s.as_str()).collect();
            let players_ref: Vec<&str> = players.iter().map(|s| s.as_str()).collect();
            storage.record_game_end(
                &game_id,
                start_time,
                end_time,
                &settings_json,
                &winners_ref,
                &players_ref,
                &player_stats,
                is_standard_ruleset,
            )
        })
        .await
        .map_err(|e| tracing::error!(%e, "record_game_end blocking task panicked"))
        .ok();
    }

    /// Async entry point for `list_game_metadata` (the `/replays` handler).
    pub async fn list_game_metadata_async(&self) -> Vec<GameMetadataRecord> {
        let storage = self.clone();
        tokio::task::spawn_blocking(move || storage.list_game_metadata())
            .await
            .unwrap_or_default()
    }

    /// Async entry point for `find_replay_path` (the `/ws/replay` resolver).
    pub async fn find_replay_path_async(&self, game_id: &str) -> Option<String> {
        let storage = self.clone();
        let game_id = game_id.to_string();
        tokio::task::spawn_blocking(move || storage.find_replay_path(&game_id))
            .await
            .ok()?
    }

    /// Async entry point for `upsert_player_names` (the start flow).
    pub async fn upsert_player_names_async(
        &self,
        names: &[(String, String)],
    ) -> Result<(), String> {
        let storage = self.clone();
        let names = names.to_vec();
        tokio::task::spawn_blocking(move || storage.upsert_player_names(&names))
            .await
            .map_err(|e| format!("player_names task panicked: {e}"))?
    }
}

/// Persist the game-end record (wall-clock start/end + the in-play settings
/// JSON) when a game terminates. All four end paths — draw, last player
/// standing, abandoned (no humans left), and room empty — share this.
///
/// **Moved verbatim from `handlers/http.rs`** (Arch #8): it is a storage
/// concern, and it is called from the tick task, which runs on a worker
/// thread — so this stays a SYNC function (no `spawn_blocking` hop on the
/// end-of-game path; see the module docs for the sync/async split). The
/// signature is unchanged: `crate::storage::persist_game_end` is the new
/// canonical path.
#[allow(clippy::too_many_arguments)]
pub fn persist_game_end(
    storage: &Storage,
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

pub struct PlayerStat {
    pub player_id: String,
    pub name: String,
    pub games_played: i64,
    pub games_won: i64,
    pub avg_casualties: f32,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::game::game::GameSettings;

    fn temp_storage() -> (Storage, std::path::PathBuf) {
        let dir = std::env::temp_dir().join(format!("wod-storage-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let storage = Storage::new(
            &dir.join("db.sqlite").display().to_string(),
            &dir.join("replays").display().to_string(),
        )
        .expect("storage opens");
        (storage, dir)
    }

    /// The list UI shows duration = end_time - start_time; both must be the
    /// REAL wall-clock values (the old start_time=0 sentinel rendered as a
    /// ~54-year game), and `settings` must carry the actual settings + map
    /// name instead of an empty string.
    #[test]
    fn record_game_end_persists_real_start_time_and_settings() {
        let (storage, dir) = temp_storage();
        let start = 1_700_000_000u64;
        let end = start + 420;
        let settings_json =
            r#"{"map":"custom-isle","settings":{"starting_funds":100,"game_speed":60.0}}"#;
        let mut stats = HashMap::new();
        stats.insert(
            "player_0".to_string(),
            PlayerGameStats {
                units_recruited: 3,
                units_lost: 1,
                cities_captured: 2,
                funds_spent: 10,
                funds_earned: 20,
            },
        );
        storage.record_game_end(
            "game-42",
            start,
            end,
            settings_json,
            &["player_0"],
            &["player_0", "player_1"],
            &stats,
            false,
        );

        let db = storage.db.lock().unwrap();
        let mut stmt = db
            .prepare(
                "SELECT settings, players, start_time, end_time, winner, replay_path \
                 FROM game_metadata WHERE id = ?",
            )
            .unwrap();
        stmt.bind([Value::String("game-42".to_string())].as_slice())
            .unwrap();
        assert!(matches!(stmt.next(), Ok(State::Row)), "row missing");
        let settings: String = stmt.read(0).unwrap();
        let players: String = stmt.read(1).unwrap();
        let start_time: i64 = stmt.read(2).unwrap();
        let end_time: i64 = stmt.read(3).unwrap();
        let winner: String = stmt.read(4).unwrap();
        let replay_path: String = stmt.read(5).unwrap();

        assert_eq!(settings, settings_json, "settings must persist verbatim");
        assert!(
            settings.contains("custom-isle"),
            "settings must carry the map name"
        );
        assert_eq!(players, r#"["player_0","player_1"]"#);
        assert_eq!(
            start_time, start as i64,
            "start_time must be the real start"
        );
        assert_eq!(end_time, end as i64);
        // The list endpoint's duration computation (end - start, guarded
        // against inverted rows) must come out to the real 7-minute game.
        let duration = if start_time < end_time {
            end_time - start_time
        } else {
            0
        };
        assert_eq!(duration, 420);
        assert_eq!(winner, r#""player_0""#);
        assert_eq!(
            replay_path,
            format!("{}/game-42.jsonl", dir.join("replays").display())
        );
    }

    /// `Storage::new` on a path whose parent is a regular file must surface
    /// `StorageError::Open` (the directory cannot be created) instead of
    /// panicking or producing a misleading Execute error.
    #[test]
    fn new_rejects_db_path_under_regular_file() {
        let (dir, _path) = temp_storage_path();
        let file = dir.join("is-a-file");
        std::fs::write(&file, b"not a directory").unwrap();
        let bad = file.join("db.sqlite");

        let err = match Storage::new(bad.to_str().unwrap(), dir.join("replays").to_str().unwrap()) {
            Err(e) => e,
            Ok(_) => panic!("path under a regular file must fail"),
        };
        match err {
            crate::errors::StorageError::Open { path: p, .. } => {
                assert_eq!(p, bad.to_string_lossy().into_owned())
            }
            other => panic!("expected StorageError::Open, got: {other}"),
        }

        std::fs::remove_file(&file).ok();
        std::fs::remove_dir_all(&dir).ok();
    }

    /// `Storage::new` on a non-SQLite file (garbage bytes) must surface
    /// `StorageError::Execute` when the first CREATE TABLE runs against the
    /// corrupt database — not a panic.
    #[test]
    fn new_rejects_corrupt_db_file_with_execute_error() {
        let (dir, path) = temp_storage_path();
        std::fs::write(&path, b"this is not a sqlite database, just text\n").unwrap();

        let err = match Storage::new(
            path.to_str().unwrap(),
            dir.join("replays").to_str().unwrap(),
        ) {
            Err(e) => e,
            Ok(_) => panic!("first statement on a corrupt file must fail"),
        };
        assert!(
            matches!(err, crate::errors::StorageError::Execute(_)),
            "expected StorageError::Execute, got: {err}"
        );

        std::fs::remove_file(&path).ok();
        std::fs::remove_dir_all(&dir).ok();
    }

    /// A database pre-seeded with an OLDER `player_stats` schema (no
    /// `standard_ruleset_*` columns) opens fine — `CREATE TABLE IF NOT
    /// EXISTS` does not re-schema — but the leaderboard query must fail with
    /// a mapped `Err(String)` naming the missing column, not panic.
    #[test]
    fn leaderboards_error_on_schema_mismatched_db() {
        let (dir, path) = temp_storage_path();

        // Seed an old-schema DB: player_stats exists but lacks the
        // standard_ruleset_* columns the leaderboard query selects.
        let conn = sqlite::Connection::open(path.to_str().unwrap()).unwrap();
        conn.execute("CREATE TABLE player_stats (player_id TEXT PRIMARY KEY, wins INTEGER)")
            .unwrap();
        conn.execute("CREATE TABLE player_names (player_id TEXT PRIMARY KEY, name TEXT)")
            .unwrap();
        drop(conn);

        let storage = Storage::new(
            path.to_str().unwrap(),
            dir.join("replays").to_str().unwrap(),
        )
        .expect("old-schema DB still opens (IF NOT EXISTS)");
        let err = match storage.get_leaderboards() {
            Err(e) => e,
            Ok(_) => panic!("missing columns must surface as an error"),
        };
        assert!(
            err.contains("no such column"),
            "expected a 'no such column' sqlite error, got: {err}"
        );

        std::fs::remove_file(&path).ok();
        std::fs::remove_dir_all(&dir).ok();
    }

    /// Fresh (empty) database: leaderboards return an empty Ok list (the
    /// JOIN over no rows must not error or panic).
    #[test]
    fn leaderboards_on_fresh_db_is_empty_ok() {
        let (storage, dir) = temp_storage();
        let boards = storage.get_leaderboards().expect("empty list is Ok");
        assert!(boards.is_empty());
        std::fs::remove_dir_all(&dir).ok();
    }

    /// Standard-ruleset games feed the leaderboard (the query only lists
    /// players with standard_ruleset_games > 0): the winner gets a win, the
    /// casualties aggregate, and a custom-ruleset-only player is excluded.
    /// Regression test for the column-swap in `get_leaderboards` (read(2) is
    /// standard_ruleset_games, read(3) is standard_ruleset_wins — they were
    /// once mapped to the wrong variables, swapping games/won in the UI).
    #[test]
    fn leaderboards_count_standard_games_only() {
        let (storage, dir) = temp_storage();
        let settings_json = r#"{"map":"default","settings":{}}"#;

        let mut stats = HashMap::new();
        stats.insert(
            "player_0".to_string(),
            PlayerGameStats {
                units_recruited: 10,
                units_lost: 4,
                cities_captured: 1,
                funds_spent: 100,
                funds_earned: 200,
            },
        );
        stats.insert(
            "player_1".to_string(),
            PlayerGameStats {
                units_recruited: 8,
                units_lost: 7,
                cities_captured: 0,
                funds_spent: 90,
                funds_earned: 110,
            },
        );
        storage.record_game_end(
            "g-std",
            1_700_000_000,
            1_700_000_600,
            settings_json,
            &["player_0"],
            &["player_0", "player_1"],
            &stats,
            true,
        );

        // Second standard game, player_0 loses; casualties accumulate.
        let mut stats2 = HashMap::new();
        stats2.insert(
            "player_0".to_string(),
            PlayerGameStats {
                units_recruited: 1,
                units_lost: 2,
                cities_captured: 0,
                funds_spent: 1,
                funds_earned: 1,
            },
        );
        stats2.insert(
            "player_1".to_string(),
            PlayerGameStats {
                units_recruited: 1,
                units_lost: 0,
                cities_captured: 0,
                funds_spent: 1,
                funds_earned: 1,
            },
        );
        storage.record_game_end(
            "g-std-2",
            1_700_000_000,
            1_700_000_600,
            settings_json,
            &["player_1"],
            &["player_0", "player_1"],
            &stats2,
            true,
        );

        // A custom-ruleset-only player must NOT appear on the standard board.
        let mut custom = HashMap::new();
        custom.insert(
            "player_9".to_string(),
            PlayerGameStats {
                units_recruited: 1,
                units_lost: 1,
                cities_captured: 0,
                funds_spent: 1,
                funds_earned: 1,
            },
        );
        storage.record_game_end(
            "g-custom",
            1_700_000_000,
            1_700_000_600,
            settings_json,
            &["player_9"],
            &["player_9"],
            &custom,
            false,
        );

        let boards = storage.get_leaderboards().expect("leaderboard query");
        let ids: Vec<&str> = boards.iter().map(|b| b.player_id.as_str()).collect();
        assert!(
            ids.contains(&"player_0"),
            "standard player missing: {ids:?}"
        );
        assert!(
            ids.contains(&"player_1"),
            "standard player missing: {ids:?}"
        );
        assert!(
            !ids.contains(&"player_9"),
            "custom-only player must be excluded: {ids:?}"
        );

        let p0 = boards.iter().find(|b| b.player_id == "player_0").unwrap();
        assert_eq!(p0.games_played, 2);
        assert_eq!(p0.games_won, 1);
        // 4 + 2 standard-ruleset losses.
        assert_eq!(p0.avg_casualties, 3.0);

        // Wins sort DESC: player_1 has 1 win in 2 games — tied on wins, so
        // ordering between ties is not asserted, but both must be present.
        let p1 = boards.iter().find(|b| b.player_id == "player_1").unwrap();
        assert_eq!(p1.games_played, 2);
        assert_eq!(p1.games_won, 1);

        std::fs::remove_dir_all(&dir).ok();
    }

    /// Arch #12 regression: the async wrappers must not deadlock under
    /// concurrency. N concurrent `get_leaderboards_async` calls share the ONE
    /// connection behind ONE `std::sync::Mutex`; each call runs on the
    /// blocking pool, so the mutex is only ever contested between blocking
    /// threads (short critical sections), never held by an async worker that
    /// could then block the runtime. All tasks must complete and agree.
    #[tokio::test]
    async fn concurrent_get_leaderboards_async_does_not_deadlock() {
        let (storage, dir) = temp_storage();
        // Seed a couple of standard-ruleset games so the JOIN has rows.
        let settings_json = r#"{"map":"default","settings":{}}"#;
        for (i, id) in ["p0", "p1"].iter().enumerate() {
            let mut stats = HashMap::new();
            stats.insert(
                id.to_string(),
                PlayerGameStats {
                    units_recruited: 1,
                    units_lost: i as u32,
                    cities_captured: 0,
                    funds_spent: 1,
                    funds_earned: 1,
                },
            );
            storage.record_game_end(
                &format!("seed-{id}"),
                1_700_000_000,
                1_700_000_300,
                settings_json,
                &[id],
                &[id],
                &stats,
                true,
            );
        }

        const N: usize = 16;
        let mut handles = Vec::new();
        for _ in 0..N {
            let storage = storage.clone();
            handles.push(tokio::spawn(async move {
                storage
                    .get_leaderboards_async()
                    .await
                    .expect("leaderboard query must succeed")
            }));
        }
        let mut boards = Vec::new();
        for h in handles {
            boards.push(h.await.expect("task must join (no deadlock)"));
        }
        // Every concurrent call sees the same two players.
        for board in &boards {
            let ids: Vec<&str> = board.iter().map(|b| b.player_id.as_str()).collect();
            assert!(ids.contains(&"p0"), "missing p0 in {ids:?}");
            assert!(ids.contains(&"p1"), "missing p1 in {ids:?}");
            assert_eq!(board.len(), 2, "exactly the seeded players: {ids:?}");
        }

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The async wrappers must be behavior-identical to the sync core:
    /// record via `record_game_end_async`, read back through
    /// `get_leaderboards_async` / `list_game_metadata_async` /
    /// `find_replay_path_async` / `upsert_player_names_async`.
    #[tokio::test]
    async fn async_wrappers_roundtrip_match_sync_core() {
        let (storage, dir) = temp_storage();
        let settings_json = r#"{"map":"default","settings":{}}"#;
        let mut stats = HashMap::new();
        stats.insert(
            "player_0".to_string(),
            PlayerGameStats {
                units_recruited: 5,
                units_lost: 2,
                cities_captured: 1,
                funds_spent: 50,
                funds_earned: 90,
            },
        );
        storage
            .record_game_end_async(
                "game-async-1",
                1_700_000_000,
                1_700_000_600,
                settings_json,
                &["player_0"],
                &["player_0", "player_1"],
                &stats,
                true,
            )
            .await;

        // Leaderboard sees the standard-ruleset game.
        let boards = storage
            .get_leaderboards_async()
            .await
            .expect("async leaderboard query");
        assert!(
            boards.iter().any(|b| b.player_id == "player_0"),
            "async leaderboard must see the recorded player ({} boards)",
            boards.len()
        );

        // Replay list sees the row, newest first, with the stored players JSON.
        let records = storage.list_game_metadata_async().await;
        assert_eq!(records.len(), 1, "exactly one recorded game: {records:?}");
        assert_eq!(records[0].id, "game-async-1");
        assert_eq!(records[0].players_json, r#"["player_0","player_1"]"#);
        assert_eq!(records[0].winner, r#""player_0""#);
        assert_eq!(records[0].start_time, 1_700_000_000);
        assert_eq!(records[0].end_time, 1_700_000_600);

        // The replay resolver finds the persisted path.
        let path = storage
            .find_replay_path_async("game-async-1")
            .await
            .expect("replay path must resolve");
        assert!(
            path.ends_with("game-async-1.jsonl"),
            "unexpected path: {path}"
        );
        assert!(
            storage
                .find_replay_path_async("missing-game")
                .await
                .is_none(),
            "unknown game id must resolve to None"
        );

        // Name upsert lands in player_names (visible through the leaderboard JOIN).
        storage
            .upsert_player_names_async(&[("player_0".to_string(), "Alice".to_string())])
            .await
            .expect("name upsert must succeed");
        let boards = storage
            .get_leaderboards_async()
            .await
            .expect("leaderboard query");
        // The name column is a JSON string (see upsert_player_names_is_idempotent).
        assert_eq!(
            boards
                .iter()
                .find(|b| b.player_id == "player_0")
                .unwrap()
                .name,
            r#""Alice""#
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// `upsert_player_names` is idempotent (INSERT OR REPLACE): re-writing a
    /// name updates the row, and the sync core reports `Ok`.
    #[test]
    fn upsert_player_names_is_idempotent() {
        let (storage, dir) = temp_storage();
        storage
            .upsert_player_names(&[("player_0".to_string(), "Alice".to_string())])
            .expect("first upsert");
        storage
            .upsert_player_names(&[("player_0".to_string(), "Alicia".to_string())])
            .expect("second upsert");

        let db = storage.db.lock().unwrap();
        let mut stmt = db
            .prepare("SELECT name FROM player_names WHERE player_id = ?")
            .unwrap();
        stmt.bind([Value::String("player_0".to_string())].as_slice())
            .unwrap();
        assert!(matches!(stmt.next(), Ok(State::Row)), "row must exist");
        let name: String = stmt.read(0).unwrap();
        assert_eq!(name, r#""Alicia""#, "the name column is a JSON string");

        std::fs::remove_dir_all(&dir).ok();
    }

    /// `persist_game_end` (moved from handlers/http.rs, Arch #8) must write
    /// the game-end row exactly as the old HTTP-handler location did: real
    /// wall-clock `end_time`, verbatim settings JSON, winner + players.
    #[test]
    fn persist_game_end_writes_metadata_row() {
        let (storage, _dir) = temp_storage();
        let map = crate::map::map_config::load_default_map();
        let settings = GameSettings {
            starting_funds: 100,
            initial_manpower: 1000,
            funds_per_city: 1.0,
            recruitment_rate: 1.0,
            game_speed: 1.0,
            use_standard_ruleset: true,
            unit_types: vec![],
            city_upgrade_settings: crate::config::CityUpgradeSettings::default(),
            water_speed: 0.5,
            influence: crate::game::game::InfluenceSettings::default(),
        };
        let game = crate::game::game::Game::new(map, settings, 2);
        let before = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs();
        persist_game_end(
            &storage,
            "game-persist-1",
            1_700_000_000,
            r#"{"map":"default"}"#,
            &["player_0"],
            &["player_0", "player_1"],
            &game,
        );
        let after = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs();

        let records = storage.list_game_metadata();
        assert_eq!(records.len(), 1, "one game-end row: {records:?}");
        let rec = &records[0];
        assert_eq!(rec.id, "game-persist-1");
        assert_eq!(rec.start_time, 1_700_000_000);
        assert!(
            (before as i64..=after as i64).contains(&rec.end_time),
            "end_time must be the real wall clock ({before}..={after}), got {}",
            rec.end_time
        );
        assert_eq!(rec.winner, r#""player_0""#);
    }

    /// Temp storage without opening (for the error-path tests): a unique dir
    /// plus the db/replay paths that `temp_storage` would use.
    fn temp_storage_path() -> (std::path::PathBuf, std::path::PathBuf) {
        let dir = std::env::temp_dir().join(format!("wod-storage-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        (dir.clone(), dir.join("db.sqlite"))
    }
}
