//! Debug overlay module.
//!
//! Provides runtime performance statistics for development and debugging.
//! The /debug/* routes are only registered in debug builds or with the
//! `debug-overlay` feature (`GameServer::build_router`), and every handler
//! re-checks the same condition — plus the runtime `debug.endpoints`
//! config flag — at request time (Sec L1: see `debug_gate` below and
//! docs/THREAT_MODEL.md), so an exposed route must not execute debug
//! logic in a non-debug deployment.

use std::sync::Arc;

use axum::extract::State;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Json, Response};
use tokio::sync::Mutex;

use crate::server::GameServer;

/// Sec L1: the request-time gate for every /debug/* handler below. The
/// routes are only registered in debug builds / with `debug-overlay`
/// (see `GameServer::build_router`), but each handler re-checks
/// `debug_endpoints_enabled` at request time, so a route that is ever
/// exposed (misconfigured proxy, feature-flag slip, a test app registering
/// the route unconditionally) must not execute debug logic — in particular
/// the CPU-heavy benchmark. The gate is the single source of truth:
/// `debug_endpoints_enabled` combines the same cfg condition as route
/// registration with the `debug.endpoints` config flag (runtime override).
async fn debug_gate(state: &Mutex<GameServer>) -> bool {
    state.lock().await.debug_endpoints_enabled()
}

/// GET /debug/stats — return performance statistics for all active games.
pub async fn debug_stats_handler(State(state): State<Arc<Mutex<GameServer>>>) -> Response {
    if !debug_gate(&state).await {
        return StatusCode::NOT_FOUND.into_response();
    }
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
        .into_response()
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
        .into_response()
    }
}

/// GET /debug/benchmark — run a benchmark with default params.
///
/// Only compiled (and routed) with the `microbenchmarks` feature: the
/// benchmark module itself is feature-gated.
///
/// Sec L1: this endpoint is an unauthenticated CPU-DoS lever; the gate in
/// `debug_gate` (checked at request time, not just at route registration)
/// is what keeps it inert when debug is disabled.
#[cfg(feature = "microbenchmarks")]
pub async fn debug_benchmark_handler(
    State(state): State<Arc<Mutex<GameServer>>>,
    axum::extract::Query(params): axum::extract::Query<std::collections::HashMap<String, String>>,
) -> Response {
    if !debug_gate(&state).await {
        return StatusCode::NOT_FOUND.into_response();
    }
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
        initial_units: None,
        map_size: 200,
        cities: 12,
        max_recruits_per_event: 2000,
        bench_spectator: false,
        bench_splice: false,
    };

    // Run benchmark in a blocking thread
    let result = tokio::task::spawn_blocking(move || crate::benchmark::run_benchmark(config))
        .await
        .unwrap();

    Json(result.summary).into_response()
}
