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
    pub fn new(world: &mut World, world_entity: hecs::Entity) -> Self {
        #[cfg(any(debug_assertions, feature = "debug-overlay"))]
        {
            // Reset all system last_ms at start of each tick so benchmark
            // only sums systems that were actually called this tick.
            if let Some(wb) = world.world_data_mut(world_entity) {
                for timings in wb.debug_stats.system_timings.values_mut() {
                    timings.last_ms = 0.0;
                }
            }
        }
        let now = Instant::now();

        Self {
            world_entity,
            tick_start: now,
        }
    }

    /// Record the tick duration and update stats.
    pub fn finish(self, world: &mut World) {
        let duration = self.tick_start.elapsed().as_secs_f64() * 1000.0;

        // Query entity count
        let entity_count = world.iter().count();
        let archetype_count = world.archetypes().len();

        // Update debug stats in WorldBundle
        if let Some(wb) = world.world_data_mut(self.world_entity) {
            #[cfg(any(debug_assertions, feature = "debug-overlay"))]
            {
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
    }

    /// Record timing for a specific system.
    pub fn record_system(&self, world: &mut World, system_name: &str, duration_ms: f64) {
        #[cfg(any(debug_assertions, feature = "debug-overlay"))]
        {
            if let Some(wb) = world.world_data_mut(self.world_entity) {
                let ds = &mut wb.debug_stats;
                let timings = &mut ds.system_timings;
                let entry = timings.entry(system_name.to_string()).or_insert_with(|| {
                    super::components::SystemTimings {
                        total_ms: 0.0,
                        call_count: 0,
                        last_ms: 0.0,
                        avg_ms: 0.0,
                        durations_ms: Vec::new(),
                    }
                });
                entry.total_ms += duration_ms;
                entry.call_count += 1;
                entry.last_ms = duration_ms;
                entry.avg_ms = entry.total_ms / entry.call_count as f64;
                // Keep bounded buffer for P90/P99
                if entry.durations_ms.len() > 200 {
                    entry.durations_ms.remove(0);
                }
                entry.durations_ms.push(duration_ms);
            }
        }
    }

    /// Record timing for a specific system sub-part (detailed benchmark mode).
    #[allow(dead_code)] // Used in debug/benchmark builds via feature flags
    pub fn record_detailed(&self, world: &mut World, key: &str, duration_ms: f64) {
        #[cfg(any(debug_assertions, feature = "debug-overlay"))]
        {
            if let Some(wb) = world.world_data_mut(self.world_entity) {
                let ds = &mut wb.debug_stats;
                // Only record if the parent system is in detailed mode
                let parent = key.split('/').next().unwrap_or("");
                if !ds.detailed_systems.contains(parent) {
                    return;
                }
                let timings = &mut ds.detailed_timings;
                let entry = timings.entry(key.to_string()).or_insert_with(|| {
                    super::components::SystemTimings {
                        total_ms: 0.0,
                        call_count: 0,
                        last_ms: 0.0,
                        avg_ms: 0.0,
                        durations_ms: Vec::new(),
                    }
                });
                entry.total_ms += duration_ms;
                entry.call_count += 1;
                entry.last_ms = duration_ms;
                entry.avg_ms = entry.total_ms / entry.call_count as f64;
                if entry.durations_ms.len() > 200 {
                    entry.durations_ms.remove(0);
                }
                entry.durations_ms.push(duration_ms);
            }
        }
    }
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
        let entry = timings.entry(system_name.to_string()).or_insert_with(|| {
            super::components::SystemTimings {
                total_ms: 0.0,
                call_count: 0,
                last_ms: 0.0,
                avg_ms: 0.0,
                durations_ms: Vec::new(),
            }
        });
        entry.total_ms += duration_ms;
        entry.call_count += 1;
        entry.last_ms = duration_ms;
        entry.avg_ms = entry.total_ms / entry.call_count as f64;
        // Keep bounded buffer for P90/P99
        if entry.durations_ms.len() > 200 {
            entry.durations_ms.remove(0);
        }
        entry.durations_ms.push(duration_ms);
    }
}

/// Get debug stats as JSON-serializable structure.
pub fn get_debug_stats(world: &World, world_entity: hecs::Entity) -> Option<serde_json::Value> {
    let wb_ref = world.world_data(world_entity)?;
    let wb = &*wb_ref;

    #[cfg(any(debug_assertions, feature = "debug-overlay"))]
    {
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
                let mut durations_sorted = timing.durations_ms.clone();
                durations_sorted.sort_by(|a, b| a.partial_cmp(b).unwrap());
                let p50 = if durations_sorted.is_empty() {
                    0.0
                } else {
                    durations_sorted[((durations_sorted.len() as f64 * 0.5) as usize)
                        .min(durations_sorted.len() - 1)]
                };
                let p90 = if durations_sorted.is_empty() {
                    0.0
                } else {
                    durations_sorted[((durations_sorted.len() as f64 * 0.9) as usize)
                        .min(durations_sorted.len() - 1)]
                };
                let p99 = if durations_sorted.is_empty() {
                    0.0
                } else {
                    durations_sorted[((durations_sorted.len() as f64 * 0.99) as usize)
                        .min(durations_sorted.len() - 1)]
                };
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
                let mut durations_sorted = timing.durations_ms.clone();
                durations_sorted.sort_by(|a, b| a.partial_cmp(b).unwrap());
                let p50 = if durations_sorted.is_empty() {
                    0.0
                } else {
                    durations_sorted[((durations_sorted.len() as f64 * 0.5) as usize)
                        .min(durations_sorted.len() - 1)]
                };
                let p90 = if durations_sorted.is_empty() {
                    0.0
                } else {
                    durations_sorted[((durations_sorted.len() as f64 * 0.9) as usize)
                        .min(durations_sorted.len() - 1)]
                };
                let p99 = if durations_sorted.is_empty() {
                    0.0
                } else {
                    durations_sorted[((durations_sorted.len() as f64 * 0.99) as usize)
                        .min(durations_sorted.len() - 1)]
                };
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

        Some(serde_json::json!({
            "tick_count": stats.tick_count,
            "entity_count": stats.entity_count,
            "archetype_count": stats.archetype_count,
            "last_tick_ms": stats.last_tick_ms,
            "avg_tick_ms": avg_tick_ms,
            "total_ticks_ms": stats.total_ticks_ms,
            "tick_durations_ms": tick_durations,
            "system_timings": system_timings,
            "detailed_timings": detailed_timings_json,
        }))
    }

    #[cfg(not(any(debug_assertions, feature = "debug-overlay")))]
    {
        None
    }
}
