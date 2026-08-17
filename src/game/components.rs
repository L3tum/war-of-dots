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
use smallvec::SmallVec;
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
    /// SmallVec keeps typical short paths (≤ 8 waypoints) stack-allocated.
    pub path: SmallVec<[(f32, f32); 8]>,
}

/// Cached list of enemy entities within COMBAT_RANGE.
/// Maintained by the nearby_enemies system. Read by combat.
#[derive(Debug, Clone, Default)]
pub struct NearbyEnemies {
    /// Enemy entities found during last spatialty update.
    /// SmallVec keeps the common case (≤ 8 enemies) stack-allocated.
    pub entities: SmallVec<[hecs::Entity; 8]>,
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
///
/// Stored as a single flat `Arc<Vec<Terrain>>` in **y-major** order
/// (`idx = y * width + x`), matching `VisionGrids`/`TerritoryGrid`. This
/// removes the row-pointer indirection of the old `Vec<Vec<Terrain>>` on every
/// per-unit terrain lookup.
#[derive(Debug, Clone)]
pub struct TerrainGrid {
    pub grid: Arc<Vec<Terrain>>,
    pub width: u32,
    pub height: u32,
}

impl TerrainGrid {
    pub fn get(&self, x: f32, y: f32) -> Terrain {
        let w = self.width as usize;
        let idx = (y as usize) * w + (x as usize);
        self.grid.get(idx).copied().unwrap_or(Terrain::Plains)
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

/// Derived territory grid: which player dominates each cell.
/// Flat 1D Vec<u8> indexed [y * width + x].
/// Value is player index (0..N) or 255 (NONE = no influence).
/// Computed by influence::run(); read by encirclement and the wire protocol.
#[derive(Debug, Clone)]
pub struct TerritoryGrid {
    pub grid: Arc<Vec<u8>>,
}
impl Default for TerritoryGrid {
    fn default() -> Self {
        Self {
            grid: Arc::new(Vec::new()),
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
    pub territory: TerritoryGrid,
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
        let w = width as usize;
        let h = height as usize;
        // Flatten the x-major input (`terrain[x][y]`) into y-major storage
        // (`flat[y * w + x]`) to match VisionGrids/TerritoryGrid indexing.
        // Behavior-preserving: new get(x,y) = flat[y*w+x] = terrain[x][y] = old get(x,y).
        let mut flat = vec![Terrain::Plains; w * h];
        for x in 0..w {
            if let Some(row) = terrain.get(x) {
                for y in 0..h {
                    if let Some(&t) = row.get(y) {
                        flat[y * w + x] = t;
                    }
                }
            }
        }
        Self {
            terrain: TerrainGrid {
                grid: Arc::new(flat),
                width,
                height,
            },
            vision: VisionGrids {
                grids: Arc::new(HashMap::new()),
            },
            territory: TerritoryGrid::default(),
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::CityUpgradeSettings;
    use std::collections::HashMap;

    /// Round-trip a NON-SQUARE (120 wide × 80 tall) terrain grid through
    /// `WorldBundle::new` and `TerrainGrid::get`. Because width != height, any
    /// x/y stride confusion in the flatten or the lookup would surface here
    /// (all other tests use square maps and would pass with a silent swap).
    #[test]
    fn non_square_terrain_roundtrip() {
        let w: u32 = 120;
        let h: u32 = 80;
        // x-major input: terrain[x][y]. Markers placed where x != y catch a swap.
        let mut terrain = vec![vec![Terrain::Plains; h as usize]; w as usize];
        terrain[7][3] = Terrain::Hills; // (x=7, y=3)
        terrain[3][7] = Terrain::Forest; // (x=3, y=7)
        terrain[119][79] = Terrain::Water; // far corner
        terrain[0][0] = Terrain::Mountain; // near corner

        let wb = WorldBundle::new(
            terrain,
            w,
            h,
            CityUpgradeSettings::default(),
            HashMap::new(),
        );

        // get(x, y) must read exactly the cell the x-major builder wrote.
        assert_eq!(wb.terrain.get(7.0, 3.0), Terrain::Hills);
        assert_eq!(wb.terrain.get(3.0, 7.0), Terrain::Forest);
        assert_eq!(wb.terrain.get(119.0, 79.0), Terrain::Water);
        assert_eq!(wb.terrain.get(0.0, 0.0), Terrain::Mountain);
        // Unmarked cells stay Plains.
        assert_eq!(wb.terrain.get(10.0, 10.0), Terrain::Plains);
        // Out-of-bounds falls back to Plains (no panic).
        assert_eq!(wb.terrain.get(120.0, 0.0), Terrain::Plains);
        assert_eq!(wb.terrain.get(0.0, 80.0), Terrain::Plains);
    }
}
