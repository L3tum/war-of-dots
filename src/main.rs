//! War of Dots — single binary RTS war game
//!
//! A LAN multiplayer RTS where 2-6 players control troops to capture cities,
//! build armies, and expand territory. Serves a WebGPU-powered HTML frontend
//! and supports LLM agents.
//!
//! Thin binary over the `war_of_dots` library: this file owns ONLY the CLI
//! surface and the server bootstrap. It declares no modules of its own —
//! every module (config/debug/errors/game/llm/map/server/storage) compiles
//! exactly once, into the library (`src/lib.rs`), so the binary can never
//! drift from the library's module tree.

use clap::Parser;
use tracing_subscriber::EnvFilter;
#[cfg(feature = "microbenchmarks")]
use war_of_dots::benchmark;
use war_of_dots::config;
use war_of_dots::server::GameServer;

#[derive(Parser, Debug)]
#[command(name = "war-of-dots", about = "A LAN RTS war game with LLM agents")]
struct Cli {
    /// Server port
    #[arg(short, long, default_value = "8080")]
    port: u16,

    /// Interface to bind listeners on (default 0.0.0.0 = all interfaces).
    /// Use 127.0.0.1 to restrict to localhost, e.g. for the E2E harness.
    #[arg(long, default_value = "0.0.0.0")]
    bind: String,

    /// Path to config file
    #[arg(short, long, default_value = "config.toml")]
    config: String,

    /// Run benchmark instead of starting server
    #[cfg(feature = "microbenchmarks")]
    #[arg(long)]
    benchmark: bool,

    /// Number of units per player for benchmark (default: 20)
    #[cfg(feature = "microbenchmarks")]
    #[arg(long, default_value = "20")]
    bench_units: u32,

    /// Number of players for benchmark (default: 4)
    #[cfg(feature = "microbenchmarks")]
    #[arg(long, default_value = "4")]
    bench_players: u32,

    /// Number of ticks for benchmark (default: 500)
    #[cfg(feature = "microbenchmarks")]
    #[arg(long, default_value = "500")]
    bench_ticks: u64,

    /// Output benchmark results to HTML file (path, default: benchmark.html)
    #[cfg(feature = "microbenchmarks")]
    #[arg(long)]
    bench_html: Option<String>,

    /// Random seed for benchmark reproducibility (0 = random)
    #[cfg(feature = "microbenchmarks")]
    #[arg(long, default_value = "0")]
    bench_seed: u64,

    /// Enable detailed sub-part timing for specific systems (comma-separated, e.g. "encirclement,combat,vision,border")
    #[cfg(feature = "microbenchmarks")]
    #[arg(long, default_value = "")]
    bench_detailed: String,

    /// Previous HTML report file for comparison (adds green/red deltas to current report)
    #[cfg(feature = "microbenchmarks")]
    #[arg(long)]
    bench_prev: Option<String>,

    /// Total initial units to recruit (stress test). Overrides --bench-units and scales
    /// the starting budget so N units actually spawn.
    #[cfg(feature = "microbenchmarks")]
    #[arg(long)]
    bench_initial_units: Option<u32>,

    /// Map size (width × height in tiles). City positions scale to fill it.
    #[cfg(feature = "microbenchmarks")]
    #[arg(long, default_value_t = 200)]
    bench_map_size: u32,

    /// Number of cities. Default (12) uses the authored 12-city layout; any other
    /// count generates an even sqrt lattice. Raise for unit-heavy stress tests so
    /// per-player city income can keep up with the army's supply drain (the
    /// default 12:10000 city:unit ratio leaves funds pools chronically exhausted,
    /// which is not representative of a real game).
    #[cfg(feature = "microbenchmarks")]
    #[arg(long, default_value_t = 12)]
    bench_cities: u32,

    /// Max units recruited per recruit event (every 100 ticks). Each city
    /// recruits `bench_units / 2` (min 1) per event, capped at this total and
    /// spread evenly across cities. Default 2000 — raise for heavier
    /// recruitment pressure (ECS spawn/churn load), lower to keep the
    /// population roughly stable.
    #[cfg(feature = "microbenchmarks")]
    #[arg(long, default_value_t = 2000)]
    bench_recruiting: u32,

    /// Run the network phase with a synthetic spectator each tick (mirrors the
    /// game_server `spectator_vision` path) to measure the spectator delta cost.
    #[cfg(feature = "microbenchmarks")]
    #[arg(long)]
    bench_spectator: bool,

    /// W9 / B0: enable the splice encoder in the benchmark's network phase
    /// (shared segments encoded once, spliced per player). Pass this for
    /// production parity — `game_server` always runs the splice path, and
    /// `run-stresstest.sh` passes it by default.
    #[cfg(feature = "microbenchmarks")]
    #[arg(long)]
    bench_splice: bool,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    // Init tracing
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::from_default_env())
        .init();

    let cli = Cli::parse();

    // Detect bench args provided without --benchmark to prevent silent hangs.
    #[cfg(feature = "microbenchmarks")]
    if !cli.benchmark {
        // Detect "the user passed a bench flag" by comparing the real parse
        // against a baseline parse of no args (pure defaults). The
        // `default_value`s above are the single source of truth — no default
        // is restated here, so the check can't drift out of sync with them.
        let baseline = Cli::try_parse_from(["war-of-dots"]).expect("empty arg list always parses");
        let has_bench_args = cli.bench_units != baseline.bench_units
            || cli.bench_players != baseline.bench_players
            || cli.bench_ticks != baseline.bench_ticks
            || cli.bench_seed != baseline.bench_seed
            || cli.bench_detailed != baseline.bench_detailed
            || cli.bench_html != baseline.bench_html
            || cli.bench_prev != baseline.bench_prev
            || cli.bench_initial_units != baseline.bench_initial_units
            || cli.bench_map_size != baseline.bench_map_size
            || cli.bench_cities != baseline.bench_cities
            || cli.bench_recruiting != baseline.bench_recruiting
            || cli.bench_spectator != baseline.bench_spectator;

        if has_bench_args {
            eprintln!("error: bench arguments provided without --benchmark flag");
            eprintln!();
            eprintln!("  Bench arguments like --bench-ticks, --bench-seed, --bench-html");
            eprintln!("  are only used when --benchmark is also provided.");
            eprintln!();
            eprintln!("  Did you forget to pass --benchmark?");
            std::process::exit(1);
        }
    }

    // Run benchmark if requested (no config needed)
    #[cfg(feature = "microbenchmarks")]
    if cli.benchmark {
        let detailed_systems: Vec<String> = if cli.bench_detailed.is_empty() {
            Vec::new()
        } else {
            cli.bench_detailed
                .split(',')
                .map(|s| s.trim().to_string())
                .collect()
        };
        let bench_cfg = benchmark::BenchmarkConfig {
            units_per_city: cli.bench_units,
            ticks: cli.bench_ticks,
            seed: cli.bench_seed,
            detailed_systems,
            bench_prev: cli.bench_prev.clone(),
            initial_units: cli.bench_initial_units,
            map_size: cli.bench_map_size,
            cities: cli.bench_cities,
            max_recruits_per_event: cli.bench_recruiting,
            bench_spectator: cli.bench_spectator,
            bench_splice: cli.bench_splice,
        };
        let results = benchmark::run_benchmark(bench_cfg);

        // Output HTML report if requested
        if let Some(html_path) = &cli.bench_html {
            let html = benchmark::generate_html_report(&results, cli.bench_prev.as_deref());
            std::fs::write(html_path, &html)?;
            println!("📊 HTML report saved to: {}", html_path);
        } else {
            // Print JSON to stdout
            println!("\nFull JSON results:");
            println!(
                "{}",
                serde_json::to_string_pretty(&results.summary).unwrap()
            );
        }
        return Ok(());
    }

    let cfg = config::load(&cli.config)?;
    let mut game_server = GameServer::new(cfg, cli.port).await?;
    game_server.set_bind(cli.bind);
    game_server.run().await?;

    Ok(())
}

// Regression guard for the bench-presence check above: a pure-default parse
// must not be flagged, a value equal to its default must not be flagged
// (indistinguishable from not passing — matches the original `!= default`
// semantics), and a non-default value must be flagged.
#[cfg(test)]
#[cfg(feature = "microbenchmarks")]
mod bench_arg_detection_tests {
    use clap::Parser;

    fn has_bench_args(cli: &super::Cli) -> bool {
        let baseline = super::Cli::try_parse_from(["war-of-dots"]).unwrap();
        cli.bench_units != baseline.bench_units
            || cli.bench_players != baseline.bench_players
            || cli.bench_ticks != baseline.bench_ticks
            || cli.bench_seed != baseline.bench_seed
            || cli.bench_detailed != baseline.bench_detailed
            || cli.bench_html != baseline.bench_html
            || cli.bench_prev != baseline.bench_prev
            || cli.bench_initial_units != baseline.bench_initial_units
            || cli.bench_map_size != baseline.bench_map_size
            || cli.bench_cities != baseline.bench_cities
            || cli.bench_recruiting != baseline.bench_recruiting
            || cli.bench_spectator != baseline.bench_spectator
    }

    #[test]
    fn pure_defaults_are_not_flagged() {
        let cli = super::Cli::try_parse_from(["war-of-dots"]).unwrap();
        assert!(
            !has_bench_args(&cli),
            "no args must not look like bench args"
        );
    }

    #[test]
    fn explicit_value_equal_to_default_is_not_flagged() {
        // Passing --bench-ticks 500 (== its default) is indistinguishable from
        // not passing it, so it is NOT flagged — exactly matching the original
        // `cli.bench_ticks != 500` behavior (a value equal to the default is a
        // no-op, so there is nothing to warn about).
        let cli = super::Cli::try_parse_from(["war-of-dots", "--bench-ticks", "500"]).unwrap();
        assert!(
            !has_bench_args(&cli),
            "value equal to default must not be flagged"
        );
    }

    #[test]
    fn explicit_non_default_value_is_flagged() {
        let cli = super::Cli::try_parse_from(["war-of-dots", "--bench-units", "7"]).unwrap();
        assert!(
            has_bench_args(&cli),
            "explicit --bench-units must be detected"
        );
    }
}
