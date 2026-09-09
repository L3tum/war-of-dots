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
        bench_spectator: false,
        bench_splice: false,
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

/// Round-3 (B0) baseline: same config as `reports/stress_22.html` (CLI default
/// `units_per_city: 20` — NOT the 5 used by the older `stress_benchmark` test)
/// with the splice encoder ON (production parity — `run-stresstest.sh` now
/// defaults to `--bench-splice`) and no detailed sub-part systems, matching
/// the shape of the pre-B0 baseline `reports/stress_22.html`. Writes the HTML
/// report to `reports/stress_base_r3.html` for the B0 A/B commit and prints
/// the network sub-phase + idle/movement × grid split for the commit message.
#[test]
#[ignore]
fn stress_benchmark_base_r3() {
    let config = BenchmarkConfig {
        units_per_city: 20,
        ticks: 1500,
        seed: 42,
        detailed_systems: vec![],
        bench_prev: None,
        initial_units: Some(10_000),
        map_size: 1000,
        cities: 100,
        max_recruits_per_event: 2000,
        bench_spectator: false,
        bench_splice: true,
    };
    let results = run_benchmark(config);
    let s = &results.summary;

    if let Ok(()) = std::fs::create_dir_all("reports") {
        let html = war_of_dots::benchmark::generate_html_report(&results, None);
        let _ = std::fs::write("reports/stress_base_r3.html", html);
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
    if let Some(arr) = s["system_timings"].as_array() {
        for row in arr {
            if row["name"] == "network" {
                println!(
                    "BENCH NETWORK avg_ms={} p50={} p90={} p99={} calls={}",
                    row["avg_ms"].as_f64().unwrap_or(0.0),
                    row["p50_ms"].as_f64().unwrap_or(0.0),
                    row["p90_ms"].as_f64().unwrap_or(0.0),
                    row["p99_ms"].as_f64().unwrap_or(0.0),
                    row["call_count"].as_u64().unwrap_or(0)
                );
            }
        }
    }
    println!("BENCH NET subphase_ms={}", s["network"]["subphase_ms"]);
    println!("BENCH NET split={}", s["network"]["split"]);
    println!("BENCH DONE");
}

/// Same as `stress_benchmark_base_r3` but with the splice encoder OFF — the
/// in-environment A/B control for the B0 splice-parity shift (stress_22 was
/// generated with splice off, before `run-stresstest.sh` defaulted it on).
#[test]
#[ignore]
fn stress_benchmark_base_r3_nosplice() {
    let config = BenchmarkConfig {
        units_per_city: 20,
        ticks: 1500,
        seed: 42,
        detailed_systems: vec![],
        bench_prev: None,
        initial_units: Some(10_000),
        map_size: 1000,
        cities: 100,
        max_recruits_per_event: 2000,
        bench_spectator: false,
        bench_splice: false,
    };
    let results = run_benchmark(config);
    let s = &results.summary;

    if let Ok(()) = std::fs::create_dir_all("reports") {
        let html = war_of_dots::benchmark::generate_html_report(&results, None);
        let _ = std::fs::write("reports/stress_base_r3_nosplice.html", html);
    }

    println!(
        "BENCH WALL avg={} p50={} p99={} (ms)",
        s["results"]["avg_tick_wall_ms"], s["latency"]["p50_ms"], s["latency"]["p99_ms"]
    );
    if let Some(arr) = s["system_timings"].as_array() {
        for row in arr {
            if row["name"] == "network" {
                println!(
                    "BENCH NETWORK avg_ms={} p50={} p90={} p99={} calls={}",
                    row["avg_ms"].as_f64().unwrap_or(0.0),
                    row["p50_ms"].as_f64().unwrap_or(0.0),
                    row["p90_ms"].as_f64().unwrap_or(0.0),
                    row["p99_ms"].as_f64().unwrap_or(0.0),
                    row["call_count"].as_u64().unwrap_or(0)
                );
            }
        }
    }
    println!("BENCH NET subphase_ms={}", s["network"]["subphase_ms"]);
    println!("BENCH NET split={}", s["network"]["split"]);
    println!("BENCH DONE");
}

/// I2a A/B: same config as `stress_benchmark_base_r3` (the B0 baseline,
/// splice on) — measures the recalibrated digest quantization
/// (men step = max(5, recruitment_rate), equipment floor-binned to 5). Writes
/// `reports/stress_i2a.html` with prev-deltas against the B0 baseline.
#[test]
#[ignore]
fn stress_benchmark_i2a() {
    let config = BenchmarkConfig {
        units_per_city: 20,
        ticks: 1500,
        seed: 42,
        detailed_systems: vec![],
        bench_prev: None,
        initial_units: Some(10_000),
        map_size: 1000,
        cities: 100,
        max_recruits_per_event: 2000,
        bench_spectator: false,
        bench_splice: true,
    };
    let results = run_benchmark(config);
    let s = &results.summary;

    if let Ok(()) = std::fs::create_dir_all("reports") {
        let prev = std::fs::read("reports/stress_base_r3.html").ok();
        let prev_html = prev.as_ref().map(|p| String::from_utf8_lossy(p));
        let html = war_of_dots::benchmark::generate_html_report(&results, prev_html.as_deref());
        let _ = std::fs::write("reports/stress_i2a.html", html);
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
    if let Some(arr) = s["system_timings"].as_array() {
        for row in arr {
            if row["name"] == "network" {
                println!(
                    "BENCH NETWORK avg_ms={} p50={} p90={} p99={} calls={}",
                    row["avg_ms"].as_f64().unwrap_or(0.0),
                    row["p50_ms"].as_f64().unwrap_or(0.0),
                    row["p90_ms"].as_f64().unwrap_or(0.0),
                    row["p99_ms"].as_f64().unwrap_or(0.0),
                    row["call_count"].as_u64().unwrap_or(0)
                );
            }
        }
    }
    println!("BENCH NET subphase_ms={}", s["network"]["subphase_ms"]);
    println!("BENCH NET split={}", s["network"]["split"]);
    println!("BENCH DONE");
}

/// I2b A/B: same config as `stress_benchmark_i2a` — measures the hot/cold
/// field split (COLD_EVERY=10 + per-field threshold gates). Writes
/// `reports/stress_i2b.html` with prev-deltas against the I2a report.
#[test]
#[ignore]
fn stress_benchmark_i2b() {
    let config = BenchmarkConfig {
        units_per_city: 20,
        ticks: 1500,
        seed: 42,
        detailed_systems: vec![],
        bench_prev: None,
        initial_units: Some(10_000),
        map_size: 1000,
        cities: 100,
        max_recruits_per_event: 2000,
        bench_spectator: false,
        bench_splice: true,
    };
    let results = run_benchmark(config);
    let s = &results.summary;

    if let Ok(()) = std::fs::create_dir_all("reports") {
        let prev = std::fs::read("reports/stress_i2a.html").ok();
        let prev_html = prev.as_ref().map(|p| String::from_utf8_lossy(p));
        let html = war_of_dots::benchmark::generate_html_report(&results, prev_html.as_deref());
        let _ = std::fs::write("reports/stress_i2b.html", html);
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
    if let Some(arr) = s["system_timings"].as_array() {
        for row in arr {
            if row["name"] == "network" {
                println!(
                    "BENCH NETWORK avg_ms={} p50={} p90={} p99={} calls={}",
                    row["avg_ms"].as_f64().unwrap_or(0.0),
                    row["p50_ms"].as_f64().unwrap_or(0.0),
                    row["p90_ms"].as_f64().unwrap_or(0.0),
                    row["p99_ms"].as_f64().unwrap_or(0.0),
                    row["call_count"].as_u64().unwrap_or(0)
                );
            }
        }
    }
    println!("BENCH NET subphase_ms={}", s["network"]["subphase_ms"]);
    println!("BENCH NET split={}", s["network"]["split"]);
    println!("BENCH DONE");
}

/// I5 A/B: same config as `stress_benchmark_i2b` — measures the
/// count-map-driven winner/active check. The per-owner map is now built ONCE
/// by the caller and passed into run_network_phase (previously recomputed
/// inside compute_shared_deltas); the game-over scan (determine_winner +
/// the O(P×N) active check) is not exercised by the headless harness.
/// Gate: flat-to-down, no metric may regress > noise (CPU-hygiene commit).
/// Writes `reports/stress_i5.html` with prev-deltas against the I2b report.
#[test]
#[ignore]
fn stress_benchmark_i5() {
    let config = BenchmarkConfig {
        units_per_city: 20,
        ticks: 1500,
        seed: 42,
        detailed_systems: vec![],
        bench_prev: None,
        initial_units: Some(10_000),
        map_size: 1000,
        cities: 100,
        max_recruits_per_event: 2000,
        bench_spectator: false,
        bench_splice: true,
    };
    let results = run_benchmark(config);
    let s = &results.summary;

    if let Ok(()) = std::fs::create_dir_all("reports") {
        let prev = std::fs::read("reports/stress_i2b.html").ok();
        let prev_html = prev.as_ref().map(|p| String::from_utf8_lossy(p));
        let html = war_of_dots::benchmark::generate_html_report(&results, prev_html.as_deref());
        let _ = std::fs::write("reports/stress_i5.html", html);
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
    if let Some(arr) = s["system_timings"].as_array() {
        for row in arr {
            if row["name"] == "network" {
                println!(
                    "BENCH NETWORK avg_ms={} p50={} p90={} p99={} calls={}",
                    row["avg_ms"].as_f64().unwrap_or(0.0),
                    row["p50_ms"].as_f64().unwrap_or(0.0),
                    row["p90_ms"].as_f64().unwrap_or(0.0),
                    row["p99_ms"].as_f64().unwrap_or(0.0),
                    row["call_count"].as_u64().unwrap_or(0)
                );
            }
        }
    }
    println!("BENCH NET subphase_ms={}", s["network"]["subphase_ms"]);
    println!("BENCH NET split={}", s["network"]["split"]);
    println!("BENCH DONE");
}

// ---------------------------------------------------------------------------
// Round-4 A/B harnesses (deep plan: I6, I8, I1, I4, I3)
//
// Each test runs the standard 10k-unit stress config, writes
// reports/stress_<suffix>.html with prev-deltas against its predecessor, and
// prints the network row + wire/update splits for the commit message.
// ---------------------------------------------------------------------------

fn round4_config(bench_spectator: bool) -> BenchmarkConfig {
    BenchmarkConfig {
        units_per_city: 20,
        ticks: 1500,
        seed: 42,
        detailed_systems: vec![],
        bench_prev: None,
        initial_units: Some(10_000),
        map_size: 1000,
        cities: 100,
        max_recruits_per_event: 2000,
        bench_spectator,
        bench_splice: true,
    }
}

fn round4_report(results: &war_of_dots::benchmark::BenchmarkResults, out: &str, prev: &str) {
    if let Ok(()) = std::fs::create_dir_all("reports") {
        let prev = std::fs::read(format!("reports/{prev}")).ok();
        let prev_html = prev.as_ref().map(|p| String::from_utf8_lossy(p));
        let html = war_of_dots::benchmark::generate_html_report(results, prev_html.as_deref());
        let _ = std::fs::write(format!("reports/{out}"), html);
        // JSON sidecar: the authoritative source for A/B numbers (the HTML
        // embeds the whole report-history chain, making the current section
        // ambiguous to parse).
        let out_json = format!("reports/{}.json", out.strip_suffix(".html").unwrap_or(out));
        let _ = std::fs::write(
            out_json,
            serde_json::to_string_pretty(&results.summary).unwrap_or_default(),
        );
    }
}

fn round4_print(s: &serde_json::Value) {
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
    if let Some(arr) = s["system_timings"].as_array() {
        for row in arr {
            if row["name"] == "network" {
                println!(
                    "BENCH NETWORK avg_ms={} p50={} p90={} p99={} calls={}",
                    row["avg_ms"].as_f64().unwrap_or(0.0),
                    row["p50_ms"].as_f64().unwrap_or(0.0),
                    row["p90_ms"].as_f64().unwrap_or(0.0),
                    row["p99_ms"].as_f64().unwrap_or(0.0),
                    row["call_count"].as_u64().unwrap_or(0)
                );
            }
        }
    }
    println!("BENCH NET subphase_ms={}", s["network"]["subphase_ms"]);
    println!("BENCH NET split={}", s["network"]["split"]);
    println!("BENCH DONE");
}

/// I6 A/B: zero-temp grid splice. Same config as I5; no wire change. Expect
/// network allocs/bytes down, workload bit-stable vs `stress_i5.html`.
#[test]
#[ignore]
fn stress_benchmark_i6() {
    let results = run_benchmark(round4_config(false));
    round4_report(&results, "stress_i6.html", "stress_i5.html");
    round4_print(&results.summary);
}

/// I8 secondary sanity: same config as I6 with a connected spectator so the
/// lazy union-vision cache is exercised. I8 is invisible to the headline
/// numbers by design (no-spectator path is free); this guards the
/// spectator path. Gated against `stress_i6.html`.
///
/// `stress_benchmark_i6_ctrl` runs the SAME config as `stress_benchmark_i6`
/// but is meant to run against the PRE-I6 tree (git stash) for a same-machine
/// control when the historical `stress_i5.html` baseline was recorded on a
/// differently-loaded box. Writes `stress_i6_ctrl.html`.
#[test]
#[ignore]
fn stress_benchmark_i8_spect() {
    let results = run_benchmark(round4_config(true));
    round4_report(&results, "stress_i8_spect.html", "stress_i6.html");
    round4_print(&results.summary);
}

/// I1 A/B: incremental baseline-keyed snapshot. Same config as I6; wire must
/// stay bit-stable while network allocs drop −50–70% vs `stress_i6.html`.
#[test]
#[ignore]
fn stress_benchmark_i1() {
    let results = run_benchmark(round4_config(false));
    round4_report(&results, "stress_i1.html", "stress_i6.html");
    round4_print(&results.summary);
}

/// I4 A/B: sparse grid diffs (wire change). Target: movement·grid-dirty wire
/// well under 9.86 MB/tick; idle wire unchanged. Gated vs `stress_i1.html`.
#[test]
#[ignore]
fn stress_benchmark_i4() {
    let results = run_benchmark(round4_config(false));
    round4_report(&results, "stress_i4.html", "stress_i1.html");
    round4_print(&results.summary);
}

/// I3 A/B: compact unit updates (wire change). Expect unit_updates wire bytes
/// down. Gated vs `stress_i4.html`.
#[test]
#[ignore]
fn stress_benchmark_i3() {
    let results = run_benchmark(round4_config(false));
    round4_report(&results, "stress_i3.html", "stress_i4.html");
    round4_print(&results.summary);
}

/// Same-machine control for the I6 A/B: identical config to
/// `stress_benchmark_i6`, run against the pre-I6 tree. Writes
/// `stress_i6_ctrl.html` (no prev-deltas against anything).
#[test]
#[ignore]
fn stress_benchmark_i6_ctrl() {
    let results = run_benchmark(round4_config(false));
    round4_report(&results, "stress_i6_ctrl.html", "stress_i5.html");
    round4_print(&results.summary);
}
