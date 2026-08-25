//! Ignored-by-default benchmark runner.
//!
//! The sandbox whitelists `cargo test` but not `cargo run` / `./run-stresstest.sh`,
//! so this drives the same full stress benchmark the CLI uses, straight from the
//! test harness, and prints compact `BENCH:` lines plus a full JSON dump.
//!
//! Run with (release, matching `run-stresstest.sh`'s features):
//! ```bash
//! cargo test --release --features debug-overlay,microbenchmarks,bench-alloc \
//!     --test bench_runner -- --ignored --nocapture
//! ```
//!
//! Config mirrors `run-stresstest.sh` defaults: 10 000 initial units,
//! 1000×1000 map, 100 cities, 1500 ticks, seed 42.

use war_of_dots::benchmark::{run_benchmark, BenchmarkConfig};

/// The per-system / per-detailed / per-microbenchmark ids whose numbers the
/// deep-plan gates depend on (everything else is still in the JSON dump).
const INTERESTING_MICROS: &[&str] = &[
    "spatial/for_each_within_500",
    "spatial/for_each_within_dense",
    "systems/movement_500",
    "systems/movement_2000",
    "systems/supply_200",
    "systems/supply_500",
    "systems/encirclement_100",
    "systems/influence_100",
    "systems/influence_500",
];

#[test]
#[ignore]
fn stress_benchmark() {
    let config = BenchmarkConfig {
        units_per_city: 5,
        ticks: 1500,
        seed: 42,
        detailed_systems: vec![
            "tickcontext".to_string(),
            "encirclement".to_string(),
            "movement".to_string(),
            "water".to_string(),
            "supply".to_string(),
            "influence".to_string(),
        ],
        bench_prev: None,
        initial_units: Some(10_000),
        map_size: 1000,
        cities: 100,
        max_recruits_per_event: 2000,
    };
    let results = run_benchmark(config);
    let s = &results.summary;

    // Full dump for later diffing / HTML regeneration.
    let json = serde_json::to_string_pretty(s).unwrap();
    if let Ok(()) = std::fs::create_dir_all("reports") {
        let _ = std::fs::write("reports/stress_postphase1.json", json);
    }

    println!(
        "BENCH WALL avg={} p50={} p99={} (ms)",
        s["results"]["avg_tick_wall_ms"], s["latency"]["p50_ms"], s["latency"]["p99_ms"]
    );
    println!(
        "BENCH SANITY units_remaining={} avg_combats={} avg_system_ms={}",
        s["results"]["units_remaining"],
        s["results"]["avg_combats_per_tick"],
        s["results"]["avg_system_ms"]
    );

    for key in ["system_timings", "detailed_timings"] {
        if let Some(arr) = s[key].as_array() {
            for row in arr {
                println!(
                    "BENCH {} name={} calls={} avg_ms={} p99_ms={}",
                    key,
                    row["name"].as_str().unwrap_or("?"),
                    row["call_count"].as_u64().unwrap_or(0),
                    row["avg_ms"].as_f64().unwrap_or(0.0),
                    row["p99_ms"].as_f64().unwrap_or(0.0)
                );
            }
        }
    }

    if let Some(arr) = s["microbenchmarks"]["benchmarks"].as_array() {
        for row in arr {
            let id = row["id"].as_str().unwrap_or("");
            if INTERESTING_MICROS.contains(&id) {
                println!(
                    "BENCH MICRO id={} median_ns={} p99_ns={} n={}",
                    id,
                    row["median_ns"].as_f64().unwrap_or(0.0),
                    row["p99_ns"].as_f64().unwrap_or(0.0),
                    row["n"].as_u64().unwrap_or(0)
                );
            }
        }
    }
    println!("BENCH DONE");
}
