//! War of Dots — single binary RTS war game
//!
//! A LAN multiplayer RTS where 2-6 players control troops to capture cities,
//! build armies, and expand territory. Serves a WebGPU-powered HTML frontend
//! and supports LLM agents.

mod benchmark;
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
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    // Init tracing
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::from_default_env())
        .init();

    let cli = Cli::parse();

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
    let game_server = server::GameServer::new(cfg, cli.port).await?;
    game_server.run().await?;

    Ok(())
}
