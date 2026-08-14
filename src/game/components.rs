//! ECS Components for the game world.
//!
//! Each component is a pure data struct — no methods, just fields.
//! Systems query these components from the hecs::World and apply game rules.
//!
//! In hecs 0.11, components must be `Send + Sync + 'static` (auto-implemented).
//! No `#[derive(Component)]` needed — it's a marker trait auto-implied.

use crate::game::economy::City;
use crate::game::unit::Equipment;
use crate::game::unit::Terrain;
use hecs;
use std::collections::HashMap;
use std::sync::Arc;

// ============================================================================
// Per-Unit Components
// ============================================================================

/// Which player owns this unit entity.
#[derive(Debug, Clone)]
pub struct UnitOwner {
    pub player_id: String,
}

/// Which unit type this entity represents.
#[derive(Debug, Clone)]
pub struct UnitTypeId {
    pub name: String,
}

/// Display symbol for this unit.
#[derive(Debug, Clone)]
pub struct UnitDisplay {
    pub symbol: String,
}

/// Current position on the map.
#[derive(Debug, Clone, Default, Copy)]
pub struct UnitPosition {
    pub x: f32,
    pub y: f32,
}

impl UnitPosition {
    pub fn as_tuple(&self) -> (f32, f32) {
        (self.x, self.y)
    }
}

/// Path waypoints the unit is following.
#[derive(Debug, Clone, Default)]
pub struct UnitPath {
    pub path: Vec<(f32, f32)>,
}

/// Cached list of enemy entities within COMBAT_RANGE.
/// Maintained by the nearby_enemies system. Read by combat.
#[derive(Debug, Clone, Default)]
pub struct NearbyEnemies {
    /// Enemy entities found during last spatialty update.
    pub entities: Vec<hecs::Entity>,
    /// Whether this list was ever populated by the spatialty system.
    /// false at spawn, set to true after first Tier 1 initialization.
    pub initialized: bool,
}

/// Dynamic stats modified by combat, attrition, healing.
#[derive(Debug, Clone, Copy)]
pub struct UnitStats {
    pub men: f32,
    pub max_men: f32,
    pub organisation: f32,
    pub max_organisation: f32,
}

/// Equipment carried by this unit (current and max).
#[derive(Debug, Clone)]
pub struct UnitEquipment {
    pub equipment: Vec<Equipment>,
    pub max_equipment: Vec<Equipment>,
    pub supply_buffer_percent: u32,
    /// Max equipment items delivered to this unit per tick by the supply system.
    pub supply_rate: u32,
    /// Current supply load (physical burden). Consumed by attrition/combat,
    /// replenished by supply system. Capped at max_supply.
    pub supply: f32,
    /// Maximum supply this unit can carry (men * carry_weight)
    #[allow(dead_code)] // Referenced by domain model; ECS systems track via equipment ratio
    pub max_supply: f32,
}

/// Boolean flags that control unit behavior.
#[derive(Debug, Clone, Default, Copy)]
pub struct UnitFlags {
    pub healing: bool,
    pub fighting: bool,  // repurposed from `attacking` — true when in combat range
    pub attacking: bool, // true when unit moved into/through enemy territory (set by spatialty)
    pub moved: bool,     // set by movement system when position changes
    pub encircled: bool,
    pub resupply_enabled: bool,
    pub on_water: bool,
}

/// Water transition state.
#[derive(Debug, Clone, Default)]
pub struct UnitWaterTransition {
    pub embarking: bool,
    pub disembarking: bool,
    pub embarking_ticks_left: u32,
    pub disembarking_ticks_left: u32,
    pub boat_supply: Vec<Equipment>,
    pub max_boat_supply: Vec<Equipment>,
}

// ============================================================================
// World-Level Components (stored on a single "world" entity)
// ============================================================================

/// Terrain grid for the map.
#[derive(Debug, Clone)]
pub struct TerrainGrid {
    pub grid: Arc<Vec<Vec<Terrain>>>,
    pub width: u32,
    pub height: u32,
}

impl TerrainGrid {
    pub fn get(&self, x: f32, y: f32) -> Terrain {
        self.grid
            .get(x as usize)
            .and_then(|r| r.get(y as usize))
            .cloned()
            .unwrap_or(Terrain::Plains)
    }
}

/// Per-player vision grids.
/// Stored as flat 1D Vec<T> indexed as [y * width + x] for cache efficiency.
#[derive(Debug, Clone)]
pub struct VisionGrids {
    pub grids: Arc<HashMap<String, Vec<bool>>>,
}
impl Default for VisionGrids {
    fn default() -> Self {
        Self {
            grids: Arc::new(HashMap::new()),
        }
    }
}

/// Per-player border influence grids.
/// Stored as flat 1D Vec<T> indexed as [y * width + x] for cache efficiency.
#[derive(Debug, Clone)]
pub struct BorderGrids {
    pub grids: Arc<HashMap<String, Vec<u32>>>,
}
impl Default for BorderGrids {
    fn default() -> Self {
        Self {
            grids: Arc::new(HashMap::new()),
        }
    }
}

/// All cities in the game.
#[derive(Debug, Clone)]
pub struct CityData {
    pub cities: Vec<City>,
}

/// Per-player global economy pools: funds and manpower.
#[derive(Debug, Clone, Default)]
pub struct EconomyFunds {
    pub funds: HashMap<String, u32>,
    /// Global manpower pool — generated by cities, consumed by recruitment + resupply
    pub manpower: HashMap<String, u32>,
}

/// City upgrade settings.
#[derive(Debug, Clone)]
pub struct CityUpgradeData {
    pub settings: crate::config::CityUpgradeSettings,
}

/// All world-level data bundled together on one entity.
#[derive(Debug, Clone)]
pub struct WorldBundle {
    pub terrain: TerrainGrid,
    pub vision: VisionGrids,
    pub borders: BorderGrids,
    pub cities: CityData,
    pub funds: EconomyFunds,
    pub city_upgrades: CityUpgradeData,
    /// Debug stats (only populated in debug builds)
    #[cfg(any(debug_assertions, feature = "debug-overlay"))]
    pub debug_stats: DebugStats,
}

impl WorldBundle {
    pub fn new(
        terrain: Vec<Vec<Terrain>>,
        width: u32,
        height: u32,
        city_upgrade_settings: crate::config::CityUpgradeSettings,
        funds: HashMap<String, u32>,
    ) -> Self {
        Self {
            terrain: TerrainGrid {
                grid: Arc::new(terrain),
                width,
                height,
            },
            vision: VisionGrids {
                grids: Arc::new(HashMap::new()),
            },
            borders: BorderGrids {
                grids: Arc::new(HashMap::new()),
            },
            cities: CityData { cities: Vec::new() },
            funds: EconomyFunds {
                funds,
                manpower: HashMap::new(),
            },
            city_upgrades: CityUpgradeData {
                settings: city_upgrade_settings,
            },
            #[cfg(any(debug_assertions, feature = "debug-overlay"))]
            debug_stats: DebugStats::default(),
        }
    }
}

/// Debug stats for the overlay.
#[derive(Debug, Default, Clone)]
pub struct DebugStats {
    pub tick_count: u64,
    pub tick_durations_ms: Vec<f64>,
    pub system_timings: HashMap<String, SystemTimings>,
    pub entity_count: u32,
    pub archetype_count: u32,
    pub total_ticks_ms: f64,
    pub last_tick_ms: f64,
    /// Systems that should record sub-part timings (set during benchmark)
    pub detailed_systems: std::collections::HashSet<String>,
    /// Sub-part timing data: "system/sub_part" -> SystemTimings
    pub detailed_timings: HashMap<String, SystemTimings>,
}

/// Timing info for a single system.
#[derive(Debug, Default, Clone)]
pub struct SystemTimings {
    pub total_ms: f64,
    pub call_count: u64,
    pub last_ms: f64,
    pub avg_ms: f64,
    /// Per-tick duration buffer (bounded to 200 entries for P90/P99 computation)
    pub durations_ms: Vec<f64>,
}
