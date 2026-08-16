//! Game facade — holds the ECS world, settings, and dispatches systems.

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
#[cfg(any(debug_assertions, feature = "debug-overlay"))]
macro_rules! tick_system {
    ($name:expr, $world:expr, $world_entity:expr, $expr:expr) => {{
        let _start = std::time::Instant::now();
        let _result = $expr;
        crate::game::debug_stats::record_system_timing(
            $world,
            $world_entity,
            $name,
            _start.elapsed().as_secs_f64() * 1000.0,
        );
        _result
    }};
}

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
    world: World,
    world_entity: hecs::Entity,
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
    /// Unit-type name -> index into `settings.unit_types`, built once at
    /// `Game::new` so the supply system does not rebuild the map every tick.
    unit_types_index: HashMap<String, usize>,
    /// Pre-allocated movement scratch, cleared and reused each tick. Holds the
    /// mover records that movement system uses.
    movement_scratch: systems::movement::MovementScratch,
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
}

/// Neighbour-graph snapshot: how nearby_enemies left the world this tick.
/// `attacking`/`NearbyEnemies` are nearby_enemies-owned, so post-combat reads
/// reflect the current tick's nearby_enemies output. Used by benchmark/status.
#[allow(dead_code)]
#[derive(Debug, Clone, Copy, Default)]
pub struct CombatHealth {
    /// Living units with UnitFlags + NearbyEnemies.
    pub units: usize,
    /// Units with `attacking == true` (movers that found enemies nearby).
    pub units_with_attacking: usize,
    /// Units whose NearbyEnemies list is non-empty.
    pub units_with_enemies: usize,
    /// Sum of NearbyEnemies list lengths across all units.
    pub total_nearby_entries: usize,
}

/// Combat resolution-stage counters from one tick's `combat::run`.
#[derive(Debug, Clone, Copy, Default)]
#[allow(dead_code)]
pub struct CombatDiag {
    /// Candidate units entering the combat loop (attacking || fighting).
    pub candidates: usize,
    /// Candidates with `attacking == true`.
    pub attackers: usize,
    /// Neighbour-list iterations across all candidates.
    pub pairs_scanned: usize,
    /// Pairs skipped because they share an owner.
    pub pairs_same_owner: usize,
    /// Pairs skipped because they are out of combat range.
    pub pairs_out_of_range: usize,
    /// Pairs skipped because they already resolved this tick.
    pub pairs_already_resolved: usize,
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
    /// Compact owner lookup: entity → player index (0..=255).
    /// Shared across systems so nearby_enemies and combat can do
    /// O(1) u8 equality instead of per-pair ECS borrows + String comparison.
    /// Updated incrementally (O(1) insert on recruit; stale entries on death
    /// are harmless since dead entities don't appear in ECS queries).
    pub owner_id_by_entity: HashMap<Entity, u8>,
    /// Persistent player→index mapping for incremental owner map updates.
    pub player_id_to_index: HashMap<String, u8>,
    /// Next available player index (monotonically increasing, never reused).
    pub next_player_idx: u8,
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

        // Populate player_id_to_index and owner_id_by_entity in a single ECS query
        let mut player_id_to_index: HashMap<String, u8> = HashMap::with_capacity(16);
        let mut owner_id_by_entity: HashMap<Entity, u8> = HashMap::new();
        let mut next_player_idx: u8 = 0;
        for (entity, owner) in world.query::<(Entity, &UnitOwner)>().iter() {
            let idx = match player_id_to_index.entry(owner.player_id.clone()) {
                Entry::Occupied(e) => *e.get(),
                Entry::Vacant(e) => {
                    let id = next_player_idx;
                    next_player_idx += 1;
                    e.insert(id);
                    id
                }
            };
            owner_id_by_entity.insert(entity, idx);
        }
        debug_assert!(next_player_idx < 255, "too many players");

        Self {
            world_entity,
            cities,
            width,
            height,
            units_by_player: HashMap::new(),
            owner_id_by_entity,
            player_id_to_index,
            next_player_idx,
        }
    }

    /// Rebuild units_by_player from ECS (positions, entity set).
    /// Called just-in-time before influence/encirclement when positions have changed.
    pub fn rebuild_units_by_player(&mut self, world: &World) {
        let mut new_units: HashMap<String, Vec<UnitHandle>> = HashMap::new();

        for (entity, owner, pos) in world.query::<(Entity, &UnitOwner, &UnitPosition)>().iter() {
            new_units
                .entry(owner.player_id.clone())
                .or_default()
                .push(UnitHandle {
                    entity,
                    pos: (*pos).as_tuple(),
                });
        }

        self.units_by_player = new_units;
    }

    /// Combined rebuild of player_id_to_index, owner_id_by_entity, AND units_by_player
    /// in a single ECS pass. Used by build_full() for benchmarks — not called in the
    /// tick loop (owner map is updated incrementally; units_by_player is rebuilt lazily).
    #[cfg(any(test, feature = "microbenchmarks"))]
    #[allow(dead_code)]
    pub fn rebuild_both(&mut self, world: &World) {
        let mut player_id_to_index: HashMap<String, u8> = HashMap::with_capacity(16);
        let mut owner_id_by_entity: HashMap<Entity, u8> = HashMap::new();
        let mut new_units: HashMap<String, Vec<UnitHandle>> = HashMap::new();
        let mut next_player_idx: u8 = 0;

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
            owner_id_by_entity.insert(entity, idx);
            new_units.entry(pid).or_default().push(UnitHandle {
                entity,
                pos: (*pos).as_tuple(),
            });
        }
        debug_assert!(next_player_idx < 255, "too many players");

        self.player_id_to_index = player_id_to_index;
        self.next_player_idx = next_player_idx;
        self.owner_id_by_entity = owner_id_by_entity;
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

        Self {
            world,
            world_entity,
            settings,
            unit_types_index,
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
            movement_scratch: systems::movement::MovementScratch::default(),
            last_mover_count: 0,
            last_engaged_pairs: 0,
            ctx,
            ctx_dirty: TickContextDirty::default(),
            units_by_player_stale: false,
            influence_dirty: true,
            encirclement_dirty: true,
            influence_scratch: systems::influence::InfluenceScratch::new(),
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

    /// Snapshot of the neighbour graph as spatialty left it this tick.
    /// `attacking`/`NearbyEnemies` are spatialty-owned, so post-combat reads
    /// reflect the current tick's spatialty output. Used by benchmark/status.
    #[allow(dead_code)]
    pub fn combat_health(&self) -> CombatHealth {
        let mut units = 0usize;
        let mut units_with_attacking = 0usize;
        let mut units_with_enemies = 0usize;
        let mut total_nearby_entries = 0usize;
        for (_, flags, nearby) in self
            .world
            .query::<(Entity, &UnitFlags, &NearbyEnemies)>()
            .iter()
        {
            units += 1;
            if flags.attacking {
                units_with_attacking += 1;
            }
            if !nearby.entities.is_empty() {
                units_with_enemies += 1;
            }
            total_nearby_entries += nearby.entities.len();
        }
        CombatHealth {
            units,
            units_with_attacking,
            units_with_enemies,
            total_nearby_entries,
        }
    }

    /// Combat resolution-stage counters from the most recent tick's run.
    #[allow(dead_code)]
    pub fn combat_diag(&self) -> CombatDiag {
        CombatDiag {
            candidates: self.combat_scratch.candidates,
            attackers: self.combat_scratch.attackers,
            pairs_scanned: self.combat_scratch.pairs_scanned,
            pairs_same_owner: self.combat_scratch.pairs_same_owner,
            pairs_out_of_range: self.combat_scratch.pairs_out_of_range,
            pairs_already_resolved: self.combat_scratch.pairs_already_resolved,
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
            let earned = tick_system!(
                "economy",
                &mut self.world,
                self.world_entity,
                systems::economy::run(
                    &mut self.world,
                    self.world_entity,
                    self.settings.funds_per_city,
                )
            );
            // Track funds_earned in player stats
            for (pid, amount) in earned {
                if let Some(stats) = self.player_stats.get_mut(&pid) {
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
            systems::water::run(&mut self.world, self.world_entity)
        );

        // Movement returns count of units that moved this tick.
        let mover_count = tick_system!(
            "movement",
            &mut self.world,
            self.world_entity,
            systems::movement::run(
                &mut self.world,
                self.world_entity,
                &self.settings,
                &mut self.movement_scratch,
                &self.unit_types_index,
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

        // NearbyEnemies: brute-force rebuild of enemy adjacency lists.
        // Runs after movement (so positions are current) and before combat.
        // Uses shared TickContext owner lookup instead of its own &UnitOwner query.
        {
            let ctx = &self.ctx;
            tick_system!(
                "nearby_enemies",
                &mut self.world,
                self.world_entity,
                systems::nearby_enemies::run(&mut self.world, ctx)
            );
        }

        // Combat (reads NearbyEnemies, uses shared TickContext for owner lookups)
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
                    &self.unit_types_index
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
            )
        );

        // City capture — returns list of city IDs whose ownership changed
        let cities_changed = tick_system!(
            "city_capture",
            &mut self.world,
            self.world_entity,
            systems::city_capture::run(&mut self.world, self.world_entity, &mut self.player_stats)
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
                    systems::encirclement::run(&mut self.world, ctx, self.tick)
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

        // Unified death cleanup: chain all dead entities from combat, attrition, and surrender
        let all_dead: Vec<hecs::Entity> = combat_dead
            .into_iter()
            .chain(supply_dead)
            .chain(surrender_dead)
            .collect();

        let death_count = tick_system!(
            "death_cleanup",
            &mut self.world,
            self.world_entity,
            systems::death_cleanup::run(&mut self.world, all_dead, &mut self.entity_reverse,)
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

        // Phase 2: All checks passed — deduct resources
        {
            let wb = self.world.world_data_mut(self.world_entity)?;
            if let Some(f) = wb.funds.funds.get_mut(owner) {
                *f = f.saturating_sub(unit_type.recruitment_cost);
            }
            if let Some(mp) = wb.funds.manpower.get_mut(owner) {
                *mp -= unit_type.men;
            }
        }

        let entity = self.world.spawn_unit(unit_type, owner, city_pos);
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
        self.ctx.owner_id_by_entity.insert(entity, player_idx);

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
                up.path = path;
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
                path: path.path.clone(),
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
                            .map(|x| match &grid[x][y] {
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
                .map(|y| (0..w).map(|x| flat[y * w + x]).collect())
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
        let mut result_flat = vec![false; grid_size];
        for pid in player_ids {
            if let Some(grid) = wb.vision.grids.get(pid) {
                for i in 0..grid_size {
                    if grid[i] {
                        result_flat[i] = true;
                    }
                }
            }
        }
        // Convert flat → 2D for API compatibility
        let result: Vec<Vec<bool>> = (0..husize)
            .map(|y| (0..wusize).map(|x| result_flat[y * wusize + x]).collect())
            .collect();
        Some(result)
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
        let current_counts: HashMap<String, usize> = self
            .world
            .query::<(Entity, &UnitOwner)>()
            .iter()
            .fold(HashMap::new(), |mut acc, (_, owner)| {
                *acc.entry(owner.player_id.clone()).or_insert(0) += 1;
                acc
            });

        for (player_id, prev_count) in &self.prev_unit_counts {
            let current_count = *current_counts.get(player_id).unwrap_or(&0);
            let lost = prev_count.saturating_sub(current_count);
            if lost > 0 {
                if let Some(stats) = self.player_stats.get_mut(player_id) {
                    stats.units_lost += lost as u32;
                }
            }
        }
        self.prev_unit_counts = current_counts;
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
}
