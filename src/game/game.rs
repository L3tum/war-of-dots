//! Game facade — holds the ECS world, settings, and dispatches systems.

use super::spatial;
use crate::config::CityUpgradeSettings;
use crate::game::components::*;
use crate::game::economy::{City, UnitType};
use crate::game::player::Player;
use crate::game::unit::Terrain;
use crate::game::world_ext::WorldExt;
use crate::map::map::Map;
use hecs::Entity;
use hecs::World;
use std::collections::hash_map::Entry;
use std::collections::HashMap;

#[cfg(any(debug_assertions, feature = "debug-overlay"))]
use super::debug_stats::DebugSystemGuard;

/// Macro to wrap system execution with timing in debug builds, no-op in release.
///
/// Required args: a name label, the world (`&mut self.world`), the world entity
/// (`self.world_entity`), and the expression to run.
///
/// Usage (statement):
///   tick_system!("movement", &mut self.world, self.world_entity,
///                systems::movement::run(&mut self.world, ...));
///
/// Usage (expression):
///   let dead = tick_system!("combat", &mut self.world, self.world_entity,
///                           systems::combat::run(...));
///
/// In debug builds each system reports its own timing via
/// `debug_stats::record_system_timing` (populating the `system_timings` breakdown
/// read by the benchmark and the debug overlay); in release it's a plain
/// pass-through. The world and world_entity handles are passed explicitly so macro
/// hygiene does not block the timing code from accessing them.
#[macro_export]
#[cfg(any(debug_assertions, feature = "debug-overlay"))]
macro_rules! tick_system {
    ($name:expr, $world:expr, $world_entity:expr, $expr:expr) => {{
        let _start = std::time::Instant::now();
        #[cfg(feature = "bench-alloc")]
        let _alloc_base = $crate::bench_alloc::peek();
        let _result = $expr;
        let _dur = _start.elapsed().as_secs_f64() * 1000.0;
        #[cfg(feature = "bench-alloc")]
        let _alloc_delta = $crate::bench_alloc::peek().diff(_alloc_base);
        $crate::game::debug_stats::record_system_timing($world, $world_entity, $name, _dur);
        #[cfg(feature = "bench-alloc")]
        {
            $crate::game::debug_stats::record_system_alloc(
                $world,
                $world_entity,
                $name,
                _alloc_delta.allocs,
                _alloc_delta.bytes,
            );
        }
        _result
    }};
}

#[macro_export]
#[cfg(not(any(debug_assertions, feature = "debug-overlay")))]
macro_rules! tick_system {
    ($name:expr, $world:expr, $world_entity:expr, $expr:expr) => {
        $expr
    };
}

/// Tunable influence/vision parameters (game balance). Defaults = equal weights.
///
/// Vision and influence share one circle per source. These knobs control the
/// weight each source adds per cell and the circle radii. Exposed so balance
/// can be playtested from `config.toml` without code changes.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, schemars::JsonSchema)]
pub struct InfluenceSettings {
    /// Influence added per cell by a unit (default 1).
    #[serde(default = "default_unit_weight")]
    pub unit_weight: u16,
    /// Influence added per cell by a city (default 1 — equal to units).
    #[serde(default = "default_city_weight")]
    pub city_weight: u16,
    /// Base circle radius for a unit (default 5.0).
    #[serde(default = "default_unit_radius")]
    pub unit_radius: f32,
    /// City radius = unit_radius × this multiplier (default 1.5 → r7).
    #[serde(default = "default_city_radius_multiplier")]
    pub city_radius_multiplier: f32,
    /// Hill terrain radius multiplier applied to unit radius (default 2.0 → r10).
    #[serde(default = "default_hill_radius_multiplier")]
    pub hill_radius_multiplier: f32,
}
impl Default for InfluenceSettings {
    fn default() -> Self {
        Self {
            unit_weight: 1,
            city_weight: 1,
            unit_radius: 5.0,
            city_radius_multiplier: 1.5,
            hill_radius_multiplier: 2.0,
        }
    }
}
fn default_unit_weight() -> u16 {
    1
}
fn default_city_weight() -> u16 {
    1
}
fn default_unit_radius() -> f32 {
    5.0
}
fn default_city_radius_multiplier() -> f32 {
    1.5
}
fn default_hill_radius_multiplier() -> f32 {
    2.0
}

/// Game-level settings.
#[derive(Debug, Clone)]
pub struct GameSettings {
    pub starting_funds: u32,
    /// Per-player initial manpower pool, seeded at game start. Manpower is always
    /// required when recruiting (regardless of city-upgrade settings).
    pub initial_manpower: u32,
    /// Funds generated per city per economy tick (base income before multipliers)
    pub funds_per_city: f32,
    /// Men restored to each supplied unit per tick from the global manpower pool.
    pub recruitment_rate: f32,
    pub game_speed: f32,
    pub use_standard_ruleset: bool,
    pub unit_types: Vec<UnitType>,
    pub city_upgrade_settings: CityUpgradeSettings,
    pub water_speed: f32,
    /// Influence/vision balance knobs (weights, radii). Defaults = equal weights.
    pub influence: InfluenceSettings,
}

/// Per-player statistics for the current game.
#[derive(Debug, Clone, Default)]
pub struct PlayerGameStats {
    pub units_recruited: u32,
    pub units_lost: u32,
    pub cities_captured: u32,
    pub funds_spent: u32,
    pub funds_earned: u32,
}

/// The Game facade wraps a hecs::World and dispatches systems on each tick.
pub struct Game {
    pub(crate) world: World,
    pub(crate) world_entity: hecs::Entity,
    settings: GameSettings,
    players: HashMap<String, Player>,
    tick: u64,
    running: bool,
    player_stats: HashMap<String, PlayerGameStats>,
    prev_unit_counts: HashMap<String, usize>,
    last_surrender_check_tick: u64,
    /// Maps string unit IDs ("unit_123") -> hecs::Entity
    entity_registry: HashMap<String, hecs::Entity>,
    /// Reverse lookup: hecs::Entity -> string unit ID — O(1) dead entity cleanup
    entity_reverse: HashMap<hecs::Entity, String>,
    /// Pre-allocated combat scratch, cleared and reused each tick.
    combat_scratch: systems::combat::CombatScratch,
    /// Pre-allocated supply/attrition scratch, cleared and reused each tick.
    supply_scratch: systems::supply::SupplyScratch,
    /// Spatial grid buffers for the combat mover-scan (target acquisition),
    /// resized on growth and reused each tick (no per-tick heap allocation at
    /// steady state). Built/updated by movement, consumed by combat.
    grid_index_scratch: spatial::GridIndexScratch,
    /// Reused scratch buffers for the encirclement flood-fill.
    encirclement_scratch: systems::encirclement::EncirclementScratch,
    /// Economy scratch: (player_id, funds_earned) per player, cleared and reused
    /// each economy tick instead of building a fresh HashMap.
    economy_earned: Vec<(String, u32)>,
    /// Unit-type name -> index into `settings.unit_types`, built once at
    /// `Game::new` so the supply system does not rebuild the map every tick.
    unit_types_index: HashMap<String, usize>,
    /// Sum of `max_count` across each unit type's equipment template — one
    /// entry per index into `settings.unit_types`. The per-unit equipment
    /// `needed` denominator is invariant (built from this template at spawn,
    /// never mutated), so supply reads this instead of re-summing per unit per
    /// tick (S-1). Same order as `settings.unit_types`.
    unit_type_total_max_equipment: Vec<f32>,
    /// Pre-allocated movement scratch, cleared and reused each tick. Holds the
    /// mover records that movement system uses.
    movement_scratch: systems::movement::MovementScratch,
    /// Pre-allocated water scratch, cleared and reused each tick.
    water_scratch: systems::water::WaterScratch,
    /// Number of units that moved in the most recent tick (from movement.rs).
    /// Used by benchmark/status reporting to gauge spatialty work.
    last_mover_count: usize,
    /// Number of engaged pairs in the most recent tick (from combat.rs).
    /// Used by benchmark/status reporting to gauge combat activity.
    last_engaged_pairs: usize,
    /// Persistent "units_by_player no longer matches the ECS" flag — set by
    /// movement, unit death, and recruitment; cleared only when a rebuild
    /// actually runs. (Not part of the per-tick ctx_dirty, which is reset at
    /// the top of every tick and would lose sets made between ticks or after
    /// the rebuild point.)
    units_by_player_stale: bool,
    /// Persisted TickContext, built once and updated incrementally.
    ctx: TickContext,
    /// Tracks which TickContext fields need updating this tick.
    ctx_dirty: TickContextDirty,
    /// Persistent "game state changed since last influence run" flag. Set by
    /// movement, city capture, unit death, successful recruitment, and
    /// assign_city_owner. Cleared only when influence actually runs. NOT part
    /// of the per-tick ctx_dirty (which is reset at the top of every tick).
    influence_dirty: bool,
    /// Set when influence runs, cleared when encirclement runs. Encirclement's
    /// inputs (borders grids, cities, units_by_player) only change via an
    /// influence run, so this is a faithful "encirclement inputs changed since
    /// last run" signal. Invariants: (i) every cities_changed source also sets
    /// influence_dirty; (ii) CITY_CAPTURE_TICK_INTERVAL % INFLUENCE_TICK_INTERVAL
    /// == 0, so the influence block always services a change before the
    /// encirclement block in the same tick.
    encirclement_dirty: bool,
    /// Pre-allocated influence scratch, cleared and reused each influence run.
    influence_scratch: systems::influence::InfluenceScratch,
    /// Pre-allocated death-cleanup scratch, cleared and reused each run.
    death_cleanup_scratch: systems::death_cleanup::DeathCleanupScratch,
}

/// Combat-targeting snapshot: how many units are holding an explicit combat
/// target this tick. Read after the tick (post-combat) for benchmark/status.
#[allow(dead_code)]
#[derive(Debug, Clone, Copy, Default)]
pub struct CombatHealth {
    /// Living units with UnitFlags.
    pub units: usize,
    /// Units holding an explicit combat target (`attacking.is_some()`).
    pub units_with_attacking: usize,
}

/// Combat resolution-stage counters from one tick's `combat::run`.
#[derive(Debug, Clone, Copy, Default)]
#[allow(dead_code)]
pub struct CombatDiag {
    /// Candidate units entering the combat loop (have an explicit target).
    pub candidates: usize,
    /// Candidates carrying a target (== candidates; kept for parity).
    pub attackers: usize,
    /// Targets that died earlier this tick (skipped, target cleared).
    pub targets_dead: usize,
    /// Targets that vanished from the ECS (defensive; stale despawn).
    pub targets_gone: usize,
    /// Targets sharing the attacker's owner (stale; target cleared).
    pub targets_same_owner: usize,
    /// Targets that retreated out of combat range (target cleared, D4).
    pub targets_out_of_range: usize,
    /// Mutual pairs skipped because the lower-id unit resolves them.
    pub targets_skipped_mutual: usize,
    /// Engagements resolved this tick (mutual pairs count once).
    pub engagements: usize,
}

/// Movement diagnostics counters from the most recent tick's run (W1).
#[allow(dead_code)]
pub struct MovementDiag {
    /// Units with a non-empty path entering the change list this tick.
    pub changes: usize,
    /// Occupancy-gate `for_each_within` scans performed.
    pub scans: usize,
    /// Movers blocked by the gate (retry next tick).
    pub blocked: usize,
    /// No-op changes skipped by the W1-① filter (writes nothing).
    pub noop_skipped: usize,
    /// Gate re-scans skipped via the W1-④ blocked-unit backoff window.
    pub cooldown_skipped: usize,
    /// Movers that passed the gate and were applied.
    pub movers: usize,
}

/// Supply diagnostics counters from the most recent tick's run (W4).
#[allow(dead_code)]
pub struct SupplyDiag {
    /// Units whose supply draw was blocked by an exhausted funds pool.
    pub blocked_funds: u64,
    /// Units whose men gain was blocked by an exhausted manpower pool.
    pub blocked_mp: u64,
}

/// Pre-grouped data built once per tick, shared across all systems.
/// Eliminates redundant player enumeration and unit filtering.
/// Only holds small data (cities ~20 entries, funds ~12 entries).
/// Grids are accessed via scoped Arc extraction in systems that need them.
/// Entity+position handle for unit grouping. Snapshot taken at tick start;
/// systems that need fresh component data (e.g. supply, which runs after
/// combat) do their own bulk query instead of per-entity world lookups.
#[derive(Debug, Clone, Copy)]
pub struct UnitHandle {
    /// Entity handle — used by tests and by the future incremental rebuild
    /// (T-1). Not read by the steady-state tick loop.
    #[allow(dead_code)]
    pub entity: hecs::Entity,
    pub pos: (f32, f32),
}

#[derive(Debug)]
pub struct TickContext {
    /// The world entity handle (needed for world_data_mut writes)
    pub world_entity: hecs::Entity,
    /// Cities snapshot — small (~20 entries), cloned lazily
    pub cities: Vec<crate::game::economy::City>,
    /// Map dimensions (cached to avoid per-system world borrows)
    pub width: u32,
    pub height: u32,
    /// Entity+position handles grouped by player ID
    pub units_by_player: HashMap<String, Vec<UnitHandle>>,
    /// Compact owner lookup: entity id → player index (0..=254), or OWNER_NONE.
    /// Shared across systems so movement and combat can do
    /// O(1) u8 equality instead of per-pair ECS borrows + String comparison.
    /// Dense Vec indexed by entity id (low 32 bits of `Entity::to_bits()`);
    /// the hecs entity generation guards id reuse (a reused id has a bumped
    /// generation, so a stale slot never matches — mirroring how the old
    /// `HashMap<Entity, u8>` key inherently included generation).
    pub owner_at: Vec<u8>,
    /// Entity generation stamped when `owner_at[id]` was written.
    pub owner_gen: Vec<u32>,
    /// Persistent player→index mapping for incremental owner map updates.
    pub player_id_to_index: HashMap<String, u8>,
    /// Next available player index (monotonically increasing, never reused).
    pub next_player_idx: u8,
}

/// Sentinel for "no owner" in `TickContext.owner_at` (owner indices are < 255).
pub const OWNER_NONE: u8 = 255;

/// Cap on the owner table's entity-id span. Live hecs ids are bounded by total
/// spawns (tens of thousands); the cap keeps a pathological id (e.g.
/// `Entity::DANGLING` = u32::MAX) from triggering a multi-GB resize. Entities
/// beyond the cap simply read as OWNER_NONE (correctness unaffected).
const OWNER_MAX_ID: usize = 1 << 20;

impl TickContext {
    /// Look up a unit's player index via the dense owner table.
    /// Returns `OWNER_NONE` for unset/out-of-cap entities or when the stored
    /// hecs generation does not match (id reuse after death+respawn).
    #[inline]
    pub fn owner_index(&self, e: hecs::Entity) -> u8 {
        let bits = e.to_bits().get();
        let id = (bits & 0xFFFF_FFFF) as usize;
        if id >= OWNER_MAX_ID {
            return OWNER_NONE;
        }
        if self.owner_gen.get(id).copied().unwrap_or(0) != (bits >> 32) as u32 {
            return OWNER_NONE;
        }
        self.owner_at.get(id).copied().unwrap_or(OWNER_NONE)
    }

    /// Record a unit's owner index (called on recruit). No-op for ids beyond
    /// the cap; safe against id reuse because the generation is stamped.
    pub fn owner_set(&mut self, e: hecs::Entity, owner_idx: u8) {
        let bits = e.to_bits().get();
        let id = (bits & 0xFFFF_FFFF) as usize;
        if id >= OWNER_MAX_ID {
            return;
        }
        if self.owner_at.len() <= id {
            self.owner_at.resize(id + 1, OWNER_NONE);
            self.owner_gen.resize(id + 1, 0);
        }
        self.owner_at[id] = owner_idx;
        self.owner_gen[id] = (bits >> 32) as u32;
    }
}

/// Tracks which TickContext fields need updating this tick.
/// NOTE: only holds intra-tick signals (set and consumed within the same
/// tick_game). Cross-tick dirtiness (units_by_player, influence) lives in
/// persistent Game fields that are cleared on consumption, not by this reset.
#[derive(Debug, Clone, Copy, Default)]
pub struct TickContextDirty {
    /// Set when city_capture reports ownership changes → clone cities
    pub cities_changed: bool,
}

impl TickContext {
    /// Build TickContext once at Game::new. Populates static fields and does
    /// initial ECS query for dynamic fields.
    /// Only clones small data (cities ~20 entries).
    /// Grids (terrain/vision/borders) are NOT cloned — systems use scoped Arc extraction.
    pub fn build_initial(world: &World, world_entity: hecs::Entity) -> Self {
        // Scoped extraction: only clone what we actually need
        let (cities, width, height) = {
            let wb = world
                .world_data(world_entity)
                .expect("world_data must exist for a valid world_entity");
            (
                wb.cities.cities.clone(),
                wb.terrain.width,
                wb.terrain.height,
            )
        };
        // wb dropped — borrow released

        // Populate player_id_to_index and owner_at/owner_gen in a single ECS query
        let player_id_to_index: HashMap<String, u8> = HashMap::with_capacity(16);
        let mut ctx = Self {
            world_entity,
            cities,
            width,
            height,
            units_by_player: HashMap::new(),
            owner_at: Vec::new(),
            owner_gen: Vec::new(),
            player_id_to_index,
            next_player_idx: 0,
        };
        for (entity, owner) in world.query::<(Entity, &UnitOwner)>().iter() {
            let idx = match ctx.player_id_to_index.entry(owner.player_id.clone()) {
                Entry::Occupied(e) => *e.get(),
                Entry::Vacant(e) => {
                    let id = ctx.next_player_idx;
                    ctx.next_player_idx += 1;
                    e.insert(id);
                    id
                }
            };
            ctx.owner_set(entity, idx);
        }
        debug_assert!(ctx.next_player_idx < 255, "too many players");

        ctx
    }

    /// Rebuild units_by_player from ECS (positions, entity set).
    /// Called just-in-time before influence/encirclement when positions have changed.
    ///
    /// Reuses the persistent map and does a *borrowed-key* lookup per unit
    /// (`HashMap<String, V>::get_mut(&str)`), so no transient `String` is cloned
    /// per unit. A `String` key is only allocated for a genuinely new player id
    /// (rare in steady state). `Vec` capacity is preserved across rebuilds.
    pub fn rebuild_units_by_player(&mut self, world: &World) {
        for v in self.units_by_player.values_mut() {
            v.clear();
        }

        for (entity, owner, pos) in world.query::<(Entity, &UnitOwner, &UnitPosition)>().iter() {
            let pid = owner.player_id.as_str();
            if self.units_by_player.get_mut(pid).is_none() {
                // New player id this tick — insert a String key once.
                self.units_by_player.entry(pid.to_string()).or_default();
            }
            self.units_by_player.get_mut(pid).unwrap().push(UnitHandle {
                entity,
                pos: (*pos).as_tuple(),
            });
        }

        // A player appears in units_by_player iff it has ≥1 unit this tick
        // (matches the old fresh-HashMap semantics, where only players with
        // units ever got a key).
        self.units_by_player.retain(|_, v| !v.is_empty());
    }

    /// Combined rebuild of player_id_to_index, owner_at/owner_gen, AND units_by_player
    /// in a single ECS pass. Used by build_full() for benchmarks — not called in the
    /// tick loop (owner map is updated incrementally; units_by_player is rebuilt lazily).
    #[cfg(any(test, feature = "microbenchmarks"))]
    #[allow(dead_code)]
    pub fn rebuild_both(&mut self, world: &World) {
        let mut player_id_to_index: HashMap<String, u8> = HashMap::with_capacity(16);
        let mut new_units: HashMap<String, Vec<UnitHandle>> = HashMap::new();
        let mut next_player_idx: u8 = 0;

        self.owner_at.clear();
        self.owner_gen.clear();

        for (entity, owner, pos) in world.query::<(Entity, &UnitOwner, &UnitPosition)>().iter() {
            let pid = owner.player_id.clone();
            let idx = match player_id_to_index.entry(pid.clone()) {
                Entry::Occupied(e) => *e.get(),
                Entry::Vacant(e) => {
                    let id = next_player_idx;
                    next_player_idx += 1;
                    e.insert(id);
                    id
                }
            };
            self.owner_set(entity, idx);
            new_units.entry(pid).or_default().push(UnitHandle {
                entity,
                pos: (*pos).as_tuple(),
            });
        }
        debug_assert!(next_player_idx < 255, "too many players");

        self.player_id_to_index = player_id_to_index;
        self.next_player_idx = next_player_idx;
        self.units_by_player = new_units;
    }

    /// Clone cities from world. Called before influence/encirclement if
    /// city ownership changed.
    pub fn update_cities(&mut self, world: &World, world_entity: hecs::Entity) {
        self.cities = {
            let wb = world
                .world_data(world_entity)
                .expect("world_data must exist for a valid world_entity");
            wb.cities.cities.clone()
        };
    }

    /// Full rebuild of all dynamic fields. Used by tests, benchmarks, and as a fallback.
    /// Equivalent to the old `build()` but returns `Self` directly.
    #[cfg(any(test, feature = "microbenchmarks"))]
    #[allow(dead_code)]
    pub fn build_full(world: &World, world_entity: hecs::Entity) -> Self {
        let mut ctx = Self::build_initial(world, world_entity);
        ctx.rebuild_both(world);
        ctx
    }
}

impl Game {
    pub fn new(map: Map, settings: GameSettings, num_players: usize) -> Self {
        let mut world = World::new();

        let mut players = HashMap::new();
        let mut player_stats = HashMap::new();
        let mut funds = HashMap::new();

        for i in 0..num_players {
            let player_id = format!("player_{i}");
            players.insert(
                player_id.clone(),
                Player::new(
                    player_id.clone(),
                    format!("Player {}", i + 1),
                    true,
                    settings.starting_funds,
                ),
            );
            player_stats.insert(player_id.clone(), PlayerGameStats::default());
            funds.insert(player_id, settings.starting_funds);
        }

        let cities: Vec<City> = map
            .cities
            .iter()
            .enumerate()
            .map(|(i, city_pos)| City::new(&format!("city_{i}"), *city_pos))
            .collect();

        let (w, h) = (map.width, map.height);
        let mut terrain = vec![vec![Terrain::Plains; h as usize]; w as usize];
        for x in 0..w {
            for y in 0..h {
                terrain[x as usize][y as usize] = map.terrain(x, y);
            }
        }

        let world_entity = world.spawn_world(
            terrain,
            w,
            h,
            settings.city_upgrade_settings.clone(),
            funds,
            cities,
        );

        // Seed each player's initial manpower pool. Manpower is always used when
        // recruiting (independent of city-upgrade settings), so the pool must be
        // populated from the start — otherwise the first recruitment wave would be
        // blocked until the economy produces manpower.
        {
            let initial_manpower = settings.initial_manpower;
            if let Some(wb) = world.world_data_mut(world_entity) {
                for i in 0..num_players {
                    wb.funds
                        .manpower
                        .insert(format!("player_{i}"), initial_manpower);
                }
            }
        }

        // Build initial TickContext before world is moved into Self
        let ctx = TickContext::build_initial(&world, world_entity);
        // Cache unit-type name -> index once (settings is moved into Self below,
        // so build the map first). Supply indexes straight into settings.unit_types.
        let unit_types_index: HashMap<String, usize> = settings
            .unit_types
            .iter()
            .enumerate()
            .map(|(i, ut)| (ut.name.clone(), i))
            .collect();
        // S-1: precompute the invariant equipment `needed` denominator per unit
        // type (sum of the template's max_count). Supply reads this instead of
        // re-summing max_equipment for every unit every tick.
        let unit_type_total_max_equipment: Vec<f32> = settings
            .unit_types
            .iter()
            .map(|ut| ut.equipment.iter().map(|e| e.max_count as f32).sum())
            .collect();

        Self {
            world,
            world_entity,
            settings,
            unit_types_index,
            unit_type_total_max_equipment,
            players,
            tick: 0,
            running: false,
            player_stats,
            prev_unit_counts: HashMap::new(),
            last_surrender_check_tick: 0,
            entity_registry: HashMap::new(),
            entity_reverse: HashMap::new(),
            combat_scratch: systems::combat::CombatScratch::new(),
            supply_scratch: systems::supply::SupplyScratch::new(),
            grid_index_scratch: spatial::GridIndexScratch::default(),
            encirclement_scratch: systems::encirclement::EncirclementScratch::new(),
            economy_earned: Vec::new(),
            movement_scratch: systems::movement::MovementScratch::default(),
            water_scratch: systems::water::WaterScratch::default(),
            last_mover_count: 0,
            last_engaged_pairs: 0,
            ctx,
            ctx_dirty: TickContextDirty::default(),
            units_by_player_stale: false,
            influence_dirty: true,
            encirclement_dirty: true,
            influence_scratch: systems::influence::InfluenceScratch::new(),
            death_cleanup_scratch: systems::death_cleanup::DeathCleanupScratch::default(),
        }
    }

    pub fn start(&mut self) {
        self.running = true;
    }

    /// Number of units that moved in the most recent tick.
    #[allow(dead_code)]
    pub fn last_mover_count(&self) -> usize {
        self.last_mover_count
    }

    /// Number of engaged combat pairs in the most recent tick (combat.rs).
    #[allow(dead_code)]
    pub fn last_engaged_pairs(&self) -> usize {
        self.last_engaged_pairs
    }

    /// Allocation-free count of units (same entity set as `query_all_units`),
    /// for benchmarking. Unlike `query_all_units().len()`, this does not build
    /// the String-materialized `UnitQueryData` for every unit.
    #[allow(dead_code)]
    pub fn unit_count(&self) -> usize {
        self.world
            .query::<(
                &UnitOwner,
                &UnitTypeId,
                &UnitDisplay,
                &UnitPosition,
                &UnitPath,
                &UnitStats,
                &UnitEquipment,
                &UnitFlags,
                &UnitWaterTransition,
            )>()
            .iter()
            .count()
    }

    /// Fill a caller-owned scratch buffer with `(id, owner, y)` for every unit,
    /// for the benchmark AI. Same 10-component query as `query_all_units`, so it
    /// sees the identical entity set (and preserves its determinism), but emits
    /// only the `(id, owner, y)` the caller needs instead of the full
    /// String/`Vec`-heavy `UnitQueryData`. The `y` coordinate lets the driver aim
    /// each unit at the centre line at its own row (a long, continuous front)
    /// rather than a single point. Slots are overwritten in place and the buffer
    /// is never shrunk, so after the first growth the steady-state call performs
    /// no allocations. Returns the live unit count.
    #[allow(dead_code)]
    pub fn fill_unit_id_owner_y(&self, out: &mut Vec<(String, String, f32)>) -> usize {
        use std::fmt::Write as _;
        let mut n = 0usize;
        for (entity, owner, _utype, _display, pos, _path, _stats, _equipment, _flags, _wt) in self
            .world
            .query::<(
                Entity,
                &UnitOwner,
                &UnitTypeId,
                &UnitDisplay,
                &UnitPosition,
                &UnitPath,
                &UnitStats,
                &UnitEquipment,
                &UnitFlags,
                &UnitWaterTransition,
            )>()
            .iter()
        {
            if n == out.len() {
                out.push((String::new(), String::new(), 0.0));
            }
            let slot = &mut out[n];
            let eid = entity.id();
            // `unit_{eid}` written in place; capacity is reused after the first
            // pass so this is allocation-free in steady state.
            slot.0.clear();
            let _ = write!(slot.0, "unit_{eid}");
            slot.1.clone_from(&owner.player_id);
            slot.2 = pos.y;
            n += 1;
        }
        n
    }

    /// Test-only: expose the persistent influence dirty flag.
    #[cfg(test)]
    pub fn influence_dirty_for_test(&self) -> bool {
        self.influence_dirty
    }

    /// Test-only: total number of units grouped in ctx.units_by_player.
    #[cfg(test)]
    pub fn ctx_unit_count_for_test(&self) -> usize {
        self.ctx.units_by_player.values().map(|v| v.len()).sum()
    }

    /// Snapshot of combat targeting: how many units hold an explicit combat
    /// target. Read after the tick (post-combat) for benchmark/status.
    #[allow(dead_code)]
    pub fn combat_health(&self) -> CombatHealth {
        let mut units = 0usize;
        let mut units_with_attacking = 0usize;
        for (_, flags) in self.world.query::<(Entity, &UnitFlags)>().iter() {
            units += 1;
            if flags.attacking.is_some() {
                units_with_attacking += 1;
            }
        }
        CombatHealth {
            units,
            units_with_attacking,
        }
    }

    /// Combat resolution-stage counters from the most recent tick's run.
    #[allow(dead_code)]
    pub fn combat_diag(&self) -> CombatDiag {
        CombatDiag {
            candidates: self.combat_scratch.candidates,
            attackers: self.combat_scratch.attackers,
            targets_dead: self.combat_scratch.targets_dead,
            targets_gone: self.combat_scratch.targets_gone,
            targets_same_owner: self.combat_scratch.targets_same_owner,
            targets_out_of_range: self.combat_scratch.targets_out_of_range,
            targets_skipped_mutual: self.combat_scratch.targets_skipped_mutual,
            engagements: self.combat_scratch.engagements,
        }
    }

    /// Movement counters from the most recent tick's run (W1 sub-profiling).
    #[allow(dead_code)]
    pub fn movement_diag(&self) -> MovementDiag {
        MovementDiag {
            changes: self.movement_scratch.changes_len(),
            scans: self.movement_scratch.diag_scans,
            blocked: self.movement_scratch.diag_blocked,
            noop_skipped: self.movement_scratch.diag_noop_skipped,
            cooldown_skipped: self.movement_scratch.diag_cooldown,
            movers: self.last_mover_count,
        }
    }

    /// Supply FCFS-gate counters from the most recent tick's run (W4).
    #[allow(dead_code)]
    pub fn supply_diag(&self) -> SupplyDiag {
        SupplyDiag {
            blocked_funds: self.supply_scratch.diag_blocked_funds,
            blocked_mp: self.supply_scratch.diag_blocked_mp,
        }
    }

    /// Main game tick.
    pub fn tick_game(&mut self) {
        if !self.running {
            return;
        }
        self.tick += 1;

        #[cfg(any(debug_assertions, feature = "debug-overlay"))]
        let guard = DebugSystemGuard::new(&mut self.world, self.world_entity);

        self.ctx_dirty = TickContextDirty::default();

        // Economy: only every ECONOMY_TICK_INTERVAL ticks
        if self
            .tick
            .is_multiple_of(super::constants::ECONOMY_TICK_INTERVAL)
        {
            tick_system!(
                "economy",
                &mut self.world,
                self.world_entity,
                systems::economy::run(
                    &mut self.world,
                    self.world_entity,
                    self.settings.funds_per_city,
                    &mut self.economy_earned,
                )
            );
            // Track funds_earned in player stats
            for (pid, amount) in &self.economy_earned {
                if let Some(stats) = self.player_stats.get_mut(pid) {
                    stats.funds_earned += amount;
                }
            }
            // Read funds directly from world for player update
            for (pid, player) in self.players.iter_mut() {
                let f = self.world.player_funds(self.world_entity, pid);
                player.funds = f;
            }
        }

        tick_system!(
            "water",
            &mut self.world,
            self.world_entity,
            systems::water::run(&mut self.world, self.world_entity, &mut self.water_scratch)
        );

        // Movement returns count of units that moved this tick. It also builds
        // the tick's one spatial grid (pre-move positions) into
        // `self.grid_index_scratch` — consumed by the occupancy gate, combat's
        // mover-scan / target acquisition, and city capture.
        let mover_count = tick_system!(
            "movement",
            &mut self.world,
            self.world_entity,
            systems::movement::run(
                &mut self.world,
                self.world_entity,
                &self.settings,
                &mut self.movement_scratch,
                &self.ctx,
                &mut self.grid_index_scratch,
            )
        );
        self.last_mover_count = mover_count;

        // ── AFTER movement: rebuild units_by_player when a consumer needs it ──
        // Owner map: always current (updated incrementally in recruit_unit).
        // units_by_player: only rebuild on consumer ticks AND only if positions changed.
        if mover_count > 0 {
            self.units_by_player_stale = true;
            self.influence_dirty = true;
        }
        let is_consumer_tick = self
            .tick
            .is_multiple_of(super::constants::INFLUENCE_TICK_INTERVAL)
            || self
                .tick
                .is_multiple_of(super::constants::CITY_CAPTURE_TICK_INTERVAL);
        if is_consumer_tick && self.units_by_player_stale {
            tick_system!("tickcontext", &mut self.world, self.world_entity, {
                self.ctx.rebuild_units_by_player(&self.world);
                self.units_by_player_stale = false;
            });
        }

        // Combat: resolves explicit combat targets. The mover-scan (target
        // acquisition, site 1) consumes the spatial grid movement built this
        // tick from PRE-move positions (3×3 cell query + circle filter,
        // O(n × c) with bounded c) — folded in from the deleted
        // `nearby_enemies` system (Phase 2, plans/COMBAT_TARGETING.md §8.8).
        let combat_dead: Vec<hecs::Entity> = {
            let ctx = &self.ctx;
            tick_system!(
                "combat",
                &mut self.world,
                self.world_entity,
                systems::combat::run(
                    &mut self.world,
                    self.world_entity,
                    &mut self.combat_scratch,
                    &self.settings,
                    ctx,
                    &self.unit_types_index,
                    &self.grid_index_scratch,
                )
            )
        };
        // Expose engaged-pair count for benchmark/status reporting.
        self.last_engaged_pairs = self.combat_scratch.engaged_pairs();

        // Supply + Attrition (merged): runs after combat so it sees post-combat
        // men/equipment; returns units that reached 0 men for death cleanup.
        let supply_dead = tick_system!(
            "supply",
            &mut self.world,
            self.world_entity,
            systems::supply::run(
                &mut self.world,
                self.world_entity,
                &self.settings,
                &mut self.supply_scratch,
                &self.ctx,
                &self.unit_types_index,
                &self.unit_type_total_max_equipment,
            )
        );

        // City capture — returns list of city IDs whose ownership changed.
        // Reuses the spatial grid movement built this tick (shared borrow —
        // combat read it too) so the candidate scan is O(cities × units_in_range)
        // instead of O(units × cities).
        let cities_changed = tick_system!(
            "city_capture",
            &mut self.world,
            self.world_entity,
            systems::city_capture::run(
                &mut self.world,
                self.world_entity,
                &mut self.player_stats,
                &self.ctx,
                &self.grid_index_scratch,
            )
        );
        if !cities_changed.is_empty() {
            self.ctx_dirty.cities_changed = true;
            self.influence_dirty = true;
        }

        // Influence: merged vision + border in one pass (every INFLUENCE_TICK_INTERVAL ticks)
        if self
            .tick
            .is_multiple_of(super::constants::INFLUENCE_TICK_INTERVAL)
        {
            // Update cities before influence reads them if they changed
            if self.ctx_dirty.cities_changed {
                self.ctx.update_cities(&self.world, self.world_entity);
                self.ctx_dirty.cities_changed = false;
            }
            if self.influence_dirty {
                let ctx = &self.ctx;
                tick_system!(
                    "influence",
                    &mut self.world,
                    self.world_entity,
                    systems::influence::run(
                        &mut self.world,
                        ctx,
                        self.tick,
                        &mut self.influence_scratch,
                        true,
                        &self.settings.influence,
                    )
                );
                self.influence_dirty = false;
                self.encirclement_dirty = true; // encirclement inputs changed
            }
            // else: skip compute + allocations; existing Arcs stay valid
        }

        // Encirclement: only every CITY_CAPTURE_TICK_INTERVAL ticks (strategic, not tactical)
        if self
            .tick
            .is_multiple_of(super::constants::CITY_CAPTURE_TICK_INTERVAL)
        {
            // NOTE: cities_changed is always serviced by the influence block above
            // (30 % 3 == 0 ⇒ every encirclement tick is an influence tick that ran
            // first), so it is always false here.
            debug_assert!(!self.ctx_dirty.cities_changed);
            if self.encirclement_dirty {
                let ctx = &self.ctx;
                tick_system!(
                    "encirclement",
                    &mut self.world,
                    self.world_entity,
                    systems::encirclement::run(
                        &mut self.world,
                        ctx,
                        self.tick,
                        &mut self.encirclement_scratch,
                    )
                );
                self.encirclement_dirty = false;
            }
        }

        // Surrender: only every 30 ticks
        let surrender_dead = if self.tick - self.last_surrender_check_tick >= 30 {
            let dead = tick_system!(
                "surrender",
                &mut self.world,
                self.world_entity,
                systems::surrender::run(&mut self.world, self.world_entity)
            );
            self.last_surrender_check_tick = self.tick;
            dead
        } else {
            Vec::new()
        };

        // Unified death cleanup: process dead entities from combat, attrition,
        // and surrender (order preserved for determinism) via slice views so no
        // fresh `all_dead` Vec is collected each tick. Clears stale explicit
        // targets pointing at post-combat deaths (lifecycle site 3), then
        // despawns and cleans the entity registry.
        let death_count = tick_system!(
            "death_cleanup",
            &mut self.world,
            self.world_entity,
            systems::death_cleanup::run(
                &mut self.world,
                &[&combat_dead[..], &supply_dead[..], &surrender_dead[..]],
                &mut self.entity_reverse,
                &mut self.death_cleanup_scratch,
            )
        );

        // Dead entities are despawned from the ECS — stale owner map entries are
        // harmless since they won't appear in ECS queries. But dead units still
        // sit in ctx.units_by_player until a rebuild — force one on the next
        // consumer tick and mark influence stale so the grids don't keep
        // projecting dead units (staleness fix).
        if death_count > 0 {
            self.units_by_player_stale = true;
            self.influence_dirty = true;
        }

        self.track_unit_losses();

        #[cfg(any(debug_assertions, feature = "debug-overlay"))]
        guard.finish(&mut self.world);
    }

    // ====================================================================
    // Public API
    // ====================================================================

    /// Recruit a unit at a city, gated by the circle occupancy rule.
    ///
    /// Looks up the city (validating it exists and belongs to `owner`), then
    /// applies the **circle gate**: if any unit's center is already within
    /// `MIN_UNIT_DISTANCE` of the city point, the recruit is rejected *before*
    /// anything is spent (returns `None`). Otherwise delegates to
    /// [`Game::recruit_at`]. Recruits are event-rate in real play, so the gate
    /// is an on-demand O(n) scan — no grid lookup needed here.
    pub fn recruit_unit(
        &mut self,
        city_id: &str,
        unit_type: &UnitType,
        owner: &str,
    ) -> Option<String> {
        // Phase 1: Validate ALL preconditions before any mutations
        let city_pos = {
            let wb = self.world.world_data(self.world_entity)?;
            let city = wb
                .cities
                .cities
                .iter()
                .find(|c| c.id == city_id && c.owner.as_deref() == Some(owner))?;
            city.position
        };

        // Circle gate: a unit may not be placed so another unit's center is
        // within MIN_UNIT_DISTANCE of it. Rejected before any resource check /
        // deduction, so a blocked recruit spends nothing.
        if self.cell_occupied(city_pos) {
            return None;
        }

        self.recruit_at(unit_type, owner, city_pos)
    }

    /// Recruit a unit at an explicit position, **without** the circle
    /// occupancy gate. This is the premade/benchmark bypass — identical
    /// resource checks + bookkeeping to [`Game::recruit_unit`], but the
    /// position is caller-chosen and stacking is allowed.
    // Used by the lib's benchmark/fixtures, not the bin's own `mod game` copy.
    #[allow(dead_code)]
    pub fn recruit_unit_at(
        &mut self,
        unit_type: &UnitType,
        owner: &str,
        pos: (f32, f32),
    ) -> Option<String> {
        self.recruit_at(unit_type, owner, pos)
    }

    /// Shared recruitment core: validate funds + manpower, deduct, spawn at
    /// `pos`, and do the post-spawn bookkeeping (entity registry, owner map,
    /// dirtiness, player stats). Used by both the gated `recruit_unit` and the
    /// `recruit_unit_at` bypass. Returns the new unit's string id.
    fn recruit_at(&mut self, unit_type: &UnitType, owner: &str, pos: (f32, f32)) -> Option<String> {
        // Check funds
        let funds = self.world.player_funds(self.world_entity, owner);
        if funds < unit_type.recruitment_cost {
            return None;
        }

        // Check manpower (before deducting anything). Manpower is a first-class
        // recruitment resource for every ruleset: the pool is seeded per player at
        // game start (see `Game::new`) and replenished by the economy system, so
        // recruitment always draws from it. It is NOT gated behind city upgrades.
        let has_manpower = {
            let wb = self.world.world_data(self.world_entity)?;
            wb.funds.manpower.get(owner).copied().unwrap_or(0) >= unit_type.men
        };
        if !has_manpower {
            return None;
        }

        // All checks passed — deduct resources
        {
            let wb = self.world.world_data_mut(self.world_entity)?;
            if let Some(f) = wb.funds.funds.get_mut(owner) {
                *f = f.saturating_sub(unit_type.recruitment_cost);
            }
            if let Some(mp) = wb.funds.manpower.get_mut(owner) {
                *mp -= unit_type.men;
            }
        }

        let entity = self.world.spawn_unit(unit_type, owner, pos);
        let unit_id = format!("unit_{}", entity.id());
        self.entity_registry.insert(unit_id.clone(), entity);
        self.entity_reverse.insert(entity, unit_id.clone());

        // Incrementally update the owner map (O(1) insert, no ECS query needed)
        let player_idx = if let Some(&idx) = self.ctx.player_id_to_index.get(owner) {
            idx
        } else {
            let idx = self.ctx.next_player_idx;
            self.ctx.next_player_idx += 1;
            self.ctx.player_id_to_index.insert(owner.to_string(), idx);
            idx
        };
        self.ctx.owner_set(entity, player_idx);

        // New unit isn't in ctx.units_by_player yet — force a rebuild on the
        // next consumer tick and mark influence stale (staleness fix).
        self.units_by_player_stale = true;
        self.influence_dirty = true;

        if let Some(player) = self.players.get_mut(owner) {
            player.units.push(unit_id.clone());
        }
        if let Some(stats) = self.player_stats.get_mut(owner) {
            stats.units_recruited += 1;
            stats.funds_spent += unit_type.recruitment_cost;
        }
        *self.prev_unit_counts.entry(owner.to_string()).or_insert(0) += 1;

        Some(unit_id)
    }

    /// True if any unit's center is within `MIN_UNIT_DISTANCE` of `pos`.
    /// On-demand O(n) scan (recruits are event-rate; no grid lookup here).
    fn cell_occupied(&self, pos: (f32, f32)) -> bool {
        let limit = crate::game::constants::MIN_UNIT_DISTANCE_SQ;
        self.world
            .query::<(Entity, &UnitPosition)>()
            .iter()
            .any(|(_e, other)| {
                let dx = pos.0 - other.x;
                let dy = pos.1 - other.y;
                dx * dx + dy * dy < limit
            })
    }

    /// Get the owner of a unit by its string ID.
    pub fn get_unit_owner(&self, unit_id: &str) -> Option<String> {
        let entity = self.entity_registry.get(unit_id)?;
        let entity = *entity;
        self.world
            .get::<&UnitOwner>(entity)
            .ok()
            .map(|o| o.player_id.clone())
    }

    pub fn move_troops(&mut self, unit_id: &str, owner: &str, path: Vec<(f32, f32)>) {
        // Single ownership enforcement point in the game layer.
        if self.get_unit_owner(unit_id).as_deref() != Some(owner) {
            return;
        }
        if let Some(entity) = self.entity_registry.get(unit_id) {
            let entity = *entity;
            if let Ok((up,)) = self.world.query_one_mut::<(&mut UnitPath,)>(entity) {
                up.path = path.into_iter().collect();
            }
        }
    }

    pub fn toggle_healing(&mut self, unit_id: &str, owner: &str, enabled: bool) {
        // Single ownership enforcement point in the game layer.
        if self.get_unit_owner(unit_id).as_deref() != Some(owner) {
            return;
        }
        if let Some(entity) = self.entity_registry.get(unit_id) {
            let entity = *entity;
            if let Ok((flags,)) = self.world.query_one_mut::<(&mut UnitFlags,)>(entity) {
                flags.healing = enabled;
            }
        }
    }

    pub fn toggle_resupply(&mut self, unit_id: &str, owner: &str, enabled: bool) {
        // Single ownership enforcement point in the game layer.
        if self.get_unit_owner(unit_id).as_deref() != Some(owner) {
            return;
        }
        if let Some(entity) = self.entity_registry.get(unit_id) {
            let entity = *entity;
            if let Ok((flags,)) = self.world.query_one_mut::<(&mut UnitFlags,)>(entity) {
                flags.resupply_enabled = enabled;
            }
        }
    }

    pub fn upgrade_city(&mut self, city_id: &str, owner: &str) -> Result<(), String> {
        let (city_idx, next_level, cost) = {
            let wb = self
                .world
                .world_data_clone(self.world_entity)
                .ok_or("No world data")?;
            let city_idx = wb
                .cities
                .cities
                .iter()
                .position(|c| c.id == city_id && c.owner.as_deref() == Some(owner))
                .ok_or_else(|| format!("City {} not owned by {}", city_id, owner))?;
            let city = &wb.cities.cities[city_idx];
            let next_level = city.level + 1;
            let settings = &wb.city_upgrades.settings;
            if !settings.enabled {
                return Err("City upgrades disabled".into());
            }
            if next_level > settings.max_level {
                return Err(format!("City at max level ({})", settings.max_level));
            }
            let cost = settings.upgrade_cost(next_level);
            let current_funds = wb.funds.funds.get(owner).copied().unwrap_or(0);
            if current_funds < cost {
                return Err(format!(
                    "Insufficient funds: need {}, have {}",
                    cost, current_funds
                ));
            }
            (city_idx, next_level, cost)
        };

        let wb = self.world.world_data_mut(self.world_entity).unwrap();
        let settings = &wb.city_upgrades.settings;
        wb.cities.cities[city_idx].level = next_level;
        wb.cities.cities[city_idx].apply_level_settings(settings);
        if let Some(funds) = wb.funds.funds.get_mut(owner) {
            *funds = funds.saturating_sub(cost);
        }
        Ok(())
    }

    pub fn embark_unit(&mut self, unit_id: &str, player_id: &str) {
        let entity = match self.entity_registry.get(unit_id) {
            Some(e) => *e,
            None => return,
        };
        let owner = match self.world.get::<&UnitOwner>(entity) {
            Ok(o) => o.player_id.clone(),
            Err(_) => return,
        };
        if owner != player_id {
            return;
        }
        let flags = match self.world.get::<&UnitFlags>(entity) {
            Ok(f) => *f,
            Err(_) => return,
        };
        let wt = match self.world.get::<&UnitWaterTransition>(entity) {
            Ok(w) => (*w).clone(),
            Err(_) => return,
        };
        if flags.on_water || wt.embarking || wt.disembarking {
            return;
        }

        let pos = match self.world.get::<&UnitPosition>(entity) {
            Ok(p) => *p,
            Err(_) => return,
        };
        let current_terrain = self.world.terrain_at(self.world_entity, pos.x, pos.y);
        let target_is_water = {
            let path = match self.world.get::<&UnitPath>(entity) {
                Ok(p) => p,
                Err(_) => return,
            };
            if let Some((nx, ny)) = path.path.first() {
                self.world.terrain_at(self.world_entity, *nx, *ny) == Terrain::Water
            } else {
                current_terrain == Terrain::Water
            }
        };
        if !target_is_water && current_terrain != Terrain::Water {
            return;
        }

        let equipment = match self.world.get::<&UnitEquipment>(entity) {
            Ok(e) => (*e).clone(),
            Err(_) => return,
        };
        let buffer_percent = equipment.supply_buffer_percent as f32 / 100.0;
        let total_max: u32 = equipment.max_equipment.iter().map(|e| e.max_count).sum();
        if total_max == 0 {
            return;
        }

        let mut boat_supply = Vec::new();
        let mut max_boat_supply = Vec::new();
        let mut total_cost = 0.0f32;
        for (eq, max_eq) in equipment.equipment.iter().zip(&equipment.max_equipment) {
            let buffer_count = (max_eq.max_count as f32 * buffer_percent).ceil() as u32;
            total_cost += buffer_count as f32 * eq.cost_per_item;
            boat_supply.push(crate::game::unit::Equipment {
                name: eq.name.clone(),
                count: buffer_count,
                max_count: buffer_count,
                cost_per_item: eq.cost_per_item,
            });
            max_boat_supply.push(crate::game::unit::Equipment {
                name: eq.name.clone(),
                count: buffer_count,
                max_count: buffer_count,
                cost_per_item: eq.cost_per_item,
            });
        }
        let cost = total_cost.ceil() as u32;
        {
            let wb = self.world.world_data_mut(self.world_entity).unwrap();
            if let Some(funds) = wb.funds.funds.get_mut(&owner) {
                if *funds < cost {
                    return;
                }
                *funds -= cost;
            }
        }
        if let Ok((wt,)) = self
            .world
            .query_one_mut::<(&mut UnitWaterTransition,)>(entity)
        {
            wt.embarking = true;
            wt.embarking_ticks_left = crate::game::constants::EMBARK_TICKS;
            wt.boat_supply = boat_supply;
            wt.max_boat_supply = max_boat_supply;
        }
    }

    pub fn disembark_unit(&mut self, unit_id: &str, player_id: &str) {
        let entity = match self.entity_registry.get(unit_id) {
            Some(e) => *e,
            None => return,
        };
        let owner = match self.world.get::<&UnitOwner>(entity) {
            Ok(o) => o.player_id.clone(),
            Err(_) => return,
        };
        if owner != player_id {
            return;
        }
        let flags = match self.world.get::<&UnitFlags>(entity) {
            Ok(f) => *f,
            Err(_) => return,
        };
        let wt = match self.world.get::<&UnitWaterTransition>(entity) {
            Ok(w) => (*w).clone(),
            Err(_) => return,
        };
        if !flags.on_water || wt.embarking || wt.disembarking {
            return;
        }

        let pos = match self.world.get::<&UnitPosition>(entity) {
            Ok(p) => *p,
            Err(_) => return,
        };
        let current_terrain = self.world.terrain_at(self.world_entity, pos.x, pos.y);
        let target_is_land = {
            let path = match self.world.get::<&UnitPath>(entity) {
                Ok(p) => p,
                Err(_) => return,
            };
            if let Some((nx, ny)) = path.path.first() {
                self.world.terrain_at(self.world_entity, *nx, *ny) != Terrain::Water
            } else {
                current_terrain != Terrain::Water
            }
        };
        if !target_is_land && current_terrain == Terrain::Water {
            return;
        }

        if let Ok((wt,)) = self
            .world
            .query_one_mut::<(&mut UnitWaterTransition,)>(entity)
        {
            wt.disembarking = true;
            wt.disembarking_ticks_left = crate::game::constants::DISEMBARK_TICKS;
        }
    }

    // ====================================================================
    // Query helpers
    // ====================================================================

    pub fn query_all_units(&self) -> Vec<UnitQueryData> {
        let mut results = Vec::new();
        for (entity, owner, utype, display, pos, path, stats, equipment, flags, wt) in self
            .world
            .query::<(
                Entity,
                &UnitOwner,
                &UnitTypeId,
                &UnitDisplay,
                &UnitPosition,
                &UnitPath,
                &UnitStats,
                &UnitEquipment,
                &UnitFlags,
                &UnitWaterTransition,
            )>()
            .iter()
        {
            let eid = entity.id();
            results.push(UnitQueryData {
                id: format!("unit_{eid}"),
                owner: owner.player_id.clone(),
                position: pos.as_tuple(),
                unit_type: utype.name.clone(),
                symbol: display.symbol.clone(),
                men: stats.men,
                max_men: stats.max_men,
                organisation: stats.organisation,
                equipment: equipment.equipment.clone(),
                max_equipment: equipment.max_equipment.clone(),
                path: path.path.to_vec(),
                healing: flags.healing,
                resupply_enabled: flags.resupply_enabled,
                encircled: flags.encircled,
                fighting: flags.fighting,
                on_water: flags.on_water,
                embarking: wt.embarking,
                disembarking: wt.disembarking,
                boat_supply: wt.boat_supply.clone(),
                max_boat_supply: wt.max_boat_supply.clone(),
            });
        }
        results
    }

    pub fn query_cities(&self) -> Vec<City> {
        self.world
            .world_data(self.world_entity)
            .map(|wb| wb.cities.cities.clone())
            .unwrap_or_default()
    }

    /// W7: run `f` over the world's cities by reference, returning an owned
    /// `R`. hecs 0.11's `Ref` guard cannot yield an escaping raw borrow, so a
    /// `cities_ref() -> &[City]` accessor is not expressible; instead the
    /// conversion (e.g. `City` → wire `CityState`) happens inside `f` while the
    /// guard is alive, avoiding the redundant full `Vec<City>` clone that
    /// `query_cities()` performs for these hot per-tick callers.
    pub fn with_cities<R, F: FnOnce(&[City]) -> R>(&self, f: F) -> R {
        match self.world.world_data(self.world_entity) {
            Some(wb) => f(&wb.cities.cities),
            None => f(&[]),
        }
    }

    /// W7: allocation-free city count (for `entity_count` in the tick loop).
    pub fn city_count(&self) -> usize {
        self.with_cities(|cities| cities.len())
    }

    /// Assign ownership of a city to a player. Returns true if successful.
    pub fn assign_city_owner(&mut self, city_id: &str, owner: &str) -> bool {
        let changed = self
            .world
            .world_data_mut(self.world_entity)
            .map(|wb| {
                wb.cities
                    .cities
                    .iter_mut()
                    .find(|c| c.id == city_id)
                    .map(|c| c.owner = Some(owner.to_string()))
                    .is_some()
            })
            .unwrap_or(false);
        if changed {
            // City ownership changed — refresh the ctx snapshot inline (a
            // ctx_dirty.cities_changed flag set here would be wiped at the top
            // of the next tick_game) and mark influence stale so the grids
            // reflect the new owner (staleness fix).
            self.ctx.update_cities(&self.world, self.world_entity);
            self.influence_dirty = true;
        }
        changed
    }

    /// Get map width.
    #[allow(dead_code)] // Used in integration tests
    pub fn width(&self) -> u32 {
        self.world
            .world_data(self.world_entity)
            .map(|wb| wb.terrain.width)
            .unwrap_or(0)
    }

    /// Get map height.
    #[allow(dead_code)] // Used in integration tests
    pub fn height(&self) -> u32 {
        self.world
            .world_data(self.world_entity)
            .map(|wb| wb.terrain.height)
            .unwrap_or(0)
    }

    /// Get terrain grid as 2D Vec<Vec<String>>[y][x] for serialization.
    pub fn terrain_grid_2d(&self) -> Vec<Vec<String>> {
        self.world
            .world_data(self.world_entity)
            .map(|wb| {
                let w = wb.terrain.width as usize;
                let h = wb.terrain.height as usize;
                let grid = &wb.terrain.grid;
                (0..h)
                    .map(|y| {
                        (0..w)
                            .map(|x| match &grid[y * w + x] {
                                Terrain::Plains => "Plains".to_string(),
                                Terrain::Hills => "Hills".to_string(),
                                Terrain::Forest => "Forest".to_string(),
                                Terrain::Water => "Water".to_string(),
                                Terrain::Mountain => "Mountain".to_string(),
                            })
                            .collect()
                    })
                    .collect()
            })
            .unwrap_or_default()
    }

    /// Get vision grid for a player as 2D Vec<Vec<bool>>[y][x].
    /// Grids are stored flat internally; this converts for wire protocol compatibility.
    pub fn vision_grid_2d(&self, player_id: &str) -> Option<Vec<Vec<bool>>> {
        let wb = self.world.world_data(self.world_entity)?;
        let w = wb.terrain.width as usize;
        let h = wb.terrain.height as usize;
        wb.vision.grids.get(player_id).map(|flat| {
            (0..h)
                .map(|y| (0..w).map(|x| flat[y * w + x] != 0).collect())
                .collect()
        })
    }

    /// Get the territory grid (dominant player per cell) as 2D `Vec<Vec<u32>>`[y][x].
    /// Values are player indices (0..N) or 255 (NONE/unclaimed).
    /// Note: `player_id` is accepted for API compatibility but ignored —
    /// the territory grid is global (same for all players).
    pub fn border_grid_2d(&self, _player_id: &str) -> Option<Vec<Vec<u32>>> {
        let wb = self.world.world_data(self.world_entity)?;
        let grid = &wb.territory.grid;
        if grid.is_empty() {
            return None; // before first influence run
        }
        let w = wb.terrain.width as usize;
        let h = wb.terrain.height as usize;
        Some(
            (0..h)
                .map(|y| (0..w).map(|x| grid[y * w + x] as u32).collect())
                .collect(),
        )
    }

    /// Returns the per-player flat vision grid as an `Arc` (no allocation —
    /// just a refcount bump). Used by the network phase for O(1) change
    /// detection via `Arc::ptr_eq`.
    pub fn vision_grid_flat(&self, player_id: &str) -> Option<std::sync::Arc<Vec<u8>>> {
        let wb = self.world.world_data(self.world_entity)?;
        wb.vision.grids.get(player_id).cloned()
    }

    /// Returns the global flat territory grid as an `Arc` (no allocation).
    /// Values are player indices (0..N) or 255 (NONE/unclaimed).
    pub fn territory_grid_flat(&self) -> Option<std::sync::Arc<Vec<u8>>> {
        let wb = self.world.world_data(self.world_entity)?;
        if wb.territory.grid.is_empty() {
            return None;
        }
        Some(std::sync::Arc::clone(&wb.territory.grid))
    }

    /// Returns the terrain width in cells (0 if no world data).
    pub fn terrain_width(&self) -> u32 {
        self.world
            .world_data(self.world_entity)
            .map(|wb| wb.terrain.width)
            .unwrap_or(0)
    }

    /// Supervisor (spectator/replay) view: union of every player's vision.
    /// A cell is visible if any player currently sees it.
    pub fn supervisor_vision_grid(&self) -> Option<Vec<Vec<bool>>> {
        let ids: Vec<String> = self.all_player_funds().keys().cloned().collect();
        let mut out: Option<Vec<Vec<bool>>> = None;
        for id in ids {
            if let Some(grid) = self.vision_grid_2d(&id) {
                out = Some(match out {
                    None => grid,
                    Some(mut acc) => {
                        for (row, src) in acc.iter_mut().zip(grid) {
                            for (cell, s) in row.iter_mut().zip(src) {
                                *cell = *cell || s;
                            }
                        }
                        acc
                    }
                });
            }
        }
        out
    }

    /// Supervisor (spectator/replay) view: the territory grid (who dominates each cell).
    pub fn supervisor_border_grid(&self) -> Option<Vec<Vec<u32>>> {
        self.border_grid_2d("")
    }

    /// Get debug stats as JSON (only available in debug builds).
    #[allow(dead_code)]
    #[cfg(any(debug_assertions, feature = "debug-overlay"))]
    pub fn get_debug_stats(&self) -> Option<serde_json::Value> {
        super::debug_stats::get_debug_stats(&self.world, self.world_entity)
    }

    /// Set which systems should record detailed sub-part timings (benchmark mode).
    #[allow(dead_code)]
    #[cfg(any(debug_assertions, feature = "debug-overlay"))]
    pub fn set_detailed_systems(&mut self, systems: Vec<String>) {
        use std::collections::HashSet;
        if let Some(wb) = self.world.world_data_mut(self.world_entity) {
            wb.debug_stats.detailed_systems = systems.into_iter().collect::<HashSet<_>>();
            wb.debug_stats.detailed_timings.clear();
        }
    }

    #[cfg(not(any(debug_assertions, feature = "debug-overlay")))]
    pub fn set_detailed_systems(&mut self, _systems: Vec<String>) {}

    /// Clear per-system allocation totals. Called by the benchmark after warmup
    /// so the per-system alloc table (and its reconciliation with the measured
    /// per-tick summary) reflects measured (steady-state) ticks only.
    #[allow(dead_code)]
    #[cfg(all(
        any(debug_assertions, feature = "debug-overlay"),
        feature = "bench-alloc"
    ))]
    pub fn reset_system_allocs(&mut self) {
        if let Some(wb) = self.world.world_data_mut(self.world_entity) {
            wb.debug_stats.system_allocs.clear();
        }
    }

    #[cfg(not(all(
        any(debug_assertions, feature = "debug-overlay"),
        feature = "bench-alloc"
    )))]
    #[allow(dead_code)]
    pub fn reset_system_allocs(&mut self) {}

    pub fn union_vision_grids(&self, player_ids: &[String]) -> Option<Vec<Vec<bool>>> {
        let wb = self.world.world_data(self.world_entity)?;
        if player_ids.is_empty() {
            return None;
        }
        let (w, h) = (wb.terrain.width, wb.terrain.height);
        let wusize = w as usize;
        let husize = h as usize;
        let grid_size = wusize * husize;
        // Accumulate union in flat array, then convert to 2D for return
        let mut result_flat = vec![0u8; grid_size];
        for pid in player_ids {
            if let Some(grid) = wb.vision.grids.get(pid) {
                for i in 0..grid_size {
                    result_flat[i] |= grid[i];
                }
            }
        }
        // Convert flat → 2D for API compatibility
        let result: Vec<Vec<bool>> = (0..husize)
            .map(|y| {
                (0..wusize)
                    .map(|x| result_flat[y * wusize + x] != 0)
                    .collect()
            })
            .collect();
        Some(result)
    }

    /// Union of every player's vision as a flat `Vec<u8>` (no 2-D
    /// conversion). Used by the spectator network path.
    pub fn union_vision_grid_flat(&self, player_ids: &[String]) -> Option<Vec<u8>> {
        let wb = self.world.world_data(self.world_entity)?;
        if player_ids.is_empty() {
            return None;
        }
        let w = wb.terrain.width as usize;
        let h = wb.terrain.height as usize;
        let grid_size = w * h;
        let mut result_flat = vec![0u8; grid_size];
        for pid in player_ids {
            if let Some(grid) = wb.vision.grids.get(pid) {
                for i in 0..grid_size {
                    result_flat[i] |= grid[i];
                }
            }
        }
        Some(result_flat)
    }

    pub fn player_funds(&self, player_id: &str) -> u32 {
        self.world.player_funds(self.world_entity, player_id)
    }

    #[allow(dead_code)] // Used in integration tests (is_player_active_requires_units_or_cities)
    pub fn is_player_active(&self, player_id: &str) -> bool {
        let has_units = self
            .world
            .query::<(Entity, &UnitOwner)>()
            .iter()
            .any(|(_, owner)| owner.player_id == player_id);
        let has_cities = self
            .world
            .world_data(self.world_entity)
            .map(|wb| {
                wb.cities
                    .cities
                    .iter()
                    .any(|c| c.owner.as_deref() == Some(player_id))
            })
            .unwrap_or(false);
        has_units || has_cities
    }

    pub fn determine_winner(&self) -> Option<String> {
        let mut players_with_power: Vec<(String, u32)> = Vec::new();
        for pid in self.players.keys() {
            let unit_count = self
                .world
                .query::<(Entity, &UnitOwner)>()
                .iter()
                .filter(|(_, owner)| &owner.player_id == pid)
                .count() as u32;
            let city_count = self
                .world
                .world_data(self.world_entity)
                .map(|wb| {
                    wb.cities
                        .cities
                        .iter()
                        .filter(|c| c.owner.as_deref() == Some(pid))
                        .count() as u32
                })
                .unwrap_or(0);
            let total_funds = self.world.player_funds(self.world_entity, pid);
            if unit_count > 0 || city_count > 0 {
                players_with_power.push((pid.clone(), unit_count + city_count + total_funds / 100));
            }
        }
        players_with_power.sort_by_key(|b| std::cmp::Reverse(b.1));
        players_with_power.first().map(|(pid, _)| pid.to_string())
    }

    fn track_unit_losses(&mut self) {
        // Count units per player WITHOUT a per-unit String clone. The prior
        // `entry(owner.player_id.clone())` heap-allocated + freed one String per
        // unit per tick (~9 600 allocs/tick at ~10 k units) just to probe a map
        // that has only a handful of keys. The player set is tiny and only grows
        // on recruit (which pre-inserts the owner into `prev_unit_counts` before
        // any unit can hold it), so match each unit's borrowed owner id against
        // the few known keys by str equality — no per-unit allocation.
        let ids: Vec<String> = self.prev_unit_counts.keys().cloned().collect();
        let mut counts = vec![0usize; ids.len()];
        for (_, owner) in self.world.query::<(Entity, &UnitOwner)>().iter() {
            for i in 0..ids.len() {
                if ids[i] == owner.player_id {
                    counts[i] += 1;
                    break;
                }
            }
        }
        for i in 0..ids.len() {
            let current = counts[i];
            let entry = self.prev_unit_counts.get_mut(&ids[i]).unwrap();
            let lost = (*entry).saturating_sub(current);
            if lost > 0 {
                if let Some(stats) = self.player_stats.get_mut(&ids[i]) {
                    stats.units_lost += lost as u32;
                }
            }
            *entry = current;
        }
    }

    // ====================================================================
    // Accessors
    // ====================================================================

    pub fn players(&self) -> &HashMap<String, Player> {
        &self.players
    }
    pub fn settings(&self) -> &GameSettings {
        &self.settings
    }
    #[allow(dead_code)] // Used in benchmarks and tests
    pub fn tick_number(&self) -> u64 {
        self.tick
    }
    #[allow(dead_code)] // Used in integration tests
    pub fn is_running(&self) -> bool {
        self.running
    }
    pub fn player_stats(&self) -> &HashMap<String, PlayerGameStats> {
        &self.player_stats
    }

    /// Get all unit data as UnitState for serialization.
    pub fn unit_states(&self) -> Vec<crate::server::protocol::UnitState> {
        self.query_all_units()
            .into_iter()
            .map(|u| crate::server::protocol::UnitState {
                id: u.id.clone(),
                owner: u.owner.clone(),
                position: u.position,
                unit_type: u.unit_type.clone(),
                symbol: u.symbol.clone(),
                men: u.men,
                max_men: u.max_men,
                organisation: u.organisation,
                equipment: u
                    .equipment
                    .iter()
                    .map(|e| crate::server::protocol::EquipmentState {
                        name: e.name.clone(),
                        count: e.count,
                        cost_per_item: e.cost_per_item,
                    })
                    .collect(),
                max_equipment: u
                    .max_equipment
                    .iter()
                    .map(|e| crate::server::protocol::EquipmentState {
                        name: e.name.clone(),
                        count: e.count,
                        cost_per_item: e.cost_per_item,
                    })
                    .collect(),
                path: u.path.clone(),
                healing: u.healing,
                resupply_enabled: u.resupply_enabled,
                supply_percent: u.supply_percent(),
                encircled: u.encircled,
                attacking: u.fighting,
                on_water: u.on_water,
                boat_supply_percent: u.boat_supply_percent(),
                embarking: u.embarking,
                disembarking: u.disembarking,
            })
            .collect()
    }

    /// Get all unit data as UnitState for serialization (direct single-pass,
    /// no intermediate `UnitQueryData`). Used by the network phase.
    pub fn unit_states_direct(&self) -> Vec<crate::server::protocol::UnitState> {
        use crate::server::protocol::{EquipmentState, UnitState};
        self.world
            .query::<(
                Entity,
                &UnitOwner,
                &UnitTypeId,
                &UnitDisplay,
                &UnitPosition,
                &UnitPath,
                &UnitStats,
                &UnitEquipment,
                &UnitFlags,
                &UnitWaterTransition,
            )>()
            .iter()
            .map(
                |(entity, owner, utype, display, pos, path, stats, equipment, flags, wt)| {
                    let eid = entity.id();
                    // supply_percent: same formula as UnitQueryData::supply_percent
                    let total_needed: f32 = equipment
                        .max_equipment
                        .iter()
                        .map(|me| (me.max_count as f32) * 0.9)
                        .sum();
                    let total_carried: f32 =
                        equipment.equipment.iter().map(|e| e.count as f32).sum();
                    let supply_percent = if total_needed <= 0.0 {
                        100.0
                    } else {
                        (total_carried / total_needed * 100.0).clamp(0.0, 100.0)
                    };
                    // boat_supply_percent
                    let total_boat_max: f32 =
                        wt.max_boat_supply.iter().map(|e| e.max_count as f32).sum();
                    let total_boat_cur: f32 = wt.boat_supply.iter().map(|e| e.count as f32).sum();
                    let boat_supply_percent = if total_boat_max <= 0.0 {
                        0.0
                    } else {
                        (total_boat_cur / total_boat_max * 100.0).clamp(0.0, 100.0)
                    };
                    UnitState {
                        id: format!("unit_{eid}"),
                        owner: owner.player_id.clone(),
                        position: pos.as_tuple(),
                        unit_type: utype.name.clone(),
                        symbol: display.symbol.clone(),
                        men: stats.men,
                        max_men: stats.max_men,
                        organisation: stats.organisation,
                        equipment: equipment
                            .equipment
                            .iter()
                            .map(|e| EquipmentState {
                                name: e.name.clone(),
                                count: e.count,
                                cost_per_item: e.cost_per_item,
                            })
                            .collect(),
                        max_equipment: equipment
                            .max_equipment
                            .iter()
                            .map(|e| EquipmentState {
                                name: e.name.clone(),
                                count: e.count,
                                cost_per_item: e.cost_per_item,
                            })
                            .collect(),
                        path: path.path.to_vec(),
                        healing: flags.healing,
                        resupply_enabled: flags.resupply_enabled,
                        supply_percent,
                        encircled: flags.encircled,
                        attacking: flags.fighting,
                        on_water: flags.on_water,
                        boat_supply_percent,
                        embarking: wt.embarking,
                        disembarking: wt.disembarking,
                    }
                },
            )
            .collect()
    }

    /// Get all funds for all players.
    pub fn all_player_funds(&self) -> HashMap<String, u32> {
        let mut funds = HashMap::new();
        for pid in self.players.keys() {
            funds.insert(pid.clone(), self.player_funds(pid));
        }
        funds
    }
}

/// Flat data structure for unit queries.
#[derive(Debug, Clone)]
pub struct UnitQueryData {
    pub id: String,
    pub owner: String,
    pub position: (f32, f32),
    pub unit_type: String,
    pub symbol: String,
    pub men: f32,
    pub max_men: f32,
    pub organisation: f32,
    pub equipment: Vec<crate::game::unit::Equipment>,
    pub max_equipment: Vec<crate::game::unit::Equipment>,
    pub path: Vec<(f32, f32)>,
    pub healing: bool,
    pub resupply_enabled: bool,
    pub encircled: bool,
    pub fighting: bool,
    pub on_water: bool,
    pub embarking: bool,
    pub disembarking: bool,
    pub boat_supply: Vec<crate::game::unit::Equipment>,
    pub max_boat_supply: Vec<crate::game::unit::Equipment>,
}

impl UnitQueryData {
    pub fn supply_percent(&self) -> f32 {
        if self.max_equipment.is_empty() {
            return 100.0;
        }
        let total_needed: f32 = self
            .max_equipment
            .iter()
            .map(|me| (me.max_count as f32) * 0.9)
            .sum();
        let total_carried: f32 = self.equipment.iter().map(|e| e.count as f32).sum();
        if total_needed <= 0.0 {
            return 100.0;
        }
        (total_carried / total_needed * 100.0).clamp(0.0, 100.0)
    }

    pub fn boat_supply_percent(&self) -> f32 {
        if self.max_boat_supply.is_empty() {
            return 0.0;
        }
        let total_max: f32 = self
            .max_boat_supply
            .iter()
            .map(|e| e.max_count as f32)
            .sum();
        if total_max <= 0.0 {
            return 0.0;
        }
        let total_current: f32 = self.boat_supply.iter().map(|e| e.count as f32).sum();
        (total_current / total_max * 100.0).clamp(0.0, 100.0)
    }
}

pub mod systems {
    pub use crate::game::systems::*;
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::CityUpgradeSettings;
    use crate::game::components::UnitStats;
    use crate::game::economy::UnitType;
    use crate::game::unit::Equipment;
    use crate::game::world_ext::WorldExt;
    use crate::map::map::{Map, MapConfig};
    use std::sync::Arc;

    fn basic_unit_type() -> UnitType {
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
            soft_defense: 5.5,
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

    fn test_settings() -> GameSettings {
        GameSettings {
            starting_funds: 1000,
            initial_manpower: 10_000,
            funds_per_city: 1.0,
            recruitment_rate: 1.0,
            game_speed: 1.0,
            use_standard_ruleset: true,
            unit_types: vec![basic_unit_type()],
            city_upgrade_settings: CityUpgradeSettings::default(),
            water_speed: 0.5,
            influence: InfluenceSettings::default(),
        }
    }

    fn test_map() -> Map {
        let config = MapConfig {
            width: 20,
            height: 20,
            cities: vec![(3.0, 3.0), (16.0, 3.0), (3.0, 16.0), (16.0, 16.0)],
            terrain_data: None,
        };
        Map::from_config(&config)
    }

    fn border_at(game: &Game, player: &str, x: i32, y: i32) -> u32 {
        game.border_grid_2d(player)
            .map(|g| g[y as usize][x as usize])
            .unwrap_or(0)
    }

    /// Proves `track_unit_losses` no longer clones a String per unit per tick.
    /// The count must stay ~constant (a handful, the player-id key table) as
    /// the unit count grows — the old `entry(owner.player_id.clone())` scaled
    /// 1:1 with units (~9 600 allocs/tick at ~10 k units, the flat "other /
    /// overhead" alloc spike in the stress report). Run single-threaded so the
    /// global counting allocator's delta around the call is clean:
    /// `cargo test --release --features bench-alloc track_unit_losses -- --test-threads=1 --nocapture`
    #[cfg(feature = "bench-alloc")]
    #[test]
    fn track_unit_losses_alloc_does_not_scale_with_units() {
        use crate::bench_alloc;
        fn measure(n_units: usize) -> (u64, u64) {
            let mut game = Game::new(test_map(), test_settings(), 2);
            game.start();
            let ut = basic_unit_type();
            let owners = ["player_0", "player_1"];
            for i in 0..n_units {
                game.world
                    .spawn_unit(&ut, owners[i % owners.len()], (1.0, 1.0));
            }
            // Recruit normally pre-inserts owners here; do the same.
            for o in owners {
                game.prev_unit_counts.insert(o.to_string(), 0);
            }
            bench_alloc::snapshot(); // settle max-window
            let a = bench_alloc::peek();
            game.track_unit_losses();
            let b = bench_alloc::peek();
            let d = b.diff(a);
            (d.allocs, d.bytes)
        }
        let (a1000, b1000) = measure(1_000);
        let (a10000, b10000) = measure(10_000);
        eprintln!(
            "track_unit_losses allocs @1000 = {a1000} ({b1000} B)  @10000 = {a10000} ({b10000} B)"
        );
        assert!(
            a10000 < 50,
            "allocs must be ~constant (player-id table), not ~n_units (got {a10000})"
        );
        assert!(
            a10000 <= a1000 + 5,
            "allocs must not grow with n_units (1000→{a1000}, 10000→{a10000})"
        );
    }

    #[test]
    fn recruit_marks_influence_dirty_and_grid_updates() {
        let mut game = Game::new(test_map(), test_settings(), 2);
        game.assign_city_owner("city_0", "player_0");
        game.start();

        let ut = basic_unit_type();
        let unit_id = game.recruit_unit("city_0", &ut, "player_0").unwrap();
        assert!(game.entity_registry.contains_key(&unit_id));

        // Before any tick: the new unit is not yet grouped, influence is dirty.
        assert_eq!(game.ctx_unit_count_for_test(), 0);
        assert!(game.influence_dirty_for_test());

        // Tick to the next multiple of 3: rebuild + influence run.
        game.tick_game(); // tick 1
        game.tick_game(); // tick 2
        game.tick_game(); // tick 3 → consumer tick
        assert_eq!(
            game.ctx_unit_count_for_test(),
            1,
            "rebuild must include the recruit"
        );
        assert!(!game.influence_dirty_for_test());
        // Territory: (3,3) is player_0's (city + unit) → value = player index 0.
        assert_eq!(
            border_at(&game, "player_0", 3, 3),
            0,
            "(3,3) must be player_0 territory after the recruit"
        );
    }

    #[test]
    fn quiet_play_skips_influence() {
        let mut game = Game::new(test_map(), test_settings(), 2);
        game.assign_city_owner("city_0", "player_0");
        game.start();

        let ut = basic_unit_type();
        let _unit_id = game.recruit_unit("city_0", &ut, "player_0").unwrap();
        // No orders, no combat → nothing moves after the first influence run.

        for _ in 0..3 {
            game.tick_game();
        }
        assert!(
            !game.influence_dirty_for_test(),
            "tick 3 must consume the dirt"
        );
        let snap_3 = game.border_grid_2d("player_0").unwrap();
        let ptr_3 = Arc::as_ptr(
            &game
                .world
                .world_data(game.world_entity)
                .unwrap()
                .territory
                .grid,
        ) as *const ();

        for _ in 0..3 {
            game.tick_game();
        }
        assert_eq!(game.tick_number(), 6);
        assert!(
            !game.influence_dirty_for_test(),
            "no changes → must stay clean"
        );
        assert_eq!(game.border_grid_2d("player_0").unwrap(), snap_3);
        assert_eq!(
            Arc::as_ptr(
                &game
                    .world
                    .world_data(game.world_entity)
                    .unwrap()
                    .territory
                    .grid,
            ) as *const (),
            ptr_3,
            "clean tick must not replace the grid Arc"
        );
    }

    #[test]
    fn death_removes_stale_unit_from_ctx_and_grid() {
        let mut game = Game::new(test_map(), test_settings(), 2);
        game.assign_city_owner("city_0", "player_0");
        game.assign_city_owner("city_1", "player_1");
        game.start();

        let ut = basic_unit_type();
        let unit_id = game
            .recruit_unit("city_0", &ut, "player_0")
            .expect("recruit");
        // Walk the unit away from the city so its grid cells are distinguishable.
        // Unit speed on Plains is 0.1/tick; (3,3)→(10,10) is ~9.9 → ~99 ticks.
        game.move_troops(&unit_id, "player_0", vec![(10.0, 10.0)]);

        for _ in 0..110 {
            game.tick_game();
        }
        // Unit arrived at (10,10); territory must show player_0 there (index 0).
        assert_eq!(
            border_at(&game, "player_0", 10, 10),
            0,
            "unit at (10,10) → player_0 territory"
        );
        assert_eq!(game.ctx_unit_count_for_test(), 1);

        // Force death deterministically: men <= 0 → the surrender sweep (every
        // 30 ticks) returns it for death_cleanup. Supply skips zero-men units,
        // so the 0 is stable in the meantime. Run until it is reaped.
        let entity = game.entity_registry[&unit_id];
        {
            let (stats,) = game
                .world
                .query_one_mut::<(&mut UnitStats,)>(entity)
                .expect("unit entity must exist");
            stats.men = 0.0;
        }
        for _ in 0..35 {
            if game.query_all_units().is_empty() {
                break;
            }
            game.tick_game();
        }

        assert!(
            game.query_all_units().is_empty(),
            "unit must be dead and despawned"
        );
        // The rebuild + influence run on the NEXT consumer tick (movement),
        // which precedes death_cleanup in a tick — so allow a few more ticks
        // before the stale unit is purged from ctx and the grid.
        for _ in 0..3 {
            game.tick_game();
        }
        assert_eq!(
            game.ctx_unit_count_for_test(),
            0,
            "rebuild after death must drop the stale unit"
        );
        assert!(!game.influence_dirty_for_test());
        assert_eq!(
            border_at(&game, "player_0", 10, 10),
            255,
            "dead unit → (10,10) is unclaimed (NONE)"
        );
    }

    #[test]
    fn assign_city_owner_updates_influence_grid() {
        let mut game = Game::new(test_map(), test_settings(), 2);
        game.start();

        // Both players own a city and recruit a unit there.
        assert!(game.assign_city_owner("city_0", "player_0"));
        assert!(game.assign_city_owner("city_1", "player_1"));
        let ut = basic_unit_type();
        assert!(game.recruit_unit("city_0", &ut, "player_0").is_some());
        assert!(game.recruit_unit("city_1", &ut, "player_1").is_some());

        assert!(game.influence_dirty_for_test());
        for _ in 0..3 {
            game.tick_game();
        }
        assert!(!game.influence_dirty_for_test());
        // Territory: (3,3) is player_0's (city + unit) → player index 0.
        assert_eq!(
            border_at(&game, "player_0", 3, 3),
            0,
            "(3,3) is player_0 territory"
        );
        // (9,4) sits in city_0's r7 ring (dist ~6.08) but outside the unit's
        // r5 and outside player_1's city_1 r7 → also player_0 territory.
        assert_eq!(
            border_at(&game, "player_0", 9, 4),
            0,
            "(9,4) is player_0's city ring"
        );

        // Steal city_0 from player_0 to player_1 → next influence run must
        // move the city projection. (assign_city_owner refreshes ctx.cities
        // inline; a ctx_dirty flag set here would be wiped at tick start.)
        assert!(game.assign_city_owner("city_0", "player_1"));
        for _ in 0..3 {
            game.tick_game();
        }
        assert!(!game.influence_dirty_for_test());
        // (3,3): player_0's unit (r5) still covers it and tie-breaks (sorted
        // first) against player_1's now-stolen city → stays player_0.
        assert_eq!(
            border_at(&game, "player_0", 3, 3),
            0,
            "(3,3) still player_0 — its unit tie-breaks"
        );
        // (9,4): now in player_1's city ring, no longer player_0's → player_1 (index 1).
        assert_eq!(
            border_at(&game, "player_0", 9, 4),
            1,
            "(9,4) flipped to player_1 after the city was stolen"
        );
    }

    // ---------------------------------------------------------------------------
    // recruit_unit — circle occupancy gate (MIN_UNIT_DISTANCE)
    // ---------------------------------------------------------------------------

    /// Read (funds, manpower) for a player from the world bundle.
    fn funds_manpower(game: &Game, player: &str) -> (u32, u32) {
        let wb = game.world.world_data(game.world_entity).unwrap();
        let funds = wb.funds.funds.get(player).copied().unwrap_or(0);
        let manpower = wb.funds.manpower.get(player).copied().unwrap_or(0);
        (funds, manpower)
    }

    /// Count units (entities carrying a UnitPosition) in the world.
    fn unit_count(game: &Game) -> usize {
        game.world
            .query::<(&crate::game::components::UnitPosition,)>()
            .iter()
            .count()
    }

    /// The first recruit at a city succeeds; a second recruit at the *same*
    /// city is blocked (a unit already sits at the city point, dist 0 <
    /// MIN_UNIT_DISTANCE) and deducts nothing.
    #[test]
    fn recruit_gate_blocks_second_unit_at_city() {
        let mut game = Game::new(test_map(), test_settings(), 2);
        game.assign_city_owner("city_0", "player_0");
        game.start();
        let ut = basic_unit_type();

        // First recruit at city_0 (3,3): succeeds, spends resources.
        assert!(game.recruit_unit("city_0", &ut, "player_0").is_some());
        assert_eq!(unit_count(&game), 1);
        let (funds_after_1, mp_after_1) = funds_manpower(&game, "player_0");

        // Second recruit at the same city: blocked → None, spends nothing.
        assert!(
            game.recruit_unit("city_0", &ut, "player_0").is_none(),
            "a unit is already at the city point — second recruit must be blocked"
        );
        assert_eq!(
            unit_count(&game),
            1,
            "blocked recruit must not spawn a unit"
        );
        let (funds_after_2, mp_after_2) = funds_manpower(&game, "player_0");
        assert_eq!(
            (funds_after_2, mp_after_2),
            (funds_after_1, mp_after_1),
            "blocked recruit must deduct nothing"
        );
    }

    /// A unit at one city does not block recruitment at a far-away city
    /// (city_0 (3,3) vs city_1 (16,3): dist 13 > MIN_UNIT_DISTANCE 8).
    #[test]
    fn recruit_gate_allows_far_city() {
        let mut game = Game::new(test_map(), test_settings(), 2);
        game.assign_city_owner("city_0", "player_0");
        game.assign_city_owner("city_1", "player_1");
        game.start();
        let ut = basic_unit_type();

        assert!(game.recruit_unit("city_0", &ut, "player_0").is_some());
        assert!(
            game.recruit_unit("city_1", &ut, "player_1").is_some(),
            "far city (dist 13 > 8) must not be blocked by the unit at city_0"
        );
        assert_eq!(unit_count(&game), 2);
    }

    /// The bypass (`recruit_unit_at`) ignores the gate: two units may be placed
    /// at the exact same position (used by premade/benchmark fixtures that
    /// stack units per city). It deducts resources and registers id/owner
    /// identically to the gated path.
    #[test]
    fn recruit_unit_at_bypass_allows_stacking() {
        let mut game = Game::new(test_map(), test_settings(), 2);
        game.assign_city_owner("city_0", "player_0");
        game.start();
        let ut = basic_unit_type();
        let (funds_before, mp_before) = funds_manpower(&game, "player_0");

        // A normal (gated) recruit at city_0 (3,3).
        assert!(game.recruit_unit("city_0", &ut, "player_0").is_some());
        // A bypass recruit at the *same* point — gate is skipped.
        let id = game
            .recruit_unit_at(&ut, "player_0", (3.0, 3.0))
            .expect("bypass must not be subject to the occupancy gate");
        assert_eq!(unit_count(&game), 2, "two units may now occupy (3,3)");

        // Identical bookkeeping to the gated path: id registered, owner set,
        // resources deducted.
        assert!(
            game.entity_registry.contains_key(&id),
            "bypass must register the unit id"
        );
        assert_eq!(
            game.get_unit_owner(&id).as_deref(),
            Some("player_0"),
            "bypass must set the owner identically to recruit_unit"
        );
        let (funds_after, mp_after) = funds_manpower(&game, "player_0");
        assert_eq!(
            funds_after,
            funds_before.saturating_sub(2 * ut.recruitment_cost),
            "both recruits must deduct the recruitment cost"
        );
        assert_eq!(
            mp_after,
            mp_before - 2 * ut.men,
            "both recruits must deduct manpower"
        );
    }

    /// The gate is a live scan of current positions, not sticky state: once the
    /// unit that was blocking the city has moved away, the next recruit at the
    /// same city succeeds again.
    #[test]
    fn recruit_gate_reallows_after_unit_moves_away() {
        let mut game = Game::new(test_map(), test_settings(), 2);
        game.assign_city_owner("city_0", "player_0");
        game.start();
        let ut = basic_unit_type();

        // First recruit sits on the city (3,3) and blocks a second.
        let id = game.recruit_unit("city_0", &ut, "player_0").unwrap();
        assert!(game.recruit_unit("city_0", &ut, "player_0").is_none());

        // The unit moves away (relocate its position far from the city).
        let entity = *game.entity_registry.get(&id).unwrap();
        let (pos,) = game
            .world
            .query_one_mut::<(&mut crate::game::components::UnitPosition,)>(entity)
            .unwrap();
        pos.x = 10.0; // (10,10) is 9.9 from (3,3) → outside MIN_UNIT_DISTANCE
        pos.y = 10.0;

        // Now the same city is free again → the next recruit succeeds.
        assert!(
            game.recruit_unit("city_0", &ut, "player_0").is_some(),
            "gate must re-allow once the blocker has moved away"
        );
        assert_eq!(unit_count(&game), 2);
    }

    /// T-3: the dense owner table must survive hecs id-reuse after death.
    /// Unit A (id, gen) is despawned; unit B reuses the same id at gen+1 with
    /// a different owner. `owner_index` for B must return B's owner, and a
    /// stale handle to A's (id, gen) must resolve to OWNER_NONE — the
    /// generation guard is what makes the dense Vec safe.
    #[test]
    fn owner_index_generation_guard_survives_id_reuse() {
        let mut world = hecs::World::new();
        let wb = WorldBundle::new(
            vec![vec![Terrain::Plains; 4]; 4],
            4,
            4,
            CityUpgradeSettings::default(),
            HashMap::new(),
        );
        let world_entity = world.spawn((wb,));

        // Unit A: owner "P1" (UnitPosition required by rebuild_both's query).
        let a = world.spawn((
            UnitOwner {
                player_id: "P1".to_string(),
            },
            UnitPosition { x: 1.0, y: 1.0 },
        ));
        let mut ctx = TickContext::build_initial(&world, world_entity);
        let p1 = ctx.player_id_to_index["P1"];
        ctx.owner_set(a, p1);
        assert_eq!(ctx.owner_index(a), p1);

        // A dies → its id is freed (generation will bump on reuse).
        world.despawn(a).unwrap();

        // Unit B reuses the same id at the next generation, owner "P2".
        let b = world.spawn((
            UnitOwner {
                player_id: "P2".to_string(),
            },
            UnitPosition { x: 1.0, y: 1.0 },
        ));
        assert_eq!(b.id(), a.id(), "hecs must reuse the freed id");
        // Generation is the upper 32 bits of the entity bits.
        let gen = |e: hecs::Entity| (e.to_bits().get() >> 32) as u32;
        assert_ne!(gen(b), gen(a), "generation must bump");

        // Rebuild from the world: B must now own the id, A's stale (id,gen)
        // must no longer resolve.
        ctx.rebuild_both(&world);
        let p2 = ctx.player_id_to_index["P2"];
        assert_eq!(ctx.owner_index(b), p2);
        assert_eq!(ctx.owner_index(a), OWNER_NONE);
    }

    /// S-1: the per-type equipment `needed` cache must equal the sum of each
    /// unit type template's max_count. This is the invariant the supply system
    /// relies on (per-unit max_equipment is a clone of the template at spawn).
    #[test]
    fn unit_type_total_max_equipment_matches_templates() {
        let game = Game::new(test_map(), test_settings(), 1);
        assert_eq!(
            game.unit_type_total_max_equipment.len(),
            game.settings.unit_types.len()
        );
        for (i, ut) in game.settings.unit_types.iter().enumerate() {
            let sum: f32 = ut.equipment.iter().map(|e| e.max_count as f32).sum();
            assert_eq!(game.unit_type_total_max_equipment[i], sum);
        }
    }
}
