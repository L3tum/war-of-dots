//! Debug overlay module.
//!
//! Provides runtime performance statistics for development and debugging.
//! Only compiled when `debug-overlay` feature is enabled or `debug_assertions` is set.

use std::sync::Arc;

use axum::extract::State;
use axum::response::Json;
use tokio::sync::Mutex;

use crate::server::GameServer;

/// GET /debug/stats — return performance statistics for all active games.
pub async fn debug_stats_handler(
    State(state): State<Arc<Mutex<GameServer>>>,
) -> Json<serde_json::Value> {
    #[cfg(any(debug_assertions, feature = "debug-overlay"))]
    {
        let server = state.lock().await;
        let games = server.games.lock().await;

        let mut total_tick_count: u64 = 0;
        let mut total_last_tick_ms: f64 = 0.0;
        let mut total_entities: usize = 0;
        let mut games_with_stats: usize = 0;
        for room in games.values() {
            if let Ok(room) = room.try_lock() {
                let gs = room.stats.lock().unwrap_or_else(|e| e.into_inner());
                total_tick_count += gs.tick_count;
                total_last_tick_ms += gs.last_tick_ms;
                total_entities += gs.entity_count;
                games_with_stats += 1;
            }
        }

        Json(serde_json::json!({
            "tick_count": total_tick_count,
            "last_tick_ms": total_last_tick_ms,
            "avg_tick_ms": if games_with_stats > 0 { total_last_tick_ms / games_with_stats as f64 } else { 0.0 },
            "entity_count": total_entities,
            "archetype_count": 0,
            "active_games": games.len(),
            "total_ticks_ms": total_last_tick_ms,
            // system_timings is populated by the benchmark or per-game tick stats
            "system_timings": null,
        }))
    }

    #[cfg(not(any(debug_assertions, feature = "debug-overlay")))]
    {
        let server = state.lock().await;
        let games = server.games.lock().await;

        Json(serde_json::json!({
            "error": "Debug stats not available in release builds",
            "hint": "Recompile with --features debug-overlay or in debug mode",
            "active_games": games.len(),
        }))
    }
}

/// GET /debug/benchmark — run a benchmark with default params.
pub async fn debug_benchmark_handler(
    axum::extract::Query(params): axum::extract::Query<std::collections::HashMap<String, String>>,
) -> Json<serde_json::Value> {
    let units_per_city: u32 = params
        .get("units_per_city")
        .and_then(|s| s.parse().ok())
        .unwrap_or(5);
    let ticks: u64 = params
        .get("ticks")
        .and_then(|s| s.parse().ok())
        .unwrap_or(500);

    let config = crate::benchmark::BenchmarkConfig {
        units_per_city: units_per_city.clamp(1, 10),
        ticks: ticks.clamp(10, 2000),
        seed: 0,
        detailed_systems: Vec::new(),
        bench_prev: None,
    };

    // Run benchmark in a blocking thread
    let result = tokio::task::spawn_blocking(move || crate::benchmark::run_benchmark(config))
        .await
        .unwrap();

    Json(result.summary)
}

/// GET /debug/tick — return tick timing info (read-only).
pub async fn debug_tick_handler(
    State(state): State<Arc<Mutex<GameServer>>>,
) -> Json<serde_json::Value> {
    #[cfg(any(debug_assertions, feature = "debug-overlay"))]
    {
        let server = state.lock().await;
        let games = server.games.lock().await;

        let mut results = Vec::new();
        for (game_id, _room) in games.iter() {
            results.push(serde_json::json!({
                "game_id": game_id,
            }));
        }

        Json(serde_json::json!({
            "active_games": results.len(),
            "games": results,
        }))
    }

    #[cfg(not(any(debug_assertions, feature = "debug-overlay")))]
    {
        Json(serde_json::json!({
            "error": "Debug tick not available in release builds",
        }))
    }
}
