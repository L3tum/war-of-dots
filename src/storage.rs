//! SQLite storage for game metadata, leaderboards

use sqlite::{State, Value};
use std::collections::HashMap;
use std::fs;
use std::sync::{Arc, Mutex};

/// Re-export player stats from game module
pub use crate::game::game::PlayerGameStats;

pub struct Storage {
    pub db: Arc<Mutex<sqlite::Connection>>,
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
    pub fn new(path: &str) -> Result<Self, crate::errors::StorageError> {
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
        db.execute("CREATE TABLE IF NOT EXISTS player_stats (player_id TEXT PRIMARY KEY, total_games INTEGER DEFAULT 0, games_won INTEGER DEFAULT 0, total_casualties INTEGER DEFAULT 0, standard_ruleset_games INTEGER DEFAULT 0, standard_ruleset_wins INTEGER DEFAULT 0, standard_ruleset_casualties INTEGER DEFAULT 0)").map_err(|source| crate::errors::StorageError::Execute(Box::new(source)))?;
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
        })
    }

    pub fn record_game_end(
        &self,
        game_id: &str,
        end_time: u64,
        winners: &[&str],
        players: &[&str],
        player_stats: &HashMap<String, PlayerGameStats>,
        is_standard_ruleset: bool,
    ) {
        let replay_path = format!("replays/{}.jsonl", game_id);
        let players_json = serde_json::to_string(players).unwrap();
        let winner = serde_json::to_string(winners.first().unwrap_or(&"")).unwrap();

        // Record game metadata (parameterized query — no SQL injection)
        {
            let db = match self.db.lock() {
                Ok(d) => d,
                Err(e) => {
                    tracing::error!(%e, "Failed to acquire DB lock for game_metadata");
                    return;
                }
            };
            let mut stmt = match db.prepare(
                "INSERT OR IGNORE INTO game_metadata (id, settings, players, start_time, end_time, winner, replay_path) VALUES (?, ?, ?, ?, ?, ?, ?)",
            ) {
                Ok(s) => s,
                Err(e) => {
                    tracing::error!(%e, "Failed to prepare game_metadata insert");
                    return;
                }
            };
            let params: Vec<Value> = vec![
                Value::String(game_id.to_string()),
                Value::String(String::new()),
                Value::String(players_json),
                Value::Integer(0),
                Value::Integer(end_time as i64),
                Value::String(winner),
                Value::String(replay_path),
            ];
            if let Err(e) = stmt.bind(params.as_slice()) {
                tracing::error!(%e, game_id, "Failed to bind game_metadata params");
                return;
            }
            if let Err(e) = stmt.next() {
                tracing::error!(%e, game_id, "Failed to insert game metadata");
            }
        }

        // Record per-player statistics (parameterized queries — no SQL injection)
        for (player_id, stats) in player_stats {
            let casualties = stats.units_lost as i64;
            let games_won = if winners.contains(&player_id.as_str()) {
                1i64
            } else {
                0i64
            };

            let db = match self.db.lock() {
                Ok(d) => d,
                Err(e) => {
                    tracing::error!(%e, "Failed to acquire DB lock for player_stats");
                    continue;
                }
            };

            if is_standard_ruleset {
                let mut stmt = match db.prepare(
                    "INSERT OR REPLACE INTO player_stats (player_id, standard_ruleset_games, standard_ruleset_wins, standard_ruleset_casualties, total_casualties) \
                     VALUES (?, COALESCE((SELECT standard_ruleset_games FROM player_stats WHERE player_id=?), 0) + 1, \
                     COALESCE((SELECT standard_ruleset_wins FROM player_stats WHERE player_id=?), 0) + ?, \
                     COALESCE((SELECT standard_ruleset_casualties FROM player_stats WHERE player_id=?), 0) + ?, \
                     COALESCE((SELECT total_casualties FROM player_stats WHERE player_id=?), 0) + ?)",
                ) {
                    Ok(s) => s,
                    Err(e) => {
                        tracing::error!(%e, "Failed to prepare player_stats insert");
                        continue;
                    }
                };
                let pid = player_id.as_str();
                let params: Vec<Value> = vec![
                    Value::String(pid.to_string()),
                    Value::String(pid.to_string()),
                    Value::String(pid.to_string()),
                    Value::Integer(games_won),
                    Value::String(pid.to_string()),
                    Value::Integer(casualties),
                    Value::String(pid.to_string()),
                    Value::Integer(casualties),
                ];
                if let Err(e) = stmt.bind(params.as_slice()) {
                    tracing::error!(%e, player_id, "Failed to bind player_stats params");
                    continue;
                }
                if let Err(e) = stmt.next() {
                    tracing::error!(%e, player_id, "Failed to insert player_stats");
                }
            } else {
                let mut stmt = match db.prepare(
                    "INSERT OR REPLACE INTO player_stats (player_id, total_casualties) \
                     VALUES (?, COALESCE((SELECT total_casualties FROM player_stats WHERE player_id=?), 0) + ?)",
                ) {
                    Ok(s) => s,
                    Err(e) => {
                        tracing::error!(%e, "Failed to prepare player_stats insert");
                        continue;
                    }
                };
                let pid = player_id.as_str();
                let params: Vec<Value> = vec![
                    Value::String(pid.to_string()),
                    Value::String(pid.to_string()),
                    Value::Integer(casualties),
                ];
                if let Err(e) = stmt.bind(params.as_slice()) {
                    tracing::error!(%e, player_id, "Failed to bind player_stats params");
                    continue;
                }
                if let Err(e) = stmt.next() {
                    tracing::error!(%e, player_id, "Failed to insert player_stats");
                }
            }

            // Record player name
            let actual_name = format!("Player {}", player_id);
            let name_str = actual_name
                .strip_prefix("Player player_")
                .map(|s| s.to_string())
                .unwrap_or_else(|| actual_name.clone());
            let name_json = serde_json::to_string(&name_str).unwrap_or(format!("\"{}\"", name_str));

            let mut stmt = match db
                .prepare("INSERT OR REPLACE INTO player_names (player_id, name) VALUES (?, ?)")
            {
                Ok(s) => s,
                Err(e) => {
                    tracing::error!(%e, "Failed to prepare player_names insert");
                    continue;
                }
            };
            let params: Vec<Value> = vec![
                Value::String(player_id.to_string()),
                Value::String(name_json),
            ];
            if let Err(e) = stmt.bind(params.as_slice()) {
                tracing::error!(%e, player_id, "Failed to bind player_names params");
                continue;
            }
            if let Err(e) = stmt.next() {
                tracing::error!(%e, player_id, "Failed to insert player_names");
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
            let standard_wins: i64 = stmt.read(2).unwrap_or(0);
            let standard_games: i64 = stmt.read(3).unwrap_or(0);
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
}

pub struct PlayerStat {
    pub player_id: String,
    pub name: String,
    pub games_played: i64,
    pub games_won: i64,
    pub avg_casualties: f32,
}
