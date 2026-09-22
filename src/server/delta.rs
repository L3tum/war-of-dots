//! Delta computation for game state updates.
//!
//! Builds minimal deltas between consecutive game states to minimize
//! bandwidth when sending updates to connected clients.
//!
//! ## Delta Digest Optimization
//! Instead of comparing full `UnitState` structs (which involves cloning),
//! we compare lightweight `UnitStateDigest` structs first. If the digest
//! matches, the unit is assumed unchanged. This reduces allocation pressure
//! significantly at scale (100+ units).
//!
//! ## Parallelism Decision
//! Rayon was previously trialed for per-player parallel delta computation but
//! was **removed** because its thread-pool dispatch overhead (~30–50 µs per
//! task) negated the gains on the small per-player work items (≈0.2 ms
//! normal / ≈0.5 ms grid-change).  The encirclement flood A/B showed a 10 %
//! regression in the micro-bench and +18 % on the real-game p2_bfs path,
//! while allocs jumped 21 → 89/call.  Any future parallelization here must
//! be preceded by its own dedicated A/B benchmark.

//! ## Future Optimization (see TODO_PHASE2_NOTES.md)
//! - Use `Arc` for grid data to enable cheap CoW cloning
//! - Parallelize per-player delta computation (NOTE: rayon was trialed and
//!   removed — its dispatch overhead regressed the encirclement flood A/B, so
//!   any parallelization here needs its own A/B before being assumed to win)
//! - Consider incremental hashing for grid diff detection

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use hecs::Entity;

use crate::game::economy::City;
use crate::game::game::{Game, UnitView};
use crate::game::NetworkBaselines;
use crate::server::protocol::{
    encode_message_with_hint, estimate_delta_size, pack_unit_position, CityState, CityUpdate,
    EquipmentState, GameDeltaMessage, PlayerState, ServerMessage, UnitState, UnitUpdate,
};

// I2a: the `men` digest quantization step is resolved at RUNTIME as
// `max(5.0, settings.recruitment_rate)` (see `run_network_phase` and the
// replay path): men refill at `recruitment_rate` per tick (`supply.rs`), so
// the step must absorb the lobby's refill rate — a fixed 1.0 step let every
// refilling unit cross a bin (and emit a `UnitUpdate`) every tick at
// rate-10 lobbies. The floor of 5 keeps the drain (1/600 ≈ 0.0017, up to
// 10/600 ≈ 0.017 under supply stress) invisible. When an update IS emitted,
// the wire carries the exact `u.men`, so no visible information is lost
// (UI men-bar granularity = the step: ≤ 0.5% of a 1000-man unit).

/// Quantization step for the `organisation` field in the unit digest (same
/// rationale as `QUANT_MEN`: organisation recovers slowly, so whole-point
/// quantization keeps the digest stable between visible changes).
const QUANT_ORG: f32 = 1.0;

/// Quantization step for `supply_percent` / `boat_supply_percent` in the unit
/// digest (W6). Supply drain is a slow fractional decrease — the same order
/// as the men drain that motivated `QUANT_MEN` — so any unit whose supply is
/// not exactly 0/100 used to emit a full `UnitUpdate` every tick. 1% steps
/// are below the UI supply-bar granularity; when an update IS emitted the
/// wire carries the exact value, so nothing visible is lost.
const QUANT_SUPPLY: f32 = 1.0;

/// I2a: quantization step for the `equipment_count` / `max_equipment_count`
/// digest fields. Equipment wears 1 item/tick per stocked slot and refills
/// toward max when funds allow (`supply.rs`), so a raw-count digest turned
/// the wear/refill oscillation into a per-tick `UnitUpdate` for every unit
/// with active equipment churn — the top idle-tick churn driver at the B0
/// baseline (~650 of the ~660 steady-state updates/tick, measured). Floor-
/// quantizing to 5-item bins absorbs up to 4 ticks of net change; when an
/// update IS emitted the wire carries the exact per-slot counts, so nothing
/// visible is lost.
const QUANT_EQUIP: u64 = 5;

/// I2a: floor-quantize an equipment total to `QUANT_EQUIP` bins
/// (100→100, 104→100, 105→105).
fn quantize_equip(count: u64) -> u64 {
    (count / QUANT_EQUIP) * QUANT_EQUIP
}

/// I2b: cold fields (`equipment`, `path`, `supply_percent`,
/// `boat_supply_percent`) are emitted on every COLD_EVERY-th tick (full
/// digest, exactly as pre-I2b) or when their per-field threshold trips
/// between cold ticks. The wire shape is unchanged — these fields are
/// `Option` + `skip_serializing_if`, so an update simply carries fewer
/// keys on a hot tick.
const COLD_EVERY: u64 = 10;

/// I2b: is this a cold tick (full digest, cold fields compared and emitted
/// exactly as pre-I2b)?
pub(crate) fn cold_due(tick: u64) -> bool {
    tick.is_multiple_of(COLD_EVERY)
}

/// I2b: emit the `equipment` Vec between cold ticks only when the total
/// item count moved by ≥ 2 × QUANT_EQUIP (10 items) — per-tick wear
/// (1/slot) never trips it, a combat casualty burst does.
fn equip_threshold(prev_count: u64, curr_count: u64) -> bool {
    prev_count.abs_diff(curr_count) >= 2 * QUANT_EQUIP
}

/// I2b: emit the `path` Vec between cold ticks only when the route starts
/// or ends (0 ↔ non-0) or its length moved by more than max(3, 10% of
/// prev_len) — captures "started moving" / "arrived" immediately while
/// mid-route waypoint reshuffles wait for the next cold tick.
fn path_threshold(prev_len: usize, curr_len: usize) -> bool {
    (prev_len == 0) != (curr_len == 0) || prev_len.abs_diff(curr_len) > usize::max(3, prev_len / 10)
}

/// I2b: emit a supply field between cold ticks only when a 10-point band
/// is crossed (0–9, 10–19, …). Sub-band drift is already invisible to the
/// UI (≤ QUANT_SUPPLY per tick), so it waits for the next cold tick.
fn band_crossed(prev: f32, curr: f32, band: f32) -> bool {
    (prev / band).floor() != (curr / band).floor()
}

/// Lightweight digest of a UnitState used for change detection.
/// Comparing two digests is O(1) with no heap allocation, whereas comparing
/// full UnitState structs requires cloning Vec<String>, Vec<EquipmentState>, etc.
///
/// If any field that affects gameplay rendering changes, the corresponding
/// digest field also changes, ensuring we never miss a real update.
///
/// I1: `from_ecs` is the single source of truth for the digest arithmetic —
/// the live in-place pass (`crate::server::delta::update_unit_baselines`,
/// fed via `from_view` over the engine's plain `UnitView`) and the wire
/// reference path (`from_unit`) both delegate to it.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct UnitStateDigest {
    position: (f32, f32),
    men: f32,
    max_men: f32,
    organisation: f32,
    equipment_count: u64, // total item count as a fast proxy
    max_equipment_count: u64,
    path_len: usize,
    healing: bool,
    resupply_enabled: bool,
    supply_percent: f32,
    encircled: bool,
    attacking: bool,
    on_water: bool,
    boat_supply_percent: f32,
    embarking: bool,
    disembarking: bool,
}

impl UnitStateDigest {
    /// Build a digest from raw (ECS or materialized) unit values; on a HOT
    /// tick (`is_cold == false`) the cold supply fields are zeroed so they
    /// cannot drive an update on their own — a cold field is compared only
    /// on cold ticks (full digest) or via its threshold gate in the update
    /// loop. men_step is I2a (see above).
    #[allow(clippy::too_many_arguments)] // one arg per digest field — the query tuple as-is
    pub(crate) fn from_ecs(
        position: (f32, f32),
        men: f32,
        max_men: f32,
        organisation: f32,
        equipment_count: u64,
        max_equipment_count: u64,
        path_len: usize,
        healing: bool,
        resupply_enabled: bool,
        supply_percent: f32,
        encircled: bool,
        attacking: bool,
        on_water: bool,
        boat_supply_percent: f32,
        embarking: bool,
        disembarking: bool,
        men_step: f32,
        is_cold: bool,
    ) -> Self {
        Self {
            position,
            men: (men / men_step).round() * men_step,
            max_men,
            organisation: (organisation / QUANT_ORG).round() * QUANT_ORG,
            equipment_count: quantize_equip(equipment_count),
            max_equipment_count: quantize_equip(max_equipment_count),
            path_len,
            healing,
            resupply_enabled,
            // I2b: cold — excluded from hot-tick equality.
            supply_percent: if is_cold {
                (supply_percent / QUANT_SUPPLY).round() * QUANT_SUPPLY
            } else {
                0.0
            },
            encircled,
            attacking,
            on_water,
            // I2b: cold — excluded from hot-tick equality.
            boat_supply_percent: if is_cold {
                (boat_supply_percent / QUANT_SUPPLY).round() * QUANT_SUPPLY
            } else {
                0.0
            },
            embarking,
            disembarking,
        }
    }

    /// Build a digest from a materialized `UnitState`. Delegates to `from_ecs`
    /// so the reference path (replay endpoint, tests) and the live in-place
    /// pass can never drift.
    pub(crate) fn from_unit(u: &UnitState, men_step: f32, is_cold: bool) -> Self {
        Self::from_ecs(
            u.position,
            u.men,
            u.max_men,
            u.organisation,
            u.equipment.iter().map(|e| e.count as u64).sum(),
            u.max_equipment.iter().map(|e| e.count as u64).sum(),
            u.path.len(),
            u.healing,
            u.resupply_enabled,
            u.supply_percent,
            u.encircled,
            u.attacking,
            u.on_water,
            u.boat_supply_percent,
            u.embarking,
            u.disembarking,
            men_step,
            is_cold,
        )
    }

    /// Build a digest from a plain engine `UnitView` (the live in-place
    /// pass). Delegates to `from_ecs` so the wire path and the engine-view
    /// path can never drift.
    pub(crate) fn from_view(u: &UnitView, men_step: f32, is_cold: bool) -> Self {
        Self::from_ecs(
            u.position,
            u.men,
            u.max_men,
            u.organisation,
            u.equipment.iter().map(|e| e.count as u64).sum(),
            u.max_equipment.iter().map(|e| e.count as u64).sum(),
            u.path.len(),
            u.healing,
            u.resupply_enabled,
            u.supply_percent,
            u.encircled,
            u.attacking,
            u.on_water,
            u.boat_supply_percent,
            u.embarking,
            u.disembarking,
            men_step,
            is_cold,
        )
    }
}

/// Player-independent delta components computed once per tick.
pub(crate) struct SharedDeltas {
    /// Full border grid (I4 full form). Mutually exclusive with
    /// `border_cells_changed`.
    pub border_grid_diff: Option<Arc<Vec<u8>>>,
    /// I4: sparse border changed-cell list (packed u32, see `diff_grid`).
    /// Mutually exclusive with `border_grid_diff`.
    pub border_cells_changed: Option<Arc<Vec<u32>>>,
    /// I4: number of cells that changed in the border grid this tick (==
    /// grid length when there was no previous border to diff against; 0 when
    /// the grid is unchanged). Feeds the A/B changed-cell-fraction metric.
    #[cfg_attr(
        not(any(feature = "microbenchmarks", feature = "bench-alloc")),
        allow(dead_code)
    )]
    pub border_changed_cells: u64,
    pub units_added: Arc<Vec<UnitState>>,
    pub units_removed: Arc<Vec<String>>,
    pub unit_updates: Arc<Vec<UnitUpdate>>,
    pub city_updates: Arc<Vec<CityUpdate>>,
    /// W8: per-owner unit counts, built once per tick in single O(N) passes so
    /// `build_delta_inner` does O(1) lookups instead of O(units) scans per
    /// player. Owned (not borrowed) on purpose: the maps arrive from the
    /// tick's `UnitDiff` (owned) and are moved into the result — no baseline
    /// borrow to outlive. Materializing costs one key clone per DISTINCT
    /// owner (≤ number of players), not per unit.
    pub prev_units_by_owner: HashMap<String, u32>,
    pub current_units_by_owner: HashMap<String, u32>,
}

/// W8: count units per owner in one pass. `owners` yields each unit's owner
/// (borrowed); the owned result clones one key per DISTINCT owner.
/// I5: exported so the tick loop can build the map ONCE and share it between
/// the active-player check and `compute_shared_deltas` (which no longer
/// recomputes it). Test-only in non-test builds since the replay
/// resimulation endpoint was replaced by JSONL streaming (kept: delta.rs and
/// game.rs unit tests exercise it).
#[cfg_attr(not(test), allow(dead_code))]
pub(crate) fn count_units_by_owner<'a>(
    owners: impl Iterator<Item = &'a str> + 'a,
) -> HashMap<String, u32> {
    let mut tmp: HashMap<&str, u32> = HashMap::new();
    for o in owners {
        *tmp.entry(o).or_insert(0) += 1;
    }
    tmp.into_iter().map(|(k, v)| (k.to_string(), v)).collect()
}

/// I1: field-level `UnitUpdate` for a unit whose digest already changed
/// (the digest check happens at the call site). Shared by the live in-place
/// pass (`update_unit_baselines`) and the HashMap reference path
/// (`compute_shared_deltas` — unit tests) so the two can never
/// drift in what fields an update carries.
///
/// `map_size` (width, height in cells) is the I3 position-quantization scale:
/// each packed axis is u16 over the full map (see `pack_unit_position`).
pub(crate) fn build_unit_update(
    prev: &UnitState,
    u: &UnitState,
    is_cold: bool,
    map_size: (u32, u32),
) -> UnitUpdate {
    let new_pos = if u.position != prev.position {
        // I3: quantized packed u32 (u16 x y over the full map) instead of
        // two f32s — the digest still compares the exact (f32, f32).
        Some(pack_unit_position(u.position, map_size))
    } else {
        None
    };

    let new_men = if u.men != prev.men { Some(u.men) } else { None };

    let new_org = if u.organisation != prev.organisation {
        Some(u.organisation)
    } else {
        None
    };

    // I2b: cold — on a hot tick only its threshold gate decides.
    let new_equipment = if is_cold
        || equip_threshold(
            prev.equipment.iter().map(|e| e.count as u64).sum(),
            u.equipment.iter().map(|e| e.count as u64).sum(),
        ) {
        if u.equipment != prev.equipment {
            // I3: per-slot COUNTS only, in `UnitState.equipment` slot order —
            // slot names/costs stay in the once-per-lifetime full state.
            Some(u.equipment.iter().map(|e| e.count).collect())
        } else {
            None
        }
    } else {
        None
    };

    let new_healing = if u.healing != prev.healing {
        Some(u.healing)
    } else {
        None
    };

    let new_resupply = if u.resupply_enabled != prev.resupply_enabled {
        Some(u.resupply_enabled)
    } else {
        None
    };

    // I2b: cold — on a hot tick only its threshold gate decides.
    let new_supply = if u.supply_percent != prev.supply_percent
        && (is_cold || band_crossed(prev.supply_percent, u.supply_percent, 10.0))
    {
        Some(u.supply_percent)
    } else {
        None
    };

    let new_encircled = if u.encircled != prev.encircled {
        Some(u.encircled)
    } else {
        None
    };

    // I2b: cold — on a hot tick only its threshold gate decides.
    let new_path = if is_cold || path_threshold(prev.path.len(), u.path.len()) {
        if u.path != prev.path {
            Some(u.path.clone())
        } else {
            None
        }
    } else {
        None
    };

    UnitUpdate {
        id: u.id.clone(),
        position: new_pos,
        men: new_men,
        organisation: new_org,
        equipment: new_equipment,
        path: new_path,
        healing: new_healing,
        resupply_enabled: new_resupply,
        supply_percent: new_supply,
        encircled: new_encircled,
        attacking: if u.attacking != prev.attacking {
            Some(u.attacking)
        } else {
            None
        },
        on_water: if u.on_water != prev.on_water {
            Some(u.on_water)
        } else {
            None
        },
        // I2b: cold — on a hot tick only its threshold gate decides.
        boat_supply_percent: if u.boat_supply_percent != prev.boat_supply_percent
            && (is_cold || band_crossed(prev.boat_supply_percent, u.boat_supply_percent, 10.0))
        {
            Some(u.boat_supply_percent)
        } else {
            None
        },
        embarking: if u.embarking != prev.embarking {
            Some(u.embarking)
        } else {
            None
        },
        disembarking: if u.disembarking != prev.disembarking {
            Some(u.disembarking)
        } else {
            None
        },
    }
}

/// H5: the identity/ownership/level fields the wire city diff compares.
/// Implemented by the engine's plain `City` (live path — the engine now owns
/// the baseline snapshot) and the protocol `CityState` (test-only HashMap
/// reference path), so both paths share one diff implementation.
trait CityDiff {
    fn city_id(&self) -> &str;
    fn city_owner(&self) -> &Option<String>;
    fn city_level(&self) -> u32;
}

impl CityDiff for City {
    fn city_id(&self) -> &str {
        &self.id
    }
    fn city_owner(&self) -> &Option<String> {
        &self.owner
    }
    fn city_level(&self) -> u32 {
        self.level
    }
}

impl CityDiff for CityState {
    fn city_id(&self) -> &str {
        &self.id
    }
    fn city_owner(&self) -> &Option<String> {
        &self.owner
    }
    fn city_level(&self) -> u32 {
        self.level
    }
}

/// City owner/level diff (shared by the live and HashMap reference paths).
/// H5: generic over the city type — the live path diffs the engine's plain
/// `City` snapshot against the engine-owned baseline, the reference path
/// keeps protocol `CityState`.
fn city_updates_diff<P: CityDiff, C: CityDiff>(
    prev_cities: &HashMap<String, P>,
    current_cities: &[C],
) -> Vec<CityUpdate> {
    let mut city_updates: Vec<CityUpdate> = Vec::new();
    for city in current_cities.iter() {
        if let Some(prev) = prev_cities.get(city.city_id()) {
            let owner_changed = city.city_owner() != prev.city_owner();
            let level_changed = city.city_level() != prev.city_level();

            if owner_changed || level_changed {
                city_updates.push(CityUpdate {
                    id: city.city_id().to_string(),
                    owner: if owner_changed {
                        city.city_owner().clone()
                    } else {
                        None
                    },
                    level: if level_changed {
                        Some(city.city_level())
                    } else {
                        None
                    },
                    manpower: None,
                });
            }
        }
    }
    city_updates
}

/// Compute the player-independent portions of the per-player delta.
/// Called once per tick, not once per player.
///
/// I1: this is the HashMap-based REFERENCE path — the unit tests keep using
/// it (no perf requirement there). The live tick loop instead runs
/// `update_unit_baselines` and builds the shared deltas from its
/// `UnitDiff` via `compute_shared_deltas_from_diff`.
/// (The old replay resimulation endpoint was its other consumer; replays are
/// now streamed JSONL, so this is test-only outside `cfg(test)`.)
#[cfg_attr(not(test), allow(dead_code))]
#[allow(clippy::too_many_arguments)] // I2a: men_step added; kept flat (not a struct) — it is the only runtime knob
pub(crate) fn compute_shared_deltas(
    prev_units: &HashMap<String, UnitState>,
    prev_cities: &HashMap<String, CityState>,
    current_units: &[UnitState],
    current_cities: &[CityState],
    current_unit_ids: &HashSet<&str>,
    prev_border_grid: Option<&Arc<Vec<u8>>>,
    current_border_grid: &Option<Arc<Vec<u8>>>,
    // I2a: runtime men digest step = `max(5.0, settings.recruitment_rate)`.
    men_step: f32,
    // I2b: current tick number — drives the cold-field gate (full digest on
    // every COLD_EVERY-th tick; threshold-gated cold fields in between).
    tick: u64,
    // I5: per-owner unit counts for the CURRENT snapshot, built by the
    // caller (`count_units_by_owner`) — one map per tick shared with the
    // active-player check instead of two.
    current_units_by_owner: HashMap<String, u32>,
    // I3: map (width, height) in cells — the packed-position quantization
    // scale (see `build_unit_update`).
    map_size: (u32, u32),
) -> SharedDeltas {
    // I2b: on a cold tick the digest is full (cold fields compared and
    // emitted exactly as pre-I2b); on a hot tick cold fields only emit when
    // their threshold trips.
    let is_cold = cold_due(tick);
    // Border grid diff (shared across all players).
    // W3: share the published Arc instead of copying the 40 KB grid — the
    // influence double-buffer publishes by refcount and never mutates the
    // buffer in place during the encode window. I4: when few cells changed,
    // ship the packed changed-cell list instead of the full grid.
    let (border_grid_diff, border_cells_changed, border_changed_cells) =
        grid_diff(prev_border_grid, current_border_grid.as_ref());

    // W8: per-owner unit counts, single O(N) pass each (build_delta_inner
    // previously rescanned every unit per player for these counts).
    // I5: the CURRENT map arrives from the caller (shared with the
    // active-player check); only the PREVIOUS map is still computed here.
    let prev_units_by_owner = count_units_by_owner(prev_units.values().map(|u| u.owner.as_str()));

    // Units added (new)
    let units_added: Arc<Vec<UnitState>> = Arc::new(
        current_units
            .iter()
            .filter(|u| !prev_units.contains_key(&u.id))
            .cloned()
            .collect(),
    );

    // Units removed (no longer in game) — N2: check against HashSet<&str>
    let units_removed: Arc<Vec<String>> = Arc::new(
        prev_units
            .keys()
            .filter(|id| !current_unit_ids.contains(id.as_str()))
            .cloned()
            .collect(),
    );

    // Units updated (position, stats, equipment changed)
    // Use digest-based comparison to avoid cloning full UnitState structs
    let mut unit_updates: Vec<UnitUpdate> = Vec::new();
    for u in current_units.iter() {
        if let Some(prev) = prev_units.get(&u.id) {
            let curr_digest = UnitStateDigest::from_unit(u, men_step, is_cold);
            let prev_digest = UnitStateDigest::from_unit(prev, men_step, is_cold);
            if curr_digest != prev_digest {
                unit_updates.push(build_unit_update(prev, u, is_cold, map_size));
            }
        }
    }

    // City updates
    let city_updates = city_updates_diff(prev_cities, current_cities);

    SharedDeltas {
        border_grid_diff,
        border_cells_changed: border_cells_changed.map(Arc::new),
        border_changed_cells,
        units_added,
        units_removed,
        unit_updates: Arc::new(unit_updates),
        city_updates: Arc::new(city_updates),
        prev_units_by_owner,
        current_units_by_owner,
    }
}

/// I4: bit width needed to pack a row-major cell index of a `cells`-cell
/// grid: `ceil(log2(cells))` (0 for a 1-cell grid). `None` when an 8-bit
/// value and the index cannot share a u32 (> 2^24 cells) — such grids
/// always use the full-grid wire form.
pub(crate) fn grid_index_bits(cells: usize) -> Option<u32> {
    let bits = (cells.max(1) as u64).next_power_of_two().trailing_zeros();
    (bits <= 24).then_some(bits)
}

/// I4: diff two flat grids of equal length into a packed changed-cell list.
///
/// Each list entry packs the NEW value and the row-major cell index into one
/// u32: `(new_value << bits) | cell_index`, `bits = ceil(log2(cells))`. The
/// value is mandatory for both grid kinds: the border value carries 255
/// (no owner) and vision flips include 1→0, so a set-bit-only encoding
/// would be lossy.
///
/// Returns `(sparse, changed)` where `sparse` is `Some(list)` when the
/// change is at or below the ~5% threshold (grid ships sparse) and `None`
/// otherwise (grid ships in full), and `changed` is always the exact number
/// of differing cells.
fn diff_grid(prev: &[u8], curr: &[u8]) -> (Option<Vec<u32>>, u64) {
    debug_assert_eq!(prev.len(), curr.len());
    let bits = match grid_index_bits(curr.len()) {
        Some(b) => b,
        None => return (None, curr.len() as u64),
    };
    // I4: the sparse form ships only while ≤ ~5% of the grid changed
    // (`changed <= len / 20`); above that the full grid is smaller on the
    // wire. This threshold is the one to tune if the I4 A/B shows the win
    // is poor (changed-cell fraction is reported in the bench grid_diag).
    let max_sparse = curr.len() / 20;
    let mut cells = Vec::new();
    let mut changed: u64 = 0;
    for (i, (&p, &c)) in prev.iter().zip(curr.iter()).enumerate() {
        if p != c {
            changed += 1;
            if cells.len() <= max_sparse {
                cells.push(((c as u32) << bits) | i as u32);
            }
        }
    }
    if changed as usize <= max_sparse {
        (Some(cells), changed)
    } else {
        (None, changed)
    }
}

/// I4: classify a grid diff into `(full_grid, sparse_cells, changed)`.
///
/// `full_grid` is the shared published Arc when the full grid must ship
/// (first delivery, >~5% change, or a map too large to pack); `sparse_cells`
/// is the packed list when the sparse form was chosen; the two are mutually
/// exclusive. `changed` is the number of differing cells (== grid length
/// when there was no previous grid to diff against; 0 when unchanged).
#[allow(clippy::type_complexity)]
pub(crate) fn grid_diff(
    prev: Option<&Arc<Vec<u8>>>,
    curr: Option<&Arc<Vec<u8>>>,
) -> (Option<Arc<Vec<u8>>>, Option<Vec<u32>>, u64) {
    match (prev, curr) {
        (Some(p), Some(c)) if !Arc::ptr_eq(p, c) => {
            let (sparse, changed) = diff_grid(p, c);
            if changed == 0 {
                (None, None, 0)
            } else {
                match sparse {
                    Some(list) => (None, Some(list), changed),
                    None => (Some(Arc::clone(c)), None, changed),
                }
            }
        }
        (None, Some(c)) => (Some(Arc::clone(c)), None, c.len() as u64),
        _ => (None, None, 0),
    }
}

/// I1: the LIVE path's unit diff, produced by `update_unit_baselines`
/// (single in-place ECS pass, no per-tick `Vec<UnitState>` materialization).
/// Wire-relevant content only: added units (full state), removed unit ids,
/// and field-level updates — plus the per-owner count maps the tick loop's
/// active-player check and `SharedDeltas` need.
pub(crate) struct UnitDiff {
    pub added: Vec<UnitState>,
    pub removed: Vec<String>,
    pub updates: Vec<UnitUpdate>,
    /// Per-owner unit counts for this tick's snapshot (replaces the caller's
    /// `count_units_by_owner` over the materialized snapshot; feeds the
    /// active-player check and `SharedDeltas::current_units_by_owner`).
    pub current_owner_counts: HashMap<String, u32>,
    /// Per-owner unit counts on the PRE-COMMIT baseline — what the old path
    /// computed from `prev_units` inside `compute_shared_deltas`.
    pub prev_owner_counts: HashMap<String, u32>,
}

/// H3: the per-tick shared (player-independent) diff inputs, bundled into
/// one structured value — the unit diff from the in-place baseline pass, the
/// current city snapshot, and the current territory (border) grid.
///
/// `run_network_phase` takes this by value, destructures it, and hands the
/// fields to `compute_shared_deltas_from_diff` (unit diff moved into
/// `SharedDeltas`; city snapshot + border grid borrowed for the diff and
/// moved into the end-of-phase baseline commit). The vision grid is
/// deliberately NOT part of this bundle: it is per-player (every player sees
/// a different grid), so its diff stays in the per-player `build_delta_inner`.
///
/// Replaces the parallel `unit_diff` + `current_cities` (+ internally
/// fetched border grid) parameters of the old `run_network_phase` /
/// `compute_shared_deltas_from_diff` call shape: a new shared diff dimension
/// now means one field, not one positional parameter at every call site.
pub(crate) struct TickDiffs {
    /// In-place unit diff (`update_unit_baselines`) — added / removed /
    /// field-level updates + the two per-owner count maps.
    pub units: UnitDiff,
    /// Current city snapshot — PLAIN engine `City` structs (Arch H2: the
    /// engine yields engine-owned data; the server converts to wire types
    /// only inside the diff/commit). The city update list is computed
    /// against `NetworkBaselines::prev_cities` in the shared compute.
    pub cities: Vec<City>,
    /// Current territory grid (published Arc — allocation-free clone); the
    /// border diff is computed against `NetworkBaselines::prev_border`.
    pub border_grid: Option<Arc<Vec<u8>>>,
}

/// I1: build the shared (player-independent) deltas from the live in-place
/// unit diff. Mirrors `compute_shared_deltas` minus the unit work, which the
/// diff already carries — so the live path no longer touches any
/// `HashMap<String, UnitState>` baseline at all.
///
/// H3: consumes the `TickDiffs` bundle's fields (destructured by the caller,
/// `run_network_phase`): the unit diff is moved into `SharedDeltas`; the city
/// snapshot and border grid are borrowed here and moved by the caller into
/// the end-of-phase baseline commit. Only the PRE-COMMIT baseline state
/// (`prev_cities`, `prev_border_grid`) is threaded separately — it lives on
/// `NetworkBaselines`.
pub(crate) fn compute_shared_deltas_from_diff(
    units: UnitDiff,
    cities: &[City],
    border_grid: Option<&Arc<Vec<u8>>>,
    prev_cities: &HashMap<String, City>,
    prev_border_grid: Option<&Arc<Vec<u8>>>,
) -> SharedDeltas {
    // Border grid diff (shared across all players) — identical to the
    // reference path: share the published Arc (full form) or the packed
    // changed-cell list (I4 sparse form) instead of copying the grid.
    let (border_grid_diff, border_cells_changed, border_changed_cells) =
        grid_diff(prev_border_grid, border_grid);

    SharedDeltas {
        border_grid_diff,
        border_cells_changed: border_cells_changed.map(Arc::new),
        border_changed_cells,
        units_added: Arc::new(units.added),
        units_removed: Arc::new(units.removed),
        unit_updates: Arc::new(units.updates),
        city_updates: Arc::new(city_updates_diff(prev_cities, cities)),
        prev_units_by_owner: units.prev_owner_counts,
        current_units_by_owner: units.current_owner_counts,
    }
}

/// Build a per-player delta message using pre-computed shared deltas.
///
/// The shared (player-independent) portions are computed once per tick via
/// `compute_shared_deltas`. This function handles the per-player vision diff,
/// player state, idle-skip check, and final message assembly.
///
/// W8: the per-owner unit counts are read from `shared` (precomputed in a
/// single O(N) pass) instead of taking `prev_units`/`current_units` and
/// scanning every unit per player.
pub(crate) fn build_delta_inner(
    prev_funds: &HashMap<String, u32>,
    prev_vision_grid: Option<&Arc<Vec<u8>>>,
    all_player_funds: &HashMap<String, u32>,
    current_vision_grid: &Option<Arc<Vec<u8>>>,
    grid_width: u32,
    player_id: &str,
    shared: &SharedDeltas,
) -> Option<(ServerMessage, u64)> {
    // Per-player vision grid diff.
    // W3: share the published Arc instead of copying the 40 KB grid (see the
    // border diff above for the lifetime invariant). I4: when few cells
    // changed, ship the packed changed-cell list instead of the full grid.
    let (vision_grid_diff, vision_cells_changed, vision_changed) =
        grid_diff(prev_vision_grid, current_vision_grid.as_ref());
    let vision_cells_changed = vision_cells_changed.map(Arc::new);

    let grid_width_out = if vision_grid_diff.is_some()
        || vision_cells_changed.is_some()
        || shared.border_grid_diff.is_some()
        || shared.border_cells_changed.is_some()
    {
        Some(grid_width)
    } else {
        None
    };

    // Per-player state
    let player_funds: u32 = all_player_funds.get(player_id).copied().unwrap_or(0);
    // W8: O(1) per-owner lookup (previously scanned all units per player).
    let units_owned: u32 = shared
        .current_units_by_owner
        .get(player_id)
        .copied()
        .unwrap_or(0);

    // Idle skip: must check BOTH shared and per-player changes.
    // W5: the spectator pid "" has no funds baseline entry, so a plain lookup
    // would report "funds changed" every tick and disable the idle-skip;
    // treat it as unchanged (spectator funds are always 0).
    let funds_changed = if player_id.is_empty() {
        false
    } else {
        prev_funds.get(player_id).copied() != Some(player_funds)
    };
    let prev_owned: u32 = shared
        .prev_units_by_owner
        .get(player_id)
        .copied()
        .unwrap_or(0);
    if shared.units_added.is_empty()
        && shared.units_removed.is_empty()
        && shared.unit_updates.is_empty()
        && shared.city_updates.is_empty()
        && shared.border_grid_diff.is_none()
        && shared.border_cells_changed.is_none()
        && vision_grid_diff.is_none()
        && vision_cells_changed.is_none()
        && !funds_changed
        && prev_owned == units_owned
    {
        return None;
    }

    // W7: only after the idle-skip passes — the `to_string()` below allocates
    // per player per tick, and the skip is the common path.
    let player_state = PlayerState {
        funds: player_funds,
        units_owned,
        player_id: player_id.to_string(),
    };

    let delta = GameDeltaMessage {
        vision_grid_diff,
        vision_cells_changed,
        border_grid_diff: shared.border_grid_diff.clone(),
        border_cells_changed: shared.border_cells_changed.clone(),
        grid_width: grid_width_out,
        units_added: shared.units_added.clone(),
        units_removed: shared.units_removed.clone(),
        unit_updates: shared.unit_updates.clone(),
        city_updates: shared.city_updates.clone(),
        player: player_state,
    };

    // I4: second return value — this player's vision changed-cell count for
    // the A/B changed-cell-fraction metric (0 when no vision diff shipped).
    Some((ServerMessage::DeltaState(delta), vision_changed))
}

// H5: the engine-neutral per-tick baselines (cities/funds/vision/border)
// live in the ENGINE module — `crate::game::NetworkBaselines` (imported
// above). They name only engine types and advance in a single commit step.
// The wire-typed unit baseline (`UnitBaselineStore`, below) stays
// server-owned (Arch H2); server callers keep the two side by side.

/// I1: entity-indexed unit baseline store — the replacement for the old
/// `HashMap<String, UnitState>` that `run_network_phase` rebuilt from a full
/// `unit_states_direct()` snapshot every tick.
///
/// Dense `Vec<Option<UnitState>>` indexed by the hecs entity id (low 32 bits
/// of `Entity::to_bits()`), with a per-slot `u32` generation stamp (upper 32
/// bits) guarding id reuse — the exact mechanism of
/// `TickContext.owner_at`/`owner_gen` (`game.rs:318–323`; hecs bumps the
/// generation on id reuse, so a stale slot never matches). The `alive` list
/// holds the entities with a live baseline; `touched` is a dense per-id
/// `(pass, gen)` stamp marking the incarnations confirmed alive by the
/// current pass (the previous scratch `HashSet<Entity>` hashed every entity
/// every tick; a dense stamp is an O(1) write/read with no hashing).
///
/// Slot reuse ⇒ same `unit_N` id, new incarnation ⇒ the gen check misses, so
/// the pass emits `removed` (tombstone) + `added` (fresh baseline) for it.
/// Strings (id/owner/unit_type/symbol, equipment names) are allocated once at
/// recruit and kept in the baseline; the common per-tick commit mutates
/// numeric fields in place (`update_unit_baselines`).
#[derive(Default)]
pub(crate) struct UnitBaselineStore {
    slots: Vec<Option<UnitState>>,
    gens: Vec<u32>,
    alive: Vec<Entity>,
    /// Last pass in which an incarnation of this id was confirmed alive:
    /// `(pass, gen)` — id+gen, not id only, so a same-tick death→respawn on
    /// the same slot can't keep the stale incarnation alive. `(0, 0)` marks
    /// untouched (hecs generations are NonZeroU32, so a real stamp is never
    /// `(0, 0)`).
    touched: Vec<(u32, u32)>,
    /// Pass counter feeding the `touched` stamps; bumped by `commit`.
    pass: u32,
    /// Fresh incarnations `insert`ed during the current pass (scratch;
    /// appended to `alive` at `commit`).
    new: Vec<Entity>,
}

/// Cap on the baseline store's entity-id span. Live hecs ids are bounded by
/// total spawns; the cap keeps a pathological id (e.g. `Entity::DANGLING` =
/// u32::MAX) from triggering a multi-GB `Vec` resize (mirrors
/// `OWNER_MAX_ID` in `TickContext`). Entities beyond the cap read as "no
/// baseline" (treated as new every pass — loudly wrong, but the input
/// can't arise from real hecs query results).
const UNIT_STORE_MAX_ID: usize = 1 << 18;

impl UnitBaselineStore {
    /// Pre-commit per-owner unit counts (what the old path derived from the
    /// `prev_units` HashMap) — one key clone per DISTINCT owner.
    pub fn prev_owner_counts(&self) -> HashMap<String, u32> {
        self.owner_counts()
    }

    /// Per-owner unit counts over the store's current alive set. Called
    /// PRE-commit for `prev_owner_counts` and POST-commit for the diff's
    /// `current_owner_counts` (after `commit`, `alive` holds exactly the
    /// live units, each with its just-refreshed owner — the same values the
    /// old path derived from the materialized snapshot via
    /// `count_units_by_owner`, without the per-tick `Vec<UnitState>`).
    pub fn owner_counts(&self) -> HashMap<String, u32> {
        let mut tmp: HashMap<&str, u32> = HashMap::new();
        for e in &self.alive {
            if let Some(u) = self.resolve(*e) {
                *tmp.entry(u.owner.as_str()).or_insert(0) += 1;
            }
        }
        tmp.into_iter().map(|(k, v)| (k.to_string(), v)).collect()
    }

    /// Resolve the live baseline for entity `e` (generation-checked).
    pub fn resolve(&self, e: Entity) -> Option<&UnitState> {
        let bits = e.to_bits().get();
        let id = (bits & 0xFFFF_FFFF) as usize;
        if id >= UNIT_STORE_MAX_ID {
            return None;
        }
        self.slots
            .get(id)
            .and_then(|s| s.as_ref())
            .filter(|_| self.gens.get(id).copied() == Some((bits >> 32) as u32))
    }

    pub fn resolve_mut(&mut self, e: Entity) -> Option<&mut UnitState> {
        let bits = e.to_bits().get();
        let id = (bits & 0xFFFF_FFFF) as usize;
        if id >= UNIT_STORE_MAX_ID {
            return None;
        }
        let gen_ok = self.gens.get(id).copied() == Some((bits >> 32) as u32);
        if gen_ok {
            self.slots.get_mut(id).and_then(|s| s.as_mut())
        } else {
            None
        }
    }

    /// Mark entity `e` as seen by the current pass (`commit` keeps exactly
    /// the touched alive entries); grows the dense tables on first sight of
    /// a new id.
    pub fn touch(&mut self, e: Entity) {
        let bits = e.to_bits().get();
        let id = (bits & 0xFFFF_FFFF) as usize;
        if id >= UNIT_STORE_MAX_ID {
            return;
        }
        let gen = (bits >> 32) as u32;
        if self.slots.len() <= id {
            self.slots.resize(id + 1, None);
            self.gens.resize(id + 1, 0);
            self.touched.resize(id + 1, (0, 0));
        }
        self.touched[id] = (self.pass, gen);
    }

    /// Take a STALE baseline out of `e`'s slot: the slot holds a different
    /// incarnation of the same id (death→respawn within a single pass; hecs
    /// reuses the id with a bumped generation). Returns the old state so the
    /// pass can emit `old.id` in `removed` before `insert`ing the fresh one
    /// (the removed+added pair for the same wire id). `None` when the slot
    /// is empty or already holds `e` itself.
    pub fn take_stale(&mut self, e: Entity) -> Option<UnitState> {
        let bits = e.to_bits().get();
        let id = (bits & 0xFFFF_FFFF) as usize;
        if id >= UNIT_STORE_MAX_ID {
            return None;
        }
        if self.gens.get(id).copied() == Some((bits >> 32) as u32) {
            return None; // slot is live for `e` — not stale
        }
        self.slots.get_mut(id).and_then(|s| s.take())
    }

    /// Insert a FRESH baseline (new incarnation). The slot must not currently
    /// resolve to a live baseline for `e` — callers gate this on `resolve`
    /// (after any `take_stale`).
    pub fn insert(&mut self, e: Entity, state: UnitState) {
        let bits = e.to_bits().get();
        let id = (bits & 0xFFFF_FFFF) as usize;
        debug_assert!(
            id < UNIT_STORE_MAX_ID && self.resolve(e).is_none(),
            "insert into a live baseline slot"
        );
        if id >= UNIT_STORE_MAX_ID {
            return;
        }
        if self.slots.len() <= id {
            self.slots.resize(id + 1, None);
            self.gens.resize(id + 1, 0);
            self.touched.resize(id + 1, (0, 0));
        }
        self.slots[id] = Some(state);
        self.gens[id] = (bits >> 32) as u32;
        self.new.push(e);
    }

    /// End of pass: prune the alive list to the touched entities and append
    /// the fresh incarnations inserted this pass. A dead unit's id is moved
    /// out of its baseline slot into `removed` — for an alive entry not
    /// touched this pass, ONLY when the slot still holds that incarnation
    /// (a same-tick respawn overwrote the slot with a NEW incarnation, and
    /// its `take_stale` already emitted the old id). Runs against the
    /// PRE-COMMIT state — the pass collected every live entity first, so
    /// dead units are still visible as `prev` when the diff was built.
    pub fn commit(&mut self, removed: &mut Vec<String>) {
        let mut n = self.alive.len();
        let mut keep = 0usize;
        while keep < n {
            let e = self.alive[keep];
            let bits = e.to_bits().get();
            let id = (bits & 0xFFFF_FFFF) as usize;
            let gen = (bits >> 32) as u32;
            if self.touched.get(id) == Some(&(self.pass, gen)) {
                keep += 1;
                continue;
            }
            // Not seen this pass. Emit its id only if the slot still holds
            // THIS incarnation (gen matches); a bumped gen means a new
            // incarnation was inserted over it this same pass and
            // `take_stale` already emitted the old id.
            if self.gens.get(id).copied() == Some(gen) {
                if let Some(u) = self.slots.get_mut(id).and_then(|s| s.take()) {
                    removed.push(u.id);
                }
            }
            self.alive.swap_remove(keep);
            n -= 1; // swapped-in tail entry is unprocessed; `keep` stays put
        }
        self.alive.append(&mut self.new);
        // Advance the pass. On u32 wrap, clear the stamps so no stale pass-0
        // stamp (from the very first pass) can match; 2^32 commits away.
        self.pass = self.pass.wrapping_add(1);
        if self.pass == 0 {
            for s in self.touched.iter_mut() {
                *s = (0, 0);
            }
            self.pass = 1;
        }
    }
}

// ============================================================================
// Arch H2 seam — engine snapshot → wire protocol conversion
//
// The engine (`crate::game`) exposes PLAIN data only: `UnitView` (borrowed,
// allocation-free, per-tick) and `UnitQueryData` (owned, full snapshot).
// This module owns the unit baseline store AND every conversion from that
// plain data to `protocol::UnitState` / `UnitUpdate` wire types — a
// protocol change touches this file + `protocol.rs`, never engine code.
// ============================================================================

/// The single engine→wire conversion for one unit (Arch H2 seam). The wire
/// id `unit_{id}` is derived from the hecs entity id — the pre-seam
/// `Game::unit_state_from_entity` did the same, from the components.
pub(crate) fn wire_unit_state(entity: Entity, u: &UnitView) -> UnitState {
    let eid = entity.id();
    UnitState {
        id: format!("unit_{eid}"),
        owner: u.owner.to_string(),
        position: u.position,
        unit_type: u.unit_type.to_string(),
        symbol: u.symbol.to_string(),
        men: u.men,
        max_men: u.max_men,
        organisation: u.organisation,
        equipment: u
            .equipment
            .iter()
            .map(|e| EquipmentState {
                name: e.name.clone(),
                count: e.count,
                cost_per_item: e.cost_per_item,
            })
            .collect(),
        max_equipment: u
            .max_equipment
            .iter()
            .map(|e| EquipmentState {
                name: e.name.clone(),
                count: e.count,
                cost_per_item: e.cost_per_item,
            })
            .collect(),
        path: u.path.to_vec(),
        healing: u.healing,
        resupply_enabled: u.resupply_enabled,
        supply_percent: u.supply_percent,
        encircled: u.encircled,
        attacking: u.attacking,
        on_water: u.on_water,
        boat_supply_percent: u.boat_supply_percent,
        embarking: u.embarking,
        disembarking: u.disembarking,
    }
}

/// Wire snapshot of every live unit (full-state builds + the HashMap
/// reference path). The body of the pre-seam `Game::unit_states()`, now
/// server-side over the engine's plain `UnitQueryData`.
pub(crate) fn wire_unit_states(game: &Game) -> Vec<UnitState> {
    game.query_all_units()
        .into_iter()
        .map(|u| {
            // Compute the derived percentages BEFORE moving the fields out.
            let supply_percent = u.supply_percent();
            let boat_supply_percent = u.boat_supply_percent();
            UnitState {
                id: u.id,
                owner: u.owner,
                position: u.position,
                unit_type: u.unit_type,
                symbol: u.symbol,
                men: u.men,
                max_men: u.max_men,
                organisation: u.organisation,
                equipment: u
                    .equipment
                    .into_iter()
                    .map(|e| EquipmentState {
                        name: e.name,
                        count: e.count,
                        cost_per_item: e.cost_per_item,
                    })
                    .collect(),
                max_equipment: u
                    .max_equipment
                    .into_iter()
                    .map(|e| EquipmentState {
                        name: e.name,
                        count: e.count,
                        cost_per_item: e.cost_per_item,
                    })
                    .collect(),
                path: u.path,
                healing: u.healing,
                resupply_enabled: u.resupply_enabled,
                supply_percent,
                encircled: u.encircled,
                attacking: u.fighting,
                on_water: u.on_water,
                boat_supply_percent,
                embarking: u.embarking,
                disembarking: u.disembarking,
            }
        })
        .collect()
}

/// I1: seed the entity-indexed unit baseline store from the current world
/// (server-side; was `Game::init_unit_baselines`). One wire materialization
/// per live unit, ONCE per game.
pub(crate) fn init_unit_baselines(game: &Game, store: &mut UnitBaselineStore) {
    game.for_each_unit(|entity, u| {
        store.touch(entity);
        store.insert(entity, wire_unit_state(entity, u));
    });
    // Seed the alive list so the first `commit` has correct prev state.
    // (update_unit_baselines's commit rebuilds `alive` from touched + fresh
    // inserts; do the first commit here so a world with zero units still
    // converges.)
    let mut init_removed = Vec::new();
    store.commit(&mut init_removed);
    debug_assert!(init_removed.is_empty());
}

/// I1: incremental snapshot — the single per-tick in-place ECS pass
/// (server-side; was `Game::update_states_in_place`). For every live unit
/// (delivered by the engine as a borrowed `UnitView`) it diffs against the
/// PRE-COMMIT baseline (`UnitBaselineStore`) and:
/// - unit absent from the baseline (new recruit, or death→respawn reusing
///   the same entity id at a bumped generation) → emit the full `UnitState`
///   in `added`, intern it in the store;
/// - digest changed → materialize ONLY this unit, emit a field-level
///   `UnitUpdate` (same `build_unit_update` field logic as the reference
///   path), and commit the materialized state into the slot;
/// - digest unchanged → commit the exact current values in place (no
///   allocation: numeric fields + equipment counts + path are written into
///   the baseline; strings are interned and invariant).
///
/// The store is committed (in-place writes + tombstone of dead slots) as
/// the final step of this call — AFTER the diff was built against the
/// pre-commit state, so dead units are still visible as `prev` for
/// `units_removed`. `UnitStateDigest` still decides what goes on the wire;
/// this pass only removes the per-tick full materialization.
pub(crate) fn update_unit_baselines(game: &Game, store: &mut UnitBaselineStore) -> UnitDiff {
    // I2a: runtime men digest step = max(5.0, recruitment_rate); I2b: the
    // cold-field gate is driven by the game tick — identical to the
    // reference path's `compute_shared_deltas` arguments.
    let men_step = game.settings().recruitment_rate.max(5.0);
    let is_cold = cold_due(game.tick_number());
    // I3: packed-position quantization scale — the map dimensions in cells.
    let map_size = (game.terrain_width(), game.terrain_height());

    // PRE-COMMIT owner counts (what the old path derived from prev_units).
    let prev_owner_counts = store.prev_owner_counts();

    let mut added: Vec<UnitState> = Vec::new();
    let mut updates: Vec<UnitUpdate> = Vec::new();
    // Dead-unit wire ids collected as the pass runs (stale-slot respawns)
    // and at `commit` (plain deaths) — moved out of the baseline slots, no
    // key clones.
    let mut removed: Vec<String> = Vec::new();

    game.for_each_unit(|entity, u| {
        store.touch(entity);

        // Digest inputs straight from the plain view (no materialization).
        let digest = UnitStateDigest::from_view(u, men_step, is_cold);

        if store.resolve(entity).is_none() {
            // New incarnation (recruit, or id reuse after death). A
            // non-empty stale slot means death→respawn within this one
            // pass: emit the old incarnation's id as removed BEFORE the
            // fresh one is inserted (removed+added pair, same wire id).
            if let Some(stale) = store.take_stale(entity) {
                removed.push(stale.id);
            }
            // Full materialization is the one place the per-unit alloc cost
            // legitimately remains.
            let ws = wire_unit_state(entity, u);
            store.insert(entity, ws.clone());
            added.push(ws);
        } else if UnitStateDigest::from_unit(store.resolve(entity).unwrap(), men_step, is_cold)
            != digest
        {
            // Digest changed: materialize ONLY this unit and build the
            // field-level update against the pre-commit baseline.
            let curr = wire_unit_state(entity, u);
            let update = store
                .resolve(entity)
                .map(|prev| build_unit_update(prev, &curr, is_cold, map_size))
                .expect("baseline slot resolved moments ago");
            // Commit: move the materialized state into the slot (the
            // update's equipment/path payloads are already cloned out).
            *store
                .resolve_mut(entity)
                .expect("baseline slot resolved moments ago") = curr;
            updates.push(update);
        } else if let Some(base) = store.resolve_mut(entity) {
            // Digest unchanged: no wire output. Commit the EXACT current
            // values in place (the baseline must track truth — the old
            // path re-committed the full snapshot every tick, so
            // thresholds like `equip_threshold`/`band_crossed` see the
            // same prev values and the wire stays bit-stable).
            refresh_baseline_in_place(base, u);
        }
    });

    // Commit: prune the alive list to the touched entities and emit the
    // plain-death removed ids — AFTER the diff was fully built against
    // the pre-commit state.
    store.commit(&mut removed);

    // Post-commit owner counts: `alive` now holds exactly the live units,
    // each with its just-refreshed owner (one key clone per DISTINCT owner,
    // same values as the old path's `count_units_by_owner` over the
    // materialized snapshot).
    let current_owner_counts = store.owner_counts();

    UnitDiff {
        added,
        removed,
        updates,
        current_owner_counts,
        prev_owner_counts,
    }
}

/// I1: in-place baseline commit for a digest-UNCHANGED unit (server-side;
/// was `Game::refresh_baseline_in_place`). Writes the exact current values
/// into the stored `UnitState`; allocation-free in the common case (the
/// path Vec only allocates on net growth, equipment slot lists are
/// invariant in length, and the owner String only changes when the owner
/// actually changes). `max_equipment`/`unit_type`/`symbol` are recruit-time
/// invariants and stay interned.
fn refresh_baseline_in_place(base: &mut UnitState, u: &UnitView) {
    base.position = u.position;
    base.men = u.men;
    base.max_men = u.max_men;
    base.organisation = u.organisation;
    base.healing = u.healing;
    base.resupply_enabled = u.resupply_enabled;
    base.supply_percent = u.supply_percent;
    base.encircled = u.encircled;
    base.attacking = u.attacking;
    base.on_water = u.on_water;
    base.boat_supply_percent = u.boat_supply_percent;
    base.embarking = u.embarking;
    base.disembarking = u.disembarking;
    // Equipment: slot names/costs are invariant; only the counts move.
    if base.equipment.len() == u.equipment.len() {
        for (b, e) in base.equipment.iter_mut().zip(u.equipment.iter()) {
            b.count = e.count;
        }
    } else {
        // Slot list changed (rare): rebuild (also keeps names/costs in sync
        // with the ECS, so the baseline stays truthful).
        base.equipment = u
            .equipment
            .iter()
            .map(|e| EquipmentState {
                name: e.name.clone(),
                count: e.count,
                cost_per_item: e.cost_per_item,
            })
            .collect();
    }
    // Path: in-place rewrite (capacity is reused on shrink/grow cycles).
    base.path.clear();
    base.path.extend_from_slice(u.path);
    // Owner: interned String, replaced only on an actual change.
    if base.owner != u.owner {
        base.owner = u.owner.to_string();
    }
}

/// Result of a single network-phase invocation.
pub(crate) struct NetworkPhaseResult {
    pub wire_bytes: u64,
    pub deltas_sent: u32,
    pub encoded: Vec<(String, Vec<u8>)>,
    /// W5: the shared (player-independent) deltas, exposed so the spectator
    /// path can reuse them instead of recomputing the whole shared phase.
    /// Computed against the pre-commit baseline (what the players saw).
    pub shared: SharedDeltas,
    pub grid_width: u32,
    /// W6: number of shared unit updates this tick (before the P× per-player
    /// fan-out) — the churn metric for digest quantization. Only read by the
    /// benchmark harness, hence the allow.
    #[allow(dead_code)]
    pub unit_updates: u32,
    /// Replay recording: when `run_network_phase` was called with a
    /// `record_pid`, the (still-decodable) `GameDeltaMessage` built for that
    /// player is retained here so the game loop can append it to the replay
    /// JSONL in the frontend's `{"delta": ...}` wire shape. `None` on idle
    /// ticks (no delta was built) and when no pid is being recorded — the
    /// clone cost is one per tick, for exactly one player.
    pub recorded_delta: Option<crate::server::protocol::GameDeltaMessage>,
    /// M3/W7: player ids whose delta encode FAILED this tick. Their frame
    /// is dropped (never sent — a truncated frame would desync the peer) and
    /// their per-player vision baseline is NOT committed (the next tick's
    /// vision diff then covers the missed change). The caller (tick task)
    /// closes these connections: the shared unit/city/funds baselines have
    /// advanced with or without the delivery, so the player's stream is
    /// unrecoverable for this game.
    pub encode_failures: Vec<String>,
    /// B0: sub-phase timing — shared compute (`compute_shared_deltas` +
    /// `SpliceShared::build`) plus the per-player `build_delta_inner` calls,
    /// in ms. Feature-gated so a production build pays zero timing cost (the
    /// field doesn't exist).
    #[cfg(any(feature = "microbenchmarks", feature = "bench-alloc"))]
    #[allow(dead_code)]
    pub delta_ms: f64,
    /// B0: sub-phase timing — the per-player CBOR encode calls
    /// (`encode_delta_spliced` / `encode_message_with_hint`), in ms.
    #[cfg(any(feature = "microbenchmarks", feature = "bench-alloc"))]
    #[allow(dead_code)]
    pub encode_ms: f64,
    /// B0: true when this tick emitted a border or per-player vision grid diff
    /// (the grid-clean vs grid-dirty split for the benchmark's regime table).
    #[cfg(any(feature = "microbenchmarks", feature = "bench-alloc"))]
    #[allow(dead_code)]
    pub grid_dirty: bool,
    /// I4: total grid cells changed this tick (border once + each player's
    /// vision; the synthetic spectator is not part of the network phase).
    /// Feeds the A/B changed-cell-fraction metric used to tune the ~5%
    /// sparse threshold.
    #[cfg(any(feature = "microbenchmarks", feature = "bench-alloc"))]
    #[allow(dead_code)]
    pub grid_changed_cells: u64,
}

/// Run the full per-tick network phase: per-player delta build, encode, and
/// baseline commit. The caller is responsible for actually sending the
/// encoded bytes over the wire.
///
/// I1: the `current_units: Vec<UnitState>` materialization param and the tail
/// `prev_units` HashMap rebuild are gone — the caller runs
/// `update_unit_baselines` first (single in-place ECS pass, baseline
/// committed there).
///
/// H3: the per-tick shared diff inputs arrive as one structured `TickDiffs`
/// value (unit diff + city snapshot + border grid) instead of parallel
/// parameters.
pub(crate) fn run_network_phase(
    game: &Game,
    baselines: &mut NetworkBaselines,
    player_ids: &[&str],
    diffs: TickDiffs,
    spliced: bool,
    record_pid: Option<&str>,
) -> NetworkPhaseResult {
    let all_funds = game.all_player_funds();
    // H3: destructure the bundle — the unit diff moves into the shared
    // compute; the city snapshot + border grid move into the end-of-phase
    // baseline commit below.
    let TickDiffs {
        units,
        cities,
        border_grid,
    } = diffs;
    let grid_width = game.terrain_width();

    // B0: sub-phase timing accumulators (feature-gated; zero cost in a
    // production build where the corresponding `Instant`s are compiled out).
    #[cfg(any(feature = "microbenchmarks", feature = "bench-alloc"))]
    let (mut delta_ms, mut encode_ms) = (0.0f64, 0.0f64);

    // W-2: compute player-independent deltas once per tick, not once per player.
    #[cfg(any(feature = "microbenchmarks", feature = "bench-alloc"))]
    let shared_start = std::time::Instant::now();
    // I1: the unit work (added/removed/updates + both per-owner count maps)
    // already happened in `update_unit_baselines` against the
    // pre-commit baseline; this only adds the border grid diff + city diff.
    let shared = compute_shared_deltas_from_diff(
        units,
        &cities,
        border_grid.as_ref(),
        &baselines.prev_cities,
        baselines.prev_border.as_ref(),
    );
    #[cfg(any(feature = "microbenchmarks", feature = "bench-alloc"))]
    {
        delta_ms += shared_start.elapsed().as_secs_f64() * 1000.0;
    }
    // B0: border diff is player-independent; per-player vision diffs are
    // detected in the loop below. I4: the sparse changed-cell form counts
    // as grid-dirty too.
    #[cfg(any(feature = "microbenchmarks", feature = "bench-alloc"))]
    let mut grid_dirty = shared.border_grid_diff.is_some() || shared.border_cells_changed.is_some();
    // I4: changed-cell counts (border once + per-player vision) for the
    // A/B changed-cell-fraction metric.
    #[cfg(any(feature = "microbenchmarks", feature = "bench-alloc"))]
    let mut grid_changed_cells = shared.border_changed_cells;

    // W9 (stretch, opt-in): pre-encode the shared delta segments ONCE per
    // tick so the per-player encode splices them in instead of re-serializing
    // them P times. Only built when the caller enables the splice path; the
    // standard `encode_message_with_hint` path (default) is untouched.
    // B0: `SpliceShared::build` is shared-segment encoding, so it is charged
    // to the encode sub-phase (the elapsed window is ~0 when `!spliced`).
    #[cfg(any(feature = "microbenchmarks", feature = "bench-alloc"))]
    let splice_start = std::time::Instant::now();
    let splice_shared = spliced.then(|| {
        crate::server::protocol::SpliceShared::build(
            shared.border_grid_diff.as_ref(),
            shared.border_cells_changed.as_ref(),
            &shared.units_added,
            &shared.units_removed,
            &shared.unit_updates,
            &shared.city_updates,
        )
    });
    #[cfg(any(feature = "microbenchmarks", feature = "bench-alloc"))]
    {
        encode_ms += splice_start.elapsed().as_secs_f64() * 1000.0;
    }

    // W6: capture the shared update count before `shared` is moved into result.
    let unit_updates = shared.unit_updates.len() as u32;

    let mut result = NetworkPhaseResult {
        wire_bytes: 0,
        deltas_sent: 0,
        encoded: Vec::new(),
        shared,
        grid_width,
        unit_updates,
        recorded_delta: None,
        encode_failures: Vec::new(),
        // B0: sub-phase fields (feature-gated; see the struct definition).
        #[cfg(any(feature = "microbenchmarks", feature = "bench-alloc"))]
        delta_ms,
        #[cfg(any(feature = "microbenchmarks", feature = "bench-alloc"))]
        encode_ms,
        #[cfg(any(feature = "microbenchmarks", feature = "bench-alloc"))]
        grid_dirty,
        #[cfg(any(feature = "microbenchmarks", feature = "bench-alloc"))]
        grid_changed_cells,
    };

    // H5: per-player vision commits collected in the loop, applied in the
    // single end-of-phase commit step below.
    let mut vision_commits: Vec<(String, Arc<Vec<u8>>)> = Vec::new();

    for pid in player_ids {
        // B0: per-player delta-build window (vision lookup + build_delta_inner
        // are the player-dependent half of the "delta" sub-phase). Deliberately
        // disjoint from the encode window below, so the report's sub-rows
        // (snapshot + delta + encode) are non-overlapping parts of `net`.
        #[cfg(any(feature = "microbenchmarks", feature = "bench-alloc"))]
        let player_delta_start = std::time::Instant::now();
        let vision_grid = game.vision_grid_flat(pid);
        let prev_vision: Option<&Arc<Vec<u8>>> = baselines.prev_vision.get(*pid);
        let built = build_delta_inner(
            &baselines.prev_funds,
            prev_vision,
            &all_funds,
            &vision_grid,
            grid_width,
            pid,
            &result.shared,
        );
        #[cfg(any(feature = "microbenchmarks", feature = "bench-alloc"))]
        {
            delta_ms += player_delta_start.elapsed().as_secs_f64() * 1000.0;
        }

        if let Some((delta, vision_changed)) = built {
            // I4: the vision changed-cell count only feeds the bench
            // changed-cell-fraction metric — drop it explicitly so
            // feature-off builds don't warn about the unused binding.
            #[cfg(not(any(feature = "microbenchmarks", feature = "bench-alloc")))]
            let _ = vision_changed;
            let delta_ref = match &delta {
                ServerMessage::DeltaState(d) => d,
                _ => unreachable!(),
            };
            // Replay recording: retain this player's delta (before it is
            // moved into the encoder) so the game loop can log it as JSON.
            if record_pid == Some(*pid) {
                result.recorded_delta = Some(delta_ref.clone());
            }
            #[cfg(any(feature = "microbenchmarks", feature = "bench-alloc"))]
            {
                if delta_ref.vision_grid_shipped() {
                    grid_dirty = true;
                }
                grid_changed_cells += vision_changed;
            }
            // B0: encode sub-phase window (per-player serialize call only).
            #[cfg(any(feature = "microbenchmarks", feature = "bench-alloc"))]
            let encode_start = std::time::Instant::now();
            let encoded = if let Some(ss) = &splice_shared {
                // W9: splice the shared segments in (byte-for-byte identical to
                // the standard encoder, pinned by the byte-for-byte test).
                // The splice path is benchmark-only and keeps its legacy
                // infallible shape (its failure modes are test-harness panics,
                // not a live peer). The production (standard) path below is
                // fallible per the M3/W7 contract.
                let map_bytes = crate::server::protocol::encode_delta_spliced(delta_ref, ss);
                let mut out = Vec::with_capacity(
                    crate::server::protocol::DELTA_STATE_VARIANT_WRAPPER.len() + map_bytes.len(),
                );
                out.extend_from_slice(&crate::server::protocol::DELTA_STATE_VARIANT_WRAPPER);
                out.extend_from_slice(&map_bytes);
                out
            } else {
                // N5: pre-size the CBOR buffer
                let cap = estimate_delta_size(delta_ref);
                match encode_message_with_hint(&delta, cap) {
                    Ok(bytes) => bytes,
                    Err(e) => {
                        // M3/W7: this player's delta encode failed. Drop the
                        // frame (the truncated buffer is NEVER sent), skip
                        // this player's vision-baseline commit below (so the
                        // next tick's diff covers the missed change), and
                        // report the pid so the tick task closes the
                        // connection — a half frame is unrecoverable for the
                        // peer, and the shared baselines advanced regardless.
                        tracing::error!(
                            error = %e,
                            player_id = %pid,
                            "delta encode failed — dropping frame for this tick"
                        );
                        result.encode_failures.push(pid.to_string());
                        continue;
                    }
                }
            };
            #[cfg(any(feature = "microbenchmarks", feature = "bench-alloc"))]
            {
                encode_ms += encode_start.elapsed().as_secs_f64() * 1000.0;
            }
            result.wire_bytes += encoded.len() as u64;
            result.deltas_sent += 1;
            result.encoded.push((pid.to_string(), encoded));
        }

        // N1: commit Arc refcount bump (no allocation) — H5: collected here,
        // applied in the single end-of-phase commit step (an encode failure
        // `continue`s above it, so that player keeps its stale grid).
        if let Some(v) = &vision_grid {
            vision_commits.push((pid.to_string(), Arc::clone(v)));
        }
    }

    // B0: the accumulators only reached `result` at construction (Copy types),
    // so copy the per-player loop totals back in here, after the loop.
    #[cfg(any(feature = "microbenchmarks", feature = "bench-alloc"))]
    {
        result.delta_ms = delta_ms;
        result.encode_ms = encode_ms;
        result.grid_dirty = grid_dirty;
        result.grid_changed_cells = grid_changed_cells;
    }

    // H5: single commit step — every engine-neutral baseline (cities/funds/
    // border + the per-player vision grids) advances in one atomic call.
    // I1: the unit baseline is committed separately by `update_unit_baselines`
    // (server-owned store, Arch H2) — it is no longer rebuilt here.
    baselines.commit(cities, all_funds, border_grid, vision_commits);

    result
}

/// Build LLM agent state with filtered units based on vision grid.
pub fn build_llm_state(game: &Game, player_id: &str) -> crate::llm::agent::GameState {
    let vision_grid = game.vision_grid_2d(player_id).unwrap_or_default();
    let border_grid = game.border_grid_2d(player_id).unwrap_or_default();

    // Arch H2 seam: source the units from the engine's plain `UnitQueryData`
    // snapshot (no wire hop) — the LLM mapping was previously fed by
    // `Game::unit_states()` wire types.
    let units: Vec<crate::llm::agent::UnitState> = game
        .query_all_units()
        .iter()
        .filter(|unit| {
            let (gx, gy) = (unit.position.0 as usize, unit.position.1 as usize);
            unit.owner == player_id
                || (gy < vision_grid.len() && gx < vision_grid[gy].len() && vision_grid[gy][gx])
        })
        .map(|u| crate::llm::agent::UnitState {
            id: u.id.clone(),
            owner: u.owner.clone(),
            position: u.position,
            men: u.men,
            max_men: u.max_men,
            organisation: u.organisation,
            equipment: u
                .equipment
                .iter()
                .zip(u.max_equipment.iter())
                .map(|(e, me)| crate::llm::agent::EquipmentState {
                    name: e.name.clone(),
                    count: e.count,
                    max_count: me.count,
                })
                .collect(),
            unit_type: u.unit_type.clone(),
            symbol: u.symbol.clone(),
            supply_percent: u.supply_percent(),
            resupply_enabled: u.resupply_enabled,
        })
        .collect();

    let cities: Vec<crate::llm::agent::CityState> = game.with_cities(|all| {
        all.iter()
            .filter(|city| {
                let (gx, gy) = (city.position.0 as usize, city.position.1 as usize);
                city.owner.as_deref() == Some(player_id)
                    || (gy < vision_grid.len() && gx < vision_grid[gy].len() && vision_grid[gy][gx])
            })
            .map(|c| crate::llm::agent::CityState {
                id: c.id.clone(),
                owner: c.owner.clone(),
                level: c.level,
                position: c.position,
                manpower: 0,
            })
            .collect()
    });

    let funds: u32 = game.all_player_funds().get(player_id).copied().unwrap_or(0);

    crate::llm::agent::GameState {
        units,
        cities,
        funds,
        player_id: player_id.to_string(),
        vision_grid,
        border_grid,
    }
}

#[cfg(test)]
mod tests {

    /// I3: the packed-position quantization map for delta tests
    /// (any positive dims work; positions are cell coords).
    const TEST_MAP: (u32, u32) = (1000, 1000);
    use super::*;
    use crate::game::economy::UnitType;

    fn unit(id: &str, owner: &str, x: f32, y: f32) -> UnitState {
        UnitState {
            id: id.to_string(),
            owner: owner.to_string(),
            position: (x, y),
            unit_type: "infantry".to_string(),
            symbol: "i".to_string(),
            men: 100.0,
            max_men: 100.0,
            organisation: 100.0,
            equipment: vec![],
            max_equipment: vec![],
            path: vec![],
            healing: false,
            resupply_enabled: true,
            supply_percent: 100.0,
            encircled: false,
            attacking: false,
            on_water: false,
            boat_supply_percent: 0.0,
            embarking: false,
            disembarking: false,
        }
    }

    fn city(id: &str, owner: Option<&str>, level: u32) -> CityState {
        CityState {
            id: id.to_string(),
            position: (1.0, 1.0),
            owner: owner.map(|s| s.to_string()),
            manpower: 0,
            level,
        }
    }

    fn units_map(units: &[UnitState]) -> HashMap<String, UnitState> {
        units.iter().map(|u| (u.id.clone(), u.clone())).collect()
    }

    fn funds(player_id: &str, amount: u32) -> HashMap<String, u32> {
        let mut m = HashMap::new();
        m.insert(player_id.to_string(), amount);
        m
    }

    /// Run build_delta_inner with the given current state over the given prev
    /// snapshots, for a single player. No grids involved.
    fn run(
        prev_units: &HashMap<String, UnitState>,
        prev_cities: &HashMap<String, CityState>,
        prev_funds: &HashMap<String, u32>,
        current_units: &[UnitState],
        current_cities: &[CityState],
        all_funds: &HashMap<String, u32>,
        player_id: &str,
    ) -> Option<ServerMessage> {
        let current_unit_ids: HashSet<&str> = current_units.iter().map(|u| u.id.as_str()).collect();
        let shared = compute_shared_deltas(
            prev_units,
            prev_cities,
            current_units,
            current_cities,
            &current_unit_ids,
            None,
            &None,
            5.0,
            1000, // I2b: cold tick — full digest (pre-I2b behavior)
            count_units_by_owner(current_units.iter().map(|u| u.owner.as_str())),
            TEST_MAP,
        );
        build_delta_inner(prev_funds, None, all_funds, &None, 0, player_id, &shared).map(|(m, _)| m)
    }

    fn as_delta(msg: Option<ServerMessage>) -> GameDeltaMessage {
        match msg {
            Some(ServerMessage::DeltaState(d)) => d,
            other => panic!(
                "expected DeltaState, got {:?}",
                other.map(|m| std::mem::discriminant(&m))
            ),
        }
    }

    #[test]
    fn identical_snapshots_return_none() {
        let units = vec![unit("u1", "a", 1.0, 2.0)];
        let cities = vec![city("c1", Some("a"), 1)];
        let all_funds = funds("a", 42);
        let prev_units = units_map(&units);
        let prev_cities: HashMap<String, CityState> =
            cities.iter().map(|c| (c.id.clone(), c.clone())).collect();
        let prev_funds = all_funds.clone();

        let msg = run(
            &prev_units,
            &prev_cities,
            &prev_funds,
            &units,
            &cities,
            &all_funds,
            "a",
        );
        assert!(msg.is_none());
    }

    #[test]
    fn single_unit_position_change_yields_one_update() {
        let prev_units = vec![unit("u1", "a", 1.0, 2.0)];
        let curr_units = vec![unit("u1", "a", 3.0, 2.0)];
        let all_funds = funds("a", 10);
        let pu = units_map(&prev_units);
        let pc = HashMap::new();
        let pf = all_funds.clone();

        let msg = run(&pu, &pc, &pf, &curr_units, &[], &all_funds, "a");
        let d = as_delta(msg);
        assert_eq!(d.unit_updates.len(), 1);
        assert_eq!(d.unit_updates[0].id, "u1");
        assert_eq!(
            d.unit_updates[0].position,
            Some(pack_unit_position((3.0, 2.0), TEST_MAP))
        );
        assert!(d.units_added.is_empty());
        assert!(d.units_removed.is_empty());
    }

    #[test]
    fn spectator_pre_commit_baseline_sees_shared_updates() {
        // W1 backfill: the spectator block runs against the PRE-commit
        // baseline (the snapshot is moved into run_network_phase after the
        // spectator delta is built), so a moved unit appears in the spectator's
        // shared unit_updates. pid "" has no prev_funds entry, mirroring
        // production (PlayerConnection::Spectator -> id "").
        let prev_units = vec![unit("u1", "a", 1.0, 2.0)];
        let curr_units = vec![unit("u1", "a", 3.0, 2.0)];
        let all_funds = funds("a", 10);
        let pu = units_map(&prev_units);
        let pc = HashMap::new();
        let pf = all_funds.clone();

        let msg = run(&pu, &pc, &pf, &curr_units, &[], &all_funds, "");
        let d = as_delta(msg);
        assert_eq!(d.player.player_id, "");
        assert_eq!(d.unit_updates.len(), 1);
        assert_eq!(d.unit_updates[0].id, "u1");
        assert_eq!(
            d.unit_updates[0].position,
            Some(pack_unit_position((3.0, 2.0), TEST_MAP))
        );
    }

    #[test]
    fn spectator_idle_tick_returns_none() {
        // W5: the spectator pid "" has no funds baseline entry, so before the
        // empty-pid special case `funds_changed` was always true and the
        // spectator received a delta message every tick. A truly idle tick
        // (no shared deltas, no vision diff, no ownership change) must yield
        // None now.
        let units = vec![unit("u1", "a", 1.0, 2.0)];
        let all_funds = funds("a", 10); // no "" entry, mirroring production
        let pu = units_map(&units);
        let pc = HashMap::new();
        let pf = all_funds.clone();

        let msg = run(&pu, &pc, &pf, &units, &[], &all_funds, "");
        assert!(msg.is_none());
    }

    #[test]
    fn funds_only_change_is_sent_to_changed_player_only() {
        let units = vec![unit("u1", "a", 1.0, 2.0)];
        let prev_units = units_map(&units);
        let prev_cities = HashMap::new();

        // Player a funds 10 -> 20, player b unchanged at 5
        let mut all_funds = HashMap::new();
        all_funds.insert("a".to_string(), 20);
        all_funds.insert("b".to_string(), 5);
        let mut prev_funds = HashMap::new();
        prev_funds.insert("a".to_string(), 10);
        prev_funds.insert("b".to_string(), 5);

        let msg_a = run(
            &prev_units,
            &prev_cities,
            &prev_funds,
            &units,
            &[],
            &all_funds,
            "a",
        );
        let d = as_delta(msg_a);
        assert_eq!(d.player.funds, 20);

        let msg_b = run(
            &prev_units,
            &prev_cities,
            &prev_funds,
            &units,
            &[],
            &all_funds,
            "b",
        );
        assert!(msg_b.is_none());
    }

    #[test]
    fn city_owner_change_yields_city_update() {
        let prev_cities = [city("c1", Some("a"), 1)];
        let curr_cities = vec![city("c1", Some("b"), 1)];
        let all_funds = funds("a", 0);
        let pu = HashMap::new();
        let pc: HashMap<String, CityState> = prev_cities
            .iter()
            .map(|c| (c.id.clone(), c.clone()))
            .collect();
        let pf = all_funds.clone();

        let msg = run(&pu, &pc, &pf, &[], &curr_cities, &all_funds, "a");
        let d = as_delta(msg);
        assert_eq!(d.city_updates.len(), 1);
        assert_eq!(d.city_updates[0].id, "c1");
        assert_eq!(d.city_updates[0].owner.as_deref(), Some("b"));
    }

    #[test]
    fn grid_change_includes_grid_diff() {
        let all_funds = funds("a", 0);
        let pu = HashMap::new();
        let pc = HashMap::new();
        let pf = all_funds.clone();
        // Flat vision: 2x2 grid, row-major: [1, 0, 0, 1]
        let vision = Arc::new(vec![1u8, 0, 0, 1]);

        let current_unit_ids: HashSet<&str> = HashSet::new();

        let shared = compute_shared_deltas(
            &pu,
            &pc,
            &[],
            &[],
            &current_unit_ids,
            None,
            &None,
            5.0,
            1000,
            count_units_by_owner(std::iter::empty::<&str>()),
            TEST_MAP,
        );
        let msg = build_delta_inner(
            &pf,
            None,
            &all_funds,
            &Some(Arc::clone(&vision)),
            2,
            "a",
            &shared,
        )
        .map(|(m, _)| m);
        let d = as_delta(msg);
        // Wire format is flat u8; W3: the Arc is shared, not copied.
        assert_eq!(
            d.vision_grid_diff.as_ref().map(|v| v.as_slice()),
            Some(vision.as_slice())
        );
        assert!(Arc::ptr_eq(d.vision_grid_diff.as_ref().unwrap(), &vision));
        assert!(d.border_grid_diff.is_none());
        assert_eq!(d.grid_width, Some(2));
    }

    #[test]
    fn grid_diff_shares_arc() {
        // W3: changed border and vision grids are shared via Arc refcount
        // instead of 40 KB deep copies.
        let all_funds = funds("a", 0);
        let pu = HashMap::new();
        let pc = HashMap::new();
        let pf = all_funds.clone();
        let current_unit_ids: HashSet<&str> = HashSet::new();

        // Border: prev differs from curr -> diff must BE the curr Arc.
        let prev_border = Arc::new(vec![255u8, 255, 255, 255]);
        let curr_border = Arc::new(vec![0u8, 1, 2, 3]);
        let shared = compute_shared_deltas(
            &pu,
            &pc,
            &[],
            &[],
            &current_unit_ids,
            Some(&prev_border),
            &Some(Arc::clone(&curr_border)),
            5.0,
            1000, // I2b: cold tick — full digest (pre-I2b behavior)
            count_units_by_owner(std::iter::empty::<&str>()),
            TEST_MAP,
        );
        assert!(
            Arc::ptr_eq(shared.border_grid_diff.as_ref().unwrap(), &curr_border),
            "border diff must share the published Arc"
        );

        // Vision: no prev -> diff must BE the curr Arc.
        let vision = Arc::new(vec![1u8, 0, 0, 1]);
        let msg = build_delta_inner(
            &pf,
            None,
            &all_funds,
            &Some(Arc::clone(&vision)),
            2,
            "a",
            &shared,
        )
        .map(|(m, _)| m);
        let d = as_delta(msg);
        assert!(
            Arc::ptr_eq(d.vision_grid_diff.as_ref().unwrap(), &vision),
            "vision diff must share the published Arc"
        );
    }

    #[test]
    fn first_tick_from_empty_prev_sends_added_units() {
        let units = vec![unit("u1", "a", 1.0, 2.0), unit("u2", "b", 3.0, 4.0)];
        let all_funds = funds("a", 7);
        let pu = HashMap::new();
        let pc = HashMap::new();
        let pf = HashMap::new();

        let msg = run(&pu, &pc, &pf, &units, &[], &all_funds, "a");
        let d = as_delta(msg);
        assert_eq!(d.units_added.len(), 2);
        assert_eq!(d.player.units_owned, 1);
    }

    #[test]
    fn units_owned_change_is_detected() {
        // Same unit set, but the unit's owner changed — only units_owned
        // differs for player a.
        let prev_units = vec![unit("u1", "a", 1.0, 2.0)];
        let curr_units = vec![unit("u1", "b", 1.0, 2.0)];
        let mut all_funds = funds("a", 0);
        all_funds.insert("b".to_string(), 0);
        let pu = units_map(&prev_units);
        let pc = HashMap::new();
        let pf = all_funds.clone();

        let msg = run(&pu, &pc, &pf, &curr_units, &[], &all_funds, "a");
        assert!(msg.is_some());
    }

    #[test]
    fn two_players_share_read_only_baseline() {
        // Both players should receive a delta when a unit moves, since the
        // baseline is read-only and not mutated by the first player's call.
        let prev_units = vec![unit("u1", "a", 1.0, 2.0)];
        let curr_units = vec![unit("u1", "a", 5.0, 2.0)];
        let mut all_funds = HashMap::new();
        all_funds.insert("a".to_string(), 10);
        all_funds.insert("b".to_string(), 10);
        let pu = units_map(&prev_units);
        let pc = HashMap::new();
        let pf = all_funds.clone();

        let msg_a = run(&pu, &pc, &pf, &curr_units, &[], &all_funds, "a");
        assert!(msg_a.is_some());

        // Player B must also see the update (baseline was not mutated by player A's call)
        let msg_b = run(&pu, &pc, &pf, &curr_units, &[], &all_funds, "b");
        assert!(msg_b.is_some());
    }

    #[test]
    fn per_player_vision_baseline_independent() {
        // Player A's vision changed; player B's did not. Player B should
        // not receive a vision grid diff.
        let mut all_funds = HashMap::new();
        all_funds.insert("a".to_string(), 0);
        all_funds.insert("b".to_string(), 0);
        let pu = HashMap::new();
        let pc = HashMap::new();
        let pf = all_funds.clone();
        let current_unit_ids: HashSet<&str> = HashSet::new();

        // Flat vision: 2x2 grid, row-major: [1, 0, 0, 1]
        let vision_a = Arc::new(vec![1u8, 0, 0, 1]);

        let shared = compute_shared_deltas(
            &pu,
            &pc,
            &[],
            &[],
            &current_unit_ids,
            None,
            &None,
            5.0,
            1000,
            count_units_by_owner(std::iter::empty::<&str>()),
            TEST_MAP,
        );

        // Player A: vision changed from None to Some(vision_a) -> diff present
        let msg_a = build_delta_inner(
            &pf,
            None,
            &all_funds,
            &Some(Arc::clone(&vision_a)),
            2,
            "a",
            &shared,
        )
        .map(|(m, _)| m);
        let d_a = as_delta(msg_a);
        assert_eq!(
            d_a.vision_grid_diff.as_ref().map(|v| v.as_slice()),
            Some(vision_a.as_slice())
        );

        // Player B: vision unchanged (both None) -> no diff
        let msg_b = build_delta_inner(&pf, None, &all_funds, &None, 2, "b", &shared);
        assert!(msg_b.is_none());
    }

    #[test]
    fn owner_count_maps_match_scan() {
        // W8: the precomputed per-owner count maps must equal the brute-force
        // per-unit scan they replace.
        let mut prev = Vec::new();
        let mut curr = Vec::new();
        // 3 owners × 5 units, plus one owner that disappears and one that appears.
        for i in 0..5 {
            prev.push(unit(&format!("p0_{}", i), "a", 0.0, 0.0));
            prev.push(unit(&format!("p1_{}", i), "b", 0.0, 0.0));
            prev.push(unit(&format!("p2_{}", i), "gone", 0.0, 0.0));
        }
        for i in 0..5 {
            curr.push(unit(&format!("c0_{}", i), "a", 0.0, 0.0));
            curr.push(unit(&format!("c1_{}", i), "b", 0.0, 0.0));
            curr.push(unit(&format!("c2_{}", i), "new", 0.0, 0.0));
        }
        let pu = units_map(&prev);
        let cu = curr.clone();
        let current_unit_ids: HashSet<&str> = cu.iter().map(|u| u.id.as_str()).collect();
        let shared = compute_shared_deltas(
            &pu,
            &HashMap::new(),
            &cu,
            &[],
            &current_unit_ids,
            None,
            &None,
            5.0,
            1000, // I2b: cold tick — full digest (pre-I2b behavior)
            count_units_by_owner(cu.iter().map(|u| u.owner.as_str())),
            TEST_MAP,
        );

        // Brute-force counts for the CURRENT set.
        for (owner, expected) in [("a", 5u32), ("b", 5), ("new", 5)] {
            assert_eq!(
                shared
                    .current_units_by_owner
                    .get(owner)
                    .copied()
                    .unwrap_or(0),
                expected,
                "current count for {owner}"
            );
        }
        assert_eq!(shared.current_units_by_owner.len(), 3);

        // Brute-force counts for the PREV set (owner "gone" present, "new" absent).
        for (owner, expected) in [("a", 5u32), ("b", 5), ("gone", 5)] {
            assert_eq!(
                shared.prev_units_by_owner.get(owner).copied().unwrap_or(0),
                expected,
                "prev count for {owner}"
            );
        }
        assert_eq!(shared.prev_units_by_owner.len(), 3);
    }

    #[test]
    fn count_units_by_owner_matches_brute_force_scan() {
        // I5: the exported helper is now built by the TICK LOOP (shared with
        // the active-player check) instead of inside compute_shared_deltas.
        // Pin its semantics against a brute-force per-unit scan: exact
        // per-owner counts, owners with zero units absent, empty input →
        // empty map.
        let mut curr = Vec::new();
        for i in 0..5 {
            curr.push(unit(&format!("p0_{}", i), "a", 0.0, 0.0));
            curr.push(unit(&format!("p1_{}", i), "b", 0.0, 0.0));
            if i < 2 {
                curr.push(unit(&format!("p2_{}", i), "a", 0.0, 0.0));
            }
        }
        let map = count_units_by_owner(curr.iter().map(|u| u.owner.as_str()));
        assert_eq!(map.get("a").copied(), Some(7)); // 5 + 2
        assert_eq!(map.get("b").copied(), Some(5));
        assert_eq!(map.len(), 2);

        // Empty input → empty map (no phantom owners).
        let empty = count_units_by_owner(std::iter::empty::<&str>());
        assert!(empty.is_empty());
    }

    #[test]
    fn digest_quantized_men_equal_within_step() {
        // I2a: the men step is now runtime (max(5.0, recruitment_rate)); the
        // tests pass 5.0 explicitly. men differs by 2.0 (< step 5.0) and both
        // values land in the 50.0 bin ((50/5).round()*5 == (52/5).round()*5 ==
        // 50.0) → no update emitted.
        let mut prev = unit("u1", "a", 1.0, 2.0);
        prev.men = 50.0;
        let mut curr = unit("u1", "a", 1.0, 2.0);
        curr.men = 52.0; // 2.0 difference — within one 5.0 quantization step

        let pu = units_map(&[prev.clone()]);
        let pu_ref = &pu;
        let cu = vec![curr];

        let current_unit_ids: HashSet<&str> = cu.iter().map(|u| u.id.as_str()).collect();
        let shared = compute_shared_deltas(
            pu_ref,
            &HashMap::new(),
            &cu,
            &[],
            &current_unit_ids,
            None,
            &None,
            5.0,
            1000, // I2b: cold tick — full digest (pre-I2b behavior)
            count_units_by_owner(cu.iter().map(|u| u.owner.as_str())),
            TEST_MAP,
        );

        // Since the quantized digests are equal, no unit_updates should be produced
        assert!(shared.unit_updates.is_empty());
    }

    #[test]
    fn digest_quantized_men_differs_across_step() {
        // I2a: men differs by 5.0, which equals the step → digests differ
        // (50.0 bin vs 55.0 bin) → update emitted.
        let mut prev = unit("u1", "a", 1.0, 2.0);
        prev.men = 50.0;
        let mut curr = unit("u1", "a", 1.0, 2.0);
        curr.men = 55.0; // exactly one 5.0 step difference

        let pu = units_map(&[prev.clone()]);
        let pu_ref = &pu;
        let cu = vec![curr];

        let current_unit_ids: HashSet<&str> = cu.iter().map(|u| u.id.as_str()).collect();
        let shared = compute_shared_deltas(
            pu_ref,
            &HashMap::new(),
            &cu,
            &[],
            &current_unit_ids,
            None,
            &None,
            5.0,
            1000, // I2b: cold tick — full digest (pre-I2b behavior)
            count_units_by_owner(cu.iter().map(|u| u.owner.as_str())),
            TEST_MAP,
        );

        // Digests differ → one unit update should be produced
        assert_eq!(shared.unit_updates.len(), 1);
        assert_eq!(shared.unit_updates[0].id, "u1");
        // The update carries the exact (unquantized) value
        assert_eq!(shared.unit_updates[0].men, Some(55.0));
    }

    #[test]
    fn digest_quantized_equip_equal_within_step() {
        // I2a: equipment total 100 → 104 stays in the same 5-item floor bin
        // (104 / 5 * 5 == 100) → no update emitted.
        let mut prev = unit("u1", "a", 1.0, 2.0);
        prev.equipment = vec![crate::server::protocol::EquipmentState {
            name: "sword".to_string(),
            count: 100,
            cost_per_item: 1.0,
        }];
        let mut curr = unit("u1", "a", 1.0, 2.0);
        curr.equipment = vec![crate::server::protocol::EquipmentState {
            name: "sword".to_string(),
            count: 104,
            cost_per_item: 1.0,
        }];

        let pu = units_map(&[prev.clone()]);
        let pu_ref = &pu;
        let cu = vec![curr];

        let current_unit_ids: HashSet<&str> = cu.iter().map(|u| u.id.as_str()).collect();
        let shared = compute_shared_deltas(
            pu_ref,
            &HashMap::new(),
            &cu,
            &[],
            &current_unit_ids,
            None,
            &None,
            5.0,
            1000, // I2b: cold tick — full digest (pre-I2b behavior)
            count_units_by_owner(cu.iter().map(|u| u.owner.as_str())),
            TEST_MAP,
        );

        // Same 5-item bin → no unit_updates should be produced
        assert!(shared.unit_updates.is_empty());
    }

    #[test]
    fn digest_quantized_equip_differs_across_step() {
        // I2a: equipment total 100 → 105 crosses into the next 5-item bin →
        // update emitted, carrying the exact (unquantized) equipment Vec.
        let mut prev = unit("u1", "a", 1.0, 2.0);
        prev.equipment = vec![crate::server::protocol::EquipmentState {
            name: "sword".to_string(),
            count: 100,
            cost_per_item: 1.0,
        }];
        let mut curr = unit("u1", "a", 1.0, 2.0);
        curr.equipment = vec![crate::server::protocol::EquipmentState {
            name: "sword".to_string(),
            count: 105,
            cost_per_item: 1.0,
        }];

        let pu = units_map(&[prev.clone()]);
        let pu_ref = &pu;
        let cu = vec![curr];

        let current_unit_ids: HashSet<&str> = cu.iter().map(|u| u.id.as_str()).collect();
        let shared = compute_shared_deltas(
            pu_ref,
            &HashMap::new(),
            &cu,
            &[],
            &current_unit_ids,
            None,
            &None,
            5.0,
            1000, // I2b: cold tick — full digest (pre-I2b behavior)
            count_units_by_owner(cu.iter().map(|u| u.owner.as_str())),
            TEST_MAP,
        );

        assert_eq!(shared.unit_updates.len(), 1);
        assert_eq!(shared.unit_updates[0].id, "u1");
        // I3: the update carries the exact (unquantized) per-slot counts.
        assert_eq!(
            shared.unit_updates[0].equipment.as_deref(),
            Some([105u32].as_slice())
        );
    }

    /// I3: slot-order invariant — an equipment update's count vector has the
    /// SAME length as the full state's slot list and each count aligns with
    /// its slot (order = `UnitState.equipment` order). The client maps counts
    /// onto the full-state slot table by index, so a length or order drift
    /// would silently assign counts to the wrong slot.
    #[test]
    fn equipment_update_slot_order_invariant() {
        // Three slots with distinct names in a specific order; two counts
        // change in a non-monotonic way so any ordering bug is visible.
        let slots = [
            ("rifle_alpha", 100u32, 1.5f32),
            ("cannon_bravo", 40, 12.0),
            ("medkit_charlie", 7, 0.5),
        ];
        let make_unit = |counts: [u32; 3]| {
            let mut u = unit("u1", "a", 1.0, 2.0);
            u.equipment = slots
                .iter()
                .enumerate()
                .map(
                    |(i, (name, _, cost))| crate::server::protocol::EquipmentState {
                        name: (*name).to_string(),
                        count: counts[i],
                        cost_per_item: *cost,
                    },
                )
                .collect();
            u
        };

        let prev = make_unit([100, 40, 7]);
        let curr = make_unit([95, 44, 3]); // slot 0 & 1 change, slot 2 untouched

        let upd = build_unit_update(&prev, &curr, true, TEST_MAP); // cold tick
        let counts = upd
            .equipment
            .as_ref()
            .expect("equipment changed → counts present");
        assert_eq!(
            counts.len(),
            curr.equipment.len(),
            "update length == full-state length"
        );
        for (i, slot) in curr.equipment.iter().enumerate() {
            assert_eq!(
                counts[i], slot.count,
                "slot {i} count aligned with full state"
            );
        }
        assert_eq!(counts.as_slice(), [95u32, 44, 3]);

        // Wire roundtrip through the shared-delta path: the counts survive
        // encode/decode with their slot order intact.
        let pu = units_map(&[prev]);
        let cu = vec![curr];
        let ids: HashSet<&str> = cu.iter().map(|u| u.id.as_str()).collect();
        let shared = compute_shared_deltas(
            &pu,
            &HashMap::new(),
            &cu,
            &[],
            &ids,
            None,
            &None,
            5.0,
            1000,
            count_units_by_owner(cu.iter().map(|u| u.owner.as_str())),
            TEST_MAP,
        );
        assert_eq!(shared.unit_updates.len(), 1);
        let encoded = crate::server::protocol::encode_message(&ServerMessage::DeltaState(
            crate::server::protocol::GameDeltaMessage {
                vision_grid_diff: None,
                vision_cells_changed: None,
                border_grid_diff: None,
                border_cells_changed: None,
                grid_width: None,
                units_added: shared.units_added.clone(),
                units_removed: shared.units_removed.clone(),
                unit_updates: shared.unit_updates.clone(),
                city_updates: shared.city_updates.clone(),
                player: crate::server::protocol::PlayerState {
                    funds: 0,
                    units_owned: 0,
                    player_id: "a".to_string(),
                },
            },
        ));
        let decoded: ServerMessage = ciborium::from_reader(encoded.as_slice()).unwrap();
        if let ServerMessage::DeltaState(d) = decoded {
            assert_eq!(
                d.unit_updates[0].equipment.as_deref(),
                Some([95u32, 44, 3].as_slice())
            );
        } else {
            panic!("Expected DeltaState");
        }
    }

    // ---- I2b: cold-field gate pure-fn tests ----

    #[test]
    fn cold_due_boundary_ticks() {
        // 9 is hot, 10 is cold; the pattern repeats every COLD_EVERY.
        assert!(!cold_due(9));
        assert!(cold_due(0)); // tick 0 is cold (0 % 10 == 0)
        assert!(cold_due(10));
        assert!(!cold_due(19));
        assert!(cold_due(20));
    }

    #[test]
    fn equip_threshold_boundary() {
        // 9 items of net wear is below the 2×QUANT_EQUIP=10 gate; 10 trips it.
        assert!(!equip_threshold(100, 91));
        assert!(!equip_threshold(91, 100)); // symmetric (abs_diff)
        assert!(equip_threshold(100, 90));
        assert!(equip_threshold(90, 100));
    }

    #[test]
    fn path_threshold_boundary() {
        // 0 ↔ non-0 always trips (started moving / arrived).
        assert!(path_threshold(0, 1));
        assert!(path_threshold(1, 0));
        assert!(!path_threshold(0, 0));
        // Same length: no trip.
        assert!(!path_threshold(10, 10));
        // Length change up to max(3, 10% of prev) is absorbed…
        assert!(!path_threshold(10, 7)); // Δ3 ≤ max(3, 1)
        assert!(!path_threshold(20, 17)); // Δ3 ≤ max(3, 2); …beyond it trips
        assert!(path_threshold(10, 6)); // Δ4 > 3
        assert!(path_threshold(50, 44)); // Δ6 > max(3, 5)
    }

    #[test]
    fn band_crossed_boundary() {
        // Same 10-point band: silent.
        assert!(!band_crossed(19.9, 19.99, 10.0));
        assert!(!band_crossed(20.0, 29.9, 10.0));
        // Crossing a band edge (20.0, 30.0, …): trips.
        assert!(band_crossed(19.9, 20.1, 10.0));
        assert!(band_crossed(20.1, 19.9, 10.0)); // symmetric
        assert!(band_crossed(29.9, 30.1, 10.0));
        // 0→9 stays in band 0.
        assert!(!band_crossed(0.0, 9.9, 10.0));
        assert!(band_crossed(9.9, 10.1, 10.0));
    }

    /// I2b integration: on a HOT tick a unit whose only visible changes are
    /// a position move plus sub-threshold cold drift (1 item of equipment
    /// wear, same-length path, sub-band supply) emits a UnitUpdate carrying
    /// position but NOT equipment/path/supply_percent. On the next COLD tick
    /// the same drift is carried in full.
    #[test]
    fn hot_tick_omits_cold_fields_cold_tick_carries_them() {
        let mut prev = unit("u1", "a", 1.0, 2.0);
        prev.equipment = vec![crate::server::protocol::EquipmentState {
            name: "sword".to_string(),
            count: 100,
            cost_per_item: 1.0,
        }];
        prev.path = vec![(2.0, 2.0), (3.0, 2.0)];
        prev.supply_percent = 55.0;

        let mut curr = prev.clone();
        curr.position = (2.0, 2.0); // hot: moved
        curr.equipment[0].count = 99; // cold: 1 item of wear — below the 10 gate
        curr.path = vec![(3.0, 2.0), (4.0, 2.0)]; // cold: same length — no gate trip
        curr.supply_percent = 56.0; // cold: 1% drift — no band crossing

        let pu = units_map(&[prev.clone()]);
        let pu_ref = &pu;
        let cu = vec![curr.clone()];
        let current_unit_ids: HashSet<&str> = cu.iter().map(|u| u.id.as_str()).collect();

        // HOT tick (tick 11): update emitted (position moved) but the cold
        // fields are omitted — their thresholds did not trip.
        let shared = compute_shared_deltas(
            pu_ref,
            &HashMap::new(),
            &cu,
            &[],
            &current_unit_ids,
            None,
            &None,
            5.0,
            11,
            count_units_by_owner(cu.iter().map(|u| u.owner.as_str())),
            TEST_MAP,
        );
        assert_eq!(shared.unit_updates.len(), 1);
        let upd = &shared.unit_updates[0];
        assert_eq!(upd.position, Some(pack_unit_position((2.0, 2.0), TEST_MAP)));
        assert!(
            upd.equipment.is_none(),
            "1-item wear must not emit equipment"
        );
        assert!(upd.path.is_none(), "same-length path must not emit path");
        assert!(
            upd.supply_percent.is_none(),
            "sub-band supply must not emit"
        );

        // COLD tick (tick 10 → 20): the same cold drift is carried in full.
        let prev2 = units_map(&[curr.clone()]);
        let prev2_ref = &prev2;
        let mut curr2 = curr.clone();
        curr2.equipment[0].count = 99; // unchanged now
        curr2.supply_percent = 56.0;
        let cu2 = vec![curr2.clone()];
        let ids2: HashSet<&str> = cu2.iter().map(|u| u.id.as_str()).collect();
        let shared2 = compute_shared_deltas(
            prev2_ref,
            &HashMap::new(),
            &cu2,
            &[],
            &ids2,
            None,
            &None,
            5.0,
            20,
            count_units_by_owner(cu2.iter().map(|u| u.owner.as_str())),
            TEST_MAP,
        );
        // No hot change and no cold change vs prev2 → no update at all on a
        // cold tick either (digest equality).
        assert!(shared2.unit_updates.is_empty());

        // Now a COLD tick vs the ORIGINAL prev (cold drift present): the cold
        // fields ARE carried.
        let shared3 = compute_shared_deltas(
            pu_ref,
            &HashMap::new(),
            &cu,
            &[],
            &current_unit_ids,
            None,
            &None,
            5.0,
            20,
            count_units_by_owner(cu.iter().map(|u| u.owner.as_str())),
            TEST_MAP,
        );
        assert_eq!(shared3.unit_updates.len(), 1);
        let upd3 = &shared3.unit_updates[0];
        assert_eq!(
            upd3.position,
            Some(pack_unit_position((2.0, 2.0), TEST_MAP))
        );
        assert!(upd3.equipment.is_some(), "cold tick carries equipment");
        assert!(upd3.path.is_some(), "cold tick carries path");
        assert_eq!(upd3.supply_percent, Some(56.0), "cold tick carries supply");
    }

    #[test]
    fn digest_quantized_org_equal_within_step() {
        // organisation differs by 0.3, within one quantization step
        let mut prev = unit("u1", "a", 1.0, 2.0);
        prev.organisation = 80.0;
        let mut curr = unit("u1", "a", 1.0, 2.0);
        curr.organisation = 80.3;

        let pu = units_map(&[prev.clone()]);
        let pu_ref = &pu;
        let cu = vec![curr];

        let current_unit_ids: HashSet<&str> = cu.iter().map(|u| u.id.as_str()).collect();
        let shared = compute_shared_deltas(
            pu_ref,
            &HashMap::new(),
            &cu,
            &[],
            &current_unit_ids,
            None,
            &None,
            5.0,
            1000, // I2b: cold tick — full digest (pre-I2b behavior)
            count_units_by_owner(cu.iter().map(|u| u.owner.as_str())),
            TEST_MAP,
        );

        assert!(shared.unit_updates.is_empty());
    }

    #[test]
    fn digest_quantized_org_differs_across_step() {
        // organisation differs by 1.0 → digests differ → update emitted
        let mut prev = unit("u1", "a", 1.0, 2.0);
        prev.organisation = 80.0;
        let mut curr = unit("u1", "a", 1.0, 2.0);
        curr.organisation = 81.0;

        let pu = units_map(&[prev.clone()]);
        let pu_ref = &pu;
        let cu = vec![curr];

        let current_unit_ids: HashSet<&str> = cu.iter().map(|u| u.id.as_str()).collect();
        let shared = compute_shared_deltas(
            pu_ref,
            &HashMap::new(),
            &cu,
            &[],
            &current_unit_ids,
            None,
            &None,
            5.0,
            1000, // I2b: cold tick — full digest (pre-I2b behavior)
            count_units_by_owner(cu.iter().map(|u| u.owner.as_str())),
            TEST_MAP,
        );

        assert_eq!(shared.unit_updates.len(), 1);
        assert_eq!(shared.unit_updates[0].id, "u1");
        assert_eq!(shared.unit_updates[0].organisation, Some(81.0));
    }

    #[test]
    fn digest_quantized_supply_equal_within_step() {
        // W6: supply differs by 0.2, within one QUANT_SUPPLY step (99.7 and
        // 99.9 both round to 100.0) → digests equal → no update emitted.
        let mut prev = unit("u1", "a", 1.0, 2.0);
        prev.supply_percent = 99.7;
        let mut curr = unit("u1", "a", 1.0, 2.0);
        curr.supply_percent = 99.9;

        let pu = units_map(&[prev.clone()]);
        let cu = vec![curr];

        let current_unit_ids: HashSet<&str> = cu.iter().map(|u| u.id.as_str()).collect();
        let shared = compute_shared_deltas(
            &pu,
            &HashMap::new(),
            &cu,
            &[],
            &current_unit_ids,
            None,
            &None,
            5.0,
            1000, // I2b: cold tick — full digest (pre-I2b behavior)
            count_units_by_owner(cu.iter().map(|u| u.owner.as_str())),
            TEST_MAP,
        );

        assert!(shared.unit_updates.is_empty());
    }

    #[test]
    fn digest_quantized_supply_differs_across_step() {
        // W6: supply differs by 1.0 (99.7 → 98.7: 100.0 vs 99.0) → digests
        // differ → update emitted, carrying the EXACT (unquantized) value.
        let mut prev = unit("u1", "a", 1.0, 2.0);
        prev.supply_percent = 99.7;
        let mut curr = unit("u1", "a", 1.0, 2.0);
        curr.supply_percent = 98.7;

        let pu = units_map(&[prev.clone()]);
        let cu = vec![curr];

        let current_unit_ids: HashSet<&str> = cu.iter().map(|u| u.id.as_str()).collect();
        let shared = compute_shared_deltas(
            &pu,
            &HashMap::new(),
            &cu,
            &[],
            &current_unit_ids,
            None,
            &None,
            5.0,
            1000, // I2b: cold tick — full digest (pre-I2b behavior)
            count_units_by_owner(cu.iter().map(|u| u.owner.as_str())),
            TEST_MAP,
        );

        assert_eq!(shared.unit_updates.len(), 1);
        assert_eq!(shared.unit_updates[0].id, "u1");
        assert_eq!(shared.unit_updates[0].supply_percent, Some(98.7));
    }

    #[test]
    fn digest_quantized_boat_supply_equal_within_step() {
        // W6: boat supply differs by 0.3, within one step (55.1 and 55.4
        // both round to 55.0) → no update emitted.
        let mut prev = unit("u1", "a", 1.0, 2.0);
        prev.on_water = true;
        prev.boat_supply_percent = 55.1;
        let mut curr = unit("u1", "a", 1.0, 2.0);
        curr.on_water = true;
        curr.boat_supply_percent = 55.4;

        let pu = units_map(&[prev.clone()]);
        let cu = vec![curr];

        let current_unit_ids: HashSet<&str> = cu.iter().map(|u| u.id.as_str()).collect();
        let shared = compute_shared_deltas(
            &pu,
            &HashMap::new(),
            &cu,
            &[],
            &current_unit_ids,
            None,
            &None,
            5.0,
            1000, // I2b: cold tick — full digest (pre-I2b behavior)
            count_units_by_owner(cu.iter().map(|u| u.owner.as_str())),
            TEST_MAP,
        );

        assert!(shared.unit_updates.is_empty());
    }

    #[test]
    fn digest_quantized_boat_supply_differs_across_step() {
        // W6: boat supply crosses a step (55.4 → 55.9: 55.0 vs 56.0) →
        // update emitted, carrying the exact value.
        let mut prev = unit("u1", "a", 1.0, 2.0);
        prev.on_water = true;
        prev.boat_supply_percent = 55.4;
        let mut curr = unit("u1", "a", 1.0, 2.0);
        curr.on_water = true;
        curr.boat_supply_percent = 55.9;

        let pu = units_map(&[prev.clone()]);
        let cu = vec![curr];

        let current_unit_ids: HashSet<&str> = cu.iter().map(|u| u.id.as_str()).collect();
        let shared = compute_shared_deltas(
            &pu,
            &HashMap::new(),
            &cu,
            &[],
            &current_unit_ids,
            None,
            &None,
            5.0,
            1000, // I2b: cold tick — full digest (pre-I2b behavior)
            count_units_by_owner(cu.iter().map(|u| u.owner.as_str())),
            TEST_MAP,
        );

        assert_eq!(shared.unit_updates.len(), 1);
        assert_eq!(shared.unit_updates[0].id, "u1");
        assert_eq!(shared.unit_updates[0].boat_supply_percent, Some(55.9));
    }

    // ---------------------------------------------------------------------------
    // I4: sparse grid diffs (packed changed-cell lists)
    // ---------------------------------------------------------------------------

    /// Apply a packed changed-cell list to a copy of `prev` (the client-side
    /// inverse of `diff_grid`) and return the resulting grid.
    fn apply_cells(prev: &[u8], cells: &[u32]) -> Vec<u8> {
        let n = prev.len();
        let bits = grid_index_bits(n).unwrap();
        let mask = if bits == 0 { 0 } else { (1u32 << bits) - 1 };
        let mut out = prev.to_vec();
        for &p in cells {
            out[(p & mask) as usize] = (p >> bits) as u8;
        }
        out
    }

    #[test]
    fn grid_index_bits_forms() {
        assert_eq!(grid_index_bits(0), Some(0));
        assert_eq!(grid_index_bits(1), Some(0));
        assert_eq!(grid_index_bits(2), Some(1));
        assert_eq!(grid_index_bits(1024), Some(10));
        assert_eq!(grid_index_bits(1000 * 1000), Some(20));
        assert_eq!(grid_index_bits(1usize << 24), Some(24));
        // > 2^24 cells: an 8-bit value and the index cannot share a u32 —
        // such grids always use the full-grid wire form.
        assert_eq!(grid_index_bits((1usize << 24) + 1), None);
    }

    #[test]
    fn sparse_vs_full_threshold_boundary() {
        // 10×10 grid: max_sparse = 100 / 20 = 5, bits = ceil(log2(100)) = 7.
        let pu = HashMap::new();
        let pc = HashMap::new();
        let current_unit_ids: HashSet<&str> = HashSet::new();
        let prev_border = Arc::new(vec![0u8; 100]);
        let make_border = |changes: usize| {
            let mut g = vec![0u8; 100];
            for (i, cell) in g.iter_mut().enumerate().take(changes) {
                *cell = (i + 1) as u8;
            }
            Arc::new(g)
        };

        // Exactly 5% (5/100) changed -> sparse form with the packed list.
        let at = compute_shared_deltas(
            &pu,
            &pc,
            &[],
            &[],
            &current_unit_ids,
            Some(&prev_border),
            &Some(make_border(5)),
            5.0,
            1000,
            count_units_by_owner(std::iter::empty::<&str>()),
            TEST_MAP,
        );
        assert!(at.border_grid_diff.is_none(), "5% must ship sparse");
        let cells = at.border_cells_changed.as_ref().unwrap();
        // Packed `(value << 7) | index` entries for values 1..=5 at indices 0..=4.
        let expected: Vec<u32> = (0..5u32).map(|i| ((i + 1) << 7) | i).collect();
        assert_eq!(cells.as_slice(), &expected);
        assert_eq!(at.border_changed_cells, 5);

        // 6/100 > 5% changed -> full-grid form (the shared published Arc).
        let over = compute_shared_deltas(
            &pu,
            &pc,
            &[],
            &[],
            &current_unit_ids,
            Some(&prev_border),
            &Some(make_border(6)),
            5.0,
            1000,
            count_units_by_owner(std::iter::empty::<&str>()),
            TEST_MAP,
        );
        assert!(over.border_cells_changed.is_none(), ">5% must ship full");
        assert!(over.border_grid_diff.is_some());
        assert_eq!(over.border_changed_cells, 6);

        // No change at all -> neither form ships.
        let none = compute_shared_deltas(
            &pu,
            &pc,
            &[],
            &[],
            &current_unit_ids,
            Some(&prev_border),
            &Some(Arc::new(vec![0u8; 100])),
            5.0,
            1000,
            count_units_by_owner(std::iter::empty::<&str>()),
            TEST_MAP,
        );
        assert!(none.border_grid_diff.is_none());
        assert!(none.border_cells_changed.is_none());
        assert_eq!(none.border_changed_cells, 0);
    }

    #[test]
    fn sparse_roundtrip_includes_clears_and_255() {
        // 20×20 grid (400 cells; max_sparse = 20, bits = ceil(log2(400)) = 9).
        // Vision must carry 1→0 flips (cells becoming invisible) and the
        // border must carry 255 (no owner) values — both are mandatory in the
        // packed (value << bits) | index form.
        let all_funds = funds("a", 0);
        let pu = HashMap::new();
        let pc = HashMap::new();
        let pf = all_funds.clone();
        let current_unit_ids: HashSet<&str> = HashSet::new();

        let prev_vision = vec![1u8; 400];
        let mut curr_vision = prev_vision.clone();
        curr_vision[7] = 0; // 1 → 0 flip — must be on the wire
        curr_vision[399] = 0;
        let prev_vision_arc = Arc::new(prev_vision.clone());
        let vision = Arc::new(curr_vision.clone());

        let prev_border = Arc::new(vec![0u8; 400]);
        let mut curr_border = vec![0u8; 400];
        curr_border[11] = 255; // ownerless cell — the 255 value is mandatory
        curr_border[22] = 2;
        let border = Arc::new(curr_border.clone());

        let shared = compute_shared_deltas(
            &pu,
            &pc,
            &[],
            &[],
            &current_unit_ids,
            Some(&prev_border),
            &Some(Arc::clone(&border)),
            5.0,
            1000,
            count_units_by_owner(std::iter::empty::<&str>()),
            TEST_MAP,
        );
        assert_eq!(
            shared.border_cells_changed.as_ref().map(|c| c.len()),
            Some(2)
        );

        let (msg, vision_changed) = build_delta_inner(
            &pf,
            Some(&prev_vision_arc),
            &all_funds,
            &Some(Arc::clone(&vision)),
            20,
            "a",
            &shared,
        )
        .unwrap();
        assert_eq!(vision_changed, 2);
        let d = match &msg {
            ServerMessage::DeltaState(d) => d,
            _ => unreachable!(),
        };
        assert!(d.vision_grid_diff.is_none());
        assert!(d.vision_cells_changed.is_some());
        assert!(d.border_grid_diff.is_none());
        assert!(d.border_cells_changed.is_some());
        assert_eq!(d.grid_width, Some(20));

        // Wire roundtrip: encode + decode, then apply the packed lists to the
        // PREV grids — the result must equal the CURRENT grids exactly.
        let encoded = crate::server::protocol::encode_message(&msg);
        let decoded: ServerMessage = ciborium::from_reader(encoded.as_slice()).unwrap();
        let d = match decoded {
            ServerMessage::DeltaState(d) => d,
            _ => unreachable!(),
        };
        assert!(d.vision_grid_diff.is_none());
        assert!(d.border_grid_diff.is_none());
        assert_eq!(
            apply_cells(&prev_vision, d.vision_cells_changed.as_ref().unwrap()),
            curr_vision
        );
        assert_eq!(
            apply_cells(&prev_border, d.border_cells_changed.as_ref().unwrap()),
            curr_border
        );
    }

    #[test]
    fn sparse_diff_1000x1000_perf() {
        // 1000×1000 grid, ~1% change: diff construction (O(cells) memcmp +
        // list build) must stay comfortably below 1 ms per grid per tick.
        // The < 1 ms bound only applies in release (debug builds are
        // unoptimized and take tens of ms for a plain 1 MB pass).
        let n = 1_000_000usize;
        let prev = vec![0u8; n];
        let mut curr = prev.clone();
        for i in (0..n).step_by(100) {
            curr[i] = 7;
        }
        let mut best = f64::INFINITY;
        for _ in 0..3 {
            let start = std::time::Instant::now();
            let (sparse, changed) = diff_grid(&prev, &curr);
            best = best.min(start.elapsed().as_secs_f64());
            assert!(sparse.is_some());
            assert_eq!(changed, 10_000);
        }
        let limit = if cfg!(debug_assertions) { 0.25 } else { 0.001 };
        assert!(
            best < limit,
            "1000×1000 sparse diff took {best:.6}s (>= {limit}s)"
        );

        // And the >5% path: 6.25% change -> full-grid form, exact count.
        let mut big = prev.clone();
        for i in (0..n).step_by(16) {
            big[i] = 3;
        }
        let (sparse, changed) = diff_grid(&prev, &big);
        assert!(sparse.is_none());
        assert_eq!(changed, 62_500);
    }

    // ── UnitBaselineStore touch/commit (dense (pass, gen) stamps) ───────────

    fn entity(id: u32, gen: u32) -> Entity {
        Entity::from_bits(((gen as u64) << 32) | id as u64).unwrap()
    }

    #[test]
    fn baseline_store_touch_commit_keeps_and_prunes() {
        let mut store = UnitBaselineStore::default();
        let e1 = entity(1, 1);
        let e2 = entity(2, 1);

        // Pass 1: both spawn.
        store.touch(e1);
        store.insert(e1, unit("u1", "p0", 0.0, 0.0));
        store.touch(e2);
        store.insert(e2, unit("u2", "p0", 1.0, 1.0));
        let mut removed = Vec::new();
        store.commit(&mut removed);
        assert!(removed.is_empty());
        assert_eq!(store.alive.len(), 2);

        // Pass 2: e2 dies (not touched), e1 survives. The game path hands
        // commit a FRESH vec each pass; so do we.
        store.touch(e1);
        let mut removed = Vec::new();
        store.commit(&mut removed);
        assert_eq!(removed, vec!["u2".to_string()]);
        assert_eq!(store.alive, vec![e1]);
        // The pruned slot no longer resolves (prev_owner_counts can't see it).
        assert!(store.resolve(e2).is_none());

        // Pass 3: e1 dies too.
        let mut removed = Vec::new();
        store.commit(&mut removed);
        assert_eq!(removed, vec!["u1".to_string()]);
        assert!(store.alive.is_empty());
    }

    #[test]
    fn baseline_store_same_tick_death_respawn_prunes_stale_incarnation() {
        let mut store = UnitBaselineStore::default();
        let old = entity(7, 1);
        let fresh = entity(7, 2); // id reuse after death, bumped generation

        // Pass 1: spawn the old incarnation.
        store.touch(old);
        store.insert(old, unit("u7", "p0", 0.0, 0.0));
        let mut removed = Vec::new();
        store.commit(&mut removed);
        assert_eq!(store.alive, vec![old]);

        // Pass 2: old dies and the id respawns in the SAME pass. The game
        // pass calls take_stale (emits the old id), then touch+insert.
        store.take_stale(fresh);
        store.touch(fresh);
        store.insert(fresh, unit("u7", "p0", 9.0, 9.0));
        let mut removed = Vec::new();
        store.commit(&mut removed);

        // Exactly one tombstone (from take_stale at the game-pass level;
        // commit must not double-emit), and the stale incarnation is gone
        // from `alive` — the pass-2 stamp for id 7 belongs to gen 2.
        assert_eq!(store.alive, vec![fresh]);
        assert!(store.resolve(old).is_none(), "stale gen must not resolve");
        assert!(store.resolve(fresh).is_some());
    }

    #[test]
    fn baseline_store_owner_counts_follow_pruning() {
        let mut store = UnitBaselineStore::default();
        let e1 = entity(3, 1);
        let e2 = entity(4, 1);
        store.touch(e1);
        store.insert(e1, unit("u3", "p0", 0.0, 0.0));
        store.touch(e2);
        store.insert(e2, unit("u4", "p1", 0.0, 0.0));
        let mut removed = Vec::new();
        store.commit(&mut removed);

        let counts = store.prev_owner_counts();
        assert_eq!(counts.get("p0"), Some(&1));
        assert_eq!(counts.get("p1"), Some(&1));

        // e1 dies → p0's count drops with the pruning.
        store.touch(e2);
        store.commit(&mut removed);
        let counts = store.prev_owner_counts();
        assert_eq!(counts.get("p0"), None);
        assert_eq!(counts.get("p1"), Some(&1));
    }

    // ---------------------------------------------------------------------
    // Replay recording hook: `run_network_phase` retains the recording
    // viewpoint's wire delta (and only when asked to).
    // ---------------------------------------------------------------------

    fn two_city_map() -> crate::map::map::Map {
        use crate::map::map::MapConfig;
        crate::map::map::Map::from_config(&MapConfig {
            width: 20,
            height: 20,
            cities: vec![(3.0, 3.0), (16.0, 3.0)],
            terrain_data: None,
        })
    }

    fn two_player_game() -> (Game, UnitType) {
        use crate::config::CityUpgradeSettings;
        use crate::game::game::{GameSettings, InfluenceSettings};
        use crate::game::unit::Equipment;

        let unit_type = UnitType {
            name: "infantry".to_string(),
            symbol: "I".to_string(),
            men: 100,
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
            max_supply: 200,
        };
        let settings = GameSettings {
            starting_funds: 1000,
            initial_manpower: 10_000,
            funds_per_city: 1.0,
            recruitment_rate: 1.0,
            game_speed: 1.0,
            use_standard_ruleset: true,
            unit_types: vec![unit_type.clone()],
            city_upgrade_settings: CityUpgradeSettings::default(),
            water_speed: 0.5,
            influence: InfluenceSettings::default(),
        };
        let mut game = Game::new(two_city_map(), settings, 2);
        game.start();
        // Mirror GameRoom::new: round-robin city ownership + starting funds,
        // so both players' funds change per tick (city income) and a delta
        // is always built.
        let cities: Vec<String> = game.query_cities().iter().map(|c| c.id.clone()).collect();
        for (i, city_id) in cities.iter().enumerate() {
            let pid = if i % 2 == 0 { "player_0" } else { "player_1" };
            game.assign_city_owner(city_id, pid);
        }
        (game, unit_type)
    }

    /// H5: seed the engine-neutral baselines (`crate::game::NetworkBaselines`)
    /// AND the server-owned unit store from the same initial snapshot.
    fn seeded_baselines(game: &Game) -> (NetworkBaselines, UnitBaselineStore) {
        let baselines = NetworkBaselines::from_snapshot(
            game.query_cities(),
            game.all_player_funds(),
            HashMap::new(),
            game.territory_grid_flat(),
        );
        let mut unit_store = UnitBaselineStore::default();
        init_unit_baselines(game, &mut unit_store);
        (baselines, unit_store)
    }

    #[test]
    fn run_network_phase_records_delta_for_record_pid_only() {
        let (mut game, unit_type) = two_player_game();
        let (mut baselines, mut unit_store) = seeded_baselines(&game);
        let pids = ["player_0", "player_1"];

        // Recruit a unit for the viewpoint AFTER seeding the baselines, so
        // the first tick produces a guaranteed non-idle delta (units_added).
        game.recruit_unit("city_0", &unit_type, "player_0");

        // Tick 1, recording player_0: the viewpoint's wire delta is retained
        // and its payload carries the player's live funds.
        game.tick_game();
        let unit_diff = update_unit_baselines(&game, &mut unit_store);
        let current_cities: Vec<City> = game.query_cities();
        let phase = run_network_phase(
            &game,
            &mut baselines,
            &pids,
            TickDiffs {
                units: unit_diff,
                cities: current_cities,
                border_grid: game.territory_grid_flat(),
            },
            false,
            Some("player_0"),
        );
        let recorded = phase
            .recorded_delta
            .expect("record_pid's delta must be retained");
        assert_eq!(
            recorded.player.funds,
            game.player_funds("player_0"),
            "viewpoint delta must carry the player's live funds (1000 - 200 recruit)"
        );

        // Tick 2, recording disabled: nothing retained.
        game.tick_game();
        let unit_diff = update_unit_baselines(&game, &mut unit_store);
        let current_cities: Vec<City> = game.query_cities();
        let phase = run_network_phase(
            &game,
            &mut baselines,
            &pids,
            TickDiffs {
                units: unit_diff,
                cities: current_cities,
                border_grid: game.territory_grid_flat(),
            },
            false,
            None,
        );
        assert!(phase.recorded_delta.is_none());

        // Tick 3, recording player_1: the other viewpoint's delta.
        game.tick_game();
        let unit_diff = update_unit_baselines(&game, &mut unit_store);
        let current_cities: Vec<City> = game.query_cities();
        let phase = run_network_phase(
            &game,
            &mut baselines,
            &pids,
            TickDiffs {
                units: unit_diff,
                cities: current_cities,
                border_grid: game.territory_grid_flat(),
            },
            false,
            Some("player_1"),
        );
        let recorded = phase
            .recorded_delta
            .expect("record_pid's delta must be retained");
        assert_eq!(
            recorded.player.funds,
            game.player_funds("player_1"),
            "viewpoint delta must carry the player's live funds"
        );
    }
}
