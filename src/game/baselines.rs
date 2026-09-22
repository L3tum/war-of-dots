//! H5: engine-neutral network baselines.
//!
//! The per-tick PRE-STATE cache the server's delta path diffs the current
//! engine snapshot against. This type lives in the ENGINE module and names
//! only engine types (plain [`City`], funds map, published grid `Arc`s) —
//! no wire protocol types, so the engine stays decoupled from the server
//! layer (Arch H2: the engine yields plain data; the server owns the
//! engine→wire conversion).
//!
//! The unit baseline store (`UnitBaselineStore`) deliberately does NOT live
//! here: it stores wire `UnitState` slots, and per Arch H2 the server layer
//! owns it. Server callers keep the two side by side and commit both in the
//! same tick step.
//!
//! All four fields advance in a SINGLE commit step ([`NetworkBaselines::commit`])
//! so the baseline cache can never be left half-advanced after a tick (the
//! old code scattered `prev_X = ...` assignments across the tail of the
//! network phase).

use std::collections::HashMap;
use std::sync::Arc;

use crate::game::economy::City;

/// Engine-neutral per-tick baselines for the network delta path.
#[derive(Debug, Default)]
pub struct NetworkBaselines {
    /// PRE-COMMIT city snapshot (engine `City`, keyed by city id) — the
    /// city diff compares owner/level against these.
    pub prev_cities: HashMap<String, City>,
    /// PRE-COMMIT per-player funds.
    pub prev_funds: HashMap<String, u32>,
    /// PRE-COMMIT per-player vision grids (published `Arc`s — shared, never
    /// copied; `Arc::ptr_eq` makes unchanged grids diff-free).
    pub prev_vision: HashMap<String, Arc<Vec<u8>>>,
    /// PRE-COMMIT territory (border) grid (published `Arc`).
    pub prev_border: Option<Arc<Vec<u8>>>,
}

impl NetworkBaselines {
    /// Seed from the engine's initial snapshot (game start, benchmark setup,
    /// or test seeding). `cities` is consumed by move — the engine's plain
    /// snapshot, not a wire conversion.
    pub fn from_snapshot(
        cities: Vec<City>,
        funds: HashMap<String, u32>,
        vision: HashMap<String, Arc<Vec<u8>>>,
        border: Option<Arc<Vec<u8>>>,
    ) -> Self {
        Self {
            prev_cities: cities.into_iter().map(|c| (c.id.clone(), c)).collect(),
            prev_funds: funds,
            prev_vision: vision,
            prev_border: border,
        }
    }

    /// Single commit step: advance EVERY engine-neutral baseline from one
    /// tick's plain engine snapshot in one atomic call.
    ///
    /// `vision` carries the per-player published grid `Arc`s to commit —
    /// only the players whose delta encoded successfully (a dropped frame
    /// must keep its stale grid so the next tick's diff re-covers the
    /// missed change, matching the old in-loop commit semantics).
    pub fn commit(
        &mut self,
        cities: Vec<City>,
        funds: HashMap<String, u32>,
        border_grid: Option<Arc<Vec<u8>>>,
        vision: Vec<(String, Arc<Vec<u8>>)>,
    ) {
        self.prev_cities = cities.into_iter().map(|c| (c.id.clone(), c)).collect();
        self.prev_funds = funds;
        self.prev_border = border_grid;
        self.prev_vision.extend(vision);
    }

    /// Commit a synthetic (non-player) vision baseline — the spectator union
    /// grid, which is not part of the per-tick player commit.
    pub fn commit_vision(&mut self, player_id: String, grid: Arc<Vec<u8>>) {
        self.prev_vision.insert(player_id, grid);
    }
}
