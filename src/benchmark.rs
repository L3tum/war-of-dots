//! Benchmark module — runs simulated game ticks with randomized AI.
//!
//! Creates a game, recruits units, and simulates a battle with:
//! - Randomized unit movement toward enemy positions
//! - Mid-benchmark unit recruiting to test entity spawning

// Benchmark structs have fields for reporting that aren't read in all code paths.
#![allow(dead_code)]
//! - Combat resolution as units collide
//!
//! Outputs detailed HTML report with per-tick charts and per-system P90/P99.
//! Requires `--features debug-overlay` for detailed per-system stats.

use std::time::Instant;

use rand::rngs::StdRng;
use rand::{Rng, SeedableRng};

use crate::config::CityUpgradeSettings;
use crate::game::economy::UnitType;
use crate::game::game::{Game, GameSettings};
use crate::map::map::{Map, MapConfig};
#[cfg(feature = "microbenchmarks")]
use crate::microbenchmarks;

/// Number of measured (timed) runs per benchmark; the per-tick series are
/// averaged across these runs and summary latency/overhead stats are computed
/// over the combined pool of all runs.
pub const MEASURED_RUNS: u32 = 3;
/// Warmup ticks per measured run (discarded; warms caches/archetypes and lets
/// the battle front converge so measured ticks start near steady state).
pub const BENCH_WARMUP_TICKS: u64 = 200;

/// Per-tick benchmark data point
#[derive(Default, Clone)]
pub struct TickData {
    pub total_ms: f64,
    pub entity_count: u32,
    pub unit_count: u32,
    /// Sum of all system timings for this tick (to compute overhead gap)
    pub system_sum_ms: f64,
    /// Number of units that moved this tick (movement.rs mover_count)
    pub movers: u32,
    /// Number of engaged combat pairs this tick (combat.rs engaged_pairs)
    pub combats: u32,
}

/// Benchmark configuration
pub struct BenchmarkConfig {
    pub units_per_city: u32,
    pub ticks: u64,
    pub seed: u64,
    /// Systems to measure in detail (sub-part timing)
    pub detailed_systems: Vec<String>,
    /// Previous HTML report file for comparison (green/red deltas)
    pub bench_prev: Option<String>,
}

impl Default for BenchmarkConfig {
    fn default() -> Self {
        Self {
            units_per_city: 5,
            ticks: 500,
            seed: 0,
            detailed_systems: Vec::new(),
            bench_prev: None,
        }
    }
}

/// Complete benchmark results
pub struct BenchmarkResults {
    pub config: BenchmarkConfig,
    pub tick_data: Vec<TickData>,
    pub summary: serde_json::Value,
}

/// Per-tick AI actions that simulate a battle
struct BattleSimulator {
    rng: StdRng,
    unit_types: Vec<UnitType>,
    city_info: Vec<(String, String)>,
    /// Convergence point for all units (map centre) — creates dense fronts so
    /// the combat/spatialty pipeline is actually exercised by the benchmark.
    center: (f32, f32),
    tick: u64,
}

impl BattleSimulator {
    fn new(
        unit_types: Vec<UnitType>,
        cities: Vec<crate::game::economy::City>,
        seed: u64,
        map_w: f32,
        map_h: f32,
    ) -> Self {
        let rng = if seed == 0 {
            StdRng::from_entropy()
        } else {
            StdRng::seed_from_u64(seed)
        };
        let city_info: Vec<(String, String)> = cities
            .iter()
            .filter_map(|c| c.owner.as_ref().map(|o| (c.id.clone(), o.clone())))
            .collect();
        Self {
            rng,
            unit_types,
            city_info,
            center: (map_w / 2.0, map_h / 2.0),
            tick: 0,
        }
    }

    fn rand_f32(&mut self) -> f32 {
        self.rng.gen_range(0.0..1.0)
    }
    fn rand_idx(&mut self, len: usize) -> usize {
        if len == 0 {
            return 0;
        }
        self.rng.gen_range(0..len)
    }

    fn tick(&mut self, game: &mut Game, config: &BenchmarkConfig) {
        self.tick += 1;
        let utype_count = self.unit_types.len();

        // Movement events: every 20 ticks, move 30% of units toward the map
        // centre. Units are sorted by id and the RNG draw order is fixed, so
        // the whole simulation is deterministic for a given seed.
        if self.tick.is_multiple_of(20) {
            let mut units = game.query_all_units();
            units.sort_by(|a, b| a.id.cmp(&b.id));
            let mut moves: Vec<(String, String, (f32, f32))> = Vec::new();
            for unit in &units {
                if self.rand_f32() < 0.3 {
                    moves.push((unit.id.clone(), unit.owner.clone(), self.center));
                }
            }
            for (unit_id, owner, target) in moves {
                game.move_troops(&unit_id, &owner, vec![target]);
            }
        }

        // Recruit events: every 100 ticks, recruit new units
        if self.tick.is_multiple_of(100) && self.tick > 0 {
            let units_to_recruit = (config.units_per_city / 2).max(1) as usize;
            let mut recruits: Vec<(String, UnitType, String)> = Vec::new();
            for ci in 0..self.city_info.len() {
                let city_id = self.city_info[ci].0.clone();
                let owner = self.city_info[ci].1.clone();
                for _ in 0..units_to_recruit {
                    let utype_idx = self.rand_idx(utype_count);
                    let utype = self.unit_types[utype_idx].clone();
                    recruits.push((city_id.clone(), utype, owner.clone()));
                }
            }
            for (city_id, utype, owner) in recruits {
                let _ = game.recruit_unit(&city_id, &utype, &owner);
            }
        }

        // Healing toggle: every 5 ticks
        if self.tick.is_multiple_of(5) {
            let units = game.query_all_units();
            if !units.is_empty() {
                let unit_idx = self.rand_idx(units.len());
                game.toggle_healing(
                    &units[unit_idx].id,
                    &units[unit_idx].owner,
                    self.rand_f32() > 0.5,
                );
            }
        }
    }
}

/// Run a benchmark using a fresh game and return detailed results.
pub fn run_benchmark(config: BenchmarkConfig) -> BenchmarkResults {
    println!("🔬 Starting benchmark...");
    println!(
        "  Units/city: {}, Ticks: {}, Seed: {}",
        config.units_per_city, config.ticks, config.seed
    );

    // Run microbenchmarks first (feature-gated)
    let microbench_json = {
        #[cfg(feature = "microbenchmarks")]
        {
            let microbench_results = microbenchmarks::run_microbenchmarks();
            microbenchmarks::results_to_json(&microbench_results)
        }
        #[cfg(not(feature = "microbenchmarks"))]
        {
            serde_json::json!(null)
        }
    };

    let map_cfg = MapConfig {
        width: 200,
        height: 200,
        cities: vec![
            (30.0, 30.0),
            (170.0, 30.0),
            (30.0, 170.0),
            (170.0, 170.0),
            (100.0, 100.0),
            (60.0, 100.0),
            (140.0, 100.0),
            (100.0, 60.0),
            (100.0, 140.0),
            (50.0, 50.0),
            (150.0, 50.0),
            (100.0, 150.0),
        ],
        terrain_data: None,
    };
    let settings_template = GameSettings {
        starting_funds: 2000,
        // Generous initial manpower so the pre-battle army spawns fully (each player
        // fields ~60 units in the default config).
        initial_manpower: 100_000,
        funds_per_city: 20.0,
        recruitment_rate: 1.0,
        game_speed: 1.0,
        use_standard_ruleset: true,
        unit_types: make_unit_types(),
        city_upgrade_settings: CityUpgradeSettings::default(),
        water_speed: 1.2,
    };

    let total_cities = map_cfg.cities.len();
    println!(
        "  Cities: {}, Players created: {}",
        total_cities, total_cities
    );

    // Benchmark protocol: one warmup pass per run, then MEASURED_RUNS measured
    // passes of `config.ticks` ticks each; the per-tick series are averaged
    // across runs to smooth out run-to-run variance.
    let mut all_tick_data: Vec<TickData> = Vec::new();
    // Per-run tick arrays, kept so we can compute a per-tick-index average
    // (each run is seeded identically, so tick `i` is the same sim phase).
    let mut runs_tick_data: Vec<Vec<TickData>> = Vec::new();
    #[cfg(any(debug_assertions, feature = "debug-overlay"))]
    let mut agg_system: std::collections::HashMap<String, Vec<serde_json::Value>> =
        std::collections::HashMap::new();
    #[cfg(any(debug_assertions, feature = "debug-overlay"))]
    let mut agg_detailed: std::collections::HashMap<String, Vec<serde_json::Value>> =
        std::collections::HashMap::new();
    let mut bench_elapsed_total = std::time::Duration::ZERO;
    let mut total_units: u32 = 0;
    let mut remaining_units = 0usize;

    // Combat diagnostics accumulators (per-tick, averaged across all runs).
    let mut diag_candidates = 0usize;
    let mut diag_attackers = 0usize;
    let mut diag_scanned = 0usize;
    let mut diag_same_owner = 0usize;
    let mut diag_out_of_range = 0usize;
    let mut diag_already_resolved = 0usize;
    let mut health_units = 0usize;
    let mut health_attacking = 0usize;
    let mut health_enemies = 0usize;
    let mut health_entries = 0usize;

    if !config.detailed_systems.is_empty() {
        println!("  Detailed timing: {}", config.detailed_systems.join(", "));
    }
    println!(
        "  Runs per config: {} measured (each {} ticks, {} warmup)",
        MEASURED_RUNS, config.ticks, BENCH_WARMUP_TICKS
    );

    for run in 0..MEASURED_RUNS {
        println!("  ── Run {}/{} ──", run + 1, MEASURED_RUNS);
        let map = Map::from_config(&map_cfg);
        let settings = settings_template.clone();
        let mut game = Game::new(map, settings, total_cities);
        let cities = game.query_cities();
        for (i, city) in cities.iter().enumerate() {
            let player_id = format!("player_{}", i % 4);
            game.assign_city_owner(&city.id, &player_id);
        }

        let settings_ref = game.settings();
        let infantry = settings_ref
            .unit_types
            .iter()
            .find(|u| u.name == "infantry")
            .cloned()
            .unwrap();
        let cavalry = settings_ref
            .unit_types
            .iter()
            .find(|u| u.name == "cavalry")
            .cloned()
            .unwrap();
        let unit_types_clone = settings_ref.unit_types.clone();
        let _ = settings_ref;

        let cities_with_owners = game.query_cities();
        total_units = 0;
        for city in &cities_with_owners {
            let owner = city.owner.clone().unwrap_or_else(|| "player_0".to_string());
            for _ in 0..config.units_per_city {
                let utype = if total_units.is_multiple_of(2) {
                    &infantry
                } else {
                    &cavalry
                };
                if game.recruit_unit(&city.id, utype, &owner).is_some() {
                    total_units += 1;
                }
            }
        }
        println!("  Recruited {} units", total_units);

        let mut sim = BattleSimulator::new(
            unit_types_clone,
            cities_with_owners,
            config.seed,
            map_cfg.width as f32,
            map_cfg.height as f32,
        );

        // Enable detailed sub-part timing if requested
        if !config.detailed_systems.is_empty() {
            game.set_detailed_systems(config.detailed_systems.clone());
        }

        game.start();

        // Warmup (with AI to warm up caches & archetypes)
        for _ in 0..BENCH_WARMUP_TICKS {
            sim.tick(&mut game, &config);
            game.tick_game();
        }

        // Collect per-tick data
        let mut run_tick_data: Vec<TickData> = Vec::with_capacity(config.ticks as usize);
        let run_start = Instant::now();

        for _ in 0..config.ticks {
            let tick_start = Instant::now();
            sim.tick(&mut game, &config);
            game.tick_game();
            let tick_ms = tick_start.elapsed().as_secs_f64() * 1000.0;
            let unit_count = game.query_all_units().len() as u32;
            let movers = game.last_mover_count() as u32;
            let combats = game.last_engaged_pairs() as u32;

            // Accumulate combat diagnostics (averaged at the end).
            let diag = game.combat_diag();
            diag_candidates += diag.candidates;
            diag_attackers += diag.attackers;
            diag_scanned += diag.pairs_scanned;
            diag_same_owner += diag.pairs_same_owner;
            diag_out_of_range += diag.pairs_out_of_range;
            diag_already_resolved += diag.pairs_already_resolved;
            let health = game.combat_health();
            health_units += health.units;
            health_attacking += health.units_with_attacking;
            health_enemies += health.units_with_enemies;
            health_entries += health.total_nearby_entries;

            #[cfg(any(debug_assertions, feature = "debug-overlay"))]
            let td = if let Some(ds) = game.get_debug_stats() {
                let entity_count = ds["entity_count"].as_u64().unwrap_or(0) as u32;
                let mut system_sum_ms = 0.0f64;
                if let Some(st) = ds["system_timings"].as_array() {
                    for s in st {
                        system_sum_ms += s["last_ms"].as_f64().unwrap_or(0.0);
                    }
                }
                TickData {
                    total_ms: tick_ms,
                    entity_count,
                    unit_count,
                    system_sum_ms,
                    movers,
                    combats,
                }
            } else {
                TickData {
                    total_ms: tick_ms,
                    entity_count: 0,
                    unit_count,
                    system_sum_ms: 0.0,
                    movers,
                    combats,
                }
            };

            #[cfg(not(any(debug_assertions, feature = "debug-overlay")))]
            let td = TickData {
                total_ms: tick_ms,
                entity_count: 0,
                unit_count,
                system_sum_ms: 0.0,
                movers,
                combats,
            };

            run_tick_data.push(td);
        }
        all_tick_data.extend(run_tick_data.clone());
        runs_tick_data.push(run_tick_data);
        bench_elapsed_total += run_start.elapsed();
        remaining_units = game.query_all_units().len();

        // Aggregate this run's system timings for cross-run averaging.
        #[cfg(any(debug_assertions, feature = "debug-overlay"))]
        {
            let final_debug = game.get_debug_stats().unwrap_or_default();
            if let Some(arr) = final_debug["system_timings"].as_array() {
                for s in arr {
                    let name = s["name"].as_str().unwrap_or("?").to_string();
                    agg_system.entry(name).or_default().push(s.clone());
                }
            }
            if let Some(arr) = final_debug["detailed_timings"].as_array() {
                for s in arr {
                    let name = s["name"].as_str().unwrap_or("?").to_string();
                    agg_detailed.entry(name).or_default().push(s.clone());
                }
            }
        }
    } // end of measured-run loop

    // Combine per-run timings by averaging the numeric fields across runs.
    #[cfg(any(debug_assertions, feature = "debug-overlay"))]
    let final_system_timings: Vec<serde_json::Value> = agg_system
        .values()
        .map(|vals| average_timings(vals))
        .collect();
    #[cfg(any(debug_assertions, feature = "debug-overlay"))]
    let final_detailed_timings: Vec<serde_json::Value> = agg_detailed
        .values()
        .map(|vals| average_timings(vals))
        .collect();

    #[cfg(not(any(debug_assertions, feature = "debug-overlay")))]
    let final_system_timings: Vec<serde_json::Value> = Vec::new();
    #[cfg(not(any(debug_assertions, feature = "debug-overlay")))]
    let final_detailed_timings: Vec<serde_json::Value> = Vec::new();

    // Combined results across all measured runs.
    let bench_elapsed = bench_elapsed_total;
    // `tick_data` is the combined raw pool of all 3×N measured ticks. Summary
    // latency/overhead percentiles, min/max and wall time are computed over this
    // honest distribution; the averages are identical to the averaged series.
    let tick_data = all_tick_data;
    // Per-tick-index average across the measured runs (length == config.ticks).
    // Each run is seeded identically so tick `i` is the same sim phase — this is
    // what the charts and the per-tick summary arrays display (one smooth
    // averaged trajectory instead of 3 stitched battles).
    let avg_tick_data: Vec<TickData> = if !runs_tick_data.is_empty() {
        (0..config.ticks as usize)
            .map(|i| {
                let n = runs_tick_data.len() as f64;
                let sum_total = runs_tick_data.iter().map(|r| r[i].total_ms).sum::<f64>();
                let sum_entity = runs_tick_data
                    .iter()
                    .map(|r| r[i].entity_count as f64)
                    .sum::<f64>();
                let sum_unit = runs_tick_data
                    .iter()
                    .map(|r| r[i].unit_count as f64)
                    .sum::<f64>();
                let sum_sys = runs_tick_data
                    .iter()
                    .map(|r| r[i].system_sum_ms)
                    .sum::<f64>();
                let sum_movers = runs_tick_data
                    .iter()
                    .map(|r| r[i].movers as f64)
                    .sum::<f64>();
                let sum_combats = runs_tick_data
                    .iter()
                    .map(|r| r[i].combats as f64)
                    .sum::<f64>();
                TickData {
                    total_ms: sum_total / n,
                    entity_count: (sum_entity / n).round() as u32,
                    unit_count: (sum_unit / n).round() as u32,
                    system_sum_ms: sum_sys / n,
                    movers: (sum_movers / n).round() as u32,
                    combats: (sum_combats / n).round() as u32,
                }
            })
            .collect()
    } else {
        Vec::new()
    };
    let recruit_events = ((config.ticks / 100).max(1) - 1) as u32;
    let total_recruited =
        total_units + recruit_events * (config.units_per_city / 2).max(1) * total_cities as u32;

    // Compute overhead gap: tick_ms - sum(system_ms) = archetype/query/spawn overhead
    let overhead_durations: Vec<f64> = tick_data
        .iter()
        .map(|td| td.total_ms - td.system_sum_ms)
        .collect();
    let overhead_avg = if overhead_durations.is_empty() {
        0.0
    } else {
        overhead_durations.iter().sum::<f64>() / overhead_durations.len() as f64
    };

    // CLI output
    println!("\n═══════════════════════════════════════════");
    println!("  BENCHMARK RESULTS");
    println!("═══════════════════════════════════════════");
    println!(
        "  Total wall time: {:.2}ms",
        bench_elapsed.as_secs_f64() * 1000.0
    );
    println!(
        "  Ticks completed: {} ({} runs × {})",
        tick_data.len(),
        MEASURED_RUNS,
        config.ticks
    );
    println!(
        "  Avg tick (wall): {:.3}ms",
        bench_elapsed.as_secs_f64() * 1000.0 / tick_data.len() as f64
    );
    println!(
        "  Avg system time: {:.3}ms",
        tick_data.iter().map(|td| td.system_sum_ms).sum::<f64>() / tick_data.len() as f64
    );
    println!("  Avg overhead (spawn/query/ECS): {:.3}ms", overhead_avg);
    println!(
        "  Units recruited: {} (initial {}) + {} (mid-battle)",
        total_recruited,
        total_units,
        total_recruited.saturating_sub(total_units)
    );
    println!("  Units remaining: {}", remaining_units);
    println!(
        "  Move events: {}, Recruit events: {}",
        config.ticks / 20 * MEASURED_RUNS as u64,
        recruit_events * MEASURED_RUNS
    );
    let avg_movers = if tick_data.is_empty() {
        0.0
    } else {
        tick_data.iter().map(|td| td.movers as f64).sum::<f64>() / tick_data.len() as f64
    };
    let total_movers: u64 = tick_data.iter().map(|td| td.movers as u64).sum();
    println!(
        "  Avg movers/tick (movement mover_count): {:.1}",
        avg_movers
    );
    println!("  Total mover-tick count: {}", total_movers);
    let avg_combats = if tick_data.is_empty() {
        0.0
    } else {
        tick_data.iter().map(|td| td.combats as f64).sum::<f64>() / tick_data.len() as f64
    };
    let total_combats: u64 = tick_data.iter().map(|td| td.combats as u64).sum();
    println!(
        "  Avg combats/tick (combat engaged pairs): {:.1}",
        avg_combats
    );
    println!("  Total combat-tick count: {}", total_combats);
    let ticks_f = tick_data.len() as f64;
    println!(
        "  Combat diagnostics (avg/tick): candidates {:.1}, attackers {:.1}, pairs scanned {:.1}",
        diag_candidates as f64 / ticks_f,
        diag_attackers as f64 / ticks_f,
        diag_scanned as f64 / ticks_f,
    );
    println!(
        "    skipped: same-owner {:.1}, out-of-range {:.1}, already-resolved {:.1}",
        diag_same_owner as f64 / ticks_f,
        diag_out_of_range as f64 / ticks_f,
        diag_already_resolved as f64 / ticks_f,
    );
    println!(
        "    health: units {:.1}, with-attacking {:.1}, with-enemies {:.1}, total-nearby-entries {:.1}",
        health_units as f64 / ticks_f,
        health_attacking as f64 / ticks_f,
        health_enemies as f64 / ticks_f,
        health_entries as f64 / ticks_f,
    );
    println!();

    // Per-system breakdown from final debug stats (includes P90/P99)
    let mut sorted_systems: Vec<&serde_json::Value> = final_system_timings.iter().collect();
    sorted_systems.sort_by(|a, b| {
        b["total_ms"]
            .as_f64()
            .unwrap_or(0.0)
            .partial_cmp(&a["total_ms"].as_f64().unwrap_or(0.0))
            .unwrap()
    });
    println!("  Per-system breakdown (total time, calls, % of ticks):");
    println!("  ───────────────────────────────────────────────────────");
    for s in &sorted_systems {
        let name = s["name"].as_str().unwrap_or("?");
        let total = s["total_ms"].as_f64().unwrap_or(0.0);
        let calls = s["call_count"].as_u64().unwrap_or(0);
        let avg = s["avg_ms"].as_f64().unwrap_or(0.0);
        let p99 = s["p99_ms"].as_f64().unwrap_or(0.0);
        let pct = if config.ticks > 0 {
            calls as f64 / (config.ticks + BENCH_WARMUP_TICKS) as f64 * 100.0
        } else {
            0.0
        };
        println!(
            "    {:<25}  {:>8.1}ms total  {:>6.3}ms avg  P99={:.3}ms  ({} calls, {:.0}%)",
            name, total, avg, p99, calls, pct
        );
    }

    // Detailed sub-part breakdown
    if !final_detailed_timings.is_empty() {
        println!("\n  Detailed sub-part timings:");
        println!("  ───────────────────────────────────────────────────────");
        let mut sorted_detailed: Vec<&serde_json::Value> = final_detailed_timings.iter().collect();
        sorted_detailed.sort_by(|a, b| {
            b["total_ms"]
                .as_f64()
                .unwrap_or(0.0)
                .partial_cmp(&a["total_ms"].as_f64().unwrap_or(0.0))
                .unwrap()
        });
        for s in &sorted_detailed {
            let name = s["name"].as_str().unwrap_or("?");
            let total = s["total_ms"].as_f64().unwrap_or(0.0);
            let calls = s["call_count"].as_u64().unwrap_or(0);
            let avg = s["avg_ms"].as_f64().unwrap_or(0.0);
            let p99 = s["p99_ms"].as_f64().unwrap_or(0.0);
            println!(
                "    {:<35}  {:>8.1}ms total  {:>6.3}ms avg  P99={:.3}ms  ({} calls)",
                name, total, avg, p99, calls
            );
        }
    }

    // Latency percentiles
    let tick_durations: Vec<f64> = tick_data.iter().map(|td| td.total_ms).collect();
    let avg_tick = tick_durations.iter().sum::<f64>() / tick_durations.len() as f64;
    let min_tick = tick_durations.iter().cloned().fold(f64::INFINITY, f64::min);
    let max_tick = tick_durations
        .iter()
        .cloned()
        .fold(f64::NEG_INFINITY, f64::max);
    let p50 = percentile(&tick_durations, 50);
    let p90 = percentile(&tick_durations, 90);
    let p95 = percentile(&tick_durations, 95);
    let p99 = percentile(&tick_durations, 99);

    // Overhead percentiles
    let overhead_p50 = percentile(&overhead_durations, 50);
    let overhead_p95 = percentile(&overhead_durations, 95);
    let overhead_p99 = percentile(&overhead_durations, 99);

    // Mover stats (movement mover_count per tick)
    let total_mover_ticks: u64 = tick_data.iter().map(|td| td.movers as u64).sum();
    let avg_movers_per_tick = if tick_data.is_empty() {
        0.0
    } else {
        total_mover_ticks as f64 / tick_data.len() as f64
    };

    // Combat stats (engaged pairs per tick)
    let total_combat_ticks: u64 = tick_data.iter().map(|td| td.combats as u64).sum();
    let avg_combats_per_tick = if tick_data.is_empty() {
        0.0
    } else {
        total_combat_ticks as f64 / tick_data.len() as f64
    };

    let summary = serde_json::json!({
        "config": {
            "cities": total_cities, "units_per_city": config.units_per_city,
            "total_units_initial": total_units, "total_units_recruited": total_recruited,
            "ticks": config.ticks, "seed": config.seed,
            "measured_runs": MEASURED_RUNS, "warmup_ticks": BENCH_WARMUP_TICKS,
            "bench_prev": config.bench_prev, "detailed_systems": config.detailed_systems,
        },
        "results": {
            "wall_time_ms": bench_elapsed.as_secs_f64() * 1000.0,
            "avg_tick_wall_ms": bench_elapsed.as_secs_f64() * 1000.0 / tick_data.len() as f64,
            "avg_system_ms": tick_data.iter().map(|td| td.system_sum_ms).sum::<f64>() / tick_data.len() as f64,
            "avg_overhead_ms": overhead_avg,
            "units_remaining": remaining_units,
            "move_events": config.ticks / 20 * MEASURED_RUNS as u64, "recruit_events": recruit_events * MEASURED_RUNS,
            "avg_movers_per_tick": avg_movers_per_tick,
            "total_mover_ticks": total_mover_ticks,
            "avg_combats_per_tick": avg_combats_per_tick,
            "total_combat_ticks": total_combat_ticks,
        },
        "latency": {
            "avg_ms": avg_tick, "min_ms": min_tick, "max_ms": max_tick,
            "p50_ms": p50, "p90_ms": p90, "p95_ms": p95, "p99_ms": p99,
        },
        "overhead": {
            "avg_ms": overhead_avg, "p50_ms": overhead_p50,
            "p95_ms": overhead_p95, "p99_ms": overhead_p99,
            "description": "Tick time minus sum of system times (archetype rebuild, query setup, entity spawn)",
        },
        "overhead_durations_ms": overhead_durations,
        "system_timings": final_system_timings,
        "detailed_timings": final_detailed_timings,
        // Per-tick arrays are the averaged series (length == config.ticks),
        // matching the charts. Note overhead_durations_ms above stays the raw
        // 3×N distribution (it backs the overhead percentiles); the length
        // divergence from the per-tick arrays is intentional (Raw-JSON only).
        "tick_durations_ms": avg_tick_data.iter().map(|td| td.total_ms).collect::<Vec<f64>>(),
        "movers_per_tick": avg_tick_data.iter().map(|td| td.movers).collect::<Vec<_>>(),
        "combats_per_tick": avg_tick_data.iter().map(|td| td.combats).collect::<Vec<_>>(),
        "combat_diag": {
            "avg_candidates_per_tick": diag_candidates as f64 / tick_data.len() as f64,
            "avg_attackers_per_tick": diag_attackers as f64 / tick_data.len() as f64,
            "avg_pairs_scanned_per_tick": diag_scanned as f64 / tick_data.len() as f64,
            "avg_pairs_same_owner_per_tick": diag_same_owner as f64 / tick_data.len() as f64,
            "avg_pairs_out_of_range_per_tick": diag_out_of_range as f64 / tick_data.len() as f64,
            "avg_pairs_already_resolved_per_tick": diag_already_resolved as f64 / tick_data.len() as f64,
            "health_avg_units": health_units as f64 / tick_data.len() as f64,
            "health_avg_units_with_attacking": health_attacking as f64 / tick_data.len() as f64,
            "health_avg_units_with_enemies": health_enemies as f64 / tick_data.len() as f64,
            "health_avg_total_nearby_entries": health_entries as f64 / tick_data.len() as f64,
        },
        "microbenchmarks": microbench_json,
    });

    BenchmarkResults {
        config,
        // Averages the per-tick series across measured runs (length == ticks);
        // summary latency/overhead stats are over the raw 3×N pool.
        tick_data: avg_tick_data,
        summary,
    }
}

fn percentile(values: &[f64], p: u32) -> f64 {
    if values.is_empty() {
        return 0.0;
    }
    let mut sorted = values.to_vec();
    sorted.sort_by(|a, b| a.partial_cmp(b).unwrap());
    let idx = ((p as f64 / 100.0) * (sorted.len() - 1) as f64) as usize;
    sorted[idx]
}

/// Backward compat: return summary JSON
pub fn run_benchmark_json(config: BenchmarkConfig) -> serde_json::Value {
    run_benchmark(config).summary
}

/// Generate a self-contained HTML report from benchmark results.
pub fn generate_html_report(results: &BenchmarkResults, prev_html: Option<&str>) -> String {
    let tick_count = results.config.ticks;
    // Total ticks per run including warmup (for the "% of Ticks" denominator).
    let total_ticks_with_warmup = tick_count + BENCH_WARMUP_TICKS;
    let wall_time = results.summary["results"]["wall_time_ms"]
        .as_f64()
        .unwrap_or(0.0);
    let avg_tick = results.summary["latency"]["avg_ms"].as_f64().unwrap_or(0.0);
    let min_tick = results.summary["latency"]["min_ms"].as_f64().unwrap_or(0.0);
    let max_tick = results.summary["latency"]["max_ms"].as_f64().unwrap_or(0.0);
    let p50 = results.summary["latency"]["p50_ms"].as_f64().unwrap_or(0.0);
    let p90 = results.summary["latency"]["p90_ms"].as_f64().unwrap_or(0.0);
    let p95 = results.summary["latency"]["p95_ms"].as_f64().unwrap_or(0.0);
    let p99 = results.summary["latency"]["p99_ms"].as_f64().unwrap_or(0.0);
    let units_initial = results.summary["config"]["total_units_initial"]
        .as_u64()
        .unwrap_or(0);
    let units_recruited = results.summary["config"]["total_units_recruited"]
        .as_u64()
        .unwrap_or(0);
    let units_remaining = results.summary["results"]["units_remaining"]
        .as_u64()
        .unwrap_or(0);
    let cities = results.summary["config"]["cities"].as_u64().unwrap_or(0);
    let seed = results.summary["config"]["seed"].as_u64().unwrap_or(0);
    let units_per_city = results.summary["config"]["units_per_city"]
        .as_u64()
        .unwrap_or(0);
    let avg_system = results.summary["results"]["avg_system_ms"]
        .as_f64()
        .unwrap_or(0.0);
    let avg_overhead = results.summary["results"]["avg_overhead_ms"]
        .as_f64()
        .unwrap_or(0.0);
    let overhead_p95 = results.summary["overhead"]["p95_ms"]
        .as_f64()
        .unwrap_or(0.0);
    let overhead_p99 = results.summary["overhead"]["p99_ms"]
        .as_f64()
        .unwrap_or(0.0);

    let avg_movers = results.summary["results"]["avg_movers_per_tick"]
        .as_f64()
        .unwrap_or(0.0);
    let avg_combats = results.summary["results"]["avg_combats_per_tick"]
        .as_f64()
        .unwrap_or(0.0);
    let overhead_p50 = results.summary["overhead"]["p50_ms"]
        .as_f64()
        .unwrap_or(0.0);
    // Deaths/tick is derived: units change only via recruitment (adds) and
    // combat deaths (removes), so total deaths = initial + recruited - remaining.
    let deaths_per_tick = if tick_count > 0 {
        (units_initial as f64 + units_recruited as f64 - units_remaining as f64) / tick_count as f64
    } else {
        0.0
    };

    // Parse previous report for comparison (per-system objects + latency metrics).
    // `prev_timings` now maps system name -> the full per-system object so the
    // report can show per-column deltas (total/avg/calls/p50/p90/p99).
    let (prev_timings, prev_json) = prev_html
        .map(parse_previous_report)
        .unwrap_or((std::collections::HashMap::new(), serde_json::Value::Null));
    let comparison_note = match prev_html {
        Some(p) => format!(
            "<p class='note'>📊 Comparison with: <b>{}</b> — 🟢 = faster/less, 🔴 = slower/more. Deltas appear under each value; comparisons are only meaningful if the previous run used the same ticks/units.</p>",
            p
        ),
        None => String::new(),
    };
    // Previous report's total ticks per run (for reconstructing % of Ticks).
    let prev_total_ticks_with_warmup: f64 = prev_json
        .get("config")
        .map(|c| {
            let ticks = c.get("ticks").and_then(|v| v.as_f64()).unwrap_or(0.0);
            let warm = c
                .get("warmup_ticks")
                .and_then(|v| v.as_f64())
                .unwrap_or(0.0);
            ticks + warm
        })
        .unwrap_or(0.0);

    // Extract previous metrics for Summary deltas (field-presence based: any
    // field missing from an older report simply yields no delta).
    let prev_avg = prev_json
        .get("latency")
        .and_then(|l| l.get("avg_ms"))
        .and_then(|v| v.as_f64());
    let prev_p50 = prev_json
        .get("latency")
        .and_then(|l| l.get("p50_ms"))
        .and_then(|v| v.as_f64());
    let prev_p90 = prev_json
        .get("latency")
        .and_then(|l| l.get("p90_ms"))
        .and_then(|v| v.as_f64());
    let prev_p95 = prev_json
        .get("latency")
        .and_then(|l| l.get("p95_ms"))
        .and_then(|v| v.as_f64());
    let prev_p99 = prev_json
        .get("latency")
        .and_then(|l| l.get("p99_ms"))
        .and_then(|v| v.as_f64());
    let prev_wall = prev_json
        .get("results")
        .and_then(|r| r.get("wall_time_ms"))
        .and_then(|v| v.as_f64());
    let prev_min = prev_json
        .get("latency")
        .and_then(|l| l.get("min_ms"))
        .and_then(|v| v.as_f64());
    let prev_max = prev_json
        .get("latency")
        .and_then(|l| l.get("max_ms"))
        .and_then(|v| v.as_f64());
    let prev_overhead_p50 = prev_json
        .get("overhead")
        .and_then(|o| o.get("p50_ms"))
        .and_then(|v| v.as_f64());
    let prev_overhead_p95 = prev_json
        .get("overhead")
        .and_then(|o| o.get("p95_ms"))
        .and_then(|v| v.as_f64());
    let prev_overhead_p99 = prev_json
        .get("overhead")
        .and_then(|o| o.get("p99_ms"))
        .and_then(|v| v.as_f64());
    let prev_avg_system = prev_json
        .get("results")
        .and_then(|r| r.get("avg_system_ms"))
        .and_then(|v| v.as_f64());
    let prev_avg_overhead = prev_json
        .get("results")
        .and_then(|r| r.get("avg_overhead_ms"))
        .and_then(|v| v.as_f64());
    let prev_units_remaining = prev_json
        .get("results")
        .and_then(|r| r.get("units_remaining"))
        .and_then(|v| v.as_f64());
    let prev_units_recruited = prev_json
        .get("config")
        .and_then(|c| c.get("total_units_recruited"))
        .and_then(|v| v.as_f64());
    let prev_avg_movers = prev_json
        .get("results")
        .and_then(|r| r.get("avg_movers_per_tick"))
        .and_then(|v| v.as_f64());
    let prev_avg_combats = prev_json
        .get("results")
        .and_then(|r| r.get("avg_combats_per_tick"))
        .and_then(|v| v.as_f64());
    // Deaths/tick derived from the previous report (initial + recruited - remaining)/ticks.
    let prev_deaths_per_tick: Option<f64> = (|| {
        let init = prev_json
            .get("config")
            .and_then(|c| c.get("total_units_initial"))
            .and_then(|v| v.as_f64())?;
        let rec = prev_json
            .get("config")
            .and_then(|c| c.get("total_units_recruited"))
            .and_then(|v| v.as_f64())?;
        let rem = prev_json
            .get("results")
            .and_then(|r| r.get("units_remaining"))
            .and_then(|v| v.as_f64())?;
        let ticks = prev_json
            .get("config")
            .and_then(|c| c.get("ticks"))
            .and_then(|v| v.as_f64())?;
        if ticks > 0.0 {
            Some((init + rec - rem) / ticks)
        } else {
            None
        }
    })();

    /// Inline delta for a summary card (lower is better). Empty when no/insignificant change.
    fn delta_html(current: f64, previous: Option<f64>, unit: &str, abs_threshold: f64) -> String {
        match previous {
            Some(prev) if (current - prev).abs() > abs_threshold => {
                let change = current - prev;
                let pct = if prev.abs() > 1e-9 {
                    change / prev * 100.0
                } else {
                    0.0
                };
                let emoji = if change < 0.0 { "🟢" } else { "🔴" };
                let color = if change < 0.0 { "#0f0" } else { "#f44" };
                format!(
                    " <span style='color:{};font-size:11px'>{}{:+.3}{} ({:+.1}%)</span>",
                    color, emoji, change, unit, pct
                )
            }
            _ => String::new(),
        }
    }

    /// Render a table cell with an inline per-value delta (lower is better).
    fn cell_delta(
        value: &str,
        current: f64,
        previous: Option<f64>,
        unit: &str,
        abs_threshold: f64,
    ) -> String {
        let inner = match previous {
            Some(prev) if (current - prev).abs() > abs_threshold => {
                let change = current - prev;
                let pct = if prev.abs() > 1e-9 {
                    change / prev * 100.0
                } else {
                    0.0
                };
                let color = if change < 0.0 { "#0f0" } else { "#f44" };
                let arrow = if change < 0.0 { "▼" } else { "▲" };
                format!(
                    "<div class='delta' style='color:{}'>{arrow}{change:+.3}{} ({pct:+.1}%)</div>",
                    color, unit
                )
            }
            Some(_) => "<div class='delta' style='color:#555'>≈0</div>".to_string(),
            None => String::new(),
        };
        format!("<td>{value}{inner}</td>")
    }

    /// Adaptive time format for microbench values: ns for <1µs, µs for <1ms, ms otherwise.
    fn fmt_ns(x: f64) -> String {
        let a = x.abs();
        if a < 1000.0 {
            format!("{x:.1}ns")
        } else if a < 1_000_000.0 {
            format!("{:.2}µs", x / 1000.0)
        } else {
            format!("{:.4}ms", x / 1_000_000.0)
        }
    }

    /// Per-column microbench delta rendered under a value (lower is better).
    /// Empty when there is no previous value or the change is ≤1 ns (noise).
    fn micro_delta_ns(current: f64, previous: Option<f64>) -> String {
        let Some(prev) = previous else {
            return String::new();
        };
        let change = current - prev;
        if change.abs() <= 1.0 {
            return String::new();
        }
        let color = if change < 0.0 { "#0f0" } else { "#f44" };
        let arrow = if change < 0.0 { "▼" } else { "▲" };
        let mag = change.abs();
        let val = if mag < 1000.0 {
            format!("{change:+.1}ns")
        } else if mag < 1_000_000.0 {
            format!("{:+.2}µs", change / 1000.0)
        } else {
            format!("{:+.3}ms", change / 1_000_000.0)
        };
        format!("<div class='delta' style='color:{color}'>{arrow}{val}</div>")
    }

    // ms thresholds keep the previous 0.0005ms; count/rate thresholds per D8.
    const MS: f64 = 0.0005;
    const COUNT: f64 = 1.0;
    const RATE: f64 = 0.05;
    let avg_tick_delta = delta_html(avg_tick, prev_avg, "ms", MS);
    let p50_delta = delta_html(p50, prev_p50, "ms", MS);
    let p90_delta = delta_html(p90, prev_p90, "ms", MS);
    let p95_delta = delta_html(p95, prev_p95, "ms", MS);
    let p99_delta = delta_html(p99, prev_p99, "ms", MS);
    let wall_time_delta = delta_html(wall_time, prev_wall, "ms", MS);
    let min_tick_delta = delta_html(min_tick, prev_min, "ms", MS);
    let max_tick_delta = delta_html(max_tick, prev_max, "ms", MS);
    let avg_system_delta = delta_html(avg_system, prev_avg_system, "ms", MS);
    let avg_overhead_delta = delta_html(avg_overhead, prev_avg_overhead, "ms", MS);
    let overhead_p50_delta = delta_html(overhead_p50, prev_overhead_p50, "ms", MS);
    let overhead_p95_delta = delta_html(overhead_p95, prev_overhead_p95, "ms", MS);
    let overhead_p99_delta = delta_html(overhead_p99, prev_overhead_p99, "ms", MS);
    let units_remaining_delta = delta_html(units_remaining as f64, prev_units_remaining, "", COUNT);
    let units_recruited_delta = delta_html(units_recruited as f64, prev_units_recruited, "", COUNT);
    let avg_movers_delta = delta_html(avg_movers, prev_avg_movers, "", COUNT);
    let avg_combats_delta = delta_html(avg_combats, prev_avg_combats, "", COUNT);
    let deaths_per_tick_delta = delta_html(deaths_per_tick, prev_deaths_per_tick, "", RATE);

    let tick_durations: Vec<f64> = results.tick_data.iter().map(|td| td.total_ms).collect();
    let entity_counts: Vec<u32> = results.tick_data.iter().map(|td| td.entity_count).collect();
    let unit_counts: Vec<u32> = results.tick_data.iter().map(|td| td.unit_count).collect();
    let mover_counts: Vec<u32> = results.tick_data.iter().map(|td| td.movers).collect();
    let combat_counts: Vec<u32> = results.tick_data.iter().map(|td| td.combats).collect();
    let overhead_durations: Vec<f64> = results
        .tick_data
        .iter()
        .map(|td| td.total_ms - td.system_sum_ms)
        .collect();

    let tick_durations_json = serde_json::to_string(&tick_durations).unwrap();
    let entity_counts_json = serde_json::to_string(&entity_counts).unwrap();
    let unit_counts_json = serde_json::to_string(&unit_counts).unwrap();
    let mover_counts_json = serde_json::to_string(&mover_counts).unwrap();
    let combat_counts_json = serde_json::to_string(&combat_counts).unwrap();
    let overhead_durations_json = serde_json::to_string(&overhead_durations).unwrap();

    // System timing table
    let system_timings = &results.summary["system_timings"];
    let system_rows = if let Some(systems) = system_timings.as_array() {
        let mut sorted: Vec<&serde_json::Value> = systems.iter().collect();
        sorted.sort_by(|a, b| {
            b["total_ms"]
                .as_f64()
                .unwrap_or(0.0)
                .partial_cmp(&a["total_ms"].as_f64().unwrap_or(0.0))
                .unwrap()
        });
        sorted.iter().map(|s| {
            let name = s["name"].as_str().unwrap_or("?");
            let total = s["total_ms"].as_f64().unwrap_or(0.0);
            let calls = s["call_count"].as_u64().unwrap_or(0);
            let avg = s["avg_ms"].as_f64().unwrap_or(0.0);
            let p50 = s["p50_ms"].as_f64().unwrap_or(0.0);
            let p90 = s["p90_ms"].as_f64().unwrap_or(0.0);
            let p99 = s["p99_ms"].as_f64().unwrap_or(0.0);
            let pct = if total_ticks_with_warmup > 0 { calls as f64 / total_ticks_with_warmup as f64 * 100.0 } else { 0.0 };

            // Previous per-system object (full: total/avg/calls/p50/p90/p99), or
            // None if this system is new / the previous report lacks it.
            let prev = prev_timings.get(name);
            let prev_total: Option<f64> = prev.and_then(|p| p.get("total_ms")).and_then(|v| v.as_f64());
            let prev_avg: Option<f64> = prev.and_then(|p| p.get("avg_ms")).and_then(|v| v.as_f64());
            let prev_calls: Option<f64> = prev.and_then(|p| p.get("call_count")).and_then(|v| v.as_f64());
            let prev_p50: Option<f64> = prev.and_then(|p| p.get("p50_ms")).and_then(|v| v.as_f64());
            let prev_p90: Option<f64> = prev.and_then(|p| p.get("p90_ms")).and_then(|v| v.as_f64());
            let prev_p99: Option<f64> = prev.and_then(|p| p.get("p99_ms")).and_then(|v| v.as_f64());
            // % of Ticks delta reconstructed with the previous report's own denominator.
            let prev_pct: Option<f64> = prev_calls.and_then(|pc| {
                if prev_total_ticks_with_warmup > 0.0 { Some(pc / prev_total_ticks_with_warmup * 100.0) } else { None }
            });

            // Total cell: flag the system "new" when it's absent from the previous report.
            let total_cell = match prev_total {
                Some(_) => cell_delta(&format!("{:.1}", total), total, prev_total, "ms", 0.01),
                None => format!("<td><span style='color:#888'>{:.1} <i>new</i></span></td>", total),
            };
            let avg_cell = cell_delta(&format!("{:.3}", avg), avg, prev_avg, "ms", 0.001);
            let calls_cell = cell_delta(&format!("{}", calls), calls as f64, prev_calls, "", 0.5);
            let pct_cell = cell_delta(&format!("{:.0}%", pct), pct, prev_pct, "", 0.5);
            let p50_cell = cell_delta(&format!("{:.3}", p50), p50, prev_p50, "ms", 0.001);
            let p90_cell = cell_delta(&format!("{:.3}", p90), p90, prev_p90, "ms", 0.001);
            let p99_cell = cell_delta(&format!("{:.3}", p99), p99, prev_p99, "ms", 0.001);

            format!("<tr><td>{}</td>{total_cell}{avg_cell}{calls_cell}{pct_cell}{p50_cell}{p90_cell}{p99_cell}</tr>", name)
        }).collect::<Vec<_>>().join("\n      ")
    } else {
        "<tr><td colspan='8'>Debug stats not available</td></tr>".to_string()
    };

    // System bar chart SVG (sorted by total_ms descending, same as table)
    let system_bars =
        if let Some(systems) = system_timings.as_array() {
            let mut sorted: Vec<&serde_json::Value> = systems.iter().collect();
            sorted.sort_by(|a, b| {
                b["total_ms"]
                    .as_f64()
                    .unwrap_or(0.0)
                    .partial_cmp(&a["total_ms"].as_f64().unwrap_or(0.0))
                    .unwrap()
            });
            let max_total = sorted
                .iter()
                .map(|s| s["total_ms"].as_f64().unwrap_or(0.0))
                .fold(0.0f64, f64::max);
            let bar_height = 20;
            let label_width = 120;
            let margin = 10;
            let chart_width = 500;
            sorted
                .iter()
                .enumerate()
                .map(|(i, s)| {
                    let name = s["name"].as_str().unwrap_or("?");
                    let total = s["total_ms"].as_f64().unwrap_or(0.0);
                    let bar_w = if max_total > 0.0 {
                        (total / max_total * chart_width as f64) as u32
                    } else {
                        0
                    };
                    let y = i * bar_height + margin;
                    let hue = (i * 37) % 360;
                    format!(
                "<rect x='{}' y='{}' width='{}' height='{}' fill='hsl({},70%,50%)' rx='2'/>\n\
                 <text x='{}' y='{}' fill='#aaa' font-size='11' text-anchor='end'>{}</text>\n\
                 <text x='{}' y='{}' fill='#fff' font-size='10' text-anchor='start'>{:.1}ms</text>",
                label_width, y, bar_w, bar_height - 2, hue,
                label_width - 5, y + bar_height - 5, name,
                label_width + bar_w + 5, y + bar_height - 7, total)
                })
                .collect::<Vec<_>>()
                .join("")
        } else {
            "<text x='20' y='30' fill='#888'>No system timing data</text>".to_string()
        };

    let svg_height_val = if let Some(systems) = system_timings.as_array() {
        systems.len() * 20 + 20
    } else {
        40
    };

    // Detailed timing table rows
    let detailed_rows = {
        let dt = &results.summary["detailed_timings"];
        if let Some(systems) = dt.as_array() {
            if systems.is_empty() {
                "<tr><td colspan='5'>No detailed timing data. Use --bench-detailed=system1,system2 to enable.</td></tr>".to_string()
            } else {
                let mut sorted: Vec<&serde_json::Value> = systems.iter().collect();
                sorted.sort_by(|a, b| {
                    b["total_ms"]
                        .as_f64()
                        .unwrap_or(0.0)
                        .partial_cmp(&a["total_ms"].as_f64().unwrap_or(0.0))
                        .unwrap()
                });
                sorted.iter().map(|s| {
                    let name = s["name"].as_str().unwrap_or("?");
                    let total = s["total_ms"].as_f64().unwrap_or(0.0);
                    let calls = s["call_count"].as_u64().unwrap_or(0);
                    let avg = s["avg_ms"].as_f64().unwrap_or(0.0);
                    let p99 = s["p99_ms"].as_f64().unwrap_or(0.0);
                    format!("<tr><td>{}</td><td>{:.1}</td><td>{:.3}</td><td>{}</td><td>{:.3}</td></tr>",
                        name, total, avg, calls, p99)
                }).collect::<Vec<_>>().join("\n      ")
            }
        } else {
            "<tr><td colspan='5'>No detailed timing data. Use --bench-detailed=system1,system2 to enable.</td></tr>".to_string()
        }
    };

    // Microbenchmark table rows. Previous per-bench object kept for per-column
    // deltas (mean/min/median/p99, field-presence: older reports only have
    // mean_ns, so their min/median/p99 cells render without a delta).
    let prev_micro: std::collections::HashMap<String, &serde_json::Value> = prev_json
        .get("microbenchmarks")
        .and_then(|mb| mb.get("benchmarks"))
        .and_then(|b| b.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|b| {
                    b.get("id")
                        .and_then(|v| v.as_str())
                        .map(|id| (id.to_string(), b))
                })
                .collect()
        })
        .unwrap_or_default();

    let microbench_rows = {
        let mb = &results.summary["microbenchmarks"];
        if let Some(benchmarks) = mb.get("benchmarks").and_then(|b| b.as_array()) {
            if benchmarks.is_empty() {
                "<tr><td colspan='8'>No microbenchmark data</td></tr>".to_string()
            } else {
                benchmarks.iter().map(|b| {
                    let id = b.get("id").and_then(|v| v.as_str()).unwrap_or("?");
                    let mean_ns = b.get("mean_ns").and_then(|v| v.as_f64()).unwrap_or(0.0);
                    let std_dev_ns = b.get("std_dev_ns").and_then(|v| v.as_f64()).unwrap_or(0.0);
                    let min_ns = b.get("min_ns").and_then(|v| v.as_f64()).unwrap_or(0.0);
                    let median_ns = b.get("median_ns").and_then(|v| v.as_f64()).unwrap_or(0.0);
                    let p99_ns = b.get("p99_ns").and_then(|v| v.as_f64()).unwrap_or(0.0);
                    let n = b.get("n").and_then(|v| v.as_u64()).unwrap_or(0);
                    let group = b.get("group").and_then(|v| v.as_str()).unwrap_or("");

                    // Per-column deltas (lower is better), each in its own unit.
                    let prev = prev_micro.get(id);
                    let mean_d = micro_delta_ns(mean_ns, prev.and_then(|p| p.get("mean_ns").and_then(|v| v.as_f64())));
                    let min_d = micro_delta_ns(min_ns, prev.and_then(|p| p.get("min_ns").and_then(|v| v.as_f64())));
                    let median_d = micro_delta_ns(median_ns, prev.and_then(|p| p.get("median_ns").and_then(|v| v.as_f64())));
                    let p99_d = micro_delta_ns(p99_ns, prev.and_then(|p| p.get("p99_ns").and_then(|v| v.as_f64())));

                    let group_color = match group {
                        "ecs" => "rgba(0,255,136,0.2)",
                        "systems" => "rgba(255,102,0,0.2)",
                        "map" => "rgba(102,204,255,0.2)",
                        "settings" => "rgba(204,204,204,0.2)",
                        "economy" => "rgba(204,153,255,0.2)",
                        _ => "rgba(255,255,255,0.05)",
                    };

                    format!("<tr style='background:{}'><td>{}</td><td>{}{}</td><td>±{}</td><td>{}{}</td><td>{}{}</td><td>{}{}</td><td>{}</td><td>{}</td></tr>",
                        group_color, id,
                        fmt_ns(mean_ns), mean_d,
                        fmt_ns(std_dev_ns),
                        fmt_ns(min_ns), min_d,
                        fmt_ns(median_ns), median_d,
                        fmt_ns(p99_ns), p99_d,
                        n, group)
                }).collect::<Vec<_>>().join("\n      ")
            }
        } else {
            "<tr><td colspan='8'>No microbenchmark data</td></tr>".to_string()
        }
    };

    let microbench_count = results
        .summary
        .get("microbenchmarks")
        .and_then(|mb| mb.get("count"))
        .and_then(|c| c.as_u64())
        .unwrap_or(0);

    // Configuration-section strings (item 4.1). Field-presence: older reports
    // without these config keys still render a sensible value.
    let measured_runs_cfg: u64 = results
        .summary
        .get("config")
        .and_then(|c| c.get("measured_runs"))
        .and_then(|v| v.as_u64())
        .unwrap_or(MEASURED_RUNS as u64);
    let measured_runs_str = measured_runs_cfg.to_string();
    let warmup_ticks_str: String = results
        .summary
        .get("config")
        .and_then(|c| c.get("warmup_ticks"))
        .and_then(|v| v.as_u64())
        .unwrap_or(BENCH_WARMUP_TICKS)
        .to_string();
    let detailed_systems_str: String = match results
        .summary
        .get("config")
        .and_then(|c| c.get("detailed_systems"))
        .and_then(|v| v.as_array())
    {
        Some(arr) if !arr.is_empty() => arr
            .iter()
            .filter_map(|v| v.as_str())
            .collect::<Vec<_>>()
            .join(", "),
        _ => "—".to_string(),
    };
    let bench_prev_str: String = match prev_html {
        Some(p) => p.to_string(),
        None => "— (no --bench-prev)".to_string(),
    };
    let prev_seed_ticks_str: String = (|| {
        let cfg = prev_json.get("config")?;
        let seed = cfg.get("seed").and_then(|v| v.as_f64())?;
        let ticks = cfg.get("ticks").and_then(|v| v.as_f64())?;
        Some(format!("{:.0} / {:.0}", seed, ticks))
    })()
    .unwrap_or_else(|| "—".to_string());
    let ticks_measured = format!("{} × {}", measured_runs_cfg, tick_count);

    // Pie chart data: top systems by total_ms
    let (pie_labels, pie_data) = if let Some(systems) = system_timings.as_array() {
        let mut sorted: Vec<&serde_json::Value> = systems.iter().collect();
        sorted.sort_by(|a, b| {
            b["total_ms"]
                .as_f64()
                .unwrap_or(0.0)
                .partial_cmp(&a["total_ms"].as_f64().unwrap_or(0.0))
                .unwrap()
        });
        // Take top 8, merge rest into "other"
        let top_n = sorted.len().min(8);
        let mut labels: Vec<String> = sorted[..top_n]
            .iter()
            .map(|s| s["name"].as_str().unwrap_or("?").to_string())
            .collect();
        let mut data: Vec<f64> = sorted[..top_n]
            .iter()
            .map(|s| s["total_ms"].as_f64().unwrap_or(0.0))
            .collect();
        if sorted.len() > top_n {
            let other_total: f64 = sorted[top_n..]
                .iter()
                .map(|s| s["total_ms"].as_f64().unwrap_or(0.0))
                .sum();
            labels.push("other".to_string());
            data.push(other_total);
        }
        (
            serde_json::to_string(&labels).unwrap(),
            serde_json::to_string(&data).unwrap(),
        )
    } else {
        ("[]".to_string(), "[]".to_string())
    };

    format!(
        r#"<!DOCTYPE html>
<html lang="en">
<head>
    <meta charset="UTF-8">
    <title>War of Dots — Benchmark Report</title>
    <script src="https://cdn.jsdelivr.net/npm/chart.js@4.4.0/dist/chart.umd.min.js"></script>
    <style>
        body {{ font-family: 'Segoe UI', system-ui, sans-serif; background: #1a1a2e; color: #e0e0e0; margin: 0; padding: 40px; }}
        .container {{ max-width: 1300px; margin: 0 auto; }}
        h1 {{ color: #ff6600; border-bottom: 2px solid #ff6600; padding-bottom: 10px; }}
        h2 {{ color: #0f0; margin-top: 30px; }}
        h3 {{ color: #66ccff; margin-top: 20px; }}
        .metrics {{ display: flex; flex-wrap: wrap; gap: 12px; margin: 15px 0; }}
        .metric {{ background: rgba(255,255,255,0.05); border-radius: 8px; padding: 12px 20px; min-width: 140px; text-align: center; }}
        .metric .value {{ font-size: 24px; font-weight: bold; color: #0f0; }}
        .metric .label {{ font-size: 11px; color: #888; margin-top: 2px; }}
        table {{ width: 100%; border-collapse: collapse; margin-top: 10px; }}
        th {{ text-align: left; color: #ff6600; border-bottom: 1px solid #444; padding: 8px; font-size: 12px; white-space: nowrap; }}
        td {{ padding: 6px 8px; border-bottom: 1px solid #222; font-size: 12px; white-space: nowrap; }}
        tr:hover {{ background: rgba(255,255,255,0.03); }}
        .config {{ color: #888; font-size: 13px; }}
        .footer {{ margin-top: 40px; color: #555; font-size: 12px; text-align: center; }}
        .chart-container {{ background: rgba(255,255,255,0.03); border-radius: 8px; padding: 15px; margin: 10px 0; }}
        pre {{ background:#111;padding:15px;border-radius:8px;overflow:auto;font-size:11px;max-height:400px; }}
        .note {{ color: #888; font-size: 12px; font-style: italic; margin-top: 5px; }}
        .delta {{ font-size: 10px; line-height: 1.1; margin-top: 1px; }}
    </style>
</head>
<body>
    <div class="container">
        <h1>⚡ War of Dots ECS Benchmark</h1>
        <p class="config">Seed: {seed} | Generated: {timestamp}</p>

        <h2>Configuration</h2>
        <table>
            <tr><td>Map</td><td>200×200 (40,000 tiles)</td></tr>
            <tr><td>Cities</td><td>{cities}</td></tr>
            <tr><td>Players</td><td>4 (round-robin city assignment)</td></tr>
            <tr><td>Units per city</td><td>{units_per_city}</td></tr>
            <tr><td>Initial units</td><td>{units_initial}</td></tr>
            <tr><td>Ticks</td><td>{ticks}</td></tr>
            <tr><td>Seed</td><td>{seed}</td></tr>
            <tr><td>Measured runs</td><td>{measured_runs_str} (per-tick series averaged; latency/overhead over all {measured_runs_str}×{ticks} ticks)</td></tr>
            <tr><td>Warmup ticks</td><td>{warmup_ticks_str} (discarded)</td></tr>
            <tr><td>Detailed systems</td><td>{detailed_systems_str}</td></tr>
            <tr><td>Compared against</td><td>{bench_prev_str}</td></tr>
            <tr><td>Previous seed / ticks</td><td>{prev_seed_ticks_str}</td></tr>
        </table>

        <h2>Summary</h2>

        <h3>Tick Latency (per tick)</h3>
        <div class="metrics">
            <div class="metric"><div class="value">{avg_tick:.3}ms{avg_tick_delta}</div><div class="label">Avg Tick</div></div>
            <div class="metric"><div class="value">{p50:.3}ms{p50_delta}</div><div class="label">P50</div></div>
            <div class="metric"><div class="value">{p90:.3}ms{p90_delta}</div><div class="label">P90</div></div>
            <div class="metric"><div class="value">{p95:.3}ms{p95_delta}</div><div class="label">P95</div></div>
            <div class="metric"><div class="value">{p99:.3}ms{p99_delta}</div><div class="label">P99</div></div>
            <div class="metric"><div class="value">{min_tick:.3}ms{min_tick_delta}</div><div class="label">Min Tick</div></div>
            <div class="metric"><div class="value">{max_tick:.3}ms{max_tick_delta}</div><div class="label">Max Tick</div></div>
        </div>

        <h3>Total / Throughput</h3>
        <div class="metrics">
            <div class="metric"><div class="value">{wall_time:.1}ms{wall_time_delta}</div><div class="label">Total Wall Time</div></div>
            <div class="metric"><div class="value">{ticks_measured}</div><div class="label">Ticks Measured</div></div>
        </div>

        <h3>Tick Composition (per tick)</h3>
        <div class="metrics">
            <div class="metric"><div class="value">{avg_system:.3}ms{avg_system_delta}</div><div class="label">Avg System Time</div></div>
            <div class="metric"><div class="value">{avg_overhead:.3}ms{avg_overhead_delta}</div><div class="label">Avg Overhead</div></div>
            <div class="metric"><div class="value">{overhead_p50:.3}ms{overhead_p50_delta}</div><div class="label">Overhead P50</div></div>
            <div class="metric"><div class="value">{overhead_p95:.3}ms{overhead_p95_delta}</div><div class="label">Overhead P95</div></div>
            <div class="metric"><div class="value">{overhead_p99:.3}ms{overhead_p99_delta}</div><div class="label">Overhead P99</div></div>
        </div>
        <p class="note">Overhead = tick time − sum of all system times (archetype rebuild, query setup, entity spawning, debug guard)</p>
        <p class="note">Per-tick cards are averages across the {measured_runs_str} measured runs. Total Wall Time is the sum over all {measured_runs_str} runs. Latency percentiles are computed over all {measured_runs_str}×{ticks} individual ticks, not the averaged series.</p>

        <h3>Battle Activity</h3>
        <div class="metrics">
            <div class="metric"><div class="value">{units_remaining}{units_remaining_delta}</div><div class="label">Units Remaining</div></div>
            <div class="metric"><div class="value">{units_recruited}{units_recruited_delta}</div><div class="label">Total Recruited</div></div>
            <div class="metric"><div class="value">{deaths_per_tick:.2}{deaths_per_tick_delta}</div><div class="label">Deaths/Tick*</div></div>
            <div class="metric"><div class="value">{avg_movers:.0}{avg_movers_delta}</div><div class="label">Avg Movers/Tick</div></div>
            <div class="metric"><div class="value">{avg_combats:.0}{avg_combats_delta}</div><div class="label">Avg Combats/Tick</div></div>
        </div>
        <p class="note">*Deaths/Tick derived: (initial + recruited − remaining) / ticks — assumes units only leave via combat death.</p>

        <h2>Per-Tick Timing</h2>
        <div class="chart-container"><canvas id="tickChart" height="120"></canvas></div>

        <h3>Overhead Breakdown Per Tick</h3>
        <div class="chart-container"><canvas id="overheadChart" height="120"></canvas></div>
        <p class="note">Spikes in overhead correlate with entity spawning (recruit events every 100 ticks). ECS archetype rebuild is the primary cost.</p>

        <h3>Entity, Unit, Mover & Combat Counts Over Time</h3>
        <div class="chart-container"><canvas id="entityChart" height="120"></canvas></div>

        <h2>System Timing Breakdown</h2>
        {comparison_note}
        <table>
            <thead><tr>
                <th>System</th><th>Total (ms)</th><th>Avg/Call</th><th>Calls</th>
                <th>% of Ticks</th><th>P50</th><th>P90</th><th>P99</th>
            </tr></thead>
            <tbody>
      {system_rows}
            </tbody>
        </table>

        <h3>System Total Time</h3>
        <div style="display:flex;gap:20px;align-items:flex-start;flex-wrap:wrap;">
            <svg width="700" height="{svg_height}" style="background:rgba(255,255,255,0.02);border-radius:8px;">
                {system_bars}
            </svg>
            <div class="chart-container" style="width:320px;height:320px;flex-shrink:0;">
                <canvas id="pieChart"></canvas>
            </div>
        </div>

        <h2>Detailed Sub-Part Timing</h2>
        <table>
            <thead><tr><th>Sub-part</th><th>Total (ms)</th><th>Avg/Call</th><th>Calls</th><th>P99</th></tr></thead>
            <tbody>
      {detailed_rows}
            </tbody>
        </table>

        <h2>Microbenchmarks</h2>
        <p class="note">{microbench_count} isolated benchmarks measuring specific operations and systems</p>
        <table>
            <thead><tr>
                <th>Benchmark</th><th>Mean</th><th>Std Dev</th><th>Min</th>
                <th>Median</th><th>P99</th><th>Iterations</th>
                <th>Group</th>
            </tr></thead>
            <tbody>
      {microbench_rows}
            </tbody>
        </table>

        <h2>Raw JSON</h2>
        <pre>{json}</pre>

        <p class="footer">War of Dots ECS Benchmark — hecs crate · ECS architecture</p>
    </div>

    <script>
        const tickData = {tick_durations_json};
        const overheadData = {overhead_durations_json};
        const systemTimeData = tickData.map((t, i) => t - overheadData[i]);

        // Tick timing chart with system time vs overhead stacked
        new Chart(document.getElementById('tickChart'), {{
            type: 'bar',
            data: {{
                labels: tickData.map((_, i) => i),
                datasets: [
                    {{ label: 'Systems', data: systemTimeData, backgroundColor: 'rgba(255,102,0,0.7)', stack: 'a' }},
                    {{ label: 'Overhead', data: overheadData, backgroundColor: 'rgba(100,150,255,0.5)', stack: 'a' }},
                ]
            }},
            options: {{
                responsive: true, plugins: {{ legend: {{ labels: {{ color: '#aaa' }} }} }},
                scales: {{
                    x: {{ title: {{ display: true, text: 'Tick' }}, ticks: {{ color: '#666' }}, grid: {{ color: '#333' }} }},
                    y: {{ title: {{ display: true, text: 'ms' }}, ticks: {{ color: '#666' }}, grid: {{ color: '#333' }} }}
                }},
                animation: {{ duration: 0 }},
                barThickness: 2,
            }}
        }});

        // Overhead chart
        new Chart(document.getElementById('overheadChart'), {{
            type: 'line',
            data: {{
                labels: overheadData.map((_, i) => i),
                datasets: [{{
                    label: 'Overhead (ms)', data: overheadData,
                    borderColor: '#66ccff', backgroundColor: 'rgba(102,204,255,0.1)',
                    borderWidth: 1.5, pointRadius: 0, fill: true, tension: 0.3,
                }}]
            }},
            options: {{
                responsive: true, plugins: {{ legend: {{ display: false }} }},
                scales: {{
                    x: {{ title: {{ display: true, text: 'Tick' }}, ticks: {{ color: '#666' }}, grid: {{ color: '#333' }} }},
                    y: {{ title: {{ display: true, text: 'ms' }}, ticks: {{ color: '#666' }}, grid: {{ color: '#333' }} }}
                }},
                animation: {{ duration: 0 }},
            }}
        }});

        // Entity count chart
        const entityCounts = {entity_counts_json};
        const unitCounts = {unit_counts_json};
        const moverCounts = {mover_counts_json};
        const combatCounts = {combat_counts_json};
        new Chart(document.getElementById('entityChart'), {{
            type: 'line',
            data: {{
                labels: entityCounts.map((_, i) => i),
                datasets: [
                    {{ label: 'Entities', data: entityCounts, borderColor: '#66ccff', borderWidth: 1.5, pointRadius: 0, tension: 0.3 }},
                    {{ label: 'Units', data: unitCounts, borderColor: '#00ff88', borderWidth: 1.5, pointRadius: 0, tension: 0.3 }},
                    {{ label: 'Movers', data: moverCounts, borderColor: '#ffcc00', borderWidth: 1.5, pointRadius: 0, tension: 0.3, borderDash: [4, 4] }},
                    {{ label: 'Combats', data: combatCounts, borderColor: '#ff6699', borderWidth: 1.5, pointRadius: 0, tension: 0.3, borderDash: [2, 2] }}
                ]
            }},
            options: {{
                responsive: true, plugins: {{ legend: {{ labels: {{ color: '#aaa' }} }} }},
                scales: {{
                    x: {{ title: {{ display: true, text: 'Tick' }}, ticks: {{ color: '#666' }}, grid: {{ color: '#333' }} }},
                    y: {{ title: {{ display: true, text: 'Count' }}, ticks: {{ color: '#666' }}, grid: {{ color: '#333' }} }}
                }},
                animation: {{ duration: 0 }},
            }}
        }});

        // Pie chart: system contribution to total system time
        const pieLabels = {pie_labels_json};
        const pieData = {pie_data_json};
        const pieColors = [
            '#ff6600', '#00cc88', '#6699ff', '#ffcc00',
            '#ff6699', '#99ccff', '#cc99ff', '#66ffcc', '#cccccc'
        ];
        new Chart(document.getElementById('pieChart'), {{
            type: 'doughnut',
            data: {{
                labels: pieLabels,
                datasets: [{{
                    data: pieData,
                    backgroundColor: pieColors.slice(0, pieData.length),
                    borderWidth: 1,
                    borderColor: '#1a1a2e',
                }}]
            }},
            options: {{
                responsive: true,
                maintainAspectRatio: true,
                plugins: {{
                    legend: {{
                        position: 'bottom',
                        labels: {{ color: '#aaa', font: {{ size: 10 }}, boxWidth: 10 }}
                    }},
                    tooltip: {{
                        callbacks: {{
                            label: function(ctx) {{
                                const total = ctx.dataset.data.reduce((a,b) => a+b, 0);
                                const pct = (ctx.parsed / total * 100).toFixed(1);
                                return ctx.label + ': ' + ctx.parsed.toFixed(1) + 'ms (' + pct + '%)';
                            }}
                        }}
                    }}
                }},
                animation: {{ duration: 0 }},
            }}
        }});
    </script>
</body>
</html>"#,
        seed = seed,
        timestamp = chrono::Utc::now().format("%Y-%m-%d %H:%M:%S UTC"),
        avg_tick = avg_tick,
        avg_tick_delta = avg_tick_delta,
        wall_time = wall_time,
        min_tick = min_tick,
        max_tick = max_tick,
        p50 = p50,
        p50_delta = p50_delta,
        p90 = p90,
        p90_delta = p90_delta,
        p95 = p95,
        p95_delta = p95_delta,
        p99 = p99,
        p99_delta = p99_delta,
        wall_time_delta = wall_time_delta,
        min_tick_delta = min_tick_delta,
        max_tick_delta = max_tick_delta,
        units_initial = units_initial,
        units_recruited = units_recruited,
        units_recruited_delta = units_recruited_delta,
        units_remaining = units_remaining,
        units_remaining_delta = units_remaining_delta,
        cities = cities,
        units_per_city = units_per_city,
        ticks = tick_count,
        avg_system = avg_system,
        avg_system_delta = avg_system_delta,
        avg_movers = avg_movers,
        avg_movers_delta = avg_movers_delta,
        avg_combats = avg_combats,
        avg_combats_delta = avg_combats_delta,
        deaths_per_tick = deaths_per_tick,
        deaths_per_tick_delta = deaths_per_tick_delta,
        avg_overhead = avg_overhead,
        avg_overhead_delta = avg_overhead_delta,
        overhead_p50 = overhead_p50,
        overhead_p50_delta = overhead_p50_delta,
        overhead_p95 = overhead_p95,
        overhead_p95_delta = overhead_p95_delta,
        overhead_p99 = overhead_p99,
        overhead_p99_delta = overhead_p99_delta,
        measured_runs_str = measured_runs_str,
        warmup_ticks_str = warmup_ticks_str,
        detailed_systems_str = detailed_systems_str,
        bench_prev_str = bench_prev_str,
        prev_seed_ticks_str = prev_seed_ticks_str,
        ticks_measured = ticks_measured,
        system_rows = system_rows,
        system_bars = system_bars,
        svg_height = svg_height_val,
        detailed_rows = detailed_rows,
        microbench_rows = microbench_rows,
        microbench_count = microbench_count,
        json = serde_json::to_string_pretty(&results.summary).unwrap_or_default(),
        tick_durations_json = tick_durations_json,
        entity_counts_json = entity_counts_json,
        unit_counts_json = unit_counts_json,
        mover_counts_json = mover_counts_json,
        combat_counts_json = combat_counts_json,
        overhead_durations_json = overhead_durations_json,
        pie_labels_json = pie_labels,
        pie_data_json = pie_data,
    )
}

/// Average per-system timing entries across multiple measured runs:
/// numeric fields (total/avg/last/p50/p90/p99) are averaged; `name` and
/// `call_count` are taken from the first run (identical across runs).
fn average_timings(entries: &[serde_json::Value]) -> serde_json::Value {
    let mut out = entries.first().cloned().unwrap_or(serde_json::Value::Null);
    if out.is_null() || entries.is_empty() {
        return out;
    }
    for f in [
        "total_ms", "avg_ms", "last_ms", "p50_ms", "p90_ms", "p99_ms",
    ] {
        if out.get(f).is_some() {
            let sum: f64 = entries.iter().filter_map(|v| v[f].as_f64()).sum();
            out[f] = serde_json::json!(sum / entries.len() as f64);
        }
    }
    out
}

fn make_unit_types() -> Vec<UnitType> {
    crate::game::economy::default_unit_types()
}

/// Parse a previous HTML benchmark report to extract system timings for comparison.
/// Extracts the JSON from the <pre> tag. Returns (system name -> full per-system
/// object with total/avg/calls/p50/p90/p99, the whole parsed JSON).
fn parse_previous_report(
    path: &str,
) -> (
    std::collections::HashMap<String, serde_json::Value>,
    serde_json::Value,
) {
    use std::collections::HashMap;

    let content = match std::fs::read_to_string(path) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("⚠️  Could not read previous report {}: {}", path, e);
            return (HashMap::new(), serde_json::Value::Null);
        }
    };

    // Extract JSON from <pre>...</pre> tag
    let start = match content.find("<pre>") {
        Some(s) => s + "<pre>".len(),
        None => return (HashMap::new(), serde_json::Value::Null),
    };
    let end = match content[start..].find("</pre>") {
        Some(e) => start + e,
        None => return (HashMap::new(), serde_json::Value::Null),
    };
    let json_str = &content[start..end];

    let json: serde_json::Value = match serde_json::from_str(json_str) {
        Ok(j) => j,
        Err(e) => {
            eprintln!("⚠️  Could not parse previous report JSON: {}", e);
            return (HashMap::new(), serde_json::Value::Null);
        }
    };

    let mut timings = HashMap::new();
    if let Some(systems) = json.get("system_timings").and_then(|s| s.as_array()) {
        for system in systems {
            if let Some(name) = system.get("name").and_then(|n| n.as_str()) {
                timings.insert(name.to_string(), system.clone());
            }
        }
    }
    (timings, json)
}
