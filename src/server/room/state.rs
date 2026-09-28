//! Protocol extensions on the engine `Game`: full-state builders
//! (`full_state_with_player_id`, `full_state_with_vision`, `build_full_state`)
//! and the `City` -> `CityState` conversion.

use crate::game::game::Game;
use crate::server::delta::wire_unit_states;
use crate::server::protocol::{CityState, GameStateMessage, PlayerState, UnitState};

impl From<&crate::game::economy::City> for CityState {
    fn from(city: &crate::game::economy::City) -> Self {
        CityState {
            id: city.id.clone(),
            position: city.position,
            owner: city.owner.clone(),
            manpower: 0, // Global pool, not per-city
            level: city.level,
        }
    }
}

/// Determine the winner of the game — the last player with units/cities
impl Game {
    pub(crate) fn full_state_with_player_id(&self, player_id: &str) -> GameStateMessage {
        let (vision_grid, border_grid) = (
            self.vision_grid_2d(player_id),
            self.border_grid_2d(player_id),
        );
        self.build_full_state(vision_grid, border_grid, Some(player_id))
    }

    pub(crate) fn full_state_with_vision(
        &self,
        vision_grid: Option<Vec<Vec<bool>>>,
        border_grid: Option<Vec<Vec<u32>>>,
    ) -> GameStateMessage {
        self.build_full_state(vision_grid, border_grid, None)
    }

    fn build_full_state(
        &self,
        vision_grid: Option<Vec<Vec<bool>>>,
        border_grid: Option<Vec<Vec<u32>>>,
        player_id: Option<&str>,
    ) -> GameStateMessage {
        // Perf #5: `wire_unit_states` already builds a fresh Vec — the old
        // `.to_vec()` here made a gratuitous second full copy (every unit's
        // strings + equipment Vecs) on every full-state build.
        let units: Vec<UnitState> = wire_unit_states(self);
        let cities: Vec<CityState> = self.query_cities().iter().map(|c| c.into()).collect();
        let terrain = self.terrain_grid_2d();
        // Per-player funds/units for the viewing player, matching the delta system.
        // For a supervisor view (player_id == None) fall back to the aggregate totals.
        let player_funds: u32 = match player_id {
            Some(pid) => self.player_funds(pid),
            None => self.all_player_funds().values().sum(),
        };
        let units_owned: u32 = match player_id {
            Some(pid) => units.iter().filter(|u| u.owner == pid).count() as u32,
            None => units.len() as u32,
        };
        GameStateMessage {
            vision_grid,
            border_grid,
            terrain,
            units,
            cities,
            player: PlayerState {
                funds: player_funds,
                units_owned,
                player_id: player_id.unwrap_or("").to_string(),
            },
        }
    }
}
