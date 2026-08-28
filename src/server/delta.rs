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

use crate::game::game::Game;
use crate::server::protocol::{
    encode_message_with_hint, estimate_delta_size, CityState, CityUpdate, GameDeltaMessage,
    PlayerState, ServerMessage, UnitState, UnitUpdate,
};

/// Quantization step for the `men` field in the unit digest.
///
/// The per-tick men drain is `1/600 ≈ 0.0017` (up to `10/600 ≈ 0.017` under
/// severe supply stress), far below 1.0. Quantizing to whole soldiers keeps a
/// healthy unit's digest stable across ~60–600 ticks, so sub-threshold
/// oscillation no longer emits a `UnitUpdate` every tick. When an update IS
/// emitted, the wire carries the exact `u.men`, not the quantized value, so no
/// visible information is lost.
const QUANT_MEN: f32 = 1.0;

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

/// Lightweight digest of a UnitState used for change detection.
/// Comparing two digests is O(1) with no heap allocation, whereas comparing
/// full UnitState structs requires cloning Vec<String>, Vec<EquipmentState>, etc.
///
/// If any field that affects gameplay rendering changes, the corresponding
/// digest field also changes, ensuring we never miss a real update.
#[derive(Debug, Clone, PartialEq)]
struct UnitStateDigest {
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
    fn from_unit(u: &UnitState) -> Self {
        Self {
            position: u.position,
            men: (u.men / QUANT_MEN).round() * QUANT_MEN,
            max_men: u.max_men,
            organisation: (u.organisation / QUANT_ORG).round() * QUANT_ORG,
            equipment_count: u.equipment.iter().map(|e| e.count as u64).sum(),
            max_equipment_count: u.max_equipment.iter().map(|e| e.count as u64).sum(),
            path_len: u.path.len(),
            healing: u.healing,
            resupply_enabled: u.resupply_enabled,
            supply_percent: (u.supply_percent / QUANT_SUPPLY).round() * QUANT_SUPPLY,
            encircled: u.encircled,
            attacking: u.attacking,
            on_water: u.on_water,
            boat_supply_percent: (u.boat_supply_percent / QUANT_SUPPLY).round() * QUANT_SUPPLY,
            embarking: u.embarking,
            disembarking: u.disembarking,
        }
    }
}

/// Player-independent delta components computed once per tick.
pub(crate) struct SharedDeltas {
    pub border_grid_diff: Option<Arc<Vec<u8>>>,
    pub units_added: Arc<Vec<UnitState>>,
    pub units_removed: Arc<Vec<String>>,
    pub unit_updates: Arc<Vec<UnitUpdate>>,
    pub city_updates: Arc<Vec<CityUpdate>>,
    /// W8: per-owner unit counts, built once per tick in single O(N) passes so
    /// `build_delta_inner` does O(1) lookups instead of O(units) scans per
    /// player. Owned (not borrowed) on purpose: `run_network_phase` reassigns
    /// `baselines.prev_units` after computing the shared deltas, so a map
    /// borrowing those keys couldn't live in the result; materializing costs
    /// one key clone per DISTINCT owner (≤ number of players), not per unit.
    pub prev_units_by_owner: HashMap<String, u32>,
    pub current_units_by_owner: HashMap<String, u32>,
}

/// W8: count units per owner in one pass. `owners` yields each unit's owner
/// (borrowed); the owned result clones one key per DISTINCT owner.
fn count_by_owner<'a>(owners: impl Iterator<Item = &'a str> + 'a) -> HashMap<String, u32> {
    let mut tmp: HashMap<&str, u32> = HashMap::new();
    for o in owners {
        *tmp.entry(o).or_insert(0) += 1;
    }
    tmp.into_iter().map(|(k, v)| (k.to_string(), v)).collect()
}

/// Compute the player-independent portions of the per-player delta.
/// Called once per tick, not once per player.
pub(crate) fn compute_shared_deltas(
    prev_units: &HashMap<String, UnitState>,
    prev_cities: &HashMap<String, CityState>,
    current_units: &[UnitState],
    current_cities: &[CityState],
    current_unit_ids: &HashSet<&str>,
    prev_border_grid: Option<&Arc<Vec<u8>>>,
    current_border_grid: &Option<Arc<Vec<u8>>>,
) -> SharedDeltas {
    // Border grid diff (shared across all players).
    // W3: share the published Arc instead of copying the 40 KB grid — the
    // influence double-buffer publishes by refcount and never mutates the
    // buffer in place during the encode window.
    let border_grid_diff: Option<Arc<Vec<u8>>> = match (prev_border_grid, current_border_grid) {
        (Some(prev), Some(curr)) => {
            if Arc::ptr_eq(prev, curr) || prev.as_slice() == curr.as_slice() {
                None
            } else {
                Some(Arc::clone(curr))
            }
        }
        (None, Some(curr)) => Some(Arc::clone(curr)),
        _ => None,
    };

    // W8: per-owner unit counts, single O(N) pass each (build_delta_inner
    // previously rescanned every unit per player for these counts).
    let prev_units_by_owner = count_by_owner(prev_units.values().map(|u| u.owner.as_str()));
    let current_units_by_owner = count_by_owner(current_units.iter().map(|u| u.owner.as_str()));

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
            let curr_digest = UnitStateDigest::from_unit(u);
            let prev_digest = UnitStateDigest::from_unit(prev);
            if curr_digest == prev_digest {
                continue;
            }

            let new_pos = if u.position != prev.position {
                Some(u.position)
            } else {
                None
            };

            let new_men = if u.men != prev.men { Some(u.men) } else { None };

            let new_org = if u.organisation != prev.organisation {
                Some(u.organisation)
            } else {
                None
            };

            let new_equipment = if u.equipment != prev.equipment {
                Some(u.equipment.clone())
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

            let new_supply = if u.supply_percent != prev.supply_percent {
                Some(u.supply_percent)
            } else {
                None
            };

            let new_encircled = if u.encircled != prev.encircled {
                Some(u.encircled)
            } else {
                None
            };

            let new_path = if u.path != prev.path {
                Some(u.path.clone())
            } else {
                None
            };

            unit_updates.push(UnitUpdate {
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
                boat_supply_percent: if u.boat_supply_percent != prev.boat_supply_percent {
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
            });
        }
    }

    // City updates
    let mut city_updates: Vec<CityUpdate> = Vec::new();
    for city in current_cities.iter() {
        if let Some(prev) = prev_cities.get(&city.id) {
            let owner_changed = city.owner != prev.owner;
            let level_changed = city.level != prev.level;

            if owner_changed || level_changed {
                city_updates.push(CityUpdate {
                    id: city.id.clone(),
                    owner: if owner_changed {
                        city.owner.clone()
                    } else {
                        None
                    },
                    level: if level_changed {
                        Some(city.level)
                    } else {
                        None
                    },
                    manpower: None,
                });
            }
        }
    }

    SharedDeltas {
        border_grid_diff,
        units_added,
        units_removed,
        unit_updates: Arc::new(unit_updates),
        city_updates: Arc::new(city_updates),
        prev_units_by_owner,
        current_units_by_owner,
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
) -> Option<ServerMessage> {
    // Per-player vision grid diff.
    // W3: share the published Arc instead of copying the 40 KB grid (see the
    // border diff above for the lifetime invariant).
    let vision_grid_diff: Option<Arc<Vec<u8>>> = match (prev_vision_grid, current_vision_grid) {
        (Some(prev), Some(curr)) => {
            if Arc::ptr_eq(prev, curr) || prev.as_slice() == curr.as_slice() {
                None
            } else {
                Some(Arc::clone(curr))
            }
        }
        (None, Some(curr)) => Some(Arc::clone(curr)),
        _ => None,
    };

    let grid_width_out = if vision_grid_diff.is_some() || shared.border_grid_diff.is_some() {
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
        && vision_grid_diff.is_none()
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
        border_grid_diff: shared.border_grid_diff.clone(),
        grid_width: grid_width_out,
        units_added: shared.units_added.clone(),
        units_removed: shared.units_removed.clone(),
        unit_updates: shared.unit_updates.clone(),
        city_updates: shared.city_updates.clone(),
        player: player_state,
    };

    Some(ServerMessage::DeltaState(delta))
}

/// Per-tick baseline state for the network phase (W0 fix: shared read-only
/// baseline with per-player vision).
/// N1: grids are stored as flat `Arc` slices for O(1) change detection.
#[derive(Default)]
pub(crate) struct NetworkBaselines {
    pub prev_units: HashMap<String, UnitState>,
    pub prev_cities: HashMap<String, CityState>,
    pub prev_funds: HashMap<String, u32>,
    pub prev_vision: HashMap<String, Arc<Vec<u8>>>,
    pub prev_border: Option<Arc<Vec<u8>>>,
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
}

/// Run the full per-tick network phase: snapshot, per-player delta build,
/// encode, and baseline commit. The caller is responsible for actually
/// sending the encoded bytes over the wire.
pub(crate) fn run_network_phase(
    game: &Game,
    baselines: &mut NetworkBaselines,
    player_ids: &[&str],
    current_units: Vec<UnitState>,
    current_cities: Vec<CityState>,
    spliced: bool,
) -> NetworkPhaseResult {
    // N2: borrowed ID set for removed-units check (no deep clones)
    let current_unit_ids: HashSet<&str> = current_units.iter().map(|u| u.id.as_str()).collect();
    let all_funds = game.all_player_funds();
    // N1: flat Arc grids (no 2-D allocation)
    let border_grid: Option<Arc<Vec<u8>>> = game.territory_grid_flat();
    let grid_width = game.terrain_width();

    // B0: sub-phase timing accumulators (feature-gated; zero cost in a
    // production build where the corresponding `Instant`s are compiled out).
    #[cfg(any(feature = "microbenchmarks", feature = "bench-alloc"))]
    let (mut delta_ms, mut encode_ms) = (0.0f64, 0.0f64);

    // W-2: compute player-independent deltas once per tick, not once per player.
    #[cfg(any(feature = "microbenchmarks", feature = "bench-alloc"))]
    let shared_start = std::time::Instant::now();
    let shared = compute_shared_deltas(
        &baselines.prev_units,
        &baselines.prev_cities,
        &current_units,
        &current_cities,
        &current_unit_ids,
        baselines.prev_border.as_ref(),
        &border_grid,
    );
    #[cfg(any(feature = "microbenchmarks", feature = "bench-alloc"))]
    {
        delta_ms += shared_start.elapsed().as_secs_f64() * 1000.0;
    }
    // B0: border diff is player-independent; per-player vision diffs are
    // detected in the loop below.
    #[cfg(any(feature = "microbenchmarks", feature = "bench-alloc"))]
    let mut grid_dirty = shared.border_grid_diff.is_some();

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
        // B0: sub-phase fields (feature-gated; see the struct definition).
        #[cfg(any(feature = "microbenchmarks", feature = "bench-alloc"))]
        delta_ms,
        #[cfg(any(feature = "microbenchmarks", feature = "bench-alloc"))]
        encode_ms,
        #[cfg(any(feature = "microbenchmarks", feature = "bench-alloc"))]
        grid_dirty,
    };

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

        if let Some(delta) = built {
            let delta_ref = match &delta {
                ServerMessage::DeltaState(d) => d,
                _ => unreachable!(),
            };
            #[cfg(any(feature = "microbenchmarks", feature = "bench-alloc"))]
            if delta_ref.vision_grid_diff.is_some() {
                grid_dirty = true;
            }
            // B0: encode sub-phase window (per-player serialize call only).
            #[cfg(any(feature = "microbenchmarks", feature = "bench-alloc"))]
            let encode_start = std::time::Instant::now();
            let encoded = if let Some(ss) = &splice_shared {
                // W9: splice the shared segments in (byte-for-byte identical to
                // the standard encoder, pinned by the byte-for-byte test).
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
                encode_message_with_hint(&delta, cap)
            };
            #[cfg(any(feature = "microbenchmarks", feature = "bench-alloc"))]
            {
                encode_ms += encode_start.elapsed().as_secs_f64() * 1000.0;
            }
            result.wire_bytes += encoded.len() as u64;
            result.deltas_sent += 1;
            result.encoded.push((pid.to_string(), encoded));
        }

        // N1: commit Arc refcount bump (no allocation)
        if let Some(v) = &vision_grid {
            baselines.prev_vision.insert(pid.to_string(), Arc::clone(v));
        }
    }

    // B0: the accumulators only reached `result` at construction (Copy types),
    // so copy the per-player loop totals back in here, after the loop.
    #[cfg(any(feature = "microbenchmarks", feature = "bench-alloc"))]
    {
        result.delta_ms = delta_ms;
        result.encode_ms = encode_ms;
        result.grid_dirty = grid_dirty;
    }

    // Commit shared baselines by move. The caller hands us the tick's snapshot
    // (W-1: built once upstream and shared with the active-player check), so
    // the baseline maps are built by moving each element in — only the key
    // String is cloned. This replaces the per-element deep clone of every
    // UnitState (4 Strings + 3 Vecs) / CityState that happened here every tick.
    baselines.prev_units = current_units
        .into_iter()
        .map(|u| (u.id.clone(), u))
        .collect();
    baselines.prev_cities = current_cities
        .into_iter()
        .map(|c| (c.id.clone(), c))
        .collect();
    baselines.prev_funds = all_funds;
    baselines.prev_border = border_grid;

    result
}

/// Build LLM agent state with filtered units based on vision grid.
pub fn build_llm_state(game: &Game, player_id: &str) -> crate::llm::agent::GameState {
    let vision_grid = game.vision_grid_2d(player_id).unwrap_or_default();
    let border_grid = game.border_grid_2d(player_id).unwrap_or_default();

    let units: Vec<crate::llm::agent::UnitState> = game
        .unit_states()
        .iter()
        .filter(|unit| {
            let (gx, gy) = (unit.position.0 as usize, unit.position.1 as usize);
            unit.owner == player_id
                || (gy < vision_grid.len() && gx < vision_grid[gy].len() && vision_grid[gy][gx])
        })
        .cloned()
        .collect::<Vec<_>>()
        .into_iter()
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
            supply_percent: u.supply_percent,
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
    use super::*;

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
        );
        build_delta_inner(prev_funds, None, all_funds, &None, 0, player_id, &shared)
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
        assert_eq!(d.unit_updates[0].position, Some((3.0, 2.0)));
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
        assert_eq!(d.unit_updates[0].position, Some((3.0, 2.0)));
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

        let shared = compute_shared_deltas(&pu, &pc, &[], &[], &current_unit_ids, None, &None);
        let msg = build_delta_inner(
            &pf,
            None,
            &all_funds,
            &Some(Arc::clone(&vision)),
            2,
            "a",
            &shared,
        );
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
        );
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

        let shared = compute_shared_deltas(&pu, &pc, &[], &[], &current_unit_ids, None, &None);

        // Player A: vision changed from None to Some(vision_a) -> diff present
        let msg_a = build_delta_inner(
            &pf,
            None,
            &all_funds,
            &Some(Arc::clone(&vision_a)),
            2,
            "a",
            &shared,
        );
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
    fn digest_quantized_men_equal_within_step() {
        // men differs by 0.3, which is less than QUANT_MEN (1.0)
        // so the quantized digests should be equal → no update emitted.
        let mut prev = unit("u1", "a", 1.0, 2.0);
        prev.men = 50.0;
        let mut curr = unit("u1", "a", 1.0, 2.0);
        curr.men = 50.3; // only 0.3 difference — within one quantization step

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
        );

        // Since the quantized digests are equal, no unit_updates should be produced
        assert!(shared.unit_updates.is_empty());
    }

    #[test]
    fn digest_quantized_men_differs_across_step() {
        // men differs by 1.0, which equals QUANT_MEN → digests differ → update emitted.
        let mut prev = unit("u1", "a", 1.0, 2.0);
        prev.men = 50.0;
        let mut curr = unit("u1", "a", 1.0, 2.0);
        curr.men = 51.0; // exactly 1.0 difference

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
        );

        // Digests differ → one unit update should be produced
        assert_eq!(shared.unit_updates.len(), 1);
        assert_eq!(shared.unit_updates[0].id, "u1");
        // The update carries the exact (unquantized) value
        assert_eq!(shared.unit_updates[0].men, Some(51.0));
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
        );

        assert_eq!(shared.unit_updates.len(), 1);
        assert_eq!(shared.unit_updates[0].id, "u1");
        assert_eq!(shared.unit_updates[0].boat_supply_percent, Some(55.9));
    }
}
