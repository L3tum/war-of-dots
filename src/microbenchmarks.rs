//! Microbenchmarks for War of Dots ECS game systems.
//!
//! Gated behind `--features microbenchmarks`. Run programmatically via
//! `run_microbenchmarks()` which returns `Vec<MicrobenchmarkResult>`.
//!
//! Each benchmark is isolated and measures a specific operation or system
//! at a defined scale. Uses a custom benchmarking harness for programmatic
//! access to timing statistics (Criterion 0.5's BenchmarkGroup::finish()
//! returns unit, so statistics cannot be extracted programmatically).

use std::collections::HashMap;
use std::hint::black_box;
use std::time::{Duration, Instant};

use crate::config::CityUpgradeSettings;
use crate::game::components::*;
use crate::game::economy::{City, UnitType};
use crate::game::game::{Game, GameSettings, TickContext};
use crate::game::systems::spatial_hash::SpatialHash;
use crate::game::unit::{Equipment, Terrain};
use crate::game::world_ext::WorldExt;
use hecs::World;

// ============================================================================
// Result Type
// ============================================================================

/// Single microbenchmark result entry.
#[derive(Debug, Clone)]
pub struct MicrobenchmarkResult {
    pub id: String,
    pub group: String,
    /// Mean time in nanoseconds.
    pub mean_ns: f64,
    /// Standard deviation in nanoseconds.
    pub std_dev_ns: f64,
    /// Median time in nanoseconds.
    pub median_ns: f64,
    /// Number of iterations.
    pub n: u32,
}

// ============================================================================
// Benchmark Harness
// ============================================================================

/// Configuration for a single benchmark run.
#[derive(Clone, Copy)]
struct BenchConfig {
    /// Number of warmup iterations.
    warmup: usize,
    /// Number of timed iterations.
    timed: usize,
}

impl Default for BenchConfig {
    fn default() -> Self {
        Self {
            warmup: 5,
            timed: 25,
        }
    }
}

impl BenchConfig {
    /// Higher iteration count for fast operations (< 20 µs) to beat clock noise.
    fn fast() -> Self {
        Self {
            warmup: 10,
            timed: 50,
        }
    }
}

/// Compute basic statistics from a list of durations.
fn compute_stats(durations: &[Duration]) -> (f64, f64, f64) {
    let n = durations.len() as f64;
    let ns: Vec<f64> = durations.iter().map(|d| d.as_nanos() as f64).collect();
    let sum: f64 = ns.iter().sum();
    let mean = sum / n;

    let variance = ns.iter().map(|v| (v - mean).powi(2)).sum::<f64>() / n;
    let std_dev = variance.sqrt();

    let mut sorted = ns.clone();
    sorted.sort_by(|a, b| a.partial_cmp(b).unwrap());
    let median = if n % 2.0 == 0.0 {
        let mid = (n / 2.0) as usize;
        (sorted[mid - 1] + sorted[mid]) / 2.0
    } else {
        sorted[(n / 2.0) as usize]
    };

    (mean, std_dev, median)
}

/// Run a benchmark with warmup + timed iterations.
fn run_bench(
    config: BenchConfig,
    setup: impl Fn(),
    bench_fn: impl FnMut(),
) -> MicrobenchmarkResult {
    // Warmup
    for _ in 0..config.warmup {
        setup();
    }

    // Timed iterations
    let mut durations = Vec::with_capacity(config.timed);
    let mut f = bench_fn;
    for _ in 0..config.timed {
        setup();
        let start = Instant::now();
        f();
        let elapsed = start.elapsed();
        durations.push(elapsed);
    }

    let (mean, std_dev, median) = compute_stats(&durations);
    MicrobenchmarkResult {
        mean_ns: mean,
        std_dev_ns: std_dev,
        median_ns: median,
        n: durations.len() as u32,
        ..MicrobenchmarkResult::default_empty()
    }
}

/// Variant of [run_bench] that runs `setup` outside the timer.
///
/// Use this for benchmarks where fixture construction (World::new, spawning
/// entities, etc.) is orders of magnitude more expensive than the operation
/// being measured. The fixture is recreated each iteration to ensure a fair
/// comparison (the real game also has per-tick archetype pressure).
fn run_bench_with_setup<T, F, S>(
    config: BenchConfig,
    mut setup: S,
    mut bench_fn: F,
) -> MicrobenchmarkResult
where
    S: FnMut() -> T,
    F: FnMut(&mut T),
{
    // Warmup
    let mut fixture = setup();
    for _ in 0..config.warmup {
        bench_fn(&mut fixture);
    }

    // Timed iterations — fixture creation is NOT timed
    let mut durations = Vec::with_capacity(config.timed);
    for _ in 0..config.timed {
        let mut f = setup();
        let start = Instant::now();
        bench_fn(&mut f);
        let elapsed = start.elapsed();
        durations.push(elapsed);
    }

    let (mean, std_dev, median) = compute_stats(&durations);
    MicrobenchmarkResult {
        mean_ns: mean,
        std_dev_ns: std_dev,
        median_ns: median,
        n: durations.len() as u32,
        ..MicrobenchmarkResult::default_empty()
    }
}

/// Cumulative benchmark for sub-10µs operations.
///
/// Runs the inner operation `repeats` times per iteration to rise above
/// clock-resolution noise, then divides by `repeats`.
fn run_bench_cumulative(
    config: BenchConfig,
    repeats: usize,
    setup: impl Fn(),
    bench_fn: impl FnMut(),
) -> MicrobenchmarkResult {
    let mut inner_fn = bench_fn;
    let result = run_bench(config, setup, || {
        for _ in 0..repeats {
            inner_fn();
        }
    });
    MicrobenchmarkResult {
        mean_ns: result.mean_ns / repeats as f64,
        std_dev_ns: result.std_dev_ns / repeats as f64,
        median_ns: result.median_ns / repeats as f64,
        n: result.n,
        ..result
    }
}

/// Cumulative variant of [run_bench_with_setup].
///
/// Fixture is created once per iteration (outside timer), then the inner
/// operation runs `repeats` times inside the same fixture. Result divided
/// by `repeats`.
fn run_bench_with_setup_cumulative<T, F>(
    config: BenchConfig,
    repeats: usize,
    setup: impl Fn() -> T,
    bench_fn: F,
) -> MicrobenchmarkResult
where
    F: Fn(&mut T),
{
    let result = run_bench_with_setup(config, setup, |f: &mut T| {
        for _ in 0..repeats {
            bench_fn(f);
        }
    });
    MicrobenchmarkResult {
        mean_ns: result.mean_ns / repeats as f64,
        std_dev_ns: result.std_dev_ns / repeats as f64,
        median_ns: result.median_ns / repeats as f64,
        n: result.n,
        ..result
    }
}

impl MicrobenchmarkResult {
    fn default_empty() -> Self {
        Self {
            id: String::new(),
            group: String::new(),
            mean_ns: 0.0,
            std_dev_ns: 0.0,
            median_ns: 0.0,
            n: 0,
        }
    }
}

// ============================================================================
// Test Fixtures
// ============================================================================

/// Create a valid dummy entity with the given id.
/// hecs::Entity requires non-zero generation bits (high 32 bits).
fn make_dummy_entity(id: u64) -> hecs::Entity {
    hecs::Entity::from_bits((1u64 << 32) | (id & 0xFFFF_FFFF)).unwrap()
}

/// Basic infantry unit type for benchmarks.
fn bench_unit_type() -> UnitType {
    UnitType {
        name: "infantry".to_string(),
        symbol: "I".to_string(),
        men: 1000,
        equipment: vec![Equipment {
            name: "rifle".to_string(),
            count: 100,
            max_count: 100,
            cost_per_item: 2.5,
        }],
        soft_attack: 5.0,
        soft_defense: 4.0,
        hard_attack: 3.0,
        hard_defense: 2.0,
        recruitment_cost: 200,
        max_organisation: 100.0,
        reinforcement_rate: 5.0,
        speed: 1.0,
        base_attrition: 0.5,
        fighting_attrition: 1.0,
        supply_buffer_percent: 10,
        supply_rate: 1,
        max_supply: 2000,
    }
}

/// Default game settings for benchmarks.
fn bench_settings() -> GameSettings {
    GameSettings {
        starting_funds: 1000,
        initial_manpower: 10_000,
        funds_per_city: 1.0,
        recruitment_rate: 1.0,
        game_speed: 1.0,
        use_standard_ruleset: true,
        unit_types: vec![bench_unit_type()],
        city_upgrade_settings: CityUpgradeSettings::default(),
        water_speed: 0.5,
    }
}

/// Create a minimal ECS world with a world_bundle entity.
fn make_world(width: u32, height: u32, num_players: usize) -> (World, hecs::Entity) {
    let mut world = World::new();
    let mut funds = HashMap::new();
    let mut cities: Vec<City> = Vec::new();

    for i in 0..num_players {
        funds.insert(format!("player_{i}"), 1000u32);
    }

    // Spawn a city near each player for meaningful benchmarks
    for i in 0..num_players.min(4) {
        let x = 3.0 + i as f32 * 4.0;
        let y = 3.0;
        cities.push(City::new(&format!("city_{i}"), (x, y)));
    }

    let terrain = vec![vec![Terrain::Plains; height as usize]; width as usize];
    let world_entity = world.spawn_world(
        terrain,
        width,
        height,
        CityUpgradeSettings::default(),
        funds,
        cities,
    );

    (world, world_entity)
}

/// Spawn N units in a world with evenly spaced positions.
fn spawn_n_units(
    world: &mut World,
    _world_entity: hecs::Entity,
    unit_type: &UnitType,
    count: usize,
) -> Vec<hecs::Entity> {
    let mut entities = Vec::with_capacity(count);
    for i in 0..count {
        let x = 5.0 + (i % 10) as f32 * 3.0;
        let y = 5.0 + (i / 10) as f32 * 3.0;
        let owner = if i % 2 == 0 { "player_0" } else { "player_1" };
        let e = world.spawn_unit(unit_type, owner, (x, y));
        entities.push(e);
    }
    entities
}

/// Spawn N units all at the same position (dense cluster for combat benchmarks).
fn spawn_dense_units(
    world: &mut World,
    _world_entity: hecs::Entity,
    unit_type: &UnitType,
    count: usize,
    pos: (f32, f32),
) -> Vec<hecs::Entity> {
    let mut entities = Vec::with_capacity(count);
    for i in 0..count {
        let owner = if i % 2 == 0 { "player_0" } else { "player_1" };
        let e = world.spawn_unit(unit_type, owner, pos);
        entities.push(e);
    }
    entities
}

/// Give units paths so movement system has work to do.
fn add_paths_to_entities(world: &mut World, entities: &[hecs::Entity]) {
    for (i, &entity) in entities.iter().enumerate() {
        let base_x = 5.0 + (i % 10) as f32 * 3.0;
        let base_y = 5.0 + (i / 10) as f32 * 3.0;
        let target_x = base_x + if i % 2 == 0 { 2.0 } else { -2.0 };
        let target_y = base_y + 2.0;
        let path = UnitPath {
            path: vec![(target_x, base_y), (target_x, target_y)],
        };
        let _ = world.insert_one(entity, (path,));
    }
}

/// Make half the units have NearbyEnemies pointing to the other half.
fn setup_nearby_enemies(world: &mut World, entities: &[hecs::Entity]) {
    let count = entities.len();
    for i in 0..count {
        let is_player_0 = i % 2 == 0;
        let enemies: Vec<hecs::Entity> = entities
            .iter()
            .copied()
            .enumerate()
            .filter(|&(j, _)| j % 2 != is_player_0 as usize)
            .map(|(_, e)| e)
            .take(5)
            .collect();
        let nearby = NearbyEnemies {
            entities: enemies,
            initialized: true,
        };
        let _ = world.insert_one(entities[i], (nearby,));
    }
}

/// Create a full Game instance with N units recruited.
fn make_game_with_units(units_per_city: usize) -> Game {
    use crate::map::map::MapConfig;

    let map_cfg = MapConfig {
        width: 50,
        height: 50,
        cities: vec![(5.0, 5.0), (40.0, 5.0), (5.0, 40.0), (40.0, 40.0)],
        terrain_data: None,
    };
    let map = crate::map::map::Map::from_config(&map_cfg);
    let settings = bench_settings();
    let mut game = Game::new(map, settings, 4);

    let cities = game.query_cities();
    let ut = bench_unit_type();
    for city in &cities {
        let owner = city.owner.clone().unwrap_or_else(|| "player_0".to_string());
        for _ in 0..units_per_city {
            let _ = game.recruit_unit(&city.id, &ut, &owner);
        }
    }

    game.start();
    game
}

// ============================================================================
// Fixture Types (for run_bench_with_setup)
// ============================================================================

/// Pre-built fixture for movement benchmarks.
struct MovementFixture {
    world: World,
    world_entity: hecs::Entity,
}

impl MovementFixture {
    fn new_empty(width: u32, height: u32, num_players: usize) -> Self {
        let (world, world_entity) = make_world(width, height, num_players);
        Self {
            world,
            world_entity,
        }
    }

    fn with_units(
        width: u32,
        height: u32,
        num_players: usize,
        unit_type: &UnitType,
        count: usize,
        with_paths: bool,
    ) -> Self {
        let (mut world, world_entity) = make_world(width, height, num_players);
        let entities = spawn_n_units(&mut world, world_entity, unit_type, count);
        if with_paths {
            add_paths_to_entities(&mut world, &entities);
        }
        Self {
            world,
            world_entity,
        }
    }
}

/// Pre-built fixture for combat benchmarks.
struct CombatFixture {
    world: World,
    world_entity: hecs::Entity,
}

impl CombatFixture {
    fn new_dense(
        width: u32,
        height: u32,
        num_players: usize,
        unit_type: &UnitType,
        count: usize,
    ) -> Self {
        let (mut world, world_entity) = make_world(width, height, num_players);
        let entities = spawn_dense_units(&mut world, world_entity, unit_type, count, (10.0, 10.0));
        setup_nearby_enemies(&mut world, &entities);
        Self {
            world,
            world_entity,
        }
    }
}

/// Pre-built fixture for supply benchmarks.
struct SupplyFixture {
    world: World,
    world_entity: hecs::Entity,
}

impl SupplyFixture {
    fn new(
        width: u32,
        height: u32,
        num_players: usize,
        unit_type: &UnitType,
        count: usize,
    ) -> Self {
        let (mut world, world_entity) = make_world(width, height, num_players);
        spawn_n_units(&mut world, world_entity, unit_type, count);
        Self {
            world,
            world_entity,
        }
    }
}

/// Pre-built fixture for spatialty benchmarks.
struct SpatialtyFixture {
    world: World,
    world_entity: hecs::Entity,
    entity_ids: Vec<hecs::Entity>,
}

impl SpatialtyFixture {
    fn new_movers(
        width: u32,
        height: u32,
        num_players: usize,
        unit_type: &UnitType,
        count: usize,
    ) -> Self {
        let (mut world, world_entity) = make_world(width, height, num_players);
        let entity_ids = spawn_n_units(&mut world, world_entity, unit_type, count);
        Self {
            world,
            world_entity,
            entity_ids,
        }
    }
}

/// Pre-built fixture for influence benchmarks.
struct InfluenceFixture {
    world: World,
    world_entity: hecs::Entity,
}

impl InfluenceFixture {
    fn new(
        width: u32,
        height: u32,
        num_players: usize,
        unit_type: &UnitType,
        count: usize,
    ) -> Self {
        let (mut world, world_entity) = make_world(width, height, num_players);
        spawn_n_units(&mut world, world_entity, unit_type, count);
        Self {
            world,
            world_entity,
        }
    }
}

/// Pre-built fixture for city capture benchmarks.
struct CityCaptureFixture {
    world: World,
    world_entity: hecs::Entity,
}

impl CityCaptureFixture {
    fn new(num_players: usize, unit_type: &UnitType, count: usize) -> Self {
        let (mut world, world_entity) = make_world(20, 20, num_players);
        spawn_n_units(&mut world, world_entity, unit_type, count);
        Self {
            world,
            world_entity,
        }
    }
}

/// Pre-built fixture for encirclement benchmarks.
struct EncirclementFixture {
    world: World,
    world_entity: hecs::Entity,
}

impl EncirclementFixture {
    fn new(num_players: usize, unit_type: &UnitType, count: usize) -> Self {
        let (mut world, world_entity) = make_world(30, 30, num_players);
        spawn_n_units(&mut world, world_entity, unit_type, count);
        Self {
            world,
            world_entity,
        }
    }
}

/// Pre-built fixture for surrender benchmarks.
struct SurrenderFixture {
    world: World,
    world_entity: hecs::Entity,
}

impl SurrenderFixture {
    fn new(total: usize, below_threshold: usize) -> Self {
        let (mut world, world_entity) = make_world(20, 20, 2);
        let ut = bench_unit_type();
        spawn_n_units(&mut world, world_entity, &ut, total - below_threshold);
        for i in 0..below_threshold {
            let x = 10.0 + i as f32;
            let y = 10.0;
            let e = world.spawn_unit(&ut, "player_0", (x, y));
            world
                .insert_one(
                    e,
                    (UnitStats {
                        men: 1.0,
                        max_men: 1000.0,
                        organisation: 50.0,
                        max_organisation: 100.0,
                    },),
                )
                .ok();
        }
        Self {
            world,
            world_entity,
        }
    }
}

/// Pre-built fixture for death cleanup benchmarks.
struct DeathCleanupFixture {
    world: World,
    dead_entities: Vec<hecs::Entity>,
}

impl DeathCleanupFixture {
    fn new(total: usize, dead_count: usize) -> Self {
        let (mut world, world_entity) = make_world(30, 30, 2);
        let ut = bench_unit_type();
        let all_entities = spawn_n_units(&mut world, world_entity, &ut, total);
        for &e in all_entities.iter().take(dead_count) {
            world
                .insert_one(
                    e,
                    (UnitStats {
                        men: 0.0,
                        max_men: 1000.0,
                        organisation: 50.0,
                        max_organisation: 100.0,
                    },),
                )
                .ok();
        }
        let dead_entities: Vec<_> = all_entities[0..dead_count].to_vec();
        Self {
            world,
            dead_entities,
        }
    }
}

// ============================================================================
// Spatial Hash Benchmarks (8) — unchanged
// ============================================================================

fn bench_spatial_hash() -> Vec<MicrobenchmarkResult> {
    let cfg = BenchConfig::default();
    let mut results = Vec::new();

    // Insert at different scales
    for n in [100, 1000, 5000] {
        let id = format!("insert_{}", n);
        let result = run_bench(
            cfg,
            || {},
            || {
                let mut hash = SpatialHash::new();
                for i in 0..n {
                    hash.insert(
                        make_dummy_entity(i as u64),
                        (i % 100) as f32,
                        (i / 100) as f32,
                    );
                }
                black_box(&hash);
            },
        );
        results.push(MicrobenchmarkResult {
            id: format!("spatial_hash/{}", id),
            group: "spatial_hash".to_string(),
            ..result
        });
    }

    // Query sparse
    let result = run_bench(
        cfg,
        || {},
        || {
            let mut hash = SpatialHash::new();
            for i in 0..100 {
                hash.insert(
                    make_dummy_entity(i as u64),
                    (i % 50) as f32 * 5.0,
                    (i / 50) as f32 * 5.0,
                );
            }
            let hash = hash;
            let mut out = Vec::new();
            for i in 0..10 {
                hash.query_nearby_into(i as f32 * 3.0 + 1.0, i as f32 * 3.0 + 1.0, &mut out);
            }
            black_box(out.len());
        },
    );
    results.push(MicrobenchmarkResult {
        id: "spatial_hash/query_sparse".to_string(),
        group: "spatial_hash".to_string(),
        ..result
    });

    // Query dense
    let result = run_bench(
        cfg,
        || {},
        || {
            let mut hash = SpatialHash::new();
            for i in 0..1000 {
                hash.insert(
                    make_dummy_entity(i as u64),
                    10.0 + (i % 10) as f32 * 0.1,
                    10.0 + (i / 10) as f32 * 0.1,
                );
            }
            let hash = hash;
            let mut out = Vec::new();
            for i in 0..10 {
                hash.query_nearby_into(10.0 + i as f32 * 0.5, 10.0 + i as f32 * 0.5, &mut out);
            }
            black_box(out.len());
        },
    );
    results.push(MicrobenchmarkResult {
        id: "spatial_hash/query_dense".to_string(),
        group: "spatial_hash".to_string(),
        ..result
    });

    // Buffer reuse
    let result = run_bench(
        cfg,
        || {},
        || {
            let mut hash = SpatialHash::new();
            for i in 0..200 {
                hash.insert(
                    make_dummy_entity(i as u64),
                    (i % 20) as f32 * 3.0,
                    (i / 20) as f32 * 3.0,
                );
            }
            let hash = hash;
            let mut out = Vec::with_capacity(100);
            for i in 0..10 {
                hash.query_nearby_into(i as f32 * 4.0 + 2.0, i as f32 * 4.0 + 2.0, &mut out);
            }
            black_box(out.len());
        },
    );
    results.push(MicrobenchmarkResult {
        id: "spatial_hash/query_buffer_reuse".to_string(),
        group: "spatial_hash".to_string(),
        ..result
    });

    // Remove + reinsert (spatialty pattern)
    let result = run_bench(
        cfg,
        || {},
        || {
            let mut hash = SpatialHash::new();
            let positions: Vec<_> = (0..100)
                .map(|i| {
                    let e = make_dummy_entity(i as u64);
                    let x = (i % 10) as f32 * 3.0;
                    let y = (i / 10) as f32 * 3.0;
                    hash.insert(e, x, y);
                    (e, x, y)
                })
                .collect();
            for (e, old_x, old_y) in &positions {
                hash.remove(*e, *old_x, *old_y);
                hash.insert(*e, old_x + 2.0, old_y + 2.0);
            }
            black_box(positions.len());
        },
    );
    results.push(MicrobenchmarkResult {
        id: "spatial_hash/remove_reinsert".to_string(),
        group: "spatial_hash".to_string(),
        ..result
    });

    // Clear
    let result = run_bench(
        cfg,
        || {},
        || {
            let mut hash = SpatialHash::new();
            for i in 0..1000 {
                hash.insert(
                    make_dummy_entity(i as u64),
                    (i % 50) as f32 * 2.0,
                    (i / 50) as f32 * 2.0,
                );
            }
            hash.clear();
            black_box(0);
        },
    );
    results.push(MicrobenchmarkResult {
        id: "spatial_hash/clear".to_string(),
        group: "spatial_hash".to_string(),
        ..result
    });

    results
}

// ============================================================================
// ECS Benchmarks (8) — renamed query_positions 100/1000 → 5000/10000
// ============================================================================

fn bench_ecs() -> Vec<MicrobenchmarkResult> {
    let cfg = BenchConfig::default();
    let cfg_fast = BenchConfig::fast();
    let mut results = Vec::new();
    let ut = bench_unit_type();

    // Spawn single entity — cumulative to beat clock noise
    let result = run_bench_with_setup_cumulative(
        cfg_fast,
        5000,
        || {
            let (world, we) = make_world(20, 20, 2);
            (world, we)
        },
        |f: &mut (World, hecs::Entity)| {
            f.0.spawn_unit(&ut, "player_0", (10.0, 10.0));
            black_box(1);
        },
    );
    results.push(MicrobenchmarkResult {
        id: "ecs/spawn_single".to_string(),
        group: "ecs".to_string(),
        ..result
    });

    // Spawn 100 units — setup outside timer
    let result = run_bench_with_setup(
        cfg,
        || {
            let (world, we) = make_world(20, 20, 2);
            (world, we)
        },
        |f: &mut (World, hecs::Entity)| {
            spawn_n_units(&mut f.0, f.1, &ut, 100);
            black_box(100);
        },
    );
    results.push(MicrobenchmarkResult {
        id: "ecs/spawn_100".to_string(),
        group: "ecs".to_string(),
        ..result
    });

    // Spawn 1000 units — setup outside timer
    let result = run_bench_with_setup(
        cfg,
        || {
            let (world, we) = make_world(30, 30, 2);
            (world, we)
        },
        |f: &mut (World, hecs::Entity)| {
            spawn_n_units(&mut f.0, f.1, &ut, 1000);
            black_box(1000);
        },
    );
    results.push(MicrobenchmarkResult {
        id: "ecs/spawn_1000".to_string(),
        group: "ecs".to_string(),
        ..result
    });

    // Despawn single entity — cumulative for accuracy
    let result = run_bench_cumulative(
        cfg_fast,
        5000,
        || {},
        || {
            let (mut world, _) = make_world(20, 20, 2);
            let entity = world.spawn_unit(&ut, "player_0", (10.0, 10.0));
            world.despawn(entity).ok();
            black_box(0);
        },
    );
    results.push(MicrobenchmarkResult {
        id: "ecs/despawn".to_string(),
        group: "ecs".to_string(),
        ..result
    });

    // Query positions at 5000 scale — cumulative, query is sub-µs
    let result = run_bench_with_setup_cumulative(
        cfg_fast,
        10000,
        || {
            let (mut world, we) = make_world(30, 30, 2);
            spawn_n_units(&mut world, we, &ut, 5000);
            (world, we)
        },
        |f: &mut (World, hecs::Entity)| {
            let count = f.0.query::<(hecs::Entity, &UnitPosition)>().iter().count();
            black_box(count);
        },
    );
    results.push(MicrobenchmarkResult {
        id: "ecs/query_positions_5000".to_string(),
        group: "ecs".to_string(),
        ..result
    });

    // Query positions at 10000 scale — cumulative, query is sub-µs
    let result = run_bench_with_setup_cumulative(
        cfg_fast,
        10000,
        || {
            let (mut world, we) = make_world(40, 40, 2);
            spawn_n_units(&mut world, we, &ut, 10000);
            (world, we)
        },
        |f: &mut (World, hecs::Entity)| {
            let count = f.0.query::<(hecs::Entity, &UnitPosition)>().iter().count();
            black_box(count);
        },
    );
    results.push(MicrobenchmarkResult {
        id: "ecs/query_positions_10000".to_string(),
        group: "ecs".to_string(),
        ..result
    });

    // Insert component (NearbyEnemies) to existing entity — cumulative for accuracy
    let result = run_bench_cumulative(
        cfg_fast,
        5000,
        || {},
        || {
            let (mut world, _) = make_world(20, 20, 2);
            let entity = world.spawn_unit(&ut, "player_0", (10.0, 10.0));
            let nearby = NearbyEnemies {
                entities: vec![],
                initialized: true,
            };
            world.insert_one(entity, (nearby,)).ok();
            black_box(0);
        },
    );
    results.push(MicrobenchmarkResult {
        id: "ecs/insert_component".to_string(),
        group: "ecs".to_string(),
        ..result
    });

    // Spawn single entity raw (no world setup) — cumulative for noise reduction
    let result = run_bench_cumulative(
        cfg_fast,
        10000,
        || {},
        || {
            let mut world = World::new();
            world.spawn(());
            black_box(0);
        },
    );
    results.push(MicrobenchmarkResult {
        id: "ecs/spawn_raw_empty".to_string(),
        group: "ecs".to_string(),
        ..result
    });

    results
}

// ============================================================================
// System-Level Benchmarks (16) — bumped scales, added movement_2000
// ============================================================================

fn bench_systems() -> Vec<MicrobenchmarkResult> {
    let cfg = BenchConfig::default();
    let cfg_fast = BenchConfig::fast();
    let mut results = Vec::new();
    let ut = bench_unit_type();
    let settings = bench_settings();

    // --- Movement ---

    // Movement with 0 units (overhead only) — cumulative to beat clock noise
    let result = run_bench_with_setup_cumulative(
        cfg_fast,
        10000,
        || MovementFixture::new_empty(20, 20, 2),
        |f: &mut MovementFixture| {
            let mut scratch = crate::game::systems::movement::MovementScratch::default();
            crate::game::systems::movement::run(
                &mut f.world,
                f.world_entity,
                &settings,
                &mut scratch,
            );
            black_box(scratch.movers.len());
        },
    );
    results.push(MicrobenchmarkResult {
        id: "systems/movement_empty".to_string(),
        group: "systems".to_string(),
        ..result
    });

    // Movement with 500 units on paths
    let result = run_bench_with_setup(
        cfg,
        || MovementFixture::with_units(30, 30, 2, &ut, 500, true),
        |f: &mut MovementFixture| {
            let mut scratch = crate::game::systems::movement::MovementScratch::default();
            crate::game::systems::movement::run(
                &mut f.world,
                f.world_entity,
                &settings,
                &mut scratch,
            );
            black_box(scratch.movers.len());
        },
    );
    results.push(MicrobenchmarkResult {
        id: "systems/movement_500".to_string(),
        group: "systems".to_string(),
        ..result
    });

    // Movement with 2000 units on paths (stress test archetype traversal)
    let result = run_bench_with_setup(
        cfg,
        || MovementFixture::with_units(50, 50, 2, &ut, 2000, true),
        |f: &mut MovementFixture| {
            let mut scratch = crate::game::systems::movement::MovementScratch::default();
            crate::game::systems::movement::run(
                &mut f.world,
                f.world_entity,
                &settings,
                &mut scratch,
            );
            black_box(scratch.movers.len());
        },
    );
    results.push(MicrobenchmarkResult {
        id: "systems/movement_2000".to_string(),
        group: "systems".to_string(),
        ..result
    });

    // --- Spatialty ---

    // Spatialty with 100 movers (setup outside timer)
    let result = run_bench_with_setup(
        cfg,
        || SpatialtyFixture::new_movers(30, 30, 2, &ut, 100),
        |f: &mut SpatialtyFixture| {
            let movers: Vec<_> = f
                .entity_ids
                .iter()
                .copied()
                .map(|e| {
                    let old = UnitPosition { x: 10.0, y: 10.0 };
                    let new = UnitPosition { x: 12.0, y: 12.0 };
                    (e, old, new)
                })
                .collect();
            let mut spatial = SpatialHash::new();
            for &e in &f.entity_ids {
                if let Ok(pos) = f.world.get::<&UnitPosition>(e) {
                    spatial.insert(e, pos.x, pos.y);
                }
            }
            crate::game::systems::spatialty::run(
                &mut f.world,
                f.world_entity,
                &mut spatial,
                &movers,
                true,
                true,
            );
            black_box(movers.len());
        },
    );
    results.push(MicrobenchmarkResult {
        id: "systems/spatialty_100".to_string(),
        group: "systems".to_string(),
        ..result
    });

    // Spatialty with 200 movers (realistic scale — game has ~200 movers/tick)
    let result = run_bench_with_setup(
        cfg,
        || SpatialtyFixture::new_movers(30, 30, 2, &ut, 200),
        |f: &mut SpatialtyFixture| {
            let movers: Vec<_> = f
                .entity_ids
                .iter()
                .copied()
                .map(|e| {
                    let old = UnitPosition { x: 10.0, y: 10.0 };
                    let new = UnitPosition { x: 12.0, y: 12.0 };
                    (e, old, new)
                })
                .collect();
            let mut spatial = SpatialHash::new();
            for &e in &f.entity_ids {
                if let Ok(pos) = f.world.get::<&UnitPosition>(e) {
                    spatial.insert(e, pos.x, pos.y);
                }
            }
            crate::game::systems::spatialty::run(
                &mut f.world,
                f.world_entity,
                &mut spatial,
                &movers,
                true,
                true,
            );
            black_box(movers.len());
        },
    );
    results.push(MicrobenchmarkResult {
        id: "systems/spatialty_200".to_string(),
        group: "systems".to_string(),
        ..result
    });

    // Spatialty with 500 movers (stress test hash density + NearbyEnemies updates)
    let result = run_bench_with_setup(
        cfg,
        || SpatialtyFixture::new_movers(40, 40, 2, &ut, 500),
        |f: &mut SpatialtyFixture| {
            let movers: Vec<_> = f
                .entity_ids
                .iter()
                .copied()
                .map(|e| {
                    let old = UnitPosition { x: 10.0, y: 10.0 };
                    let new = UnitPosition { x: 12.0, y: 12.0 };
                    (e, old, new)
                })
                .collect();
            let mut spatial = SpatialHash::new();
            for &e in &f.entity_ids {
                if let Ok(pos) = f.world.get::<&UnitPosition>(e) {
                    spatial.insert(e, pos.x, pos.y);
                }
            }
            crate::game::systems::spatialty::run(
                &mut f.world,
                f.world_entity,
                &mut spatial,
                &movers,
                true,
                true,
            );
            black_box(movers.len());
        },
    );
    results.push(MicrobenchmarkResult {
        id: "systems/spatialty_500".to_string(),
        group: "systems".to_string(),
        ..result
    });

    // --- Combat ---

    // Combat with ~50 engaged pairs
    let result = run_bench_with_setup(
        cfg,
        || CombatFixture::new_dense(20, 20, 2, &ut, 100),
        |f: &mut CombatFixture| {
            let mut scratch = crate::game::systems::combat::CombatScratch::new();
            crate::game::systems::combat::run(
                &mut f.world,
                f.world_entity,
                &mut scratch,
                &settings,
            );
            black_box(scratch.engaged_pairs());
        },
    );
    results.push(MicrobenchmarkResult {
        id: "systems/combat_50pairs".to_string(),
        group: "systems".to_string(),
        ..result
    });

    // Combat with ~200 engaged pairs
    let result = run_bench_with_setup(
        cfg,
        || CombatFixture::new_dense(30, 30, 2, &ut, 400),
        |f: &mut CombatFixture| {
            let mut scratch = crate::game::systems::combat::CombatScratch::new();
            crate::game::systems::combat::run(
                &mut f.world,
                f.world_entity,
                &mut scratch,
                &settings,
            );
            black_box(scratch.engaged_pairs());
        },
    );
    results.push(MicrobenchmarkResult {
        id: "systems/combat_200pairs".to_string(),
        group: "systems".to_string(),
        ..result
    });

    // --- Supply ---

    // Supply with 200 units
    let result = run_bench_with_setup(
        cfg,
        || SupplyFixture::new(30, 30, 2, &ut, 200),
        |f: &mut SupplyFixture| {
            crate::game::systems::supply::run(&mut f.world, f.world_entity, &settings);
            black_box(200);
        },
    );
    results.push(MicrobenchmarkResult {
        id: "systems/supply_200".to_string(),
        group: "systems".to_string(),
        ..result
    });

    // Supply with 500 units (stress test supply range queries)
    let result = run_bench_with_setup(
        cfg,
        || SupplyFixture::new(40, 40, 2, &ut, 500),
        |f: &mut SupplyFixture| {
            crate::game::systems::supply::run(&mut f.world, f.world_entity, &settings);
            black_box(500);
        },
    );
    results.push(MicrobenchmarkResult {
        id: "systems/supply_500".to_string(),
        group: "systems".to_string(),
        ..result
    });

    // --- Water ---

    // Water system with 0 units (overhead only — most ticks have no water transitions)
    let result = run_bench_with_setup_cumulative(
        cfg_fast,
        50000,
        || {
            let (world, world_entity) = make_world(20, 20, 2);
            (world, world_entity)
        },
        |f: &mut (World, hecs::Entity)| {
            crate::game::systems::water::run(&mut f.0, f.1);
            black_box(0);
        },
    );
    results.push(MicrobenchmarkResult {
        id: "systems/water_empty".to_string(),
        group: "systems".to_string(),
        ..result
    });

    // --- Influence ---

    // Influence with 100 units (runs every 3rd tick in the real game)
    let result = run_bench_with_setup(
        cfg,
        || InfluenceFixture::new(30, 30, 2, &ut, 100),
        |f: &mut InfluenceFixture| {
            let ctx = TickContext::build(&f.world, f.world_entity).unwrap();
            crate::game::systems::influence::run(&mut f.world, &ctx, 1);
            black_box(100);
        },
    );
    results.push(MicrobenchmarkResult {
        id: "systems/influence_100".to_string(),
        group: "systems".to_string(),
        ..result
    });

    // Influence with 500 units (stress test per-player grid computation)
    let result = run_bench_with_setup(
        cfg,
        || InfluenceFixture::new(40, 40, 2, &ut, 500),
        |f: &mut InfluenceFixture| {
            let ctx = TickContext::build(&f.world, f.world_entity).unwrap();
            crate::game::systems::influence::run(&mut f.world, &ctx, 1);
            black_box(500);
        },
    );
    results.push(MicrobenchmarkResult {
        id: "systems/influence_500".to_string(),
        group: "systems".to_string(),
        ..result
    });

    // --- Surrender ---

    // Surrender with 100 units (10 below threshold) — cumulative to beat clock noise
    let result = run_bench_with_setup_cumulative(
        cfg_fast,
        5000,
        || SurrenderFixture::new(100, 10),
        |f: &mut SurrenderFixture| {
            crate::game::systems::surrender::run(&mut f.world, f.world_entity);
            black_box(100);
        },
    );
    results.push(MicrobenchmarkResult {
        id: "systems/surrender_100".to_string(),
        group: "systems".to_string(),
        ..result
    });

    // --- Death Cleanup ---

    // Death cleanup with 50 dead entities from 200 total
    let result = run_bench_with_setup(
        cfg,
        || DeathCleanupFixture::new(200, 50),
        |f: &mut DeathCleanupFixture| {
            let mut spatial = SpatialHash::new();
            let mut entity_reverse: HashMap<hecs::Entity, String> = HashMap::new();
            crate::game::systems::death_cleanup::run(
                &mut f.world,
                &mut spatial,
                f.dead_entities.clone(),
                &mut entity_reverse,
            );
            black_box(f.dead_entities.len());
        },
    );
    results.push(MicrobenchmarkResult {
        id: "systems/death_cleanup_50".to_string(),
        group: "systems".to_string(),
        ..result
    });

    // --- City Capture ---

    // City capture with 4 cities, 100 units
    let result = run_bench_with_setup(
        cfg,
        || CityCaptureFixture::new(4, &ut, 100),
        |f: &mut CityCaptureFixture| {
            let mut stats: HashMap<String, crate::game::game::PlayerGameStats> = HashMap::new();
            for i in 0..4 {
                stats.insert(format!("player_{i}"), Default::default());
            }
            crate::game::systems::city_capture::run(&mut f.world, f.world_entity, &mut stats);
            black_box(stats.len());
        },
    );
    results.push(MicrobenchmarkResult {
        id: "systems/city_capture_100".to_string(),
        group: "systems".to_string(),
        ..result
    });

    // --- Encirclement ---

    // Encirclement with 100 units spread across map
    let result = run_bench_with_setup(
        cfg,
        || EncirclementFixture::new(4, &ut, 100),
        |f: &mut EncirclementFixture| {
            let ctx = TickContext::build(&f.world, f.world_entity).unwrap();
            crate::game::systems::encirclement::run(&mut f.world, &ctx, 1);
            black_box(100);
        },
    );
    results.push(MicrobenchmarkResult {
        id: "systems/encirclement_100".to_string(),
        group: "systems".to_string(),
        ..result
    });

    // --- Economy ---

    // Economy with 20 cities — cumulative, economy is extremely fast (sub-µs)
    let result = run_bench_with_setup_cumulative(
        cfg_fast,
        50000,
        || {
            let (world, world_entity) = make_world(20, 20, 20);
            (world, world_entity)
        },
        |f: &mut (World, hecs::Entity)| {
            crate::game::systems::economy::run(&mut f.0, f.1, settings.funds_per_city);
            black_box(20);
        },
    );
    results.push(MicrobenchmarkResult {
        id: "systems/economy_20cities".to_string(),
        group: "systems".to_string(),
        ..result
    });

    // --- Full Tick ---
    // These use run_bench (not run_bench_with_setup) because game creation
    // is part of the one-off cost. We measure total setup + first tick.

    // Full tick with ~100 units (25 per city × 4 cities)
    let result = run_bench(
        cfg,
        || {},
        || {
            let mut game = make_game_with_units(25);
            game.tick_game();
            black_box(game.tick_number());
        },
    );
    results.push(MicrobenchmarkResult {
        id: "systems/full_tick_100".to_string(),
        group: "systems".to_string(),
        ..result
    });

    // Full tick with ~200 units (50 per city × 4 cities)
    let result = run_bench(
        cfg,
        || {},
        || {
            let mut game = make_game_with_units(50);
            game.tick_game();
            black_box(game.tick_number());
        },
    );
    results.push(MicrobenchmarkResult {
        id: "systems/full_tick_200".to_string(),
        group: "systems".to_string(),
        ..result
    });

    // Full tick with ~500 units (125 per city × 4 cities)
    let result = run_bench(
        cfg,
        || {},
        || {
            let mut game = make_game_with_units(125);
            game.tick_game();
            black_box(game.tick_number());
        },
    );
    results.push(MicrobenchmarkResult {
        id: "systems/full_tick_500".to_string(),
        group: "systems".to_string(),
        ..result
    });

    results
}

// ============================================================================
// Map + Settings Benchmarks (6) — higher iteration counts
// ============================================================================

fn bench_map_settings() -> Vec<MicrobenchmarkResult> {
    let cfg = BenchConfig::default();
    let cfg_fast = BenchConfig::fast();
    let mut results = Vec::new();

    // Map creation at different scales
    for (w, h, name) in [(200u32, 200u32, "200x200"), (500u32, 500u32, "500x500")] {
        let id = format!("create_{}", name);
        let result = run_bench(
            cfg,
            || {},
            || {
                let config = crate::map::map::MapConfig {
                    width: w,
                    height: h,
                    cities: vec![(10.0, 10.0), (w as f32 - 10.0, 10.0)],
                    terrain_data: None,
                };
                crate::map::map::Map::from_config(&config);
                black_box(0);
            },
        );
        results.push(MicrobenchmarkResult {
            id: format!("map/{}", id),
            group: "map".to_string(),
            ..result
        });
    }

    // Terrain lookup (100x100 region)
    let result = run_bench(
        cfg,
        || {},
        || {
            let config = crate::map::map::MapConfig {
                width: 200,
                height: 200,
                cities: vec![],
                terrain_data: None,
            };
            let map = crate::map::map::Map::from_config(&config);
            for x in 0..100u32 {
                for y in 0..100u32 {
                    let _ = map.terrain(x, y);
                }
            }
            black_box(0);
        },
    );
    results.push(MicrobenchmarkResult {
        id: "map/terrain_lookup".to_string(),
        group: "map".to_string(),
        ..result
    });

    // GameSettings clone (cumulative — sub-microsecond operation)
    let result = run_bench_cumulative(
        cfg_fast,
        50000,
        || {},
        || {
            let s = bench_settings();
            let _ = s.clone();
            black_box(0);
        },
    );
    results.push(MicrobenchmarkResult {
        id: "settings/clone".to_string(),
        group: "settings".to_string(),
        ..result
    });

    // Default unit types construction (cumulative)
    let result = run_bench_cumulative(
        cfg_fast,
        50000,
        || {},
        || {
            let _ = bench_unit_type();
            black_box(0);
        },
    );
    results.push(MicrobenchmarkResult {
        id: "settings/default_unit_types".to_string(),
        group: "settings".to_string(),
        ..result
    });

    // City creation (cumulative — 20 creations per inner loop)
    let result = run_bench_cumulative(
        cfg_fast,
        5000,
        || {},
        || {
            for i in 0..20 {
                let _ = City::new(&format!("city_{i}"), (10.0 + i as f32, 10.0));
            }
            black_box(20);
        },
    );
    results.push(MicrobenchmarkResult {
        id: "economy/city_new_batch20".to_string(),
        group: "economy".to_string(),
        ..result
    });

    results
}

// ============================================================================
// Public Runner
// ============================================================================

/// Run all microbenchmarks and return results.
pub fn run_microbenchmarks() -> Vec<MicrobenchmarkResult> {
    let mut results = Vec::new();

    println!("\n🔬 Running microbenchmarks...");
    println!("  ───────────────────────────────────────────────");

    let spatial = bench_spatial_hash();
    for r in &spatial {
        print_result(r);
    }
    results.extend(spatial);

    let ecs = bench_ecs();
    for r in &ecs {
        print_result(r);
    }
    results.extend(ecs);

    let systems = bench_systems();
    for r in &systems {
        print_result(r);
    }
    results.extend(systems);

    let map_settings = bench_map_settings();
    for r in &map_settings {
        print_result(r);
    }
    results.extend(map_settings);

    println!("  ───────────────────────────────────────────────");
    println!("  Microbenchmarks complete: {} benchmarks", results.len());

    results
}

fn print_result(r: &MicrobenchmarkResult) {
    let mean_us = r.mean_ns / 1000.0;
    let std_us = r.std_dev_ns / 1000.0;
    if mean_us < 1000.0 {
        println!(
            "  {:<40} {:>10.2}µs ± {:.2}µs  ({} iters)",
            r.id, mean_us, std_us, r.n
        );
    } else {
        println!(
            "  {:<40} {:>10.2}ms ± {:.2}ms  ({} iters)",
            r.id,
            mean_us / 1000.0,
            std_us / 1000.0,
            r.n
        );
    }
}

/// Convert results to JSON for integration into the HTML report.
pub fn results_to_json(results: &[MicrobenchmarkResult]) -> serde_json::Value {
    serde_json::json!({
        "benchmarks": results.iter().map(|r| serde_json::json!({
            "id": r.id,
            "group": r.group,
            "mean_ns": r.mean_ns,
            "std_dev_ns": r.std_dev_ns,
            "median_ns": r.median_ns,
            "n": r.n,
        })).collect::<Vec<_>>(),
        "count": results.len()
    })
}
