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
//!
//! Perf #2/#3: the city and funds baselines advance by SHARED OWNERSHIP,
//! not by clone. `prev_cities` is the same `Arc<Vec<City>>` the server's
//! tick loop caches (rebuilt only on a city-generation change), and
//! `prev_funds` ping-pongs with the caller's persistent scratch map via a
//! swap — so an idle tick commits zero city/funds content allocations
//! (one refcount bump + one map swap), whereas the pre-cleanup commit
//! re-materialized the whole snapshot (C city String clones + a fresh
//! funds HashMap bucket table + P key String clones) every tick.

use std::collections::HashMap;
use std::sync::Arc;

use crate::game::economy::City;

/// Engine-neutral per-tick baselines for the network delta path.
#[derive(Debug, Default)]
pub struct NetworkBaselines {
    /// PRE-COMMIT city snapshot — the shared published `Arc` (Perf #2):
    /// the same allocation the server's tick loop caches, rebuilt only
    /// when the city generation changes. The city diff looks each current
    /// city up by id inside this slice (linear scan — city counts are
    /// small, ~20, so it is allocation-free and faster than rebuilding a
    /// keyed map per tick). The diff compares owner/level against these.
    pub prev_cities: Arc<Vec<City>>,
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
    /// snapshot, not a wire conversion — and published as the shared Arc
    /// the per-tick commit swaps in (Perf #2).
    pub fn from_snapshot(
        cities: Vec<City>,
        funds: HashMap<String, u32>,
        vision: HashMap<String, Arc<Vec<u8>>>,
        border: Option<Arc<Vec<u8>>>,
    ) -> Self {
        Self {
            prev_cities: Arc::new(cities),
            prev_funds: funds,
            prev_vision: vision,
            prev_border: border,
        }
    }

    /// Single commit step: advance EVERY engine-neutral baseline from one
    /// tick's plain engine snapshot in one atomic call.
    ///
    /// `cities` is the shared snapshot `Arc` (Perf #2): the server's tick
    /// loop hands over the `Arc<Vec<City>>` it cached (rebuilt only on a
    /// city-generation change), so committing an unchanged snapshot costs
    /// one refcount bump, not a full city re-clone. The baseline and the
    /// tick loop's cache share the allocation; when the next generation
    /// change replaces both, the old `Vec<City>` is freed exactly once.
    ///
    /// `funds` is the caller's PERSISTENT per-player funds scratch map
    /// (Perf #3): keys are the stable player-id set (seeded once at game
    /// start, values refilled in place each tick), and the commit SWAPS it
    /// with `prev_funds` — the baseline takes ownership of the current
    /// values (identical advancement to the old `self.prev_funds = funds`)
    /// and hands the pre-commit map back to the caller's scratch slot
    /// ("drained back") to be refilled next tick. Zero per-tick allocation
    /// on the funds path.
    ///
    /// `vision` carries the per-player published grid `Arc`s to commit —
    /// only the players whose delta encoded successfully (a dropped frame
    /// must keep its stale grid so the next tick's diff re-covers the
    /// missed change, matching the old in-loop commit semantics).
    pub fn commit(
        &mut self,
        cities: Arc<Vec<City>>,
        funds: &mut HashMap<String, u32>,
        border_grid: Option<Arc<Vec<u8>>>,
        vision: Vec<(String, Arc<Vec<u8>>)>,
    ) {
        self.prev_cities = cities;
        // Drain-back swap: baseline takes the current values; the caller's
        // scratch receives the pre-commit map to refill in place next tick.
        std::mem::swap(&mut self.prev_funds, funds);
        self.prev_border = border_grid;
        self.prev_vision.extend(vision);
    }

    /// Commit a synthetic (non-player) vision baseline — the spectator union
    /// grid, which is not part of the per-tick player commit.
    pub fn commit_vision(&mut self, player_id: String, grid: Arc<Vec<u8>>) {
        self.prev_vision.insert(player_id, grid);
    }
}
