//! War of Dots — single binary RTS war game
//!
//! A LAN multiplayer RTS where 2-6 players control troops to capture cities,
//! build armies, and expand territory. Serves a WebGPU-powered HTML frontend
//! and supports LLM agents.

use war_of_dots::benchmark;
// Re-export the counting allocator into this binary's crate root so the
// `tick_system!` macro (compiled into the bin via `mod game`) can resolve
// `crate::bench_alloc`. A `use` re-export does NOT duplicate the
// `#[global_allocator]` static — the allocator still lives in the lib.
#[cfg(feature = "bench-alloc")]
pub use war_of_dots::bench_alloc;
mod config;
mod debug;
mod errors;
mod frontend;
mod game;
mod llm;
mod map;
mod replay;
mod server;
mod storage;

use clap::Parser;
use tracing_subscriber::EnvFilter;

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
    #[arg(long)]
    benchmark: bool,

    /// Number of units per player for benchmark (default: 20)
    #[arg(long, default_value = "20")]
    bench_units: u32,

    /// Number of players for benchmark (default: 4)
    #[arg(long, default_value = "4")]
    bench_players: u32,

    /// Number of ticks for benchmark (default: 500)
    #[arg(long, default_value = "500")]
    bench_ticks: u64,

    /// Output benchmark results to HTML file (path, default: benchmark.html)
    #[arg(long)]
    bench_html: Option<String>,

    /// Random seed for benchmark reproducibility (0 = random)
    #[arg(long, default_value = "0")]
    bench_seed: u64,

    /// Enable detailed sub-part timing for specific systems (comma-separated, e.g. "encirclement,combat,vision,border")
    #[arg(long, default_value = "")]
    bench_detailed: String,

    /// Previous HTML report file for comparison (adds green/red deltas to current report)
    #[arg(long)]
    bench_prev: Option<String>,

    /// Total initial units to recruit (stress test). Overrides --bench-units and scales
    /// the starting budget so N units actually spawn.
    #[arg(long)]
    bench_initial_units: Option<u32>,

    /// Map size (width × height in tiles). City positions scale to fill it.
    #[arg(long, default_value_t = 200)]
    bench_map_size: u32,

    /// Number of cities. Default (12) uses the authored 12-city layout; any other
    /// count generates an even sqrt lattice. Raise for unit-heavy stress tests so
    /// per-player city income can keep up with the army's supply drain (the
    /// default 12:10000 city:unit ratio leaves funds pools chronically exhausted,
    /// which is not representative of a real game).
    #[arg(long, default_value_t = 12)]
    bench_cities: u32,

    /// Max units recruited per recruit event (every 100 ticks). Each city
    /// recruits `bench_units / 2` (min 1) per event, capped at this total and
    /// spread evenly across cities. Default 2000 — raise for heavier
    /// recruitment pressure (ECS spawn/churn load), lower to keep the
    /// population roughly stable.
    #[arg(long, default_value_t = 2000)]
    bench_recruiting: u32,

    /// Run the network phase with a synthetic spectator each tick (mirrors the
    /// game_server `spectator_vision` path) to measure the spectator delta cost.
    #[arg(long)]
    bench_spectator: bool,

    /// W9 / B0: enable the splice encoder in the benchmark's network phase
    /// (shared segments encoded once, spliced per player). Pass this for
    /// production parity — `game_server` always runs the splice path, and
    /// `run-stresstest.sh` passes it by default.
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
    if !cli.benchmark {
        let has_bench_args = cli.bench_units != 20
            || cli.bench_players != 4
            || cli.bench_ticks != 500
            || cli.bench_seed != 0
            || !cli.bench_detailed.is_empty()
            || cli.bench_html.is_some()
            || cli.bench_prev.is_some()
            || cli.bench_initial_units.is_some()
            || cli.bench_map_size != 200
            || cli.bench_cities != 12
            || cli.bench_recruiting != 2000
            || cli.bench_spectator;

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
    let mut game_server = server::GameServer::new(cfg, cli.port).await?;
    game_server.bind = cli.bind;
    game_server.run().await?;

    Ok(())
}
