//! Game facade — holds the ECS world, settings, and dispatches systems.

use crate::config::CityUpgradeSettings;
use crate::game::components::*;
use crate::game::economy::{City, UnitType};
use crate::game::player::Player;
use crate::game::unit::Terrain;
use crate::game::world_ext::WorldExt;
use crate::map::map::Map;
use hecs::World;
use std::collections::HashMap;

#[cfg(any(debug_assertions, feature = "debug-overlay"))]
use super::debug_stats::DebugSystemGuard;

/// Macro to wrap system execution with timing in debug builds, no-op in release.
///
/// Usage (statement):
///   tick_system!("movement", systems::movement::run(&mut self.world, ...));
///
/// Usage (expression):
///   let dead = tick_system!("combat", systems::combat::run(...));
///
/// This eliminates the repeated #[cfg] blocks throughout tick_game(),
/// reducing ~200 lines of boilerplate.
///
/// NOTE: debug timing was dropped from the macro because Rust's macro hygiene
/// prevents macros from accessing `guard`/`self` in the caller's scope.
#[cfg(any(debug_assertions, feature = "debug-overlay"))]
macro_rules! tick_system {
    ($name:expr, $expr:expr) => {
        $expr
    };
}

#[cfg(not(any(debug_assertions, feature = "debug-overlay")))]
macro_rules! tick_system {
    ($name:expr, $expr:expr) => {
        $expr
    };
}

/// Game-level settings.
#[derive(Debug, Clone)]
pub struct GameSettings {
    pub starting_funds: u32,
    /// Funds generated per city per economy tick (base income before multipliers)
    pub funds_per_city: f32,
    /// Ticks between recruitment actions for a player (0 = unlimited)
    pub recruitment_rate: f32,
    pub game_speed: f32,
    pub use_standard_ruleset: bool,
    pub unit_types: Vec<UnitType>,
    pub city_upgrade_settings: CityUpgradeSettings,
    pub water_speed: f32,
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
}

/// Pre-grouped data built once per tick, shared across all systems.
/// Eliminates redundant player enumeration and unit filtering.
/// Only holds small data (cities ~20 entries, funds ~12 entries).
/// Grids are accessed via scoped Arc extraction in systems that need them.
/// Entity+position handle for unit grouping
type UnitHandle = (hecs::Entity, (f32, f32));

#[derive(Debug)]
pub struct TickContext {
    /// The world entity handle (needed for world_data_mut writes)
    pub world_entity: hecs::Entity,
    /// Cities snapshot — small (~20 entries), cloned once per tick
    pub cities: Vec<crate::game::economy::City>,
    /// Economy funds snapshot
    pub funds: HashMap<String, u32>,
    /// Map dimensions (cached to avoid per-system world borrows)
    pub width: u32,
    pub height: u32,
    /// Entity+position handles grouped by player ID
    pub units_by_player: HashMap<String, Vec<UnitHandle>>,
}

impl TickContext {
    /// Build TickContext: scoped Arc extraction + single-pass unit grouping.
    /// Only clones small data (cities ~20 entries, funds ~12 entries).
    /// Grids (terrain/vision/borders) are NOT cloned — systems use scoped Arc extraction.
    pub fn build(world: &World, world_entity: hecs::Entity) -> Option<Self> {
        // Scoped extraction: only clone what we actually need
        let (cities, funds, width, height) = {
            let wb = world.world_data(world_entity)?;
            (
                wb.cities.cities.clone(),
                wb.funds.funds.clone(),
                wb.terrain.width,
                wb.terrain.height,
            )
        };
        // wb dropped — borrow released

        // Single pass: group entities+positions by player
        let mut units_by_player: HashMap<String, Vec<UnitHandle>> = HashMap::new();
        for (entity, (owner, pos)) in world.query::<(&UnitOwner, &UnitPosition)>().iter() {
            let pos_tuple = (*pos).as_tuple();
            units_by_player
                .entry(owner.player_id.clone())
                .or_default()
                .push((entity, pos_tuple));
        }

        Some(Self {
            world_entity,
            cities,
            funds,
            width,
            height,
            units_by_player,
        })
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

        Self {
            world,
            world_entity,
            settings,
            players,
            tick: 0,
            running: false,
            player_stats,
            prev_unit_counts: HashMap::new(),
            last_surrender_check_tick: 0,
            entity_registry: HashMap::new(),
            entity_reverse: HashMap::new(),
        }
    }

    pub fn start(&mut self) {
        self.running = true;
    }

    /// Main game tick.
    pub fn tick_game(&mut self) {
        if !self.running {
            return;
        }
        self.tick += 1;

        #[cfg(any(debug_assertions, feature = "debug-overlay"))]
        let guard = DebugSystemGuard::new(&mut self.world, self.world_entity);

        // Build TickContext once per tick
        let ctx = tick_system!(
            "tickcontext",
            TickContext::build(&self.world, self.world_entity)
        );

        // Economy: only every ECONOMY_TICK_INTERVAL ticks
        if self
            .tick
            .is_multiple_of(super::constants::ECONOMY_TICK_INTERVAL)
        {
            let earned = tick_system!(
                "economy",
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
            // Clone funds for player update (only every 10 ticks)
            if let Some(ref wb) = ctx {
                for (pid, player) in self.players.iter_mut() {
                    if let Some(f) = wb.funds.get(pid) {
                        player.funds = *f;
                    }
                }
            }
        }

        tick_system!(
            "water",
            systems::water::run(&mut self.world, self.world_entity)
        );

        // Build spatial hash once per tick — shared by Movement and Combat
        let spatial = tick_system!("spatial_hash", self.world.build_spatial_hash());

        tick_system!(
            "movement",
            systems::movement::run(&mut self.world, self.world_entity, &self.settings)
        );
        tick_system!(
            "attrition",
            systems::attrition::run(&mut self.world, self.world_entity, &self.settings)
        );

        // Combat
        let dead_entities = tick_system!(
            "combat",
            systems::combat::run(&mut self.world, self.world_entity, spatial)
        );

        for entity in &dead_entities {
            // O(1) removal via reverse lookup instead of O(N) linear scan
            if let Some(id) = self.entity_reverse.remove(entity) {
                self.entity_registry.remove(&id);
            }
            self.world.despawn(*entity).ok();
        }

        // City capture
        tick_system!(
            "city_capture",
            systems::city_capture::run(&mut self.world, self.world_entity, &mut self.player_stats)
        );

        // Influence: merged vision + border in one pass (every INFLUENCE_TICK_INTERVAL ticks)
        if self
            .tick
            .is_multiple_of(super::constants::INFLUENCE_TICK_INTERVAL)
        {
            if let Some(ref ctx) = ctx {
                tick_system!(
                    "influence",
                    systems::influence::run(&mut self.world, ctx, self.tick)
                );
            }
        }

        // Healing: uses TickContext for per-player iteration
        if let Some(ref ctx) = ctx {
            tick_system!("healing", systems::healing::run(&mut self.world, ctx));
        }

        // Supply: uses TickContext for per-player iteration
        if let Some(ref ctx) = ctx {
            tick_system!(
                "supply",
                systems::supply::run(
                    &mut self.world,
                    self.world_entity,
                    ctx,
                    self.settings.recruitment_rate,
                )
            );
        }

        // Encirclement: only every CITY_CAPTURE_TICK_INTERVAL ticks (strategic, not tactical)
        if self
            .tick
            .is_multiple_of(super::constants::CITY_CAPTURE_TICK_INTERVAL)
        {
            if let Some(ref ctx) = ctx {
                tick_system!(
                    "encirclement",
                    systems::encirclement::run(&mut self.world, ctx, self.tick)
                );
            }
        }

        // Surrender: only every 30 ticks
        if self.tick - self.last_surrender_check_tick >= 30 {
            let dead = tick_system!(
                "surrender",
                systems::surrender::run(&mut self.world, self.world_entity)
            );

            for entity in &dead {
                // O(1) removal via reverse lookup
                if let Some(id) = self.entity_reverse.remove(entity) {
                    self.entity_registry.remove(&id);
                }
                self.world.despawn(*entity).ok();
            }
            self.last_surrender_check_tick = self.tick;
        }

        self.track_unit_losses();

        tick_system!(
            "reinforce",
            systems::reinforce::run(&mut self.world, self.world_entity)
        );

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

        // Check manpower (before deducting anything)
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

    pub fn move_troops(&mut self, unit_id: &str, path: Vec<(f32, f32)>) {
        if let Some(entity) = self.entity_registry.get(unit_id) {
            let entity = *entity;
            if let Ok((up,)) = self.world.query_one_mut::<(&mut UnitPath,)>(entity) {
                up.path = path;
            }
        }
    }

    pub fn toggle_healing(&mut self, unit_id: &str, enabled: bool) {
        if let Some(entity) = self.entity_registry.get(unit_id) {
            let entity = *entity;
            if let Ok((flags,)) = self.world.query_one_mut::<(&mut UnitFlags,)>(entity) {
                flags.healing = enabled;
            }
        }
    }

    pub fn toggle_resupply(&mut self, unit_id: &str, enabled: bool) {
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
        for (entity, (owner, utype, display, pos, path, stats, equipment, flags, wt)) in self
            .world
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
        self.world
            .world_data_mut(self.world_entity)
            .map(|wb| {
                wb.cities
                    .cities
                    .iter_mut()
                    .find(|c| c.id == city_id)
                    .map(|c| c.owner = Some(owner.to_string()))
                    .is_some()
            })
            .unwrap_or(false)
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

    /// Get border grid for a player as 2D Vec<Vec<u32>>[y][x].
    /// Grids are stored flat internally; this converts for wire protocol compatibility.
    pub fn border_grid_2d(&self, player_id: &str) -> Option<Vec<Vec<u32>>> {
        let wb = self.world.world_data(self.world_entity)?;
        let w = wb.terrain.width as usize;
        let h = wb.terrain.height as usize;
        wb.borders.grids.get(player_id).map(|flat| {
            (0..h)
                .map(|y| (0..w).map(|x| flat[y * w + x]).collect())
                .collect()
        })
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

    /// Supervisor (spectator/replay) view: strongest border influence across all players.
    pub fn supervisor_border_grid(&self) -> Option<Vec<Vec<u32>>> {
        let ids: Vec<String> = self.all_player_funds().keys().cloned().collect();
        let mut out: Option<Vec<Vec<u32>>> = None;
        for id in ids {
            if let Some(grid) = self.border_grid_2d(&id) {
                out = Some(match out {
                    None => grid,
                    Some(mut acc) => {
                        for (row, src) in acc.iter_mut().zip(grid) {
                            for (cell, s) in row.iter_mut().zip(src) {
                                if s > *cell {
                                    *cell = s;
                                }
                            }
                        }
                        acc
                    }
                });
            }
        }
        out
    }

    /// Get debug stats as JSON (only available in debug builds).
    #[cfg(any(debug_assertions, feature = "debug-overlay"))]
    pub fn get_debug_stats(&self) -> Option<serde_json::Value> {
        super::debug_stats::get_debug_stats(&self.world, self.world_entity)
    }

    /// Set which systems should record detailed sub-part timings (benchmark mode).
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
            .query::<(&UnitOwner,)>()
            .iter()
            .any(|(_, (owner,))| owner.player_id == player_id);
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

    #[allow(dead_code)] // Used in integration tests (determine_winner_returns_none_when_game_running)
    pub fn determine_winner(&self) -> Option<String> {
        let mut players_with_power: Vec<(String, u32)> = Vec::new();
        for pid in self.players.keys() {
            let unit_count = self
                .world
                .query::<(&UnitOwner,)>()
                .iter()
                .filter(|(_, (owner,))| &owner.player_id == pid)
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
            .query::<(&UnitOwner,)>()
            .iter()
            .fold(HashMap::new(), |mut acc, (_, (owner,))| {
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
