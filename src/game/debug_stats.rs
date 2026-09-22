//! Debug statistics for the overlay.
//!
//! Tracks per-tick performance metrics: tick duration, entity counts, system timing.
//! Only compiled when `debug-overlay` feature is enabled or `debug_assertions` is set.
//!
//! NOTE: Some items are unused because the tick_system! macro can't invoke them
//! due to Rust's macro hygiene. Kept for future use.
#![allow(dead_code)]

use std::time::Instant;

use hecs::World;

use super::world_ext::WorldExt;

/// Debug system guard — tracks tick timing.
pub struct DebugSystemGuard {
    pub world_entity: hecs::Entity,
    pub tick_start: Instant,
}

impl DebugSystemGuard {
    /// Create a new guard that will record tick timing.
    #[cfg(any(debug_assertions, feature = "debug-overlay"))]
    pub fn new(world: &mut World, world_entity: hecs::Entity) -> Self {
        // Reset all system last_ms at start of each tick so benchmark
        // only sums systems that were actually called this tick.
        if let Some(wb) = world.world_data_mut(world_entity) {
            for timings in wb.debug_stats.system_timings.values_mut() {
                timings.last_ms = 0.0;
            }
        }

        Self {
            world_entity,
            tick_start: Instant::now(),
        }
    }
    /// Release builds record no tick timing; the stub keeps call sites
    /// (and the struct fields, used by `SystemTimer`) uniform.
    #[cfg(not(any(debug_assertions, feature = "debug-overlay")))]
    pub fn new(_world: &mut World, world_entity: hecs::Entity) -> Self {
        Self {
            world_entity,
            tick_start: Instant::now(),
        }
    }

    /// Record the tick duration and update stats.
    #[cfg(any(debug_assertions, feature = "debug-overlay"))]
    pub fn finish(self, world: &mut World) {
        let duration = self.tick_start.elapsed().as_secs_f64() * 1000.0;

        // Query entity count
        let entity_count = world.iter().count();
        let archetype_count = world.archetypes().len();

        // Update debug stats in WorldBundle
        if let Some(wb) = world.world_data_mut(self.world_entity) {
            let ds = &mut wb.debug_stats;
            ds.tick_count += 1;
            ds.entity_count = entity_count as u32;
            ds.archetype_count = archetype_count as u32;
            ds.last_tick_ms = duration;
            ds.total_ticks_ms += duration;

            // Keep only last 100 tick durations
            if ds.tick_durations_ms.len() > 100 {
                ds.tick_durations_ms.remove(0);
            }
            ds.tick_durations_ms.push(duration);
        }
    }
    /// Release builds record no tick stats; this is a no-op.
    #[cfg(not(any(debug_assertions, feature = "debug-overlay")))]
    pub fn finish(self, _world: &mut World) {}

    /// Record timing for a specific system.
    #[cfg(any(debug_assertions, feature = "debug-overlay"))]
    pub fn record_system(&self, world: &mut World, system_name: &str, duration_ms: f64) {
        if let Some(wb) = world.world_data_mut(self.world_entity) {
            let ds = &mut wb.debug_stats;
            let timings = &mut ds.system_timings;
            let entry = timings.entry(system_name.to_string()).or_default();
            entry.record(duration_ms);
        }
    }
    /// Release builds record no system timings; this is a no-op.
    #[cfg(not(any(debug_assertions, feature = "debug-overlay")))]
    pub fn record_system(&self, _world: &mut World, _system_name: &str, _duration_ms: f64) {}

    /// Record timing for a specific system sub-part (detailed benchmark mode).
    #[allow(dead_code)] // Used in debug/benchmark builds via feature flags
    #[cfg(any(debug_assertions, feature = "debug-overlay"))]
    pub fn record_detailed(&self, world: &mut World, key: &str, duration_ms: f64) {
        if let Some(wb) = world.world_data_mut(self.world_entity) {
            let ds = &mut wb.debug_stats;
            // Only record if the parent system is in detailed mode
            let parent = key.split('/').next().unwrap_or("");
            if !ds.detailed_systems.contains(parent) {
                return;
            }
            let timings = &mut ds.detailed_timings;
            // get-first: no throwaway `to_string()` per call (see record_system_timing).
            let entry = match timings.get_mut(key) {
                Some(e) => e,
                None => timings.entry(key.to_string()).or_default(),
            };
            entry.record(duration_ms);
        }
    }
    /// Release builds record no detailed timings; this is a no-op.
    #[allow(dead_code)] // Kept in lockstep with the debug implementation above
    #[cfg(not(any(debug_assertions, feature = "debug-overlay")))]
    pub fn record_detailed(&self, _world: &mut World, _key: &str, _duration_ms: f64) {}
}

/// Helper to measure system execution time.
pub struct SystemTimer {
    pub name: String,
    pub start: Instant,
}

impl SystemTimer {
    pub fn new(name: &str) -> Self {
        Self {
            name: name.to_string(),
            start: Instant::now(),
        }
    }

    pub fn finish(self, guard: &DebugSystemGuard, world: &mut World) -> f64 {
        let duration = self.start.elapsed().as_secs_f64() * 1000.0;
        guard.record_system(world, &self.name, duration);
        duration
    }
}

/// Helper to measure sub-part timing within a system (detailed benchmark mode).
/// Drop it to record the timing.
pub struct DetailedTimer {
    pub key: String,
    pub start: Instant,
}

impl DetailedTimer {
    pub fn new(key: &str) -> Self {
        Self {
            key: key.to_string(),
            start: Instant::now(),
        }
    }

    pub fn finish(self, world: &mut World, world_entity: hecs::Entity) -> f64 {
        let duration = self.start.elapsed().as_secs_f64() * 1000.0;
        world.record_detailed(world_entity, &self.key, duration);
        duration
    }
}

/// Record a whole-system timing into `system_timings` (the per-system breakdown
/// read by the benchmark and the debug overlay). Called once per system invocation
/// by the `tick_system!` macro; the world/world_entity are passed explicitly so
/// the macro does not need to touch `self`/`guard` (macro-hygiene safe).
#[cfg(any(debug_assertions, feature = "debug-overlay"))]
pub fn record_system_timing(
    world: &mut World,
    world_entity: hecs::Entity,
    system_name: &str,
    duration_ms: f64,
) {
    if let Some(wb) = world.world_data_mut(world_entity) {
        let ds = &mut wb.debug_stats;
        let timings = &mut ds.system_timings;
        // `get_mut` borrows via `String: Borrow<str>` — no throwaway `to_string()`
        // per call (this runs once per system per tick and its allocs land in the
        // unattributed “other/overhead” bucket). `entry`/`to_string` only fires when
        // a system name is recorded for the first time.
        let entry = match timings.get_mut(system_name) {
            Some(e) => e,
            None => timings.entry(system_name.to_string()).or_default(),
        };
        entry.record(duration_ms);
    }
}

/// Record per-system allocation totals into `system_allocs`. Called once per
/// system invocation by the `tick_system!` macro (bench-alloc builds only).
/// `allocs`/`bytes` are the counting-allocator delta for this one system call.
///
/// Requires both the debug stats (debug-overlay/debug_assertions) and the
/// counting allocator (bench-alloc); the benchmark enables both.
#[cfg(all(
    any(debug_assertions, feature = "debug-overlay"),
    feature = "bench-alloc"
))]
pub fn record_system_alloc(
    world: &mut World,
    world_entity: hecs::Entity,
    system_name: &str,
    allocs: u64,
    bytes: u64,
) {
    if let Some(wb) = world.world_data_mut(world_entity) {
        let ds = &mut wb.debug_stats;
        // get-first: `get_mut` borrows via `Borrow<str>` (no throwaway `to_string()`
        // per call — ~9×/tick in the unattributed bucket); `entry` only on first record.
        let entry = match ds.system_allocs.get_mut(system_name) {
            Some(e) => e,
            None => ds.system_allocs.entry(system_name.to_string()).or_default(),
        };
        entry.total_allocs += allocs;
        entry.total_bytes += bytes;
        entry.call_count += 1;
    }
}

/// Run-wide percentile from a `SystemTimings` log histogram.
///
/// `rank = p × total_calls`; returns the **upper edge** of the first bucket
/// whose cumulative count reaches the rank (see `system_timings_bucket_upper_edge`).
/// Semantics: run-wide (all calls since start), and the value is a bucket
/// edge, so it overestimates the true percentile by at most the bucket factor
/// 10^(1/3) ≈ 1.21. Returns 0.0 for an empty histogram.
pub fn hist_percentile(
    hist: &[u32; super::components::SYSTEM_TIMINGS_HIST_BUCKETS],
    p: f64,
) -> f64 {
    let total: u64 = hist.iter().map(|&c| c as u64).sum();
    if total == 0 {
        return 0.0;
    }
    let p = p.clamp(0.0, 1.0);
    let rank = (p * total as f64).floor().max(1.0) as u64;
    let mut cumulative = 0u64;
    for (i, &count) in hist.iter().enumerate() {
        cumulative += count as u64;
        if cumulative >= rank {
            return super::components::system_timings_bucket_upper_edge(i);
        }
    }
    // Unreachable (cumulative reaches total ≥ rank), but keep a defined
    // fallback for the last bucket's upper edge.
    super::components::system_timings_bucket_upper_edge(hist.len() - 1)
}

/// Get debug stats as JSON-serializable structure.
pub fn get_debug_stats(world: &World, world_entity: hecs::Entity) -> Option<serde_json::Value> {
    #[cfg(any(debug_assertions, feature = "debug-overlay"))]
    {
        let wb = world.world_data(world_entity)?;
        let stats = &wb.debug_stats;
        let tick_durations: Vec<f64> = stats.tick_durations_ms.to_vec();
        let avg_tick_ms = if tick_durations.is_empty() {
            0.0
        } else {
            tick_durations.iter().sum::<f64>() / tick_durations.len() as f64
        };

        let system_timings: Vec<serde_json::Value> = stats
            .system_timings
            .iter()
            .map(|(name, timing)| {
                // Run-wide percentiles from the log histogram: the value is
                // the upper edge of the bucket holding the rank-th call, so
                // it overestimates the true percentile by at most ×1.21.
                let p50 = hist_percentile(&timing.hist, 0.5);
                let p90 = hist_percentile(&timing.hist, 0.9);
                let p99 = hist_percentile(&timing.hist, 0.99);
                serde_json::json!({
                    "name": name,
                    "total_ms": timing.total_ms,
                    "call_count": timing.call_count,
                    "last_ms": timing.last_ms,
                    "avg_ms": timing.avg_ms,
                    "p50_ms": p50,
                    "p90_ms": p90,
                    "p99_ms": p99,
                })
            })
            .collect();

        // Detailed sub-part timings
        let detailed_timings_json: Vec<serde_json::Value> = stats
            .detailed_timings
            .iter()
            .map(|(key, timing)| {
                let p50 = hist_percentile(&timing.hist, 0.5);
                let p90 = hist_percentile(&timing.hist, 0.9);
                let p99 = hist_percentile(&timing.hist, 0.99);
                serde_json::json!({
                    "name": key,
                    "total_ms": timing.total_ms,
                    "call_count": timing.call_count,
                    "last_ms": timing.last_ms,
                    "avg_ms": timing.avg_ms,
                    "p50_ms": p50,
                    "p90_ms": p90,
                    "p99_ms": p99,
                })
            })
            .collect();

        // Per-system allocation totals (bench-alloc builds only). avg_* are
        // per-call averages; the benchmark aggregates across runs and formats.
        // Computed in both cfg branches (empty when bench-alloc is off) so the
        // assignment below is unconditional and `stats_json` always needs `mut`.
        let system_allocs_json: Vec<serde_json::Value> = {
            #[cfg(feature = "bench-alloc")]
            {
                stats
                    .system_allocs
                    .iter()
                    .map(|(name, a)| {
                        let calls = a.call_count.max(1) as f64;
                        serde_json::json!({
                            "name": name,
                            "total_allocs": a.total_allocs,
                            "total_bytes": a.total_bytes,
                            "call_count": a.call_count,
                            "avg_allocs": a.total_allocs as f64 / calls,
                            "avg_bytes": a.total_bytes as f64 / calls,
                        })
                    })
                    .collect()
            }
            #[cfg(not(feature = "bench-alloc"))]
            {
                Vec::new()
            }
        };

        let mut stats_json = serde_json::json!({
            "tick_count": stats.tick_count,
            "entity_count": stats.entity_count,
            "archetype_count": stats.archetype_count,
            "last_tick_ms": stats.last_tick_ms,
            "avg_tick_ms": avg_tick_ms,
            "total_ticks_ms": stats.total_ticks_ms,
            "tick_durations_ms": tick_durations,
            "system_timings": system_timings,
            "detailed_timings": detailed_timings_json,
        });
        stats_json["system_allocs"] = serde_json::json!(system_allocs_json);

        Some(stats_json)
    }

    #[cfg(not(any(debug_assertions, feature = "debug-overlay")))]
    {
        // No debug stats to serve in release builds; keep the debug branch's
        // "None when the world entity has no data" contract.
        if world.world_data(world_entity).is_none() {
            return None;
        }
        None
    }
}

#[cfg(test)]
mod tests {
    use super::super::components::{
        system_timings_bucket_index, SystemTimings, SYSTEM_TIMINGS_HIST_BUCKETS,
    };
    use super::hist_percentile;

    #[test]
    fn record_updates_scalars_and_histogram_bucket() {
        let mut t = SystemTimings::default();
        t.record(1.0);
        t.record(1.0);
        t.record(100.0);
        assert_eq!(t.call_count, 3);
        assert!((t.total_ms - 102.0).abs() < 1e-12);
        assert!((t.last_ms - 100.0).abs() < 1e-12);
        assert!((t.avg_ms - 102.0 / 3.0).abs() < 1e-12);
        let total_hist: u64 = t.hist.iter().map(|&c| c as u64).sum();
        assert_eq!(total_hist, 3);
        assert_eq!(t.hist[system_timings_bucket_index(1.0)], 2);
        assert_eq!(t.hist[system_timings_bucket_index(100.0)], 1);
    }

    #[test]
    fn record_clamps_out_of_range_durations() {
        let mut t = SystemTimings::default();
        t.record(0.0005); // below the first edge (0.001 ms) → bucket 0
        t.record(99999.0); // ≥ 10 s → last bucket
        t.record(0.0); // zero → bucket 0 (no NaN in the index math)
        assert_eq!(t.hist[0], 2);
        assert_eq!(t.hist[SYSTEM_TIMINGS_HIST_BUCKETS - 1], 1);
    }

    #[test]
    fn bucket_index_is_monotonic_and_decadal() {
        // 3 buckets per decade: 0.001 ms → 0, 0.01 → 3, 0.1 → 6, 1 → 9,
        // 10 → 12, 100 → 15, 1000 → 18, 10000 → 21, 99999 → 23 (clamped).
        assert_eq!(system_timings_bucket_index(0.001), 0);
        assert_eq!(system_timings_bucket_index(0.01), 3);
        assert_eq!(system_timings_bucket_index(0.1), 6);
        assert_eq!(system_timings_bucket_index(1.0), 9);
        assert_eq!(system_timings_bucket_index(10.0), 12);
        assert_eq!(system_timings_bucket_index(100.0), 15);
        assert_eq!(system_timings_bucket_index(1000.0), 18);
        assert_eq!(system_timings_bucket_index(10000.0), 21);
        assert_eq!(
            system_timings_bucket_index(99999.0),
            SYSTEM_TIMINGS_HIST_BUCKETS - 1
        );
    }

    #[test]
    fn hist_percentile_returns_bucket_edges() {
        // 100 calls, all in the 1 ms bucket (index 9): every percentile is
        // that bucket's upper edge (≈1.259 ms = 10^(1/3) ms).
        let mut hist = [0u32; SYSTEM_TIMINGS_HIST_BUCKETS];
        hist[9] = 100;
        let edge = super::super::components::system_timings_bucket_upper_edge(9);
        assert!((hist_percentile(&hist, 0.5) - edge).abs() < 1e-9);
        assert!((hist_percentile(&hist, 0.9) - edge).abs() < 1e-9);
        assert!((hist_percentile(&hist, 0.99) - edge).abs() < 1e-9);

        // 90 calls in bucket 9, 10 in bucket 15 (100 ms bucket): p50/p90 hit
        // bucket 9's edge, p99 hits bucket 15's edge.
        let mut hist2 = [0u32; SYSTEM_TIMINGS_HIST_BUCKETS];
        hist2[9] = 90;
        hist2[15] = 10;
        let edge9 = super::super::components::system_timings_bucket_upper_edge(9);
        let edge15 = super::super::components::system_timings_bucket_upper_edge(15);
        assert!((hist_percentile(&hist2, 0.5) - edge9).abs() < 1e-9);
        assert!((hist_percentile(&hist2, 0.9) - edge9).abs() < 1e-9);
        assert!((hist_percentile(&hist2, 0.99) - edge15).abs() < 1e-9);

        // Empty histogram → 0.0.
        let empty = [0u32; SYSTEM_TIMINGS_HIST_BUCKETS];
        assert_eq!(hist_percentile(&empty, 0.5), 0.0);
    }
}
