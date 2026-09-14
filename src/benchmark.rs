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

use std::sync::Arc;
use std::time::Instant;

use rand::rngs::StdRng;
use rand::{Rng, SeedableRng};

use crate::config::CityUpgradeSettings;
use crate::game::economy::UnitType;
use crate::game::game::{Game, GameSettings, InfluenceSettings};
use crate::map::map::{Map, MapConfig};
#[cfg(feature = "microbenchmarks")]
use crate::microbenchmarks;
use crate::server::delta::{
    build_delta_inner, run_network_phase, NetworkBaselines, UnitBaselineStore,
};
use crate::server::protocol::{
    encode_message_with_hint, estimate_delta_size, CityState, ServerMessage,
};

/// Number of measured (timed) runs per benchmark; the per-tick series are
/// averaged across these runs and summary latency/overhead stats are computed
/// over the combined pool of all runs.
pub const MEASURED_RUNS: u32 = 3;
/// Warmup ticks per measured run (discarded; warms caches/archetypes and lets
/// the battle front converge so measured ticks start near steady state).
pub const BENCH_WARMUP_TICKS: u64 = 200;
/// Number of players the battle is run between. Cities are round-robin
/// assigned among these players (one city each when cities == players).
pub const BENCH_PLAYERS: u32 = 4;

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
    /// Allocation calls during this tick (0 when bench-alloc is off).
    pub allocs: u64,
    /// Heap bytes allocated during this tick (0 when bench-alloc is off).
    pub bytes: u64,
    /// Biggest single allocation (bytes) during this tick (0 when bench-alloc is off).
    pub max_alloc: u64,
    /// Network phase: total encoded bytes sent this tick
    pub net_wire_bytes: u64,
    /// Network phase duration in ms (0 if not measured)
    pub net_ms: f64,
    /// Network phase: shared unit updates this tick (W6 churn metric)
    pub net_unit_updates: u32,
    /// B0 sub-phase: unit/city snapshot (`unit_states_direct` + cities read-back)
    /// in ms. 0 when the timing features are off.
    pub net_snapshot_ms: f64,
    /// B0 sub-phase: shared compute (`compute_shared_deltas` +
    /// `SpliceShared::build`) plus the per-player `build_delta_inner` calls,
    /// in ms. 0 when the timing features are off.
    pub net_delta_ms: f64,
    /// B0 sub-phase: per-player CBOR encode (splice or standard) in ms.
    /// 0 when the timing features are off.
    pub net_encode_ms: f64,
    /// B0 sub-phase: egress send loop in ms. Always 0 in the headless
    /// benchmark: it encodes bytes in memory but models no outbox/WS bridge.
    pub net_send_ms: f64,
    /// B0: true when this tick emitted a vision or border grid diff (any
    /// player, or the synthetic spectator under --bench-spectator). Drives the
    /// grid-clean vs grid-dirty sub-split of the idle/movement regimes.
    pub net_grid_dirty: bool,
    /// I4: grid cells changed this tick (border once + per-player vision;
    /// 0 when the timing features are off). Backs the `grid_diag` changed-cell
    /// fraction used to tune the ~5% sparse threshold.
    pub net_grid_changed_cells: u64,
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
    /// Total initial units to recruit across all cities (stress test). When set,
    /// overrides `units_per_city` and the starting budget is scaled so N units
    /// can actually spawn. `None` = use `units_per_city` per city (default path).
    pub initial_units: Option<u32>,
    /// Map size (width = height = this, in tiles). Default 200. City positions
    /// scale proportionally to fill the map, so a stress test can spread units
    /// over a larger area (e.g. 1000×1000) to cut local combat density.
    pub map_size: u32,
    /// Number of cities. Default 12 uses the authored 12-city layout (scaled
    /// to the map); any other count generates an even sqrt lattice instead.
    /// Stress tests with many units should raise this so per-player city income
    /// (funds_per_city × cities/tick) can actually keep up with the army's
    /// supply drain — at the default 12 cities the funds pools are chronically
    /// exhausted and `supply`'s FCFS gate blocks ~25% of units every tick
    /// (run 11w2m), which does not match a real game's city:unit ratio.
    pub cities: u32,
    /// Max units recruited per recruit event (every 100 ticks). Each city
    /// recruits `units_per_city / 2` (min 1) per event, capped at this total
    /// per event and spread evenly across cities (first `total % cities`
    /// cities get one extra). Default 2000. Raise for heavier recruitment
    /// pressure; lower to keep the population roughly stable.
    pub max_recruits_per_event: u32,
    /// W5: run the synthetic-spectator network path each tick (mirrors the
    /// game_server `spectator_vision` code path) so the A/B harness can measure
    /// the spectator delta cost. Seeds a baseline for pid "" from the union
    /// vision grid before warmup.
    pub bench_spectator: bool,
    /// W9 / B0: enable the splice encoder path in the benchmark's network
    /// phase so the A/B harness measures the SAME encoder production uses
    /// (`game_server` always passes `spliced = true`). Default off when built
    /// in code, but `run-stresstest.sh` passes `--bench-splice` by default
    /// for production parity.
    pub bench_splice: bool,
}

impl Default for BenchmarkConfig {
    fn default() -> Self {
        Self {
            units_per_city: 5,
            ticks: 500,
            seed: 0,
            detailed_systems: Vec::new(),
            bench_prev: None,
            initial_units: None,
            map_size: 200,
            cities: 12,
            max_recruits_per_event: 2000,
            bench_spectator: false,
            bench_splice: false,
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
    /// Map centre. `.0` is the centre line's x — the front the armies converge
    /// on so the combat/spatiality pipeline is actually exercised by the
    /// benchmark. Each unit aims at `(center.0, own_y)` (its own row on the
    /// centre line), which builds a long continuous front rather than piling the
    /// whole map onto one spot.
    center: (f32, f32),
    tick: u64,
    /// Reusable `(id, owner, y)` list for the movement event. Filled in place by
    /// `Game::fill_unit_id_owner_y` so steady-state (post-warmup) movement runs
    /// allocation-free instead of rebuilding a fat `Vec<UnitQueryData>` each time.
    /// The `y` coordinate lets each unit aim at its own row on the centre line.
    unit_id_owner_y: Vec<(String, String, f32)>,
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
            unit_id_owner_y: Vec::new(),
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

        // Movement events: every 20 ticks, move 30% of units toward the centre
        // line (x = map_w/2) at their own row. Units are sorted by id and the RNG
        // draw order is fixed, so the whole simulation is deterministic for a
        // given seed (ids are unique, so the id-sort is a total order). The unit
        // list is a reusable scratch buffer filled in place by
        // `Game::fill_unit_id_owner_y`, so this builds no `Vec<UnitQueryData>` and
        // allocates nothing in steady state.
        //
        // Targeting `(centre_x, own_y)` — not a single point — matters: aiming
        // every unit at one point piles the whole map onto that spot and the
        // front collapses (measured: ~5 combats/tick). Aiming each unit at its
        // own row on the centre line makes the left and right halves meet along
        // a long, continuous front and sustains combat (~95 pairs/tick) while
        // keeping the population intact (see the 9 vs 9b stress comparison).
        if self.tick.is_multiple_of(20) {
            let n = game.fill_unit_id_owner_y(&mut self.unit_id_owner_y);
            // `sort_unstable_by`, NOT `sort_by`: the stable `sort_by` allocates a
            // same-size temporary buffer (~48 B × unit count ≈ 240 kB at 5k
            // units) on *every* call, which fired every 20 ticks and dominated
            // the benchmark's "biggest allocation per tick." Unit ids are unique,
            // so the id ordering is a total order and stability is irrelevant —
            // the in-place unstable sort yields the identical, deterministic
            // result with no heap allocation (and is faster on large slices).
            self.unit_id_owner_y[..n].sort_unstable_by(|a, b| a.0.cmp(&b.0));
            let front_x = self.center.0;
            for i in 0..n {
                if self.rand_f32() < 0.3 {
                    let (unit_id, owner, y) = &self.unit_id_owner_y[i];
                    game.move_troops(unit_id, owner, vec![(front_x, *y)]);
                }
            }
        }

        // Recruit events: every 100 ticks, recruit new units. Per-city count
        // is `units_per_city / 2`, capped at `max_recruits_per_event` per
        // event and spread evenly across cities (see `recruit_event_plan`).
        if self.tick.is_multiple_of(100) && self.tick > 0 {
            let (base, extra) = recruit_event_plan(config, self.city_info.len());
            let mut recruits: Vec<(String, UnitType, String)> =
                Vec::with_capacity(base * self.city_info.len() + extra);
            for ci in 0..self.city_info.len() {
                let city_id = self.city_info[ci].0.clone();
                let owner = self.city_info[ci].1.clone();
                for _ in 0..base + usize::from(ci < extra) {
                    let utype_idx = self.rand_idx(utype_count);
                    let utype = self.unit_types[utype_idx].clone();
                    recruits.push((city_id.clone(), utype, owner.clone()));
                }
            }
            for (city_id, utype, owner) in recruits {
                let _ = game.recruit_unit(&city_id, &utype, &owner);
            }
        }
    }
}

/// Max units that can be laid out non-overlapping at `MIN_UNIT_DISTANCE`
/// spacing on a `map_size`×`map_size` map. Conservative floor-based bound
/// (true packing fits slightly more); used for the fail-fast capacity check so
/// an infeasible benchmark config errors out before any work.
pub fn lattice_capacity(map_size: u32) -> u64 {
    let cells = (map_size as f32 / crate::game::constants::MIN_UNIT_DISTANCE).floor() as u64;
    cells * cells
}

/// Per-city recruit plan for one recruit event (every 100 ticks): each city
/// recruits `units_per_city / 2` (min 1), capped at `max_recruits_per_event`
/// total per event and spread evenly — returns `(base, extra)` so city `i`
/// recruits `base + (i < extra)`. Deterministic for a given config.
pub fn recruit_event_plan(config: &BenchmarkConfig, n_cities: usize) -> (usize, usize) {
    if n_cities == 0 {
        return (0, 0);
    }
    let per_city = (config.units_per_city / 2).max(1) as usize;
    let total = (per_city * n_cities).min(config.max_recruits_per_event as usize);
    (total / n_cities, total % n_cities)
}

/// W5: build + encode the synthetic-spectator delta, mirroring the
/// game_server `spectator_vision` path: `phase.shared` (pre-commit baseline)
/// is reused instead of recomputing the shared phase; the union vision grid
/// is the only spectator-specific work. Per-owner counts come from `phase.shared` (W8). Returns the encoded byte length (0 when the
/// idle-skip fires) and commits the spectator vision baseline.
fn bench_spectator_delta(
    game: &Game,
    net_baselines: &mut NetworkBaselines,
    player_ids: &[String],
    phase: &crate::server::delta::NetworkPhaseResult,
    spliced: bool,
) -> (u64, bool) {
    let spec_pid = String::new();
    let spec_vision: Option<Arc<Vec<u8>>> = game.union_vision_grid_flat(player_ids).map(Arc::new);
    let prev_spec_vision: Option<&Arc<Vec<u8>>> = net_baselines.prev_vision.get(&spec_pid);
    // B0: second return value — did the emitted spectator delta carry the
    // union vision grid diff (i.e. did this tick ship a grid diff to a viewer)?
    let mut spec_grid_dirty = false;
    let emitted = if let Some((delta, _)) = build_delta_inner(
        &net_baselines.prev_funds,
        prev_spec_vision,
        &net_baselines.prev_funds,
        &spec_vision,
        phase.grid_width,
        &spec_pid,
        &phase.shared,
    ) {
        let dref = match &delta {
            ServerMessage::DeltaState(d) => d,
            _ => unreachable!(),
        };
        // I4: sparse changed-cell form counts as grid-dirty too.
        spec_grid_dirty = dref.vision_grid_shipped();
        if spliced {
            let ss = crate::server::protocol::SpliceShared::build(
                phase.shared.border_grid_diff.as_ref(),
                phase.shared.border_cells_changed.as_ref(),
                &phase.shared.units_added,
                &phase.shared.units_removed,
                &phase.shared.unit_updates,
                &phase.shared.city_updates,
            );
            let map_bytes = crate::server::protocol::encode_delta_spliced(dref, &ss);
            let mut out = Vec::with_capacity(
                crate::server::protocol::DELTA_STATE_VARIANT_WRAPPER.len() + map_bytes.len(),
            );
            out.extend_from_slice(&crate::server::protocol::DELTA_STATE_VARIANT_WRAPPER);
            out.extend_from_slice(&map_bytes);
            Some(out)
        } else {
            Some(encode_message_with_hint(&delta, estimate_delta_size(dref)))
        }
    } else {
        None
    };
    // Commit the vision baseline unconditionally (game_server does the same
    // even when the idle-skip fires).
    if let Some(v) = &spec_vision {
        net_baselines
            .prev_vision
            .insert(spec_pid.clone(), Arc::clone(v));
    }
    (
        emitted.map(|e| e.len() as u64).unwrap_or(0),
        spec_grid_dirty,
    )
}

/// Run a benchmark using a fresh game and return detailed results.
pub fn run_benchmark(config: BenchmarkConfig) -> BenchmarkResults {
    println!("🔬 Starting benchmark...");
    match config.initial_units {
        Some(n) => println!(
            "  Map: {}×{}, Initial units: {} (total), Ticks: {}, Seed: {}",
            config.map_size, config.map_size, n, config.ticks, config.seed
        ),
        None => println!(
            "  Map: {}×{}, Units/city: {}, Ticks: {}, Seed: {}",
            config.map_size, config.map_size, config.units_per_city, config.ticks, config.seed
        ),
    };

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

    // The city layout is authored for a 200×200 map; scale positions by
    // map_size/200 so the same arrangement fills the map at any size. At the
    // default (200) the scale is 1.0 and the layout is byte-identical.
    let map_size = config.map_size;
    let city_scale = map_size as f32 / 200.0;
    let base_cities: &[(f32, f32)] = &[
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
    ];
    let cities: Vec<(f32, f32)> = if config.cities == 12 {
        // Authored layout (identical to pre-`--bench-cities` behavior).
        base_cities
            .iter()
            .map(|&(x, y)| (x * city_scale, y * city_scale))
            .collect()
    } else {
        // Even sqrt lattice filling the map with `config.cities` cities
        // (margin = one cell of the lattice pitch, so no city sits on the edge).
        let n = config.cities.max(1);
        let cols = (n as f32).sqrt().ceil() as u32;
        let rows = n.div_ceil(cols);
        let m = map_size as f32;
        (0..n)
            .map(|i| {
                let cx = (i % cols) as f32;
                let cy = (i / cols) as f32;
                (
                    m * (cx + 1.0) / (cols as f32 + 1.0),
                    m * (cy + 1.0) / (rows as f32 + 1.0),
                )
            })
            .collect()
    };
    let map_cfg = MapConfig {
        width: map_size,
        height: map_size,
        cities,
        terrain_data: None,
    };
    // Size the per-player starting budget to the initial army. The default army
    // (units_per_city × cities ≈ 240) fits the fixed 2000 funds / 100k manpower.
    // When an explicit `--bench-initial-units N` total is requested (stress test),
    // scale the budget so N units actually spawn — manpower is the binding resource
    // (infantry 1000 men, cavalry 80 men; funds secondary). ~2× headroom mirrors the
    // default (100k manpower for ~60 units/player ≈ 2.5×).
    let (init_funds, init_manpower) = match config.initial_units {
        Some(n) => {
            let n64 = n as u64;
            // Army: N units, half infantry (1000 men / cost 20), half cavalry
            // (80 men / cost 15). Per player (BENCH_PLAYERS=4):
            //   manpower = (540 * N) / 4 per player, ×2 headroom = 270 * N.
            //   funds    = (17.5 * N) / 4 per player, ×2 headroom = 8.75 * N.
            let mp = (n64 * 270).max(100_000);
            let funds = (n64 * 35).div_ceil(4).max(2000);
            (funds as u32, mp as u32)
        }
        None => (2000, 100_000),
    };
    let settings_template = GameSettings {
        starting_funds: init_funds,
        // Sized to the initial army (see above); generous enough that the full
        // pre-battle force spawns, with headroom for mid-battle recruitment.
        initial_manpower: init_manpower,
        funds_per_city: 20.0,
        recruitment_rate: 1.0,
        game_speed: 1.0,
        use_standard_ruleset: true,
        unit_types: make_unit_types(),
        city_upgrade_settings: CityUpgradeSettings::default(),
        water_speed: 1.2,
        influence: InfluenceSettings::default(),
    };

    let total_cities = map_cfg.cities.len();
    println!(
        "  Cities: {}, Players: {} (cities round-robin across them)",
        total_cities, BENCH_PLAYERS
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
    // Per-system allocation totals, averaged across runs (bench-alloc only).
    #[cfg(all(
        any(debug_assertions, feature = "debug-overlay"),
        feature = "bench-alloc"
    ))]
    let mut agg_system_allocs: std::collections::HashMap<String, Vec<serde_json::Value>> =
        std::collections::HashMap::new();
    let mut bench_elapsed_total = std::time::Duration::ZERO;
    let mut total_units: u32 = 0;
    let mut remaining_units = 0usize;

    // Combat diagnostics accumulators (per-tick, averaged across all runs).
    let mut diag_candidates = 0usize;
    let mut diag_attackers = 0usize;
    let mut diag_targets_dead = 0usize;
    let mut diag_targets_same_owner = 0usize;
    let mut diag_targets_out_of_range = 0usize;
    let mut diag_targets_skipped_mutual = 0usize;
    let mut diag_engagements = 0usize;
    let mut health_units = 0usize;
    let mut health_attacking = 0usize;

    // Movement / supply diagnostics accumulators (per-tick, averaged across
    // all runs). Movement: W1 sub-profiling counters. Supply: FCFS gate block
    // counts (they are counts, not ms — hence not in the detailed-timings table).
    let mut mdiag_changes = 0usize;
    let mut mdiag_scans = 0usize;
    let mut mdiag_blocked = 0usize;
    let mut mdiag_noop = 0usize;
    let mut mdiag_cooldown = 0usize;
    let mut sdiag_blocked_funds = 0u64;
    let mut sdiag_blocked_mp = 0u64;

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
        let mut game = Game::new(map, settings, BENCH_PLAYERS as usize);
        let cities = game.query_cities();
        for (i, city) in cities.iter().enumerate() {
            let player_id = format!("player_{}", i % (BENCH_PLAYERS as usize));
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
        let requested_total = config.initial_units.unwrap_or(
            config
                .units_per_city
                .saturating_mul(cities_with_owners.len() as u32),
        );
        total_units = 0;

        // Fail-fast capacity check: the lattice below lays units out at a spacing
        // of `map_size / ceil(sqrt(N))`, which is ≥ MIN_UNIT_DISTANCE iff
        // N ≤ floor(map_size / MIN_UNIT_DISTANCE)². Reject an infeasible config
        // before any work (a non-overlapping placement is impossible otherwise).
        // (Conservative lower bound — true packing fits slightly more; see the
        // plan's R8.)
        let capacity = lattice_capacity(map_cfg.width);
        if requested_total as u64 > capacity {
            eprintln!(
                "✖ Cannot place {} units non-overlapping on a {}×{} map: the max \
                 capacity at min spacing {} is {}. Increase \
                 --bench-map-size or reduce --bench-initial-units.",
                requested_total,
                map_cfg.width,
                map_cfg.height,
                crate::game::constants::MIN_UNIT_DISTANCE,
                capacity
            );
            std::process::exit(1);
        }

        // Non-overlapping lattice via the gate-free bypass. An n×n grid with
        // n = ceil(sqrt(N)); spacing = map/n (≥ MIN_UNIT_DISTANCE, guaranteed by
        // the capacity check). Units start non-overlapping from tick 0 → no
        // warm-up drain, and movement/combat immediately see bounded density.
        //
        // Owners are assigned by quadrant (P0 top-left, P1 top-right, P2
        // bottom-left, P3 bottom-right), not round-robin per cell. Per-cell
        // round-robin would make a checkerboard of enemies 1 spacing apart
        // (= COMBAT_DISTANCE), so ~half the units die during the 200-tick
        // warmup before measurement. Quadrants give two clean front lines
        // (x = map_w/2 and y = map_h/2): interior units are surrounded by their
        // own side, so combat + the nearby-enemies query are still exercised at
        // the front, but the population starts intact. Infantry/cavalry still
        // alternate by global parity.
        let map_size_f = map_cfg.width as f32;
        let map_h_f = map_cfg.height as f32;
        let n = (requested_total as f32).sqrt().ceil().max(1.0) as usize;
        let spacing = map_size_f / n as f32;
        // The four quadrant owners: distinct city owners in first-seen order
        // (player_0..player_{BENCH_PLAYERS-1}). Fall back to player_0 if none.
        let mut quadrant_owners: Vec<String> = Vec::new();
        for c in &cities_with_owners {
            if let Some(o) = &c.owner {
                if !quadrant_owners.iter().any(|q| q == o) {
                    quadrant_owners.push(o.clone());
                }
            }
        }
        if quadrant_owners.is_empty() {
            quadrant_owners.push("player_0".to_string());
        }
        let cx = map_size_f / 2.0;
        let cy = map_h_f / 2.0;
        for k in 0..requested_total as usize {
            let utype = if total_units.is_multiple_of(2) {
                &infantry
            } else {
                &cavalry
            };
            let pos = (
                ((k % n) as f32 + 0.5) * spacing,
                ((k / n) as f32 + 0.5) * spacing,
            );
            // Quadrant → owner index: left/top = 0, right/top = 1,
            // left/bottom = 2, right/bottom = 3.
            let quad = (if pos.0 < cx { 0 } else { 1 }) + (if pos.1 < cy { 0 } else { 2 });
            let owner = &quadrant_owners[quad % quadrant_owners.len()];
            if game.recruit_unit_at(utype, owner, pos).is_some() {
                total_units += 1;
            }
        }
        println!(
            "  Recruited {} units (requested {})",
            total_units, requested_total
        );

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

        // Initialize network baselines for the network phase benchmark
        let player_ids: Vec<String> = (0..BENCH_PLAYERS)
            .map(|i| format!("player_{}", i))
            .collect();
        // W4: run_network_phase takes &[&str] — borrowed pid list, no clones.
        let player_id_strs: Vec<&str> = player_ids.iter().map(|s| s.as_str()).collect();
        let mut net_baselines: NetworkBaselines = NetworkBaselines {
            prev_units: UnitBaselineStore::default(),
            prev_cities: game
                .query_cities()
                .iter()
                .map(|c| (c.id.clone(), c.into()))
                .collect(),
            prev_funds: game.all_player_funds(),
            prev_border: game.territory_grid_flat(),
            prev_vision: {
                let mut m = std::collections::HashMap::new();
                for pid in &player_ids {
                    if let Some(v) = game.vision_grid_flat(pid) {
                        m.insert(pid.clone(), v);
                    }
                }
                m
            },
        };
        // I1: seed the entity-indexed unit baseline store ONCE (the live tick
        // loop does the same) — the per-tick `prev_units` HashMap rebuild is
        // gone; the in-place pass commits it instead.
        game.init_unit_baselines(&mut net_baselines.prev_units);

        // W5: synthetic spectator baseline (pid ""), seeded from the union
        // vision grid so warmup reaches steady state like game_server does.
        if config.bench_spectator {
            if let Some(v) = game.union_vision_grid_flat(&player_ids) {
                net_baselines.prev_vision.insert(String::new(), Arc::new(v));
            }
        }

        // Warmup (with AI to warm up caches & archetypes)
        for _ in 0..BENCH_WARMUP_TICKS {
            sim.tick(&mut game, &config);
            game.tick_game();
            let unit_diff = game.update_states_in_place(&mut net_baselines.prev_units);
            // W7: convert City → CityState while the world guard is alive (no
            // redundant full Vec<City> clone).
            let current_cities: Vec<CityState> =
                game.with_cities(|cities| cities.iter().map(|c| c.into()).collect());
            let phase = run_network_phase(
                &game,
                &mut net_baselines,
                &player_id_strs,
                unit_diff,
                current_cities,
                config.bench_splice,
                None, // no replay recording in the benchmark
            );
            if config.bench_spectator {
                bench_spectator_delta(
                    &game,
                    &mut net_baselines,
                    &player_ids,
                    &phase,
                    config.bench_splice,
                );
            }
        }

        // Reset per-system alloc accumulators so the per-system table reflects
        // measured (steady-state) ticks only, matching the measured-only summary.
        #[cfg(all(
            any(debug_assertions, feature = "debug-overlay"),
            feature = "bench-alloc"
        ))]
        game.reset_system_allocs();

        // Collect per-tick data
        let mut run_tick_data: Vec<TickData> = Vec::with_capacity(config.ticks as usize);
        let run_start = Instant::now();

        for _ in 0..config.ticks {
            let tick_start = Instant::now();

            // Harness input generation (move/recruit orders). It is *timed*
            // (a real tick includes applying orders) but kept OUT of the
            // allocation window: the alloc stats measure the *game's* per-tick
            // allocations, not the benchmark's input driver. The driver is now
            // allocation-free in steady state anyway (see `sort_unstable_by` above
            // and `fill_unit_id_owner_y`'s reused buffer).
            sim.tick(&mut game, &config);

            #[cfg(feature = "bench-alloc")]
            let alloc_base = crate::bench_alloc::snapshot();

            game.tick_game();
            // World read-back + counters are part of the per-tick work, so they
            // are timed: this keeps total_ms (systems + overhead) reconciling
            // with the wall-clock avg tick. The read-backs are allocation-free,
            // so leaving them inside the alloc window is harmless.
            // `unit_count()` is an allocation-free count (no UnitQueryData build).
            let unit_count = game.unit_count() as u32;
            let movers = game.last_mover_count() as u32;
            let combats = game.last_engaged_pairs() as u32;

            // Network phase (W1): snapshot + build + encode per-player deltas,
            // measured as a system via tick_system! so it appears in the
            // per-system P90/P99 breakdown and alloc table alongside ECS systems.
            // The in-place unit diff (update_states_in_place) + cities
            // read-back snapshot is inside the timing block so its allocs
            // are attributed to "network" (matching the pre-W1 behaviour
            // where run_network_phase called them internally).
            let (net_phase, net_snapshot_ms) =
                crate::tick_system!("network", &mut game.world, game.world_entity, {
                    // B0: snapshot sub-phase (I1 in-place unit diff + cities
                    // read-back), feature-gated so production builds pay zero
                    // timing cost.
                    #[cfg(any(feature = "microbenchmarks", feature = "bench-alloc"))]
                    let snapshot_start = std::time::Instant::now();
                    let unit_diff = game.update_states_in_place(&mut net_baselines.prev_units);
                    // W7: convert City → CityState while the world guard is alive
                    // (no per-tick Vec<City> clone).
                    let current_cities: Vec<CityState> =
                        game.with_cities(|cities| cities.iter().map(|c| c.into()).collect());
                    #[cfg(any(feature = "microbenchmarks", feature = "bench-alloc"))]
                    let snapshot_ms = snapshot_start.elapsed().as_secs_f64() * 1000.0;
                    #[cfg(not(any(feature = "microbenchmarks", feature = "bench-alloc")))]
                    let snapshot_ms = 0.0f64;
                    (
                        run_network_phase(
                            &game,
                            &mut net_baselines,
                            &player_id_strs,
                            unit_diff,
                            current_cities,
                            config.bench_splice,
                            None, // no replay recording in the benchmark
                        ),
                        snapshot_ms,
                    )
                });
            let mut net_wire_bytes = net_phase.wire_bytes;

            // B0: sub-phase ms reported by run_network_phase (delta = shared
            // compute + per-player build; encode = per-player serialize).
            // Feature-gated: the result fields only exist with the timing
            // features, so fall back to 0.0 otherwise.
            #[cfg(any(feature = "microbenchmarks", feature = "bench-alloc"))]
            let (net_delta_ms, net_encode_ms) = (net_phase.delta_ms, net_phase.encode_ms);
            #[cfg(not(any(feature = "microbenchmarks", feature = "bench-alloc")))]
            let (net_delta_ms, net_encode_ms) = (0.0f64, 0.0f64);
            // B0: grid-dirty = a vision/border grid diff was emitted this tick
            // (players in the network phase, plus the synthetic spectator
            // when --bench-spectator is on).
            #[cfg(any(feature = "microbenchmarks", feature = "bench-alloc"))]
            let mut net_grid_dirty = net_phase.grid_dirty;
            #[cfg(not(any(feature = "microbenchmarks", feature = "bench-alloc")))]
            let net_grid_dirty = false;

            // W5: synthetic spectator (mirrors the game_server spectator_vision
            // path) — kept OUT of the "network" tick_system block, matching
            // game_server where the spectator build is plain tick overhead.
            // Its encoded bytes are added to net_wire_bytes so idle-skip savings
            // show up in the wire metric.
            if config.bench_spectator {
                let (spec_wire_bytes, spec_grid_dirty) = bench_spectator_delta(
                    &game,
                    &mut net_baselines,
                    &player_ids,
                    &net_phase,
                    config.bench_splice,
                );
                net_wire_bytes += spec_wire_bytes;
                #[cfg(any(feature = "microbenchmarks", feature = "bench-alloc"))]
                {
                    net_grid_dirty |= spec_grid_dirty;
                }
                // `spec_grid_dirty` is only meaningful with the timing
                // features; consume it elsewhere to keep the shared binding.
                #[cfg(not(any(feature = "microbenchmarks", feature = "bench-alloc")))]
                let _ = spec_grid_dirty;
            }

            // Capture tick_ms AFTER the network phase so total_ms includes
            // network time and (total_ms - system_sum_ms) stays non-negative.
            let tick_ms = tick_start.elapsed().as_secs_f64() * 1000.0;

            #[cfg(feature = "bench-alloc")]
            let (td_allocs, td_bytes, td_max) = {
                let d = crate::bench_alloc::snapshot().diff(alloc_base);
                (d.allocs, d.bytes, d.max_alloc)
            };
            #[cfg(not(feature = "bench-alloc"))]
            let (td_allocs, td_bytes, td_max) = (0u64, 0u64, 0u64);

            // Accumulate combat diagnostics (averaged at the end).
            let diag = game.combat_diag();
            diag_candidates += diag.candidates;
            diag_attackers += diag.attackers;
            diag_targets_dead += diag.targets_dead;
            diag_targets_same_owner += diag.targets_same_owner;
            diag_targets_out_of_range += diag.targets_out_of_range;
            diag_targets_skipped_mutual += diag.targets_skipped_mutual;
            diag_engagements += diag.engagements;
            let health = game.combat_health();
            health_units += health.units;
            health_attacking += health.units_with_attacking;
            let mdiag = game.movement_diag();
            mdiag_changes += mdiag.changes;
            mdiag_scans += mdiag.scans;
            mdiag_blocked += mdiag.blocked;
            mdiag_noop += mdiag.noop_skipped;
            mdiag_cooldown += mdiag.cooldown_skipped;
            let sdiag = game.supply_diag();
            sdiag_blocked_funds += sdiag.blocked_funds;
            sdiag_blocked_mp += sdiag.blocked_mp;

            #[cfg(any(debug_assertions, feature = "debug-overlay"))]
            let td = if let Some(ds) = game.get_debug_stats() {
                let entity_count = ds["entity_count"].as_u64().unwrap_or(0) as u32;
                let mut system_sum_ms = 0.0f64;
                let mut net_ms = 0.0f64;
                if let Some(st) = ds["system_timings"].as_array() {
                    for s in st {
                        let ms = s["last_ms"].as_f64().unwrap_or(0.0);
                        system_sum_ms += ms;
                        if s["name"] == "network" {
                            net_ms = ms;
                        }
                    }
                }
                TickData {
                    total_ms: tick_ms,
                    entity_count,
                    unit_count,
                    system_sum_ms,
                    movers,
                    combats,
                    allocs: td_allocs,
                    bytes: td_bytes,
                    max_alloc: td_max,
                    net_wire_bytes,
                    net_ms,
                    net_unit_updates: net_phase.unit_updates,
                    // B0 sub-phases (send = 0: headless bench models no egress).
                    net_snapshot_ms,
                    net_delta_ms,
                    net_encode_ms,
                    net_send_ms: 0.0,
                    net_grid_dirty,
                    // I4: changed-cell fraction metric (timing features only).
                    #[cfg(any(feature = "microbenchmarks", feature = "bench-alloc"))]
                    net_grid_changed_cells: net_phase.grid_changed_cells,
                    #[cfg(not(any(feature = "microbenchmarks", feature = "bench-alloc")))]
                    net_grid_changed_cells: 0,
                }
            } else {
                TickData {
                    total_ms: tick_ms,
                    entity_count: 0,
                    unit_count,
                    system_sum_ms: 0.0,
                    movers,
                    combats,
                    allocs: td_allocs,
                    bytes: td_bytes,
                    max_alloc: td_max,
                    net_wire_bytes,
                    net_ms: 0.0,
                    net_unit_updates: net_phase.unit_updates,
                    // B0 sub-phases (send = 0: headless bench models no egress).
                    net_snapshot_ms,
                    net_delta_ms,
                    net_encode_ms,
                    net_send_ms: 0.0,
                    net_grid_dirty,
                    // I4: changed-cell fraction metric (timing features only).
                    #[cfg(any(feature = "microbenchmarks", feature = "bench-alloc"))]
                    net_grid_changed_cells: net_phase.grid_changed_cells,
                    #[cfg(not(any(feature = "microbenchmarks", feature = "bench-alloc")))]
                    net_grid_changed_cells: 0,
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
                allocs: td_allocs,
                bytes: td_bytes,
                max_alloc: td_max,
                net_wire_bytes,
                net_ms: 0.0,
                net_unit_updates: net_phase.unit_updates,
                // B0 sub-phases (send = 0: headless bench models no egress).
                net_snapshot_ms,
                net_delta_ms,
                net_encode_ms,
                net_send_ms: 0.0,
                net_grid_dirty,
                // I4: changed-cell fraction metric (timing features only).
                #[cfg(any(feature = "microbenchmarks", feature = "bench-alloc"))]
                net_grid_changed_cells: net_phase.grid_changed_cells,
                #[cfg(not(any(feature = "microbenchmarks", feature = "bench-alloc")))]
                net_grid_changed_cells: 0,
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
            #[cfg(feature = "bench-alloc")]
            if let Some(arr) = final_debug["system_allocs"].as_array() {
                for s in arr {
                    let name = s["name"].as_str().unwrap_or("?").to_string();
                    agg_system_allocs.entry(name).or_default().push(s.clone());
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

    // Per-system allocation totals averaged across runs (empty unless bench-alloc
    // + debug stats are both active).
    let final_system_allocs: Vec<serde_json::Value> = {
        #[cfg(all(
            any(debug_assertions, feature = "debug-overlay"),
            feature = "bench-alloc"
        ))]
        {
            agg_system_allocs
                .values()
                .map(|vals| average_allocs(vals))
                .collect()
        }
        #[cfg(not(all(
            any(debug_assertions, feature = "debug-overlay"),
            feature = "bench-alloc"
        )))]
        {
            Vec::new()
        }
    };

    // Per-system allocation split, expressed both as per-tick averages (divide
    // each run's measured-tick total by `ticks`) and per-call averages (like the
    // timing breakdown's Avg/Call). Computed early so the CLI section can use it.
    // Empty unless bench-alloc + debug stats are on.
    let system_allocs: Vec<serde_json::Value> = final_system_allocs
        .iter()
        .map(|s| {
            let total_allocs = s["total_allocs"].as_f64().unwrap_or(0.0);
            let total_bytes = s["total_bytes"].as_f64().unwrap_or(0.0);
            let calls = s["call_count"].as_f64().unwrap_or(0.0);
            let ticks = config.ticks.max(1) as f64;
            serde_json::json!({
                "name": s["name"],
                "total_allocs": total_allocs,
                "total_bytes": total_bytes,
                "call_count": s["call_count"].as_u64().unwrap_or(0),
                "allocs_per_tick": total_allocs / ticks,
                "bytes_per_tick": total_bytes / ticks,
                "avg_allocs": if calls > 0.0 { total_allocs / calls } else { 0.0 },
                "avg_bytes": if calls > 0.0 { total_bytes / calls } else { 0.0 },
            })
        })
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
                let sum_allocs = runs_tick_data
                    .iter()
                    .map(|r| r[i].allocs as f64)
                    .sum::<f64>();
                let sum_bytes = runs_tick_data
                    .iter()
                    .map(|r| r[i].bytes as f64)
                    .sum::<f64>();
                // "Biggest" aggregates as the max across runs (peak single alloc at this tick).
                let max_max_alloc = runs_tick_data
                    .iter()
                    .map(|r| r[i].max_alloc)
                    .max()
                    .unwrap_or(0);
                let sum_net_wire = runs_tick_data
                    .iter()
                    .map(|r| r[i].net_wire_bytes as f64)
                    .sum::<f64>();
                let sum_net_ms = runs_tick_data.iter().map(|r| r[i].net_ms).sum::<f64>();
                let sum_net_updates = runs_tick_data
                    .iter()
                    .map(|r| r[i].net_unit_updates as f64)
                    .sum::<f64>();
                // B0: sub-phase ms average across runs; grid-dirty is a
                // per-tick classification (deterministic across runs, so any()
                // == all() — the sim is seeded identically per run).
                let sum_net_snap = runs_tick_data
                    .iter()
                    .map(|r| r[i].net_snapshot_ms)
                    .sum::<f64>();
                let sum_net_delta = runs_tick_data
                    .iter()
                    .map(|r| r[i].net_delta_ms)
                    .sum::<f64>();
                let sum_net_encode = runs_tick_data
                    .iter()
                    .map(|r| r[i].net_encode_ms)
                    .sum::<f64>();
                let sum_net_send = runs_tick_data.iter().map(|r| r[i].net_send_ms).sum::<f64>();
                let grid_dirty = runs_tick_data.iter().any(|r| r[i].net_grid_dirty);
                // I4: changed-cell counts are deterministic across runs (same
                // seed) — average like the other per-tick network metrics.
                let sum_net_grid_cells = runs_tick_data
                    .iter()
                    .map(|r| r[i].net_grid_changed_cells as f64)
                    .sum::<f64>();
                TickData {
                    total_ms: sum_total / n,
                    entity_count: (sum_entity / n).round() as u32,
                    unit_count: (sum_unit / n).round() as u32,
                    system_sum_ms: sum_sys / n,
                    movers: (sum_movers / n).round() as u32,
                    combats: (sum_combats / n).round() as u32,
                    allocs: (sum_allocs / n).round() as u64,
                    bytes: (sum_bytes / n).round() as u64,
                    max_alloc: max_max_alloc,
                    net_wire_bytes: (sum_net_wire / n).round() as u64,
                    net_ms: sum_net_ms / n,
                    net_unit_updates: (sum_net_updates / n).round() as u32,
                    net_snapshot_ms: sum_net_snap / n,
                    net_delta_ms: sum_net_delta / n,
                    net_encode_ms: sum_net_encode / n,
                    net_send_ms: sum_net_send / n,
                    net_grid_dirty: grid_dirty,
                    net_grid_changed_cells: (sum_net_grid_cells / n).round() as u64,
                }
            })
            .collect()
    } else {
        Vec::new()
    };
    let recruit_events = ((config.ticks / 100).max(1) - 1) as u32;
    // Per-event count under the `max_recruits_per_event` cap (same logic the
    // simulator uses), so the "Units recruited" total matches what actually spawned.
    let per_recruit_event = {
        let (base, extra) = recruit_event_plan(&config, total_cities);
        (base * total_cities + extra) as u32
    };
    let total_recruited = total_units + recruit_events * per_recruit_event;

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
    println!(
        "    movement (avg/tick): changes {:.1}, gate scans {:.1}, gate-blocked {:.1}, cooldown-skipped {:.1}, noop-skipped {:.1}",
        mdiag_changes as f64 / tick_data.len() as f64,
        mdiag_scans as f64 / tick_data.len() as f64,
        mdiag_blocked as f64 / tick_data.len() as f64,
        mdiag_cooldown as f64 / tick_data.len() as f64,
        mdiag_noop as f64 / tick_data.len() as f64
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
    println!(
        "    supply (avg/tick): funds-blocked {:.1}, mp-blocked {:.1}",
        sdiag_blocked_funds as f64 / tick_data.len() as f64,
        sdiag_blocked_mp as f64 / tick_data.len() as f64
    );
    println!("  Total combat-tick count: {}", total_combats);

    // Allocation stats (only when bench-alloc is on)
    {
        let total_a: u64 = tick_data.iter().map(|td| td.allocs).sum();
        let total_b: u64 = tick_data.iter().map(|td| td.bytes).sum();
        let avg_a = total_a as f64 / tick_data.len().max(1) as f64;
        let avg_b = total_b as f64 / tick_data.len().max(1) as f64;
        let avg_alloc_size = if total_a > 0 {
            total_b as f64 / total_a as f64
        } else {
            0.0
        };
        let max_alloc = tick_data.iter().map(|td| td.max_alloc).max().unwrap_or(0);
        if avg_a > 0.0 {
            println!("  Avg allocs/tick: {:.1}", avg_a);
            println!("  Avg bytes/tick: {}", fmt_bytes(avg_b));
            println!(
                "  Avg alloc size: {} (bytes per allocation)",
                fmt_bytes(avg_alloc_size)
            );
            println!(
                "  Biggest alloc:  {} (largest single allocation)",
                fmt_bytes(max_alloc as f64)
            );
        }
    }

    // Stack-resident scratch struct sizes (static, zero cost)
    println!("  Stack-resident sizes:");
    println!(
        "    Game:               {}",
        fmt_bytes(std::mem::size_of::<crate::game::game::Game>() as f64)
    );
    println!(
        "    MovementScratch:    {}",
        fmt_bytes(std::mem::size_of::<crate::game::systems::movement::MovementScratch>() as f64)
    );
    println!(
        "    CombatScratch:      {}",
        fmt_bytes(std::mem::size_of::<crate::game::systems::combat::CombatScratch>() as f64)
    );
    println!(
        "    InfluenceScratch:   {}",
        fmt_bytes(std::mem::size_of::<crate::game::systems::influence::InfluenceScratch>() as f64)
    );
    println!(
        "    SupplyScratch:      {}",
        fmt_bytes(std::mem::size_of::<crate::game::systems::supply::SupplyScratch>() as f64)
    );
    println!(
        "    TickContext:        {}",
        fmt_bytes(std::mem::size_of::<crate::game::game::TickContext>() as f64)
    );
    let ticks_f = tick_data.len() as f64;
    println!(
        "  Combat diagnostics (avg/tick): candidates {:.1}, attackers {:.1}, engagements {:.1}",
        diag_candidates as f64 / ticks_f,
        diag_attackers as f64 / ticks_f,
        diag_engagements as f64 / ticks_f,
    );
    println!(
        "    target clears: dead {:.1}, same-owner {:.1}, out-of-range {:.1}, skipped-mutual {:.1}",
        diag_targets_dead as f64 / ticks_f,
        diag_targets_same_owner as f64 / ticks_f,
        diag_targets_out_of_range as f64 / ticks_f,
        diag_targets_skipped_mutual as f64 / ticks_f,
    );
    println!(
        "    health: units {:.1}, with-attacking {:.1}",
        health_units as f64 / ticks_f,
        health_attacking as f64 / ticks_f,
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

    // Per-system allocation breakdown (bench-alloc + debug stats only). Mirrors
    // the timing table: allocs and bytes attributed to each system, as per-tick
    // averages (÷ ticks) and per-call averages.
    if !system_allocs.is_empty() {
        let mut sorted_allocs: Vec<&serde_json::Value> = system_allocs.iter().collect();
        sorted_allocs.sort_by(|a, b| {
            b["bytes_per_tick"]
                .as_f64()
                .unwrap_or(0.0)
                .partial_cmp(&a["bytes_per_tick"].as_f64().unwrap_or(0.0))
                .unwrap()
        });
        println!("  Per-system allocations (avg per tick, bench-alloc):");
        println!("  ───────────────────────────────────────────────────────");
        println!(
            "    {:<25}  {:>10} {:>14} {:>12} {:>14}  {:>8}",
            "system", "allocs/tick", "bytes/tick", "allocs/call", "bytes/call", "calls"
        );
        for s in &sorted_allocs {
            let name = s["name"].as_str().unwrap_or("?");
            let apt = s["allocs_per_tick"].as_f64().unwrap_or(0.0);
            let bpt = s["bytes_per_tick"].as_f64().unwrap_or(0.0);
            let a_c = s["avg_allocs"].as_f64().unwrap_or(0.0);
            let b_c = s["avg_bytes"].as_f64().unwrap_or(0.0);
            let calls = s["call_count"].as_u64().unwrap_or(0);
            println!(
                "    {:<25}  {:>10.2} {:>14} {:>12.2} {:>14}  {:>8}",
                name,
                apt,
                fmt_bytes(bpt),
                a_c,
                fmt_bytes(b_c),
                calls
            );
        }
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

    // Allocation stats (per tick, 0 when bench-alloc is off)
    let total_alloc_ticks: u64 = tick_data.iter().map(|td| td.allocs).sum();
    let avg_allocs_per_tick = if tick_data.is_empty() {
        0.0
    } else {
        total_alloc_ticks as f64 / tick_data.len() as f64
    };
    let total_alloc_bytes: u64 = tick_data.iter().map(|td| td.bytes).sum();
    let avg_bytes_per_tick = if tick_data.is_empty() {
        0.0
    } else {
        total_alloc_bytes as f64 / tick_data.len() as f64
    };
    // Average single-allocation size (bytes/alloc) and the biggest single
    // allocation observed anywhere in the benchmark. 0 when bench-alloc is off.
    let avg_alloc_size = if total_alloc_ticks > 0 {
        total_alloc_bytes as f64 / total_alloc_ticks as f64
    } else {
        0.0
    };
    let max_alloc_size = tick_data.iter().map(|td| td.max_alloc).max().unwrap_or(0);

    // B0: network sub-phase averages + idle/movement × grid-clean/dirty
    // regime split, over the raw 3×N measured-tick pool (the same honest
    // distribution the latency percentiles use). Feeds the "Network Phase"
    // HTML section and the per-commit A/B tables.
    let net_n = tick_data.len().max(1) as f64;
    let net_subphase_ms = serde_json::json!({
        "net": tick_data.iter().map(|t| t.net_ms).sum::<f64>() / net_n,
        "snapshot": tick_data.iter().map(|t| t.net_snapshot_ms).sum::<f64>() / net_n,
        "delta": tick_data.iter().map(|t| t.net_delta_ms).sum::<f64>() / net_n,
        "encode": tick_data.iter().map(|t| t.net_encode_ms).sum::<f64>() / net_n,
        "send": tick_data.iter().map(|t| t.net_send_ms).sum::<f64>() / net_n,
    });

    // I4: changed-cell density on grid-dirty ticks — the number that
    // decides the ~5% sparse-vs-full threshold (sparse is chosen while
    // per-grid changed fraction <= ~5%). `avg_changed_fraction` is the
    // per-GRID average over the (1 border + BENCH_PLAYERS vision) grids
    // shipped on grid-dirty ticks; 0.0 when the timing features are off.
    // (Computed here, outside the summary `json!` below: a value starting
    // with `{` would be parsed as a nested object literal by the macro.)
    let grid_cells = config.map_size as u64 * config.map_size as u64;
    let grid_dirty_ticks = tick_data.iter().filter(|t| t.net_grid_dirty).count() as f64;
    let sum_grid_changed: f64 = tick_data
        .iter()
        .filter(|t| t.net_grid_dirty)
        .map(|t| t.net_grid_changed_cells as f64)
        .sum();
    let grids_per_dirty_tick = (BENCH_PLAYERS + 1) as f64;
    let grid_diag = {
        let avg_changed_cells = if grid_dirty_ticks > 0.0 {
            sum_grid_changed / grid_dirty_ticks
        } else {
            0.0
        };
        let avg_changed_fraction = if grid_dirty_ticks > 0.0 && grid_cells > 0 {
            sum_grid_changed / grid_dirty_ticks / grids_per_dirty_tick / grid_cells as f64
        } else {
            0.0
        };
        serde_json::json!({
            "grid_cells": grid_cells,
            "grids_per_dirty_tick": grids_per_dirty_tick,
            "grid_dirty_ticks": grid_dirty_ticks,
            "avg_changed_cells_per_dirty_tick": avg_changed_cells,
            "avg_changed_fraction": avg_changed_fraction,
        })
    };

    let summary = serde_json::json!({
        "config": {
            "cities": total_cities, "units_per_city": config.units_per_city,
            "max_recruits_per_event": config.max_recruits_per_event,
            "initial_units": config.initial_units,
            "map_size": config.map_size,
            "total_units_initial": total_units, "total_units_recruited": total_recruited,
            "ticks": config.ticks, "seed": config.seed,
            "measured_runs": MEASURED_RUNS, "warmup_ticks": BENCH_WARMUP_TICKS,
            "bench_prev": config.bench_prev, "detailed_systems": config.detailed_systems, "bench_spectator": config.bench_spectator, "bench_splice": config.bench_splice,
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
            "avg_gate_scans_per_tick": mdiag_scans as f64 / tick_data.len() as f64,
            "avg_gate_blocked_per_tick": mdiag_blocked as f64 / tick_data.len() as f64,
            "avg_cooldown_skipped_per_tick": mdiag_cooldown as f64 / tick_data.len() as f64,
            "avg_supply_funds_blocked_per_tick": sdiag_blocked_funds as f64 / tick_data.len() as f64,
            "avg_supply_mp_blocked_per_tick": sdiag_blocked_mp as f64 / tick_data.len() as f64,
            "avg_combats_per_tick": avg_combats_per_tick,
            "total_combat_ticks": total_combat_ticks,
            "allocs_per_tick": avg_allocs_per_tick,
            "bytes_per_tick": avg_bytes_per_tick,
            "avg_alloc_size": avg_alloc_size,
            "max_alloc_size": max_alloc_size,
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
        // Per-system allocation split (bench-alloc + debug stats). `allocs_per_tick`
        // / `bytes_per_tick` divide each run's measured-tick totals by `ticks`;
        // `avg_allocs`/`avg_bytes` are per system call. Empty unless bench-alloc is on.
        "system_allocs": {
            "ticks": config.ticks,
            "systems": system_allocs,
        },
        // B0: sub-phase split of net_ms + per-regime aggregates (idle vs
        // movement × grid-clean vs grid-dirty). `send` is always 0 in the
        // headless benchmark (no outbox/bridge modeled).
        "network": {
            "splice": config.bench_splice,
            "subphase_ms": net_subphase_ms,
            "split": {
                "idle_grid_clean": network_regime_stats(&tick_data, false, false),
                "idle_grid_dirty": network_regime_stats(&tick_data, false, true),
                "movement_grid_clean": network_regime_stats(&tick_data, true, false),
                "movement_grid_dirty": network_regime_stats(&tick_data, true, true),
            },
        },
        // Per-tick arrays are the averaged series (length == config.ticks),
        // matching the charts. Note overhead_durations_ms above stays the raw
        // 3×N distribution (it backs the overhead percentiles); the length
        // divergence from the per-tick arrays is intentional (Raw-JSON only).
        "tick_durations_ms": avg_tick_data.iter().map(|td| td.total_ms).collect::<Vec<f64>>(),
        "movers_per_tick": avg_tick_data.iter().map(|td| td.movers).collect::<Vec<_>>(),
        "combats_per_tick": avg_tick_data.iter().map(|td| td.combats).collect::<Vec<_>>(),
        "allocs_per_tick_series": avg_tick_data.iter().map(|td| td.allocs as f64).collect::<Vec<f64>>(),
        "bytes_per_tick_series": avg_tick_data.iter().map(|td| td.bytes as f64).collect::<Vec<f64>>(),
        "avg_alloc_size_series": avg_tick_data
            .iter()
            .map(|td| {
                if td.allocs > 0 {
                    td.bytes as f64 / td.allocs as f64
                } else {
                    0.0
                }
            })
            .collect::<Vec<f64>>(),
        "max_alloc_per_tick_series": avg_tick_data
            .iter()
            .map(|td| td.max_alloc as f64)
            .collect::<Vec<f64>>(),
        "net_wire_bytes_per_tick": avg_tick_data.iter().map(|td| td.net_wire_bytes as f64).collect::<Vec<f64>>(),
        "net_unit_updates_per_tick": avg_tick_data.iter().map(|td| td.net_unit_updates as f64).collect::<Vec<f64>>(),
        "net_ms_per_tick": avg_tick_data.iter().map(|td| td.net_ms).collect::<Vec<f64>>(),
        "stack": {
            "Game": std::mem::size_of::<crate::game::game::Game>(),
            "MovementScratch": std::mem::size_of::<crate::game::systems::movement::MovementScratch>(),
            "CombatScratch": std::mem::size_of::<crate::game::systems::combat::CombatScratch>(),
            "InfluenceScratch": std::mem::size_of::<crate::game::systems::influence::InfluenceScratch>(),
            "SupplyScratch": std::mem::size_of::<crate::game::systems::supply::SupplyScratch>(),
            "TickContext": std::mem::size_of::<crate::game::game::TickContext>(),
        },
        "combat_diag": {
            "avg_candidates_per_tick": diag_candidates as f64 / tick_data.len() as f64,
            "avg_attackers_per_tick": diag_attackers as f64 / tick_data.len() as f64,
            "avg_engagements_per_tick": diag_engagements as f64 / tick_data.len() as f64,
            "avg_targets_dead_per_tick": diag_targets_dead as f64 / tick_data.len() as f64,
            "avg_targets_same_owner_per_tick": diag_targets_same_owner as f64 / tick_data.len() as f64,
            "avg_targets_out_of_range_per_tick": diag_targets_out_of_range as f64 / tick_data.len() as f64,
            "avg_targets_skipped_mutual_per_tick": diag_targets_skipped_mutual as f64 / tick_data.len() as f64,
            "health_avg_units": health_units as f64 / tick_data.len() as f64,
            "health_avg_units_with_attacking": health_attacking as f64 / tick_data.len() as f64,
        },
        "movement_diag": {
            "avg_changes_per_tick": mdiag_changes as f64 / tick_data.len() as f64,
            "avg_gate_scans_per_tick": mdiag_scans as f64 / tick_data.len() as f64,
            "avg_gate_blocked_per_tick": mdiag_blocked as f64 / tick_data.len() as f64,
            "avg_cooldown_skipped_per_tick": mdiag_cooldown as f64 / tick_data.len() as f64,
            "avg_noop_skipped_per_tick": mdiag_noop as f64 / tick_data.len() as f64,
        },
        "grid_diag": grid_diag,
        "supply_diag": {
            "avg_funds_blocked_per_tick": sdiag_blocked_funds as f64 / tick_data.len() as f64,
            "avg_mp_blocked_per_tick": sdiag_blocked_mp as f64 / tick_data.len() as f64,
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

/// Human-readable byte size: B for <1KB, then KB/MB/GB/TB (base 1024).
/// e.g. 24 -> "24 B", 1456 -> "1.42 KB", 434267 -> "424.09 KB".
fn fmt_bytes(b: f64) -> String {
    const UNITS: [&str; 5] = ["B", "KB", "MB", "GB", "TB"];
    let mut v = b;
    let mut i = 0;
    while v >= 1024.0 && i < UNITS.len() - 1 {
        v /= 1024.0;
        i += 1;
    }
    if i == 0 {
        format!("{:.0} B", v)
    } else {
        format!("{:.2} {}", v, UNITS[i])
    }
}

/// Signed variant of [fmt_bytes] for deltas: "+4.9 KB" / "-2.1 KB".
fn fmt_bytes_signed(b: f64) -> String {
    let sign = if b < 0.0 { "-" } else { "+" };
    format!("{sign}{}", fmt_bytes(b.abs()))
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

/// B0: per-regime network aggregates over the measured-tick pool. `movement`
/// selects `movers > 0` ticks (false = idle), `grid_dirty` selects ticks that
/// emitted a vision/border grid diff. Values are per-tick AVERAGES within the
/// regime; all-zero when no tick matches (an empty regime is not an error).
fn network_regime_stats(ticks: &[TickData], movement: bool, grid_dirty: bool) -> serde_json::Value {
    let matching: Vec<&TickData> = ticks
        .iter()
        .filter(|t| (t.movers > 0) == movement && t.net_grid_dirty == grid_dirty)
        .collect();
    let n = matching.len();
    if n == 0 {
        return serde_json::json!({
            "ticks": 0,
            "net_ms": 0.0f64,
            "net_wire_bytes": 0.0f64,
            "net_unit_updates": 0.0f64,
            "allocs": 0.0f64,
            "bytes": 0.0f64,
        });
    }
    let f = n as f64;
    serde_json::json!({
        "ticks": n,
        "net_ms": matching.iter().map(|t| t.net_ms).sum::<f64>() / f,
        "net_wire_bytes": matching.iter().map(|t| t.net_wire_bytes as f64).sum::<f64>() / f,
        "net_unit_updates": matching.iter().map(|t| t.net_unit_updates as f64).sum::<f64>() / f,
        "allocs": matching.iter().map(|t| t.allocs as f64).sum::<f64>() / f,
        "bytes": matching.iter().map(|t| t.bytes as f64).sum::<f64>() / f,
    })
}

/// Backward compat: return summary JSON
pub fn run_benchmark_json(config: BenchmarkConfig) -> serde_json::Value {
    run_benchmark(config).summary
}

/// Generate a self-contained HTML report from benchmark results.
pub fn generate_html_report(results: &BenchmarkResults, prev_path: Option<&str>) -> String {
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
    let map_size = results.summary["config"]["map_size"]
        .as_u64()
        .unwrap_or(200);
    let map_desc = format!("{}×{} ({} tiles)", map_size, map_size, map_size * map_size);
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
    let max_recruits_per_event = results.summary["config"]["max_recruits_per_event"]
        .as_u64()
        .unwrap_or(2000);
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
    // Movement / supply per-tick counters (Battle Activity cards). 0.0 when the
    // report predates them (field-presence degrades to "no delta" via the cards').
    let avg_gate_scans = results.summary["results"]["avg_gate_scans_per_tick"]
        .as_f64()
        .unwrap_or(0.0);
    let avg_gate_blocked = results.summary["results"]["avg_gate_blocked_per_tick"]
        .as_f64()
        .unwrap_or(0.0);
    let avg_cooldown_skipped = results.summary["results"]["avg_cooldown_skipped_per_tick"]
        .as_f64()
        .unwrap_or(0.0);
    let avg_supply_funds_blocked = results.summary["results"]["avg_supply_funds_blocked_per_tick"]
        .as_f64()
        .unwrap_or(0.0);
    let avg_supply_mp_blocked = results.summary["results"]["avg_supply_mp_blocked_per_tick"]
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
    let (prev_timings, prev_json) = prev_path
        .map(parse_previous_report)
        .unwrap_or((std::collections::HashMap::new(), serde_json::Value::Null));
    let comparison_note = match prev_path {
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
    let prev_avg_gate_scans = prev_json
        .get("results")
        .and_then(|r| r.get("avg_gate_scans_per_tick"))
        .and_then(|v| v.as_f64());
    let prev_avg_gate_blocked = prev_json
        .get("results")
        .and_then(|r| r.get("avg_gate_blocked_per_tick"))
        .and_then(|v| v.as_f64());
    let prev_avg_cooldown_skipped = prev_json
        .get("results")
        .and_then(|r| r.get("avg_cooldown_skipped_per_tick"))
        .and_then(|v| v.as_f64());
    let prev_avg_supply_funds_blocked = prev_json
        .get("results")
        .and_then(|r| r.get("avg_supply_funds_blocked_per_tick"))
        .and_then(|v| v.as_f64());
    let prev_avg_supply_mp_blocked = prev_json
        .get("results")
        .and_then(|r| r.get("avg_supply_mp_blocked_per_tick"))
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

    /// Inline delta for a byte-valued card whose magnitude is rendered
    /// human-readable (KB/MB/…) so it matches the card value's scale.
    fn delta_bytes_html(current: f64, previous: Option<f64>, abs_threshold: f64) -> String {
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
                    " <span style='color:{};font-size:11px'>{}{} ({:+.1}%)</span>",
                    color,
                    emoji,
                    fmt_bytes_signed(change),
                    pct
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

    /// Render a table cell with an inline per-value byte delta (lower is better),
    /// the byte counterpart of [cell_delta]. The change is rendered
    /// human-readable (B/KB/MB/…) so it matches the cell value's scale. Hidden when
    /// there is no previous value; shown as ≈0 when the change is negligible (<1
    /// byte, or <0.5% of the previous value, so large values don't flag sub-percent
    /// run-to-run noise).
    fn cell_delta_bytes(value: &str, current: f64, previous: Option<f64>) -> String {
        let inner = match previous {
            Some(prev) => {
                let change = current - prev;
                let negligible =
                    change.abs() < 1.0 || (prev.abs() > 1e-9 && (change / prev).abs() < 0.005);
                if negligible {
                    "<div class='delta' style='color:#555'>≈0</div>".to_string()
                } else {
                    let pct = if prev.abs() > 1e-9 {
                        change / prev * 100.0
                    } else {
                        0.0
                    };
                    let color = if change < 0.0 { "#0f0" } else { "#f44" };
                    let arrow = if change < 0.0 { "▼" } else { "▲" };
                    format!(
                        "<div class='delta' style='color:{}'>{arrow}{} ({pct:+.1}%)</div>",
                        color,
                        fmt_bytes_signed(change)
                    )
                }
            }
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
        let pct = if prev.abs() > 1e-9 {
            change / prev * 100.0
        } else {
            0.0
        };
        format!("<div class='delta' style='color:{color}'>{arrow}{val} ({pct:+.1}%)</div>")
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
    let avg_gate_scans_delta = delta_html(avg_gate_scans, prev_avg_gate_scans, "", COUNT);
    let avg_gate_blocked_delta = delta_html(avg_gate_blocked, prev_avg_gate_blocked, "", COUNT);
    let avg_cooldown_skipped_delta =
        delta_html(avg_cooldown_skipped, prev_avg_cooldown_skipped, "", COUNT);
    let avg_supply_funds_blocked_delta = delta_html(
        avg_supply_funds_blocked,
        prev_avg_supply_funds_blocked,
        "",
        COUNT,
    );
    let avg_supply_mp_blocked_delta =
        delta_html(avg_supply_mp_blocked, prev_avg_supply_mp_blocked, "", COUNT);
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

    // Previous detailed sub-part objects, keyed by sub-part name (mirrors
    // `prev_timings`). A report that predates the sub-part (or that was run
    // without `--bench-detailed`) simply yields no deltas for it.
    let prev_detailed: std::collections::HashMap<String, &serde_json::Value> = prev_json
        .get("detailed_timings")
        .and_then(|dt| dt.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|s| {
                    s.get("name")
                        .and_then(|n| n.as_str())
                        .map(|n| (n.to_string(), s))
                })
                .collect()
        })
        .unwrap_or_default();

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
                sorted
                    .iter()
                    .map(|s| {
                        let name = s["name"].as_str().unwrap_or("?");
                        let total = s["total_ms"].as_f64().unwrap_or(0.0);
                        let calls = s["call_count"].as_u64().unwrap_or(0);
                        let avg = s["avg_ms"].as_f64().unwrap_or(0.0);
                        let p99 = s["p99_ms"].as_f64().unwrap_or(0.0);
                        let prev = prev_detailed.get(name);
                        let prev_total: Option<f64> = prev
                            .and_then(|p| p.get("total_ms"))
                            .and_then(|v| v.as_f64());
                        let prev_avg: Option<f64> =
                            prev.and_then(|p| p.get("avg_ms")).and_then(|v| v.as_f64());
                        let prev_calls: Option<f64> = prev
                            .and_then(|p| p.get("call_count"))
                            .and_then(|v| v.as_f64());
                        let prev_p99: Option<f64> =
                            prev.and_then(|p| p.get("p99_ms")).and_then(|v| v.as_f64());
                        // Total cell: flag the sub-part "new" when absent from the previous report.
                        let total_cell = match prev_total {
                            Some(_) => {
                                cell_delta(&format!("{:.1}", total), total, prev_total, "ms", 0.01)
                            }
                            None => format!(
                                "<td><span style='color:#888'>{:.1} <i>new</i></span></td>",
                                total
                            ),
                        };
                        let avg_cell =
                            cell_delta(&format!("{:.3}", avg), avg, prev_avg, "ms", 0.001);
                        let calls_cell =
                            cell_delta(&format!("{}", calls), calls as f64, prev_calls, "", 0.5);
                        let p99_cell =
                            cell_delta(&format!("{:.3}", p99), p99, prev_p99, "ms", 0.001);
                        format!(
                            "<tr><td>{}</td>{total_cell}{avg_cell}{calls_cell}{p99_cell}</tr>",
                            name
                        )
                    })
                    .collect::<Vec<_>>()
                    .join("\n      ")
            }
        } else {
            "<tr><td colspan='5'>No detailed timing data. Use --bench-detailed=system1,system2 to enable.</td></tr>".to_string()
        }
    };

    // Per-system allocation table rows (bench-alloc). Sorted by bytes/tick; a
    // trailing "other / overhead" row shows the share of the per-tick budget NOT
    // attributed to a tick_system!-wrapped system (ECS archetype rebuild, query
    // setup, benchmark AI such as query_all_units / recruit).
    // Previous per-system allocation objects, keyed by system name (mirrors
    // `prev_timings` / `prev_micro`). Field-presence: a report that predates
    // per-system allocs simply yields no deltas. The previous report's own
    // bytes-per-tick total reconstructs its per-system "% Bytes/Tick" share.
    let prev_alloc: std::collections::HashMap<String, &serde_json::Value> = prev_json
        .get("system_allocs")
        .and_then(|sa| sa.get("systems"))
        .and_then(|s| s.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|s| {
                    s.get("name")
                        .and_then(|n| n.as_str())
                        .map(|n| (n.to_string(), s))
                })
                .collect()
        })
        .unwrap_or_default();
    let prev_total_bytes_per_tick: Option<f64> = prev_json
        .get("results")
        .and_then(|r| r.get("bytes_per_tick"))
        .and_then(|v| v.as_f64());
    // A real previous report was supplied (vs. no comparison at all). Only then is
    // the per-system "new" flag meaningful; with no prev report nothing is new.
    let has_prev_report = !prev_json.is_null();

    let alloc_rows = {
        let sa = &results.summary["system_allocs"];
        let systems = sa.get("systems").and_then(|s| s.as_array());
        let total_bytes_per_tick = results.summary["results"]["bytes_per_tick"]
            .as_f64()
            .unwrap_or(0.0);
        match systems {
            Some(systems) if !systems.is_empty() => {
                let mut sorted: Vec<&serde_json::Value> = systems.iter().collect();
                sorted.sort_by(|a, b| {
                    b["bytes_per_tick"]
                        .as_f64()
                        .unwrap_or(0.0)
                        .partial_cmp(&a["bytes_per_tick"].as_f64().unwrap_or(0.0))
                        .unwrap()
                });
                let mut rows: Vec<String> = sorted
                    .iter()
                    .map(|s| {
                        let name = s["name"].as_str().unwrap_or("?");
                        let apt = s["allocs_per_tick"].as_f64().unwrap_or(0.0);
                        let bpt = s["bytes_per_tick"].as_f64().unwrap_or(0.0);
                        let a_c = s["avg_allocs"].as_f64().unwrap_or(0.0);
                        let b_c = s["avg_bytes"].as_f64().unwrap_or(0.0);
                        let calls = s["call_count"].as_u64().unwrap_or(0);
                        let share = if total_bytes_per_tick > 0.0 {
                            bpt / total_bytes_per_tick * 100.0
                        } else {
                            0.0
                        };

                        // Previous per-system values, or None when new / absent.
                        let prev = prev_alloc.get(name);
                        let prev_apt: Option<f64> =
                            prev.and_then(|p| p.get("allocs_per_tick")).and_then(|v| v.as_f64());
                        let prev_bpt: Option<f64> =
                            prev.and_then(|p| p.get("bytes_per_tick")).and_then(|v| v.as_f64());
                        let prev_a_c: Option<f64> =
                            prev.and_then(|p| p.get("avg_allocs")).and_then(|v| v.as_f64());
                        let prev_b_c: Option<f64> =
                            prev.and_then(|p| p.get("avg_bytes")).and_then(|v| v.as_f64());
                        let prev_calls: Option<f64> =
                            prev.and_then(|p| p.get("call_count")).and_then(|v| v.as_f64());
                        // % share reconstructed with the previous report's own total.
                        let prev_share: Option<f64> = prev_bpt.and_then(|pb| {
                            prev_total_bytes_per_tick
                                .filter(|t| *t > 0.0)
                                .map(|t| pb / t * 100.0)
                        });

                        // Name cell: flag a system "new" when a previous report exists
                        // but this system is absent from it (not when there's no prev).
                        let name_cell = if has_prev_report && prev.is_none() {
                            format!("<td>{name} <span style='color:#888'><i>new</i></span></td>")
                        } else {
                            format!("<td>{name}</td>")
                        };
                        let apt_cell = cell_delta(&format!("{:.2}", apt), apt, prev_apt, "", 0.5);
                        let bpt_cell = cell_delta_bytes(&fmt_bytes(bpt), bpt, prev_bpt);
                        let share_cell =
                            cell_delta(&format!("{:.1}%", share), share, prev_share, "pts", 0.5);
                        let ac_cell = cell_delta(&format!("{:.2}", a_c), a_c, prev_a_c, "", 0.5);
                        let bc_cell = cell_delta_bytes(&fmt_bytes(b_c), b_c, prev_b_c);
                        let calls_cell =
                            cell_delta(&format!("{}", calls), calls as f64, prev_calls, "", 0.5);

                        format!(
                            "<tr>{name_cell}{apt_cell}{bpt_cell}{share_cell}{ac_cell}{bc_cell}{calls_cell}</tr>"
                        )
                    })
                    .collect();
                let attributed: f64 = sorted
                    .iter()
                    .map(|s| s["bytes_per_tick"].as_f64().unwrap_or(0.0))
                    .sum();
                let unattributed = total_bytes_per_tick - attributed;
                if unattributed > 0.5 {
                    let un_attr_share = if total_bytes_per_tick > 0.0 {
                        unattributed / total_bytes_per_tick * 100.0
                    } else {
                        0.0
                    };
                    rows.push(format!(
                        "<tr style='background:rgba(255,255,255,0.05)'><td><i>other / overhead</i></td><td>—</td><td>{}</td><td>{:.1}%</td><td>—</td><td>—</td><td>—</td></tr>",
                        fmt_bytes(unattributed), un_attr_share
                    ));
                }
                rows.join("\n      ")
            }
            _ => "<tr><td colspan='7'>No per-system allocation data. Enable --features bench-alloc to populate.</td></tr>".to_string(),
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
    let bench_prev_str: String = match prev_path {
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

    // B0: "Network Phase" section — net_ms sub-phase averages + the
    // idle/movement × grid-clean/grid-dirty split. Field-presence: reports
    // generated before B0 have no "network" summary key and the section
    // degrades to a note; previous-report deltas appear only when the prev
    // report also carried the section.
    let net_section_html = (|| {
        let net = results.summary.get("network")?;
        let sub = net.get("subphase_ms")?;
        let cur = |k: &str| -> f64 { sub.get(k).and_then(|v| v.as_f64()).unwrap_or(0.0) };
        let prev_sub = prev_json.get("network").and_then(|n| n.get("subphase_ms"));
        let prev = |k: &str| -> Option<f64> {
            prev_sub.and_then(|s| s.get(k)).and_then(|v| v.as_f64())
        };
        let splice_str = match net.get("splice").and_then(|v| v.as_bool()) {
            Some(true) => "on",
            Some(false) => "off",
            None => "?",
        };
        let mut html = format!(
            "<h2>Network Phase</h2>\n        <p class='note'>Sub-phases of the per-tick network phase (splice encoder <b>{}</b> in the benchmark). <b>net</b> = the whole tick_system!-wrapped phase (snapshot + delta + encode + baseline commit); the sub-rows are the measured parts, so their sum is ≤ net. <b>send</b> is always 0: the headless benchmark encodes in memory and models no outbox/WS bridge. ms/Tick = average over all {} measured ticks.</p>\n        <table>\n            <thead><tr><th>Sub-phase</th><th>ms/Tick (avg)</th></tr></thead>\n            <tbody>\n      ",
            splice_str, measured_runs_cfg * tick_count.max(1)
        );
        for name in ["net", "snapshot", "delta", "encode", "send"] {
            let value = format!("{:.3}", cur(name));
            html.push_str(&format!("{}\n      ", cell_delta(&value, cur(name), prev(name), "ms", MS)));
        }
        html.push_str("\n            </tbody>\n        </table>");

        if let Some(split) = net.get("split") {
            html.push_str(
                "\n        <h3>Idle / Movement × Grid-Clean / Grid-Dirty</h3>\n        <p class='note'>Per-regime averages over measured ticks. Regime = movers == 0 (idle) vs movers &gt; 0 (movement); grid-dirty = a vision/border grid diff was emitted that tick (influence recompute makes ~29% of ticks grid-dirty even at zero movers). Wire/allocs/bytes are per-tick averages within the regime.</p>\n        <table>\n            <thead><tr><th>Regime</th><th>Ticks</th><th>net ms/Tick</th><th>Wire/Tick</th><th>Unit Updates/Tick</th><th>Allocs/Tick</th><th>Bytes/Tick</th></tr></thead>\n            <tbody>",
            );
            let regimes: [(&str, &str); 4] = [
                ("Idle · grid-clean", "idle_grid_clean"),
                ("Idle · grid-dirty", "idle_grid_dirty"),
                ("Movement · grid-clean", "movement_grid_clean"),
                ("Movement · grid-dirty", "movement_grid_dirty"),
            ];
            for (label, key) in regimes {
                let r = match split.get(key) {
                    Some(r) => r,
                    None => continue,
                };
                let n = r.get("ticks").and_then(|v| v.as_u64()).unwrap_or(0);
                let net_ms = r.get("net_ms").and_then(|v| v.as_f64()).unwrap_or(0.0);
                let wire = r.get("net_wire_bytes").and_then(|v| v.as_f64()).unwrap_or(0.0);
                let updates = r
                    .get("net_unit_updates")
                    .and_then(|v| v.as_f64())
                    .unwrap_or(0.0);
                let allocs = r.get("allocs").and_then(|v| v.as_f64()).unwrap_or(0.0);
                let bytes = r.get("bytes").and_then(|v| v.as_f64()).unwrap_or(0.0);
                html.push_str(&format!(
                    "\n      <tr><td>{}</td><td>{}</td><td>{:.3}</td><td>{}</td><td>{:.1}</td><td>{:.1}</td><td>{}</td></tr>",
                    label,
                    n,
                    net_ms,
                    fmt_bytes(wire),
                    updates,
                    allocs,
                    fmt_bytes(bytes)
                ));
            }
            html.push_str("\n            </tbody>\n        </table>");
        }
        Some(html)
    })()
    .unwrap_or_else(|| {
        "<h2>Network Phase</h2>\n        <p class='note'>No network sub-phase data (report predates B0, or the timing features are off).</p>".to_string()
    });

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

    // Allocation per-tick chart data (only rendered when bench-alloc produced data).
    let allocs_series: Vec<f64> = results
        .tick_data
        .iter()
        .map(|td| td.allocs as f64)
        .collect();
    let bytes_series: Vec<f64> = results.tick_data.iter().map(|td| td.bytes as f64).collect();
    let avg_alloc_size_series: Vec<f64> = results
        .tick_data
        .iter()
        .map(|td| {
            if td.allocs > 0 {
                td.bytes as f64 / td.allocs as f64
            } else {
                0.0
            }
        })
        .collect();
    let max_alloc_series: Vec<f64> = results
        .tick_data
        .iter()
        .map(|td| td.max_alloc as f64)
        .collect();
    let has_alloc_data =
        allocs_series.iter().any(|&v| v > 0.0) || bytes_series.iter().any(|&v| v > 0.0);
    let allocs_series_json =
        serde_json::to_string(&allocs_series).unwrap_or_else(|_| "[]".to_string());
    let bytes_series_json =
        serde_json::to_string(&bytes_series).unwrap_or_else(|_| "[]".to_string());
    let avg_alloc_size_series_json =
        serde_json::to_string(&avg_alloc_size_series).unwrap_or_else(|_| "[]".to_string());
    let max_alloc_series_json =
        serde_json::to_string(&max_alloc_series).unwrap_or_else(|_| "[]".to_string());

    // Previous alloc series (if --bench-prev given and previous report has alloc data).
    let prev_allocs_series: Option<Vec<f64>> = prev_json
        .get("allocs_per_tick_series")
        .and_then(|v| v.as_array())
        .map(|arr| arr.iter().filter_map(|v| v.as_f64()).collect());
    let prev_bytes_series: Option<Vec<f64>> = prev_json
        .get("bytes_per_tick_series")
        .and_then(|v| v.as_array())
        .map(|arr| arr.iter().filter_map(|v| v.as_f64()).collect());
    let prev_avg_alloc_size_series: Option<Vec<f64>> = prev_json
        .get("avg_alloc_size_series")
        .and_then(|v| v.as_array())
        .map(|arr| arr.iter().filter_map(|v| v.as_f64()).collect());
    let prev_max_alloc_series: Option<Vec<f64>> = prev_json
        .get("max_alloc_per_tick_series")
        .and_then(|v| v.as_array())
        .map(|arr| arr.iter().filter_map(|v| v.as_f64()).collect());
    let prev_allocs_json = prev_allocs_series
        .as_ref()
        .map(|v| serde_json::to_string(v).unwrap_or_else(|_| "[]".to_string()))
        .unwrap_or_else(|| "null".to_string());
    let prev_bytes_json = prev_bytes_series
        .as_ref()
        .map(|v| serde_json::to_string(v).unwrap_or_else(|_| "[]".to_string()))
        .unwrap_or_else(|| "null".to_string());
    let prev_avg_alloc_size_json = prev_avg_alloc_size_series
        .as_ref()
        .map(|v| serde_json::to_string(v).unwrap_or_else(|_| "[]".to_string()))
        .unwrap_or_else(|| "null".to_string());
    let prev_max_alloc_json = prev_max_alloc_series
        .as_ref()
        .map(|v| serde_json::to_string(v).unwrap_or_else(|_| "[]".to_string()))
        .unwrap_or_else(|| "null".to_string());
    let prev_alloc_note = if prev_allocs_series.is_some() || prev_bytes_series.is_some() {
        "Grey dashed = previous benchmark."
    } else {
        ""
    };

    // Avg alloc/bytes per tick for summary cards (with prev comparison).
    let avg_allocs_tick = results.summary["results"]["allocs_per_tick"]
        .as_f64()
        .unwrap_or(0.0);
    let avg_bytes_tick = results.summary["results"]["bytes_per_tick"]
        .as_f64()
        .unwrap_or(0.0);
    let avg_alloc_size_tick = results.summary["results"]["avg_alloc_size"]
        .as_f64()
        .unwrap_or(0.0);
    let max_alloc_size_tick = results.summary["results"]["max_alloc_size"]
        .as_u64()
        .unwrap_or(0);
    let prev_avg_allocs = prev_json
        .get("results")
        .and_then(|r| r.get("allocs_per_tick"))
        .and_then(|v| v.as_f64());
    let prev_avg_bytes = prev_json
        .get("results")
        .and_then(|r| r.get("bytes_per_tick"))
        .and_then(|v| v.as_f64());
    let prev_avg_alloc_size = prev_json
        .get("results")
        .and_then(|r| r.get("avg_alloc_size"))
        .and_then(|v| v.as_f64());
    let prev_max_alloc_size = prev_json
        .get("results")
        .and_then(|r| r.get("max_alloc_size"))
        .and_then(|v| v.as_u64());
    let avg_allocs_delta = delta_html(avg_allocs_tick, prev_avg_allocs, "", COUNT);
    let avg_bytes_delta = delta_bytes_html(avg_bytes_tick, prev_avg_bytes, 0.5);
    let avg_alloc_size_delta = delta_bytes_html(avg_alloc_size_tick, prev_avg_alloc_size, 0.01);
    let max_alloc_size_delta = delta_bytes_html(
        max_alloc_size_tick as f64,
        prev_max_alloc_size.map(|v| v as f64),
        1.0,
    );

    // Stack size table HTML (always present, zero cost).
    let stack_json = &results.summary["stack"];
    let stack_rows: Vec<String> = stack_json
        .as_object()
        .map(|obj| {
            obj.iter()
                .map(|(name, size)| {
                    format!(
                        "<tr><td>{}</td><td>{}</td></tr>",
                        name,
                        fmt_bytes(size.as_u64().unwrap_or(0) as f64)
                    )
                })
                .collect()
        })
        .unwrap_or_default();
    let stack_table_html = if stack_rows.is_empty() {
        String::new()
    } else {
        format!(
            "<h3>Stack-Resident Scratch Sizes</h3>\n         <table>\n         <thead><tr><th>Struct</th><th>Size</th></tr></thead>\n         <tbody>{}\n         </tbody>\n         </table>",
            stack_rows.join("\n         ")
        )
    };

    // Alloc chart HTML section (only when bench-alloc data is present).
    let alloc_chart_html = if has_alloc_data {
        let bytes_human = fmt_bytes(avg_bytes_tick);
        let alloc_size_human = fmt_bytes(avg_alloc_size_tick);
        let max_alloc_human = fmt_bytes(max_alloc_size_tick as f64);
        format!(
            "<h2>📦 Allocation Profile</h2>\n         <p class='note'>Allocation activity per tick (averaged across {} runs). {}</p>\n         <div class='metrics'>\n         <div class='metric'><div class='value'>{:.1}{}</div><div class='label'>Avg Allocs/Tick</div></div>\n         <div class='metric'><div class='value'>{}{}</div><div class='label'>Avg Bytes/Tick</div></div>\n         <div class='metric'><div class='value'>{}{}</div><div class='label'>Avg Alloc Size</div></div>\n         <div class='metric'><div class='value'>{}{}</div><div class='label'>Biggest Single Alloc</div></div>\n         </div>\n         <div style='display:flex;gap:20px;flex-wrap:wrap;'>\n         <div class='chart-container' style='flex:1;min-width:300px;'><canvas id='allocChart' height='120'></canvas></div>\n         <div class='chart-container' style='flex:1;min-width:300px;'><canvas id='bytesChart' height='120'></canvas></div>\n         </div>\n         <div style='display:flex;gap:20px;flex-wrap:wrap;margin-top:10px;'>\n         <div class='chart-container' style='flex:1;min-width:300px;'><canvas id='avgAllocSizeChart' height='120'></canvas></div>\n         <div class='chart-container' style='flex:1;min-width:300px;'><canvas id='maxAllocChart' height='120'></canvas></div>\n         </div>",
            measured_runs_str,
            prev_alloc_note,
            avg_allocs_tick, avg_allocs_delta,
            bytes_human, avg_bytes_delta,
            alloc_size_human, avg_alloc_size_delta,
            max_alloc_human, max_alloc_size_delta,
        )
    } else {
        String::new()
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
            <tr><td>Map</td><td>{map_desc}</td></tr>
            <tr><td>Cities</td><td>{cities}</td></tr>
            <tr><td>Players</td><td>{bench_players} (cities round-robin)</td></tr>
            <tr><td>Units per city</td><td>{units_per_city}</td></tr>
            <tr><td>Max recruits/event</td><td>{max_recruits_per_event} (every 100 ticks, spread evenly across cities)</td></tr>
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
            <div class="metric"><div class="value">{avg_gate_scans:.0}{avg_gate_scans_delta}</div><div class="label">Gate Scans/Tick</div></div>
            <div class="metric"><div class="value">{avg_gate_blocked:.0}{avg_gate_blocked_delta}</div><div class="label">Gate-Blocked/Tick</div></div>
            <div class="metric"><div class="value">{avg_cooldown_skipped:.0}{avg_cooldown_skipped_delta}</div><div class="label">Cooldown-Skipped/Tick</div></div>
            <div class="metric"><div class="value">{avg_supply_funds_blocked:.0}{avg_supply_funds_blocked_delta}</div><div class="label">Supply-Blocked (Funds)</div></div>
            <div class="metric"><div class="value">{avg_supply_mp_blocked:.0}{avg_supply_mp_blocked_delta}</div><div class="label">Supply-Blocked (MP)</div></div>
        </div>
        <p class="note">*Deaths/Tick derived: (initial + recruited − remaining) / ticks — assumes units only leave via combat death. Gate counters: occupancy-gate scans per tick in movement; gate-blocked = movers whose target was occupied; cooldown-skipped = blocked movers in their 3-tick backoff (W1-④). Supply-Blocked = units whose supply delivery was denied by the player's funds / manpower pool (FCFS gate; see city:unit ratio caveat).</p>

        <h2>Per-Tick Timing</h2>
        <div class="chart-container"><canvas id="tickChart" height="120"></canvas></div>

        <h3>Overhead Breakdown Per Tick</h3>
        <div class="chart-container"><canvas id="overheadChart" height="120"></canvas></div>
        <p class="note">Spikes in overhead correlate with entity spawning (recruit events every 100 ticks). ECS archetype rebuild is the primary cost.</p>

        <h3>Entity, Unit, Mover & Combat Counts Over Time</h3>
        <div class="chart-container"><canvas id="entityChart" height="120"></canvas></div>

        {alloc_chart_html}

        {stack_table_html}

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

        <h2>Per-System Allocations</h2>
        <p class="note">Allocation budget attributed to each system (bench-alloc only). <b>per tick</b> = the system's measured-tick total ÷ {ticks}; <b>per call</b> = average over its invocations. <i>other / overhead</i> is everything outside the tick_system!-wrapped systems (ECS archetype rebuild, query setup, benchmark AI like query_all_units / recruit).</p>
        <table>
            <thead><tr>
                <th>System</th><th>Allocs/Tick</th><th>Bytes/Tick</th><th>% Bytes/Tick</th>
                <th>Allocs/Call</th><th>Bytes/Call</th><th>Calls</th>
            </tr></thead>
            <tbody>
      {alloc_rows}
            </tbody>
        </table>

        {net_section_html}

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

        // Human-readable bytes helper for chart axes and tooltips
        function fmtBytes(v) {{
            if (v >= 1073741824) return (v / 1073741824).toFixed(2) + ' GB';
            if (v >= 1048576) return (v / 1048576).toFixed(2) + ' MB';
            if (v >= 1024) return (v / 1024).toFixed(1) + ' KB';
            return v.toFixed(0) + ' B';
        }}

        // Allocation per-tick charts (only rendered when bench-alloc data is present)
        if (document.getElementById('allocChart')) {{
            const allocsData = {allocs_series_json};
            const prevAllocs = {prev_allocs_json};
            const allocDatasets = [{{
                label: 'Allocs/Tick', data: allocsData,
                borderColor: '#e74c3c', backgroundColor: 'rgba(231,76,60,0.1)',
                borderWidth: 1.5, pointRadius: 0, fill: true, tension: 0.3,
            }}];
            if (prevAllocs && prevAllocs.length > 0) {{
                allocDatasets.push({{
                    label: 'Previous', data: prevAllocs,
                    borderColor: '#888', borderWidth: 1, pointRadius: 0,
                    borderDash: [4, 4], fill: false, tension: 0.3,
                }});
            }}
            new Chart(document.getElementById('allocChart'), {{
                type: 'line',
                data: {{ labels: allocsData.map((_, i) => i), datasets: allocDatasets }},
                options: {{
                    responsive: true, plugins: {{ title: {{ display: true, text: 'Allocs/Tick', color: '#e74c3c' }}, legend: {{ labels: {{ color: '#aaa' }} }} }},
                    scales: {{
                        x: {{ title: {{ display: true, text: 'Tick' }}, ticks: {{ color: '#666' }}, grid: {{ color: '#333' }} }},
                        y: {{ title: {{ display: true, text: 'Allocs' }}, ticks: {{ color: '#666' }}, grid: {{ color: '#333' }} }}
                    }},
                    animation: {{ duration: 0 }},
                }}
            }});
        }}
        if (document.getElementById('bytesChart')) {{
            const bytesData = {bytes_series_json};
            const prevBytes = {prev_bytes_json};
            const bytesDatasets = [{{
                label: 'Bytes/Tick', data: bytesData,
                borderColor: '#3498db', backgroundColor: 'rgba(52,152,219,0.1)',
                borderWidth: 1.5, pointRadius: 0, fill: true, tension: 0.3,
            }}];
            if (prevBytes && prevBytes.length > 0) {{
                bytesDatasets.push({{
                    label: 'Previous', data: prevBytes,
                    borderColor: '#888', borderWidth: 1, pointRadius: 0,
                    borderDash: [4, 4], fill: false, tension: 0.3,
                }});
            }}
            new Chart(document.getElementById('bytesChart'), {{
                type: 'line',
                data: {{ labels: bytesData.map((_, i) => i), datasets: bytesDatasets }},
                options: {{
                    responsive: true, plugins: {{
                        title: {{ display: true, text: 'Bytes/Tick', color: '#3498db' }},
                        legend: {{ labels: {{ color: '#aaa' }} }},
                        tooltip: {{ callbacks: {{ label: function(ctx) {{ return ctx.dataset.label + ': ' + fmtBytes(ctx.parsed.y); }} }} }}
                    }},
                    scales: {{
                        x: {{ title: {{ display: true, text: 'Tick' }}, ticks: {{ color: '#666' }}, grid: {{ color: '#333' }} }},
                        y: {{
                            title: {{ display: true, text: 'Bytes' }}, ticks: {{ color: '#666', callback: function(v) {{ return fmtBytes(v); }} }},
                            grid: {{ color: '#333' }}
                        }}
                    }},
                    animation: {{ duration: 0 }},
                }}
            }});
        }}
        if (document.getElementById('avgAllocSizeChart')) {{
            const avgSizeData = {avg_alloc_size_series_json};
            const prevAvgSize = {prev_avg_alloc_size_json};
            const avgSizeDatasets = [{{
                label: 'Avg Alloc Size/Tick', data: avgSizeData,
                borderColor: '#f39c12', backgroundColor: 'rgba(243,156,18,0.1)',
                borderWidth: 1.5, pointRadius: 0, fill: true, tension: 0.3,
            }}];
            if (prevAvgSize && prevAvgSize.length > 0) {{
                avgSizeDatasets.push({{
                    label: 'Previous', data: prevAvgSize,
                    borderColor: '#888', borderWidth: 1, pointRadius: 0,
                    borderDash: [4, 4], fill: false, tension: 0.3,
                }});
            }}
            new Chart(document.getElementById('avgAllocSizeChart'), {{
                type: 'line',
                data: {{ labels: avgSizeData.map((_, i) => i), datasets: avgSizeDatasets }},
                options: {{
                    responsive: true, plugins: {{
                        title: {{ display: true, text: 'Avg Allocation Size per Tick', color: '#f39c12' }},
                        legend: {{ labels: {{ color: '#aaa' }} }},
                        tooltip: {{ callbacks: {{ label: function(ctx) {{ return ctx.dataset.label + ': ' + fmtBytes(ctx.parsed.y); }} }} }}
                    }},
                    scales: {{
                        x: {{ title: {{ display: true, text: 'Tick' }}, ticks: {{ color: '#666' }}, grid: {{ color: '#333' }} }},
                        y: {{
                            title: {{ display: true, text: 'Avg Size' }}, ticks: {{ color: '#666', callback: function(v) {{ return fmtBytes(v); }} }},
                            grid: {{ color: '#333' }}
                        }}
                    }},
                    animation: {{ duration: 0 }},
                }}
            }});
        }}
        if (document.getElementById('maxAllocChart')) {{
            const maxAllocData = {max_alloc_series_json};
            const prevMaxAlloc = {prev_max_alloc_json};
            const maxAllocDatasets = [{{
                label: 'Biggest Alloc/Tick', data: maxAllocData,
                borderColor: '#9b59b6', backgroundColor: 'rgba(155,89,182,0.1)',
                borderWidth: 1.5, pointRadius: 0, fill: true, tension: 0.3,
            }}];
            if (prevMaxAlloc && prevMaxAlloc.length > 0) {{
                maxAllocDatasets.push({{
                    label: 'Previous', data: prevMaxAlloc,
                    borderColor: '#888', borderWidth: 1, pointRadius: 0,
                    borderDash: [4, 4], fill: false, tension: 0.3,
                }});
            }}
            new Chart(document.getElementById('maxAllocChart'), {{
                type: 'line',
                data: {{ labels: maxAllocData.map((_, i) => i), datasets: maxAllocDatasets }},
                options: {{
                    responsive: true, plugins: {{
                        title: {{ display: true, text: 'Biggest Single Alloc per Tick', color: '#9b59b6' }},
                        legend: {{ labels: {{ color: '#aaa' }} }},
                        tooltip: {{ callbacks: {{ label: function(ctx) {{ return ctx.dataset.label + ': ' + fmtBytes(ctx.parsed.y); }} }} }}
                    }},
                    scales: {{
                        x: {{ title: {{ display: true, text: 'Tick' }}, ticks: {{ color: '#666' }}, grid: {{ color: '#333' }} }},
                        y: {{
                            title: {{ display: true, text: 'Size' }}, ticks: {{ color: '#666', callback: function(v) {{ return fmtBytes(v); }} }},
                            grid: {{ color: '#333' }}
                        }}
                    }},
                    animation: {{ duration: 0 }},
                }}
            }});
        }}
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
        map_desc = map_desc,
        ticks = tick_count,
        avg_system = avg_system,
        avg_system_delta = avg_system_delta,
        avg_movers = avg_movers,
        avg_movers_delta = avg_movers_delta,
        avg_combats = avg_combats,
        avg_combats_delta = avg_combats_delta,
        avg_gate_scans = avg_gate_scans,
        avg_gate_scans_delta = avg_gate_scans_delta,
        avg_gate_blocked = avg_gate_blocked,
        avg_gate_blocked_delta = avg_gate_blocked_delta,
        avg_cooldown_skipped = avg_cooldown_skipped,
        avg_cooldown_skipped_delta = avg_cooldown_skipped_delta,
        avg_supply_funds_blocked = avg_supply_funds_blocked,
        avg_supply_funds_blocked_delta = avg_supply_funds_blocked_delta,
        avg_supply_mp_blocked = avg_supply_mp_blocked,
        avg_supply_mp_blocked_delta = avg_supply_mp_blocked_delta,
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
        alloc_rows = alloc_rows,
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
        allocs_series_json = allocs_series_json,
        bytes_series_json = bytes_series_json,
        avg_alloc_size_series_json = avg_alloc_size_series_json,
        max_alloc_series_json = max_alloc_series_json,
        prev_allocs_json = prev_allocs_json,
        prev_bytes_json = prev_bytes_json,
        prev_avg_alloc_size_json = prev_avg_alloc_size_json,
        prev_max_alloc_json = prev_max_alloc_json,
        alloc_chart_html = alloc_chart_html,
        stack_table_html = stack_table_html,
        net_section_html = net_section_html,
        bench_players = BENCH_PLAYERS,
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

/// Average numeric per-system allocation fields across measured runs.
///
/// `total_allocs`/`total_bytes`/`call_count` are summed (they are cumulative
/// over each run's ticks), then divided by the run count to yield the average
/// per-run totals. `avg_allocs`/`avg_bytes` are recomputed from the averaged
/// totals (not averaged-of-averages) so the per-call figures stay consistent.
fn average_allocs(entries: &[serde_json::Value]) -> serde_json::Value {
    let mut out = entries.first().cloned().unwrap_or(serde_json::Value::Null);
    if out.is_null() || entries.is_empty() {
        return out;
    }
    let n = entries.len() as f64;
    let mut sum_total_allocs = 0u64;
    let mut sum_total_bytes = 0u64;
    let mut sum_calls = 0u64;
    for v in entries {
        sum_total_allocs += v["total_allocs"].as_u64().unwrap_or(0);
        sum_total_bytes += v["total_bytes"].as_u64().unwrap_or(0);
        sum_calls += v["call_count"].as_u64().unwrap_or(0);
    }
    let avg_total_allocs = sum_total_allocs as f64 / n;
    let avg_total_bytes = sum_total_bytes as f64 / n;
    let avg_calls = sum_calls as f64 / n;
    out["total_allocs"] = serde_json::json!(avg_total_allocs);
    out["total_bytes"] = serde_json::json!(avg_total_bytes);
    // Per-run average call count (NOT the 3-run sum). total_allocs/total_bytes
    // above are per-run averages, so call_count must be too for the table to be
    // self-consistent: allocs_per_tick / avg_allocs == call_count / ticks ==
    // calls-per-tick. (Previously stored the raw sum, inflating call_count 3×
    // and deflating the derived allocs/call + bytes/call by the same factor.)
    out["call_count"] = serde_json::json!(avg_calls.round() as u64);
    out["avg_allocs"] = serde_json::json!(if avg_calls > 0.0 {
        avg_total_allocs / avg_calls
    } else {
        0.0
    });
    out["avg_bytes"] = serde_json::json!(if avg_calls > 0.0 {
        avg_total_bytes / avg_calls
    } else {
        0.0
    });
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

#[cfg(test)]
mod tests {
    use super::*;

    /// Minimal but valid `generate_html_report` summary with a given per-system
    /// alloc array and per-tick bytes total.
    fn alloc_summary(systems: serde_json::Value, total_bytes_per_tick: f64) -> serde_json::Value {
        serde_json::json!({
            "config": {
                "cities": 12, "units_per_city": 5, "initial_units": 10000,
                "max_recruits_per_event": 2000,
                "map_size": 1000, "total_units_initial": 10000, "total_units_recruited": 11680,
                "ticks": 500, "seed": 42, "measured_runs": 3, "warmup_ticks": 200,
                "bench_prev": null, "detailed_systems": []
            },
            "results": {
                "wall_time_ms": 1000.0, "avg_system_ms": 10.0, "avg_overhead_ms": 0.5,
                "units_remaining": 8775, "avg_movers_per_tick": 8655.0, "avg_combats_per_tick": 585.0,
                "allocs_per_tick": 15784.0, "bytes_per_tick": total_bytes_per_tick,
                "avg_alloc_size": 1280.0, "max_alloc_size": 1880000.0
            },
            "latency": { "avg_ms": 16.8, "min_ms": 5.0, "max_ms": 40.0,
                "p50_ms": 12.0, "p90_ms": 20.0, "p95_ms": 25.0, "p99_ms": 30.0 },
            "overhead": { "p50_ms": 0.4, "p95_ms": 0.8, "p99_ms": 1.2 },
            "system_timings": [],
            "detailed_timings": [],
            "system_allocs": { "ticks": 500, "systems": systems },
            "microbenchmarks": { "benchmarks": [] }
        })
    }

    /// The `<tr>…</tr>` segment (up to the next `<tr>`) that contains `needle`.
    fn row_containing<'a>(html: &'a str, needle: &str) -> Option<&'a str> {
        html.split("<tr>").find(|seg| seg.contains(needle))
    }

    #[test]
    fn recruit_event_plan_caps_and_spreads_evenly() {
        let mut cfg = BenchmarkConfig::default(); // units_per_city 5 → 2/city, cap 2000
                                                  // Uncapped: 2/city × 12 cities = 24 < 2000.
        assert_eq!(recruit_event_plan(&cfg, 12), (2, 0));
        // Cap binds: 2/city × 1500 cities = 3000 > 2000 → 2000/1500 = 1 + 500 extra.
        assert_eq!(recruit_event_plan(&cfg, 1500), (1, 500));
        // Cap below city count: only the first `cap` cities recruit one each.
        cfg.max_recruits_per_event = 3;
        assert_eq!(recruit_event_plan(&cfg, 12), (0, 3));
        // units_per_city 1 → (1/2).max(1) = 1/city.
        cfg = BenchmarkConfig::default();
        cfg.units_per_city = 1;
        assert_eq!(recruit_event_plan(&cfg, 12), (1, 0));
        // No cities → nothing.
        assert_eq!(recruit_event_plan(&cfg, 0), (0, 0));
    }

    #[test]
    fn alloc_table_shows_prev_deltas_and_new_flag() {
        // Previous report: beta_sys heavy (7.14 MB/tick), alpha_sys light (5 MB/tick).
        let prev = alloc_summary(
            serde_json::json!([
                { "name": "beta_sys",  "allocs_per_tick": 1172.0, "bytes_per_tick": 7140000.0,
                  "avg_allocs": 390.67, "avg_bytes": 2380000.0, "call_count": 1500 },
                { "name": "alpha_sys", "allocs_per_tick": 572.0, "bytes_per_tick": 5000000.0,
                  "avg_allocs": 190.67, "avg_bytes": 1666666.0, "call_count": 1500 }
            ]),
            15000000.0,
        );

        // Current: beta_sys shrank (green), alpha_sys grew (red), gamma_sys added.
        let current = alloc_summary(
            serde_json::json!([
                { "name": "beta_sys",  "allocs_per_tick": 500.0, "bytes_per_tick": 3000000.0,
                  "avg_allocs": 333.33, "avg_bytes": 1000000.0, "call_count": 1500 },
                { "name": "alpha_sys", "allocs_per_tick": 700.0, "bytes_per_tick": 7350000.0,
                  "avg_allocs": 233.33, "avg_bytes": 2450000.0, "call_count": 1500 },
                { "name": "gamma_sys", "allocs_per_tick": 0.2, "bytes_per_tick": 100.0,
                  "avg_allocs": 0.2, "avg_bytes": 100.0, "call_count": 500 }
            ]),
            20000000.0,
        );

        // Write the previous report as a fake HTML file (JSON lives in <pre>).
        let path = std::env::temp_dir().join("war_of_dots_alloc_test_prev.html");
        std::fs::write(
            &path,
            format!("<html><body><pre>{}</pre></body></html>", prev),
        )
        .expect("write temp prev report");

        let results = BenchmarkResults {
            config: BenchmarkConfig::default(),
            tick_data: Vec::new(),
            summary: current,
        };
        let html = generate_html_report(&results, Some(path.to_str().unwrap()));
        let _ = std::fs::remove_file(&path);

        assert!(
            html.contains("Per-System Allocations"),
            "alloc table header missing"
        );

        // beta_sys shrank 7.14→3.00 MB/tick: green + downward arrow + ≈-58%.
        let beta = row_containing(&html, "beta_sys").expect("beta_sys row");
        assert!(
            beta.contains("#0f0"),
            "beta_sys should be green (decrease): {beta}"
        );
        assert!(
            beta.contains("▼"),
            "beta_sys should show a downward arrow: {beta}"
        );
        assert!(
            beta.contains("-58.0%"),
            "beta_sys bytes delta ≈ -58%: {beta}"
        );

        // alpha_sys grew 5.00→7.35 MB/tick: red + upward arrow + ≈+47%.
        let alpha = row_containing(&html, "alpha_sys").expect("alpha_sys row");
        assert!(
            alpha.contains("#f44"),
            "alpha_sys should be red (increase): {alpha}"
        );
        assert!(
            alpha.contains("▲"),
            "alpha_sys should show an upward arrow: {alpha}"
        );
        assert!(
            alpha.contains("+47.0%"),
            "alpha_sys bytes delta ≈ +47%: {alpha}"
        );

        // gamma_sys is new: flagged, and carries no delta arrows.
        let gamma = row_containing(&html, "gamma_sys").expect("gamma_sys row");
        assert!(
            gamma.contains("<i>new</i>"),
            "gamma_sys should be flagged new: {gamma}"
        );
        assert!(
            !gamma.contains("▲") && !gamma.contains("▼"),
            "gamma_sys should have no delta arrows: {gamma}"
        );
    }

    #[test]
    fn alloc_table_has_no_deltas_without_prev() {
        // No previous report → values render, but no delta arrows anywhere in the table.
        let current = alloc_summary(
            serde_json::json!([
                { "name": "beta_sys", "allocs_per_tick": 500.0, "bytes_per_tick": 3000000.0,
                  "avg_allocs": 333.33, "avg_bytes": 1000000.0, "call_count": 1500 }
            ]),
            3000000.0,
        );
        let results = BenchmarkResults {
            config: BenchmarkConfig::default(),
            tick_data: Vec::new(),
            summary: current,
        };
        let html = generate_html_report(&results, None);
        let row = row_containing(&html, "beta_sys").expect("beta_sys row");
        assert!(
            !row.contains("▲") && !row.contains("▼"),
            "no prev → no delta arrows: {row}"
        );
        assert!(!row.contains("new"), "no prev → no 'new' flag: {row}");
    }

    #[test]
    fn lattice_capacity_scales_with_map_size() {
        // capacity = floor(map_size / MIN_UNIT_DISTANCE)². With MIN_UNIT_DISTANCE
        // = 8.0: a 1000×1000 map holds floor(125)² = 15 625; a 100×100 map holds
        // floor(12.5)² = 144. The stress config (10 000 units on 1000) fits;
        // 20 000 on 1000 does not (would fail fast). The exact-value asserts
        // below also pin the formula against a future constant change.
        let cap_1000 = lattice_capacity(1000);
        assert_eq!(cap_1000, 15_625);
        assert!(10_000u64 <= cap_1000, "stress config must fit the 1000-map");
        assert!(20_000u64 > cap_1000, "over-capacity must be rejected");

        let cap_100 = lattice_capacity(100);
        assert!(cap_100 < cap_1000, "capacity must grow with map size");
        assert_eq!(cap_100, 144);

        // The plan's concrete infeasible example: 10 000 on 200×200 →
        // capacity floor(200/8)² = 25² = 625 < 10 000 → must fail fast.
        let cap_200 = lattice_capacity(200);
        assert_eq!(cap_200, 625);
        assert!(
            10_000u64 > cap_200,
            "10 000 units cannot lay out non-overlapping on 200×200 (capacity 625)"
        );
    }
}
