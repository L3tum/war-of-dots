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
use crate::game::unit::Equipment;
use crate::map::map::{Map, MapConfig};

/// Per-tick benchmark data point
#[derive(Default)]
pub struct TickData {
    pub total_ms: f64,
    pub entity_count: u32,
    pub unit_count: u32,
    /// Sum of all system timings for this tick (to compute overhead gap)
    pub system_sum_ms: f64,
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
    city_positions: Vec<(f32, f32)>,
    tick: u64,
}

impl BattleSimulator {
    fn new(unit_types: Vec<UnitType>, cities: Vec<crate::game::economy::City>, seed: u64) -> Self {
        let rng = if seed == 0 {
            StdRng::from_entropy()
        } else {
            StdRng::seed_from_u64(seed)
        };
        let city_info: Vec<(String, String)> = cities
            .iter()
            .filter_map(|c| c.owner.as_ref().map(|o| (c.id.clone(), o.clone())))
            .collect();
        let city_positions: Vec<(f32, f32)> = cities.iter().map(|c| c.position).collect();
        Self {
            rng,
            unit_types,
            city_info,
            city_positions,
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
        let city_count = self.city_positions.len();
        let cities_empty = city_count == 0;
        let utype_count = self.unit_types.len();

        // Movement events: every 20 ticks, move random units
        if self.tick.is_multiple_of(20) {
            let units = game.query_all_units();
            let players: Vec<String> = game.players().keys().cloned().collect();
            let player_count = players.len();
            let mut moves: Vec<(String, String, (f32, f32))> = Vec::new();
            for unit in &units {
                if self.rand_f32() < 0.3 {
                    let enemy_idx = self.rand_idx(player_count);
                    let enemy_id = &players[enemy_idx];
                    if unit.owner.clone() != *enemy_id {
                        let target = if cities_empty {
                            (100.0, 100.0)
                        } else {
                            let ci = self.rand_idx(city_count);
                            self.city_positions[ci]
                        };
                        moves.push((unit.id.clone(), unit.owner.clone(), target));
                    }
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
    let map = Map::from_config(&map_cfg);

    let settings = GameSettings {
        starting_funds: 2000,
        // Generous initial manpower so the pre-battle army spawns fully (each player
        // fields ~60 units in the default config).
        initial_manpower: 100_000,
        funds_per_city: 5.0,
        recruitment_rate: 10.0,
        game_speed: 1.0,
        use_standard_ruleset: true,
        unit_types: make_unit_types(),
        city_upgrade_settings: CityUpgradeSettings::default(),
        water_speed: 1.2,
    };

    let total_cities = map.cities.len();
    let mut game = Game::new(map, settings, total_cities);
    let cities = game.query_cities();
    println!(
        "  Cities: {}, Players created: {}",
        total_cities, total_cities
    );

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
    let mut total_units = 0u32;
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

    let mut sim = BattleSimulator::new(unit_types_clone, cities_with_owners, config.seed);

    // Enable detailed sub-part timing if requested
    if !config.detailed_systems.is_empty() {
        println!("  Detailed timing: {}", config.detailed_systems.join(", "));
        game.set_detailed_systems(config.detailed_systems.clone());
    }

    game.start();

    // Warmup (with AI to warm up caches & archetypes)
    for _ in 0..50 {
        sim.tick(&mut game, &config);
        game.tick_game();
    }

    // Collect per-tick data
    let mut tick_data: Vec<TickData> = Vec::with_capacity(config.ticks as usize);
    let bench_start = Instant::now();

    for _ in 0..config.ticks {
        let tick_start = Instant::now();
        sim.tick(&mut game, &config);
        game.tick_game();
        let tick_ms = tick_start.elapsed().as_secs_f64() * 1000.0;
        let unit_count = game.query_all_units().len() as u32;

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
            }
        } else {
            TickData {
                total_ms: tick_ms,
                entity_count: 0,
                unit_count,
                system_sum_ms: 0.0,
            }
        };

        #[cfg(not(any(debug_assertions, feature = "debug-overlay")))]
        let td = TickData {
            total_ms: tick_ms,
            entity_count: 0,
            unit_count,
            system_sum_ms: 0.0,
        };

        tick_data.push(td);
    }
    let bench_elapsed = bench_start.elapsed();

    let remaining_units = game.query_all_units().len();
    let recruit_events = ((config.ticks / 100).max(1) - 1) as u32;
    let total_recruited =
        total_units + recruit_events * (config.units_per_city / 2).max(1) * total_cities as u32;

    // Get final system timings from debug stats (has P90/P99 from duration buffer)
    #[cfg(any(debug_assertions, feature = "debug-overlay"))]
    let final_debug = game.get_debug_stats().unwrap_or_default();
    #[cfg(any(debug_assertions, feature = "debug-overlay"))]
    let final_system_timings = final_debug["system_timings"]
        .as_array()
        .cloned()
        .unwrap_or_default();
    #[cfg(any(debug_assertions, feature = "debug-overlay"))]
    let final_detailed_timings = final_debug["detailed_timings"]
        .as_array()
        .cloned()
        .unwrap_or_default();

    #[cfg(not(any(debug_assertions, feature = "debug-overlay")))]
    let final_system_timings: Vec<serde_json::Value> = Vec::new();
    #[cfg(not(any(debug_assertions, feature = "debug-overlay")))]
    let final_detailed_timings: Vec<serde_json::Value> = Vec::new();

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
    println!("  Ticks completed: {}", config.ticks);
    println!(
        "  Avg tick (wall): {:.3}ms",
        bench_elapsed.as_secs_f64() * 1000.0 / config.ticks as f64
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
        config.ticks / 20,
        recruit_events
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
            calls as f64 / (config.ticks + 50) as f64 * 100.0
        } else {
            0.0
        }; // +50 warmup
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

    let summary = serde_json::json!({
        "config": {
            "cities": total_cities, "units_per_city": config.units_per_city,
            "total_units_initial": total_units, "total_units_recruited": total_recruited,
            "ticks": config.ticks, "seed": config.seed,
        },
        "results": {
            "wall_time_ms": bench_elapsed.as_secs_f64() * 1000.0,
            "avg_tick_wall_ms": bench_elapsed.as_secs_f64() * 1000.0 / config.ticks as f64,
            "avg_system_ms": tick_data.iter().map(|td| td.system_sum_ms).sum::<f64>() / tick_data.len() as f64,
            "avg_overhead_ms": overhead_avg,
            "units_remaining": remaining_units,
            "move_events": config.ticks / 20, "recruit_events": recruit_events,
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
        "tick_durations_ms": tick_durations,
    });

    BenchmarkResults {
        config,
        tick_data,
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
    // Actual total ticks including 50-tick warmup
    let total_ticks_with_warmup = tick_count + 50;
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

    // Extract spatial hash timing for Tick Composition
    let avg_spatial_hash = if let Some(systems) = results.summary["system_timings"].as_array() {
        systems
            .iter()
            .find_map(|s| {
                if s["name"].as_str() == Some("spatial_hash") {
                    Some(s["avg_ms"].as_f64().unwrap_or(0.0))
                } else {
                    None
                }
            })
            .unwrap_or(0.0)
    } else {
        0.0
    };

    // Parse previous report for comparison (system timings + latency metrics)
    let (prev_timings, prev_json) = prev_html
        .map(parse_previous_report)
        .unwrap_or((std::collections::HashMap::new(), serde_json::Value::Null));
    let comparison_note = if prev_html.is_some() {
        "<p class='note'>📊 Comparison with previous run enabled. 🟢 = faster, 🔴 = slower</p>"
    } else {
        ""
    };

    // Extract previous latency metrics for Summary deltas
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
    let prev_avg_system = prev_json
        .get("results")
        .and_then(|r| r.get("avg_system_ms"))
        .and_then(|v| v.as_f64());
    let prev_avg_overhead = prev_json
        .get("results")
        .and_then(|r| r.get("avg_overhead_ms"))
        .and_then(|v| v.as_f64());

    /// Helper: format a metric with optional delta indicator
    fn delta_html(current: f64, previous: Option<f64>) -> String {
        match previous {
            Some(prev) if (current - prev).abs() > 0.0005 => {
                let change = current - prev;
                let pct = change / prev * 100.0;
                let emoji = if change < 0.0 { "🟢" } else { "🔴" };
                let color = if change < 0.0 { "#0f0" } else { "#f44" };
                format!(
                    " <span style='color:{};font-size:11px'>{}{:+.3}ms ({:+.1}%)</span>",
                    color, emoji, change, pct
                )
            }
            _ => "".to_string(),
        }
    }

    let avg_tick_delta = delta_html(avg_tick, prev_avg);
    let p50_delta = delta_html(p50, prev_p50);
    let p90_delta = delta_html(p90, prev_p90);
    let p95_delta = delta_html(p95, prev_p95);
    let p99_delta = delta_html(p99, prev_p99);
    let avg_system_delta = delta_html(avg_system, prev_avg_system);
    let avg_overhead_delta = delta_html(avg_overhead, prev_avg_overhead);

    let tick_durations: Vec<f64> = results.tick_data.iter().map(|td| td.total_ms).collect();
    let entity_counts: Vec<u32> = results.tick_data.iter().map(|td| td.entity_count).collect();
    let unit_counts: Vec<u32> = results.tick_data.iter().map(|td| td.unit_count).collect();
    let overhead_durations: Vec<f64> = results
        .tick_data
        .iter()
        .map(|td| td.total_ms - td.system_sum_ms)
        .collect();

    let tick_durations_json = serde_json::to_string(&tick_durations).unwrap();
    let entity_counts_json = serde_json::to_string(&entity_counts).unwrap();
    let unit_counts_json = serde_json::to_string(&unit_counts).unwrap();
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

            // Comparison delta
            let delta = if let Some(prev_total) = prev_timings.get(name) {
                let change = total - prev_total;
                let pct_change = if *prev_total > 0.0 { change / prev_total * 100.0 } else { 0.0 };
                let emoji = if change < -0.01 { "🟢" } else if change > 0.01 { "🔴" } else { "⚪" };
                format!("<td style='color:{}'>{:.1}ms ({}){:+.1}%</td>",
                    if change < 0.0 { "#0f0" } else if change > 0.0 { "#f44" } else { "#aaa" },
                    change, emoji, pct_change)
            } else {
                "<td style='color:#888'>new</td>".to_string()
            };

            format!("<tr><td>{}</td><td>{:.1}</td><td>{:.3}</td><td>{}</td><td>{:.0}%</td><td>{:.3}</td><td>{:.3}</td><td>{:.3}</td>{}</tr>",
                name, total, avg, calls, pct, p50, p90, p99, delta)
        }).collect::<Vec<_>>().join("\n      ")
    } else {
        "<tr><td colspan='9'>Debug stats not available</td></tr>".to_string()
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
        </table>

        <h2>Summary</h2>
        <div class="metrics">
            <div class="metric"><div class="value">{avg_tick:.3}ms{avg_tick_delta}</div><div class="label">Avg Tick</div></div>
            <div class="metric"><div class="value">{wall_time:.1}ms</div><div class="label">Total Wall Time</div></div>
            <div class="metric"><div class="value">{min_tick:.3}ms</div><div class="label">Min Tick</div></div>
            <div class="metric"><div class="value">{max_tick:.3}ms</div><div class="label">Max Tick</div></div>
            <div class="metric"><div class="value">{p50:.3}ms{p50_delta}</div><div class="label">P50</div></div>
            <div class="metric"><div class="value">{p90:.3}ms{p90_delta}</div><div class="label">P90</div></div>
            <div class="metric"><div class="value">{p95:.3}ms{p95_delta}</div><div class="label">P95</div></div>
            <div class="metric"><div class="value">{p99:.3}ms{p99_delta}</div><div class="label">P99</div></div>
            <div class="metric"><div class="value">{units_remaining}</div><div class="label">Units Remaining</div></div>
            <div class="metric"><div class="value">{units_recruited}</div><div class="label">Total Recruited</div></div>
        </div>

        <h3>Tick Composition</h3>
        <div class="metrics">
            <div class="metric"><div class="value">{avg_system:.3}ms{avg_system_delta}</div><div class="label">Avg System Time</div></div>
            <div class="metric"><div class="value">{avg_spatial_hash:.3}ms</div><div class="label">Avg Spatial Hash</div></div>
            <div class="metric"><div class="value">{avg_overhead:.3}ms{avg_overhead_delta}</div><div class="label">Avg Overhead</div></div>
            <div class="metric"><div class="value">{overhead_p95:.3}ms</div><div class="label">Overhead P95</div></div>
            <div class="metric"><div class="value">{overhead_p99:.3}ms</div><div class="label">Overhead P99</div></div>
        </div>
        <p class="note">Overhead = tick time − sum of all system times (archetype rebuild, query setup, entity spawning, debug guard)</p>

        <h2>Per-Tick Timing</h2>
        <div class="chart-container"><canvas id="tickChart" height="120"></canvas></div>

        <h3>Overhead Breakdown Per Tick</h3>
        <div class="chart-container"><canvas id="overheadChart" height="120"></canvas></div>
        <p class="note">Spikes in overhead correlate with entity spawning (recruit events every 100 ticks). ECS archetype rebuild is the primary cost.</p>

        <h3>Entity & Unit Counts Over Time</h3>
        <div class="chart-container"><canvas id="entityChart" height="120"></canvas></div>

        <h2>System Timing Breakdown</h2>
        {comparison_note}
        <table>
            <thead><tr>
                <th>System</th><th>Total (ms)</th><th>Avg/Call</th><th>Calls</th>
                <th>% of Ticks</th><th>P50</th><th>P90</th><th>P99</th><th>Delta (prev)</th>
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
        new Chart(document.getElementById('entityChart'), {{
            type: 'line',
            data: {{
                labels: entityCounts.map((_, i) => i),
                datasets: [
                    {{ label: 'Entities', data: entityCounts, borderColor: '#66ccff', borderWidth: 1.5, pointRadius: 0, tension: 0.3 }},
                    {{ label: 'Units', data: unitCounts, borderColor: '#00ff88', borderWidth: 1.5, pointRadius: 0, tension: 0.3 }}
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
        units_initial = units_initial,
        units_recruited = units_recruited,
        units_remaining = units_remaining,
        cities = cities,
        units_per_city = units_per_city,
        ticks = tick_count,
        avg_system = avg_system,
        avg_system_delta = avg_system_delta,
        avg_spatial_hash = avg_spatial_hash,
        avg_overhead = avg_overhead,
        avg_overhead_delta = avg_overhead_delta,
        overhead_p95 = overhead_p95,
        overhead_p99 = overhead_p99,
        system_rows = system_rows,
        system_bars = system_bars,
        svg_height = svg_height_val,
        detailed_rows = detailed_rows,
        json = serde_json::to_string_pretty(&results.summary).unwrap_or_default(),
        tick_durations_json = tick_durations_json,
        entity_counts_json = entity_counts_json,
        unit_counts_json = unit_counts_json,
        overhead_durations_json = overhead_durations_json,
        pie_labels_json = pie_labels,
        pie_data_json = pie_data,
    )
}

fn make_unit_types() -> Vec<UnitType> {
    vec![
        UnitType {
            name: "infantry".to_string(),
            symbol: "I".to_string(),
            men: 100,
            max_organisation: 100.0,
            reinforcement_rate: 1.0,
            speed: 1.0,
            equipment: vec![Equipment {
                name: "rifles".to_string(),
                count: 10,
                max_count: 10,
                cost_per_item: 0.5,
            }],
            recruitment_cost: 10,
            supply_buffer_percent: 20,
            max_supply: 200, // men(100) * CARRY_WEIGHT(2)
            soft_attack: 5.0,
            soft_defense: 5.0,
            hard_attack: 2.0,
            hard_defense: 3.0,
            base_attrition: 0.1,
            fighting_attrition: 0.5,
        },
        UnitType {
            name: "cavalry".to_string(),
            symbol: "C".to_string(),
            men: 80,
            max_organisation: 100.0,
            reinforcement_rate: 1.0,
            speed: 1.5,
            equipment: vec![],
            recruitment_cost: 15,
            supply_buffer_percent: 15,
            max_supply: 160, // men(80) * CARRY_WEIGHT(2)
            soft_attack: 7.0,
            soft_defense: 4.0,
            hard_attack: 3.0,
            hard_defense: 2.0,
            base_attrition: 0.15,
            fighting_attrition: 0.6,
        },
    ]
}

/// Parse a previous HTML benchmark report to extract system timings for comparison.
/// Extracts the JSON from the <pre> tag and reads system_timings + latency.
fn parse_previous_report(
    path: &str,
) -> (std::collections::HashMap<String, f64>, serde_json::Value) {
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
            if let (Some(name), Some(total)) = (
                system.get("name").and_then(|n| n.as_str()),
                system.get("total_ms").and_then(|t| t.as_f64()),
            ) {
                timings.insert(name.to_string(), total);
            }
        }
    }
    (timings, json)
}
