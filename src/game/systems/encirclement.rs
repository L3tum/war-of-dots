//! EncirclementSystem — reverse flood-fill from safe anchors with dominant player grid.
//!
//! Single-pass dominant player grid replaces max_influence + per-player friendly grid.
//! Then reverse flood-fill from safe anchors (edges + cities) through friendly cells.
//! Any friendly cell NOT reached = encircled.
//!
//! Runs every 30 ticks (strategic, not tactical).
//! Uses TickContext for pre-grouped players and units.

use crate::game::components::*;
use crate::game::game::TickContext;
use crate::game::systems::influence::NONE;
use crate::game::world_ext::WorldExt;
use hecs::World;
use std::collections::HashMap;
use std::sync::Arc;

/// Per-tick wall-clock budget for the flood-fill (see the module docs). Each
/// encirclement tick runs at most this much of the BFS, so a large flood is
/// spread across several ticks instead of one ~5–10 ms hitch. `FLOOD_STEP`
/// is how often the budget is re-checked (a `Instant::now()` is a ~25 ns van
/// der Waerden read, so it is amortised over 64 dequeues — an extra check per
/// cell would roughly double the flood's cost).
const FLOOD_TICK_BUDGET: std::time::Duration = std::time::Duration::from_millis(2);
const FLOOD_STEP_CHECK: u32 = 64;

/// Reused scratch buffers for the encirclement flood-fill. `safe` is the
/// per-cell claim grid (NONE = unvisited, else the owning player idx); `queue`
/// is the BFS frontier of packed coordinates (4 bytes/entry). Both grow only
/// on map resize and are reused each run (no per-run alloc).
///
/// The `fill_*` fields back the **skip + work-slicing** fast paths:
/// `safe` holds the flood for the *inputs* recorded in `fill_*` (the territory
/// grid `Arc`, the city anchors, and the player id list). The `safe` grid is a
/// pure function of those three, so:
///   • inputs unchanged and `fill_complete` → skip straight to the flag pass
///     (bit-identical `safe`, no seed/BFS); or
///   • inputs unchanged and a fill is mid-flight → resume the BFS from `qhead`
///     for up to `FLOOD_TICK_BUDGET`.
/// When inputs change while a fill is in progress we **finish** the old fill
/// (we own its snapshot) rather than abort — aborting would leave the flag
/// stale forever under continuous churn.
pub struct EncirclementScratch {
    pub safe: Vec<u8>,
    /// BFS frontier of packed coordinates `(cy << 16) | cx` (4 bytes/entry, and
    /// it removes the per-cell `/`+`%` the old `usize` index required on
    /// dequeue). Grows on demand and keeps capacity for reuse.
    pub queue: Vec<u32>,
    /// Dequeue cursor for the in-flight fill (persists across ticks; the
    /// frontier `Vec` itself is kept, only drained by advancing `qhead`).
    qhead: usize,
    /// Reused snapshot of the territory grid the `safe` grid is derived from.
    /// Sized once and memcopied (`copy_from_slice`) on each fill restart, so
    /// steady state is allocation-free. Owned (not borrowed) so the fill is
    /// immune to the influence double-buffer rotation between encirclement
    /// ticks. A 1 MB memcpy is ~50 µs, negligible for a 30-tick strategic
    /// system. Valid only when `fill_active` is true.
    fill_territory: Vec<u8>,
    /// Whether `fill_territory` / `fill_players` / `fill_cities` hold a valid
    /// snapshot for an in-flight or completed fill. Replaces the old
    /// `Option::is_none()` "no fill yet" check now that the buffer is reused.
    fill_active: bool,
    /// City anchor set (`cities_by_player`) the fill was seeded from.
    fill_cities: Vec<Vec<(i32, i32)>>,
    /// Player id list the fill's `safe` values are indexed against (sorted
    /// positions). Kept so the per-unit flag pass always pairs a `safe` grid
    /// with the *same* player ordering it was built with.
    fill_players: Vec<String>,
    /// Whether `safe` currently holds a **completed** flood for `fill_*`.
    fill_complete: bool,
}

impl EncirclementScratch {
    pub fn new() -> Self {
        Self {
            safe: Vec::new(),
            queue: Vec::new(),
            qhead: 0,
            fill_territory: Vec::new(),
            fill_active: false,
            fill_cities: Vec::new(),
            fill_players: Vec::new(),
            fill_complete: false,
        }
    }

    /// Grow `safe` to `grid_size` once (no-op when already sized). `queue`
    /// grows on demand and keeps its capacity for reuse. A resize invalidates
    /// any in-flight fill (the old `safe` is the wrong size) — `run` detects
    /// that via the territory-length mismatch and restarts.
    fn ensure(&mut self, grid_size: usize) {
        if self.safe.len() != grid_size {
            self.safe.resize(grid_size, NONE);
            self.fill_complete = false;
        }
    }

    /// True when the current inputs are identical to the snapshot the `safe`
    /// grid is (partially) built from: same territory grid (same `Arc`, or
    /// byte-identical contents), same city anchors, and same player id list.
    /// The `safe` grid is a pure function of these, so a match means the
    /// cached/in-flight `safe` is reusable bit-identically.
    fn inputs_unchanged(
        &self,
        territory: &Arc<Vec<u8>>,
        players: &[String],
        cities_by_player: &Vec<Vec<(i32, i32)>>,
    ) -> bool {
        self.fill_active
            && self.fill_territory[..] == territory[..]
            && self.fill_players == players
            && self.fill_cities == *cities_by_player
    }
}

impl Default for EncirclementScratch {
    fn default() -> Self {
        Self::new()
    }
}

pub fn run(world: &mut World, ctx: &TickContext, _tick: u64, scratch: &mut EncirclementScratch) {
    // Per-phase timing (no-op unless `--bench-detailed encirclement`):
    // p1_seed = safe-fill + edge/city seeding + queue seeding; p2_bfs = the flood
    // fill; p3_flags = per-unit encircled-flag loop. Surface in the stress report.
    let t0 = std::time::Instant::now();
    // Read the pre-computed territory grid (computed by influence::run): which
    // player dominates each cell (player index 0..N, or NONE=255 where none).
    let territory: Arc<Vec<u8>> = match world.world_data(ctx.world_entity) {
        Some(wb) => Arc::clone(&wb.territory.grid),
        None => return,
    };
    // wb dropped — borrow released
    let dominant: &[u8] = &territory[..];

    let (w, h) = (ctx.width as i32, ctx.height as i32);
    let wusize = w as usize;
    let grid_size = wusize * (h as usize);

    // Map player IDs to indices 0..N by enumeration (still needed for the
    // flag-setting loop). Sort for deterministic index assignment + tie-breaking
    // (the underlying units_by_player HashMap has process-random iteration order).
    // The index is the sorted position — must stay in lockstep with
    // `influence::run`, which builds the territory grid the same way.
    let mut players: Vec<&String> = ctx.units_by_player.keys().collect();
    players.sort();
    let nplayers = players.len();

    // Collect all cities by player index for seeding. nplayers is tiny, so a
    // linear `position` lookup beats building a HashMap for it.
    let mut cities_by_player: Vec<Vec<(i32, i32)>> = vec![Vec::new(); nplayers];
    for c in &ctx.cities {
        if let Some(owner) = &c.owner {
            if let Some(idx) = players.iter().position(|p| p.as_str() == owner.as_str()) {
                cities_by_player[idx].push((c.position.0 as i32, c.position.1 as i32));
            }
        }
    }

    // Combined multi-player flood-fill: one BFS pass for ALL players
    // simultaneously. safe[i] == NONE → unvisited, safe[i] == player_idx →
    // claimed by that player.
    //
    // Two fast paths around the ~5 ms flood (see plans/W2_ENCIRCLEMENT.md and
    // plans/ENCIRCLEMENT_PLAN.md):
    //   1. SKIP  — inputs (territory + city anchors + player list) unchanged
    //              from the last *completed* fill → reuse the cached `safe`
    //              grid and only re-run the per-unit flag pass (bit-identical).
    //   2. SLICE — a large fill is spread across several encirclement ticks,
    //              each doing at most FLOOD_TICK_BUDGET of BFS, so it never
    //              shows up as a single 5–10 ms hitch.
    // The `safe` grid is a pure function of (dominant, city anchors, player
    // ordering) and BFS visit order does not affect the result, so both are
    // correctness-preserving. If inputs change while a fill is in progress we
    // finish the old fill (we own its snapshot) rather than abort — aborting
    // would strand the flag stale forever under continuous churn.
    scratch.ensure(grid_size);
    // Snapshot the current inputs (owned, so they can be stored on `scratch`
    // as the in-flight fill's source of truth and compared for the skip).
    let current_players: Vec<String> = players.iter().map(|s| (*s).clone()).collect();
    // `&self` borrow of the whole scratch — must end before `let safe = &mut
    // scratch.safe` below.
    let changed = !scratch.inputs_unchanged(&territory, &current_players, &cities_by_player);
    let safe = &mut scratch.safe;

    // (Re)start a fill when there is no in-flight fill, or the last fill is
    // complete and the inputs have since changed. An **in-progress** fill is
    // never aborted here even if inputs changed — it is finished (on its own
    // snapshot) over subsequent ticks. Aborting on churn would restart a fresh
    // 2 ms chunk every tick and never complete, leaving the flag stale forever;
    // finishing the old fill keeps the flag bounded-stale (one fill cycle) and
    // always makes progress.
    let need_start = !scratch.fill_active || (scratch.fill_complete && changed);
    if need_start {
        safe.fill(NONE);
        // Mark all edge cells as safe for ALL players (edges are universally safe)
        mark_safe_edges_multi(safe, wusize, h as usize, dominant);
        // Mark cells near cities as safe (each city's radius marks cells for
        // that player)
        for (pidx, cities) in cities_by_player.iter().take(nplayers).enumerate() {
            mark_safe_near_cities_multi(safe, cities, wusize, h as usize, pidx as u8);
        }
        // Seed queue with all safe anchor cells that have a dominant player.
        // The nested loop avoids the per-cell `/`+`%` an index-based seed
        // would have needed to pack coords.
        let queue_buf = &mut scratch.queue;
        queue_buf.clear();
        for cy in 0..h {
            for cx in 0..w {
                let idx = (cy as usize) * wusize + (cx as usize);
                if safe[idx] != NONE && dominant[idx] == safe[idx] {
                    queue_buf.push(pack_coord(cx, cy));
                }
            }
        }
        scratch.qhead = 0;
        // Reused snapshot: sized once, then a plain memcpy (no per-restart
        // allocation). Copying (not borrowing) the territory keeps the fill
        // immune to the influence double-buffer rotation between ticks.
        if scratch.fill_territory.len() != territory.len() {
            scratch.fill_territory.resize(territory.len(), NONE);
        }
        scratch.fill_territory.copy_from_slice(territory.as_slice());
        scratch.fill_players = current_players;
        scratch.fill_cities = cities_by_player;
        scratch.fill_active = true;
        scratch.fill_complete = false;
    }

    // `fill_*` is the snapshot the in-flight (or just-completed) `safe` grid
    // reflects. It may be the *current* inputs (just (re)started) or a slightly
    // older set if we are finishing an in-progress fill that predates the last
    // input change — that bounded staleness is the accepted tradeoff (see the
    // plan). The flag pass below always pairs `safe` with this same snapshot.
    let fill_terr = &scratch.fill_territory[..];
    let t1 = std::time::Instant::now();
    // Advance the BFS by at most FLOOD_TICK_BUDGET this tick (skipped entirely
    // when the fill is already complete and unchanged — the SKIP fast path).
    // Packed coords: dequeue is a shift/and (no division), neighbors are pure
    // index math (±1 / ±w). The `safe` check short-circuits the `dominant` load
    // for already-claimed (interior) neighbors — do NOT merge the two arrays
    // into one wider type (measured 10 % slower, see plans/W2). `safe` is
    // order-independent, so chunking the BFS across ticks yields a
    // bit-identical result.
    let mut complete = scratch.fill_complete;
    if !complete {
        let mut qhead = scratch.qhead;
        complete = flood_step(
            safe,
            fill_terr,
            w,
            h,
            &mut scratch.queue,
            &mut qhead,
            Some(FLOOD_TICK_BUDGET),
        );
        scratch.qhead = qhead;
    }
    scratch.fill_complete = complete;
    let t2 = std::time::Instant::now();

    // Per-unit encirclement flags: dominated but not safe. Updated only when
    // the fill is complete — a partial `safe` would over-report encirclement,
    // so on mid-flight ticks the flag simply keeps its previous value.
    //
    // E-3: one query sweep instead of a per-unit `query_one_mut` ECS lookup.
    // The player index must come from the fill's sorted `fill_players` — the
    // same index space the territory/safe grids use. `ctx.owner_index` is NOT
    // usable here: its index space is first-seen order from `build_initial`,
    // not sorted order, so it would mis-assign players.
    //
    // Membership decision (full sweep, not units_by_player): the sweep covers
    // every entity with (UnitPosition, UnitFlags, UnitOwner) — a superset of
    // `units_by_player` (identical component set; that map is rebuilt on
    // consumer ticks before this system runs anyway). It also reads *live*
    // positions instead of the cached UnitHandle snapshot, which is strictly
    // more correct if a unit moved after the rebuild. Per-unit flag results
    // are independent, so the final world state is bit-identical to the old
    // per-player loop for identical inputs.
    if scratch.fill_complete {
        let player_idx: HashMap<&str, u8> = scratch
            .fill_players
            .iter()
            .enumerate()
            .map(|(i, p)| (p.as_str(), i as u8))
            .collect();
        for (pos, flags, owner) in world
            .query::<(&UnitPosition, &mut UnitFlags, &UnitOwner)>()
            .iter()
        {
            // Player index via owner id → sorted position (see E-3 note above).
            let Some(&pidx) = player_idx.get(owner.player_id.as_str()) else {
                continue;
            };
            let xi = pos.x as i32;
            let yi = pos.y as i32;
            let idx = (yi as usize) * wusize + (xi as usize);
            let encircled = idx < grid_size && fill_terr[idx] == pidx && safe[idx] != pidx;
            // Skip the write when the flag already matches — steady state
            // then does zero ECS mutations in this pass.
            if flags.encircled != encircled {
                flags.encircled = encircled;
            }
        }
    }
    let t3 = std::time::Instant::now();

    let to_ms = |d: std::time::Duration| d.as_secs_f64() * 1000.0;
    world.record_detailed(ctx.world_entity, "encirclement/p1_seed", to_ms(t1 - t0));
    world.record_detailed(ctx.world_entity, "encirclement/p2_bfs", to_ms(t2 - t1));
    world.record_detailed(ctx.world_entity, "encirclement/p3_flags", to_ms(t3 - t2));
}

/// Mark all edge cells as safe for ALL players (edges are universally safe).
/// safe[i] is set to dominant[i] if the edge cell has a dominant player.
pub fn mark_safe_edges_multi(safe: &mut [u8], w: usize, h: usize, dominant: &[u8]) {
    // Top and bottom rows
    for x in 0..w {
        let top = x;
        let bot = (h - 1) * w + x;
        if dominant[top] != NONE {
            safe[top] = dominant[top];
        }
        if dominant[bot] != NONE {
            safe[bot] = dominant[bot];
        }
    }
    // Left and right columns
    for y in 0..h {
        let left = y * w;
        let right = y * w + w - 1;
        if dominant[left] != NONE {
            safe[left] = dominant[left];
        }
        if dominant[right] != NONE {
            safe[right] = dominant[right];
        }
    }
}

/// Mark cells near friendly cities as safe for a specific player.
fn mark_safe_near_cities_multi(
    safe: &mut [u8],
    cities: &[(i32, i32)],
    w: usize,
    h: usize,
    player_idx: u8,
) {
    let city_radius: i32 = 2;
    for &(cx, cy) in cities {
        for dy in -city_radius..=city_radius {
            for dx in -city_radius..=city_radius {
                let nx = cx + dx;
                let ny = cy + dy;
                if nx >= 0 && ny >= 0 && (nx as usize) < w && (ny as usize) < h {
                    let idx = (ny as usize) * w + (nx as usize);
                    // Only mark if not already claimed by another player
                    if safe[idx] == NONE {
                        safe[idx] = player_idx;
                    }
                }
            }
        }
    }
}

// Pack/unpack a `(cx, cy)` map coordinate into the 32-bit packed-coord form
// `(cy << 16) | cx` used by the BFS queue. Map dims must be < 65536 (the
// 1000×1000 stress map is far under this).
#[inline]
fn pack_coord(cx: i32, cy: i32) -> u32 {
    ((cy as u32) << 16) | ((cx as u32) & 0xFFFF)
}

/// Run one bounded step of the combined multi-player flood fill and return
/// whether the frontier is now drained (the fill is complete).
///
/// `safe` is the claim grid (NONE = unvisited, else the owning player idx),
/// `dominant` the territory grid, `queue` the packed-coord frontier, `qhead`
/// the dequeue cursor (advanced in place and persisted across calls), and
/// `budget` an optional wall-clock cap for this step (`None` = run to
/// completion). Each dequeue expands up to four neighbors; `qhead` only ever
/// moves forward, so a step is safe to resume on a later tick.
///
/// **Order-independent:** a cell is only ever written to its own `dominant`
/// value (we only claim `nidx` when `dominant[nidx] == pidx`, where `pidx` is
/// the source cell's owner, which equals `dominant[source]`). So the final
/// `safe` grid — and hence the per-unit encircled flags — is bit-identical
/// regardless of visit order, and therefore regardless of how the work is
/// chunked across steps.
///
/// The budget is re-checked every `FLOOD_STEP_CHECK` dequeues rather than per
/// cell: an `Instant::now()` read (~25 ns) per cell would roughly double the
/// flood's cost, so the check is amortised (worst-case overshoot is a few
/// microseconds).
pub fn flood_step(
    safe: &mut [u8],
    dominant: &[u8],
    w: i32,
    h: i32,
    queue: &mut Vec<u32>,
    qhead: &mut usize,
    budget: Option<std::time::Duration>,
) -> bool {
    debug_assert!(
        w < 65536 && h < 65536,
        "packed coords need map dims < 65536"
    );
    let wusize = w as usize;
    let budget_start = budget.map(|_| std::time::Instant::now());
    let mut steps = 0u32;
    while *qhead < queue.len() {
        // Amortised budget check (see doc): only costs a branch when there is no
        // budget; the clock is read every FLOOD_STEP_CHECK dequeues.
        if steps >= FLOOD_STEP_CHECK {
            steps = 0;
            if let (Some(start), Some(b)) = (budget_start, budget) {
                if start.elapsed() >= b {
                    break; // budget exhausted; resume next tick
                }
            }
        }
        steps += 1;
        let v = queue[*qhead];
        *qhead += 1;
        let cx = (v & 0xFFFF) as i32;
        let cy = (v >> 16) as i32;
        let idx = (cy as usize) * wusize + (cx as usize);
        let pidx = safe[idx];
        // Neighbor order (right, left, down, up) matches the pre-refactor loop.
        // The `safe` check short-circuits the `dominant` load for
        // already-claimed (interior) neighbors — do NOT merge the two arrays
        // into one wider type (measured 10 % slower, see plans/W2).
        if cx + 1 < w {
            let nidx = idx + 1;
            if safe[nidx] == NONE && dominant[nidx] == pidx {
                safe[nidx] = pidx;
                queue.push(pack_coord(cx + 1, cy));
            }
        }
        if cx > 0 {
            let nidx = idx - 1;
            if safe[nidx] == NONE && dominant[nidx] == pidx {
                safe[nidx] = pidx;
                queue.push(pack_coord(cx - 1, cy));
            }
        }
        if cy + 1 < h {
            let nidx = idx + wusize;
            if safe[nidx] == NONE && dominant[nidx] == pidx {
                safe[nidx] = pidx;
                queue.push(pack_coord(cx, cy + 1));
            }
        }
        if cy > 0 {
            let nidx = idx - wusize;
            if safe[nidx] == NONE && dominant[nidx] == pidx {
                safe[nidx] = pidx;
                queue.push(pack_coord(cx, cy - 1));
            }
        }
    }
    *qhead >= queue.len()
}

/// Serial combined multi-player flood fill (the full, un-bounded path used by
/// tests and the microbenchmark). Wraps `flood_step` with `budget: None` from a
/// fresh cursor, then clears the queue. The production path is `flood_step`
/// (budgeted + resumable); this thin wrapper is only referenced by tests and
/// the `encirclement/flood_serial_1000` microbenchmark.
#[cfg(any(test, feature = "microbenchmarks"))]
#[allow(dead_code)]
pub fn flood_serial(safe: &mut [u8], dominant: &[u8], w: i32, h: i32, queue: &mut Vec<u32>) {
    let mut qhead = 0;
    flood_step(safe, dominant, w, h, queue, &mut qhead, None);
    queue.clear();
}

/// Build a deterministic, contended 4-player `dominant` grid for the flood A/B
/// benchmark and the serial/parallel equivalence test: four solid quadrants
/// (players 0..3) with a central NONE (uncontested) square, so each player's
/// flood covers a large contiguous region and the NONE sentinel is exercised.
#[cfg(any(test, feature = "microbenchmarks"))]
#[allow(dead_code)]
pub fn build_bench_dominant(w: i32, h: i32, dominant: &mut [u8]) {
    let wu = w as usize;
    let hu = h as usize;
    let half_x = wu / 2;
    let half_y = hu / 2;
    let band = (wu / 20).max(1);
    for y in 0..hu {
        for x in 0..wu {
            let in_center = x >= half_x.saturating_sub(band)
                && x < half_x + band
                && y >= half_y.saturating_sub(band)
                && y < half_y + band;
            dominant[y * wu + x] = if in_center {
                NONE
            } else if x < half_x && y < half_y {
                0
            } else if x >= half_x && y < half_y {
                1
            } else if x < half_x {
                2
            } else {
                3
            };
        }
    }
}

/// Seed a packed-coord BFS frontier with every safe anchor cell that matches its
/// dominant player. For tests/benches (allocates); `run` uses an inline variant
/// that writes into the reused scratch queue.
#[cfg(any(test, feature = "microbenchmarks"))]
#[allow(dead_code)]
pub fn seed_queue(safe: &[u8], dominant: &[u8], w: i32, h: i32) -> Vec<u32> {
    let mut q = Vec::new();
    let wu = w as usize;
    for cy in 0..h {
        for cx in 0..w {
            let idx = (cy as usize) * wu + (cx as usize);
            if safe[idx] != NONE && dominant[idx] == safe[idx] {
                q.push(pack_coord(cx, cy));
            }
        }
    }
    q
}

// The dominant-player (now territory) grid logic lives in influence.rs and is
// unit-tested there as `build_territory_grid_*`.

#[cfg(test)]
mod tests {
    use super::*;
    use crate::game::unit::Terrain;
    use std::collections::HashMap;

    /// E-3: the single-sweep flag pass, end to end on a real world.
    /// Player 0 dominates a 10×10 map except a ring of player-1 cells around
    /// (5,5), which walls off a one-cell pocket:
    ///   • unit A (player_0) inside the pocket → encircled (flood can't reach it);
    ///   • unit B (player_0) near the edge → safe → not encircled;
    ///   • unit C (player_1) on the ring → no safe cells for player 1 → encircled.
    /// A second run must leave the flags bit-identical (idempotent sweep).
    #[test]
    fn flag_sweep_sets_encircled_correctly() {
        let (w, h) = (10, 10);
        let wu = w as usize;
        let n = wu * h as usize;
        let mut dominant = vec![0u8; n];
        let deltas: [(i32, i32); 8] = [
            (-1, 0),
            (1, 0),
            (0, -1),
            (0, 1),
            (-1, -1),
            (-1, 1),
            (1, -1),
            (1, 1),
        ];
        for (dx, dy) in deltas {
            let cx = 5 + dx;
            let cy = 5 + dy;
            dominant[(cy as usize) * wu + (cx as usize)] = 1;
        }

        let mut world = World::new();
        let terrain = vec![vec![Terrain::Plains; wu]; h as usize];
        let mut wb = WorldBundle::new(
            terrain,
            w as u32,
            h as u32,
            crate::config::CityUpgradeSettings::default(),
            HashMap::new(),
        );
        wb.territory.grid = Arc::new(dominant);
        let world_entity = world.spawn((wb,));

        let a = world.spawn((
            UnitOwner {
                player_id: "player_0".to_string(),
            },
            UnitPosition { x: 5.0, y: 5.0 },
            UnitFlags::default(),
        ));
        let b = world.spawn((
            UnitOwner {
                player_id: "player_0".to_string(),
            },
            UnitPosition { x: 2.0, y: 2.0 },
            UnitFlags::default(),
        ));
        let c = world.spawn((
            UnitOwner {
                player_id: "player_1".to_string(),
            },
            UnitPosition { x: 6.0, y: 5.0 },
            UnitFlags::default(),
        ));

        let mut ctx = TickContext::build_initial(&world, world_entity);
        ctx.rebuild_units_by_player(&world);
        let mut scratch = EncirclementScratch::new();
        run(&mut world, &ctx, 0, &mut scratch);
        assert!(
            scratch.fill_complete,
            "a 10×10 flood must complete in one tick"
        );

        // Second run: inputs unchanged → SKIP fast path, flags bit-identical.
        run(&mut world, &ctx, 30, &mut scratch);

        let flag = |e: hecs::Entity| *world.get::<&UnitFlags>(e).unwrap();
        assert!(
            flag(a).encircled,
            "unit inside the pocket must be encircled"
        );
        assert!(!flag(b).encircled, "unit near the edge must be safe");
        assert!(
            flag(c).encircled,
            "player-1 unit with no safe cells must be encircled"
        );
    }

    /// Timing probe: 1000×1000 contested quadrants (same fixture as the
    /// `encirclement/flood_serial_1000` microbenchmark), min/median over
    /// repeated runs. Splits p1 (reset + seed) from p2 (the BFS) so the
    /// flood micro-optimizations have a local before/after signal.
    #[test]
    fn flood_serial_timing_probe() {
        let (w, h) = (1000i32, 1000);
        let n = (w * h) as usize;
        let mut dominant = vec![0u8; n];
        build_bench_dominant(w, h, &mut dominant);
        let mut safe = vec![NONE; n];
        mark_safe_edges_multi(&mut safe, w as usize, h as usize, &dominant);
        let safe_template = safe.clone(); // post-edge-mark state == run's reset state

        // Warmup (page faults, allocator growth)
        for _ in 0..2 {
            let mut s = safe_template.clone();
            let mut q = seed_queue(&s, &dominant, w, h);
            flood_serial(&mut s, &dominant, w, h, &mut q);
        }
        let iters = 5u32;
        let mut setup_ms = Vec::new();
        let mut flood_ms = Vec::new();
        for _ in 0..iters {
            let t0 = std::time::Instant::now();
            let mut s = safe_template.clone();
            let mut q = seed_queue(&s, &dominant, w, h);
            let t1 = std::time::Instant::now();
            flood_serial(&mut s, &dominant, w, h, &mut q);
            let t2 = std::time::Instant::now();
            std::hint::black_box(&s);
            setup_ms.push(t1.duration_since(t0).as_secs_f64() * 1e3);
            flood_ms.push(t2.duration_since(t1).as_secs_f64() * 1e3);
        }
        setup_ms.sort_by(|a, b| a.partial_cmp(b).unwrap());
        flood_ms.sort_by(|a, b| a.partial_cmp(b).unwrap());
        let med = |v: &Vec<f64>| v[v.len() / 2];
        eprintln!(
            "flood_timing_probe: setup(min {:?} med {:?})  flood(min {:?} med {:?})  ms",
            setup_ms[0],
            med(&setup_ms),
            flood_ms[0],
            med(&flood_ms)
        );
    }

    #[test]
    fn flood_serial_degenerate_cases() {
        let (w, h) = (16i32, 16);
        let wu = w as usize;
        let n = wu * h as usize;

        // All-NONE dominant: no anchors, no flood.
        let dominant = vec![NONE; n];
        let mut safe = vec![NONE; n];
        mark_safe_edges_multi(&mut safe, wu, h as usize, &dominant);
        let mut q = seed_queue(&safe, &dominant, w, h);
        assert!(q.is_empty(), "no dominant → no anchors");
        flood_serial(&mut safe, &dominant, w, h, &mut q);
        assert!(safe.iter().all(|&c| c == NONE), "nothing should be safe");

        // All-one-player interior: edges seed, whole map floods safe.
        let dominant2 = vec![0u8; n];
        let mut safe2 = vec![NONE; n];
        mark_safe_edges_multi(&mut safe2, wu, h as usize, &dominant2);
        let mut q2 = seed_queue(&safe2, &dominant2, w, h);
        flood_serial(&mut safe2, &dominant2, w, h, &mut q2);
        assert!(safe2.iter().all(|&c| c == 0), "one player → whole map safe");

        // City anchor on enemy territory: marked safe but not seeded (dominant
        // mismatch) → safe cell but does not flood. Mirrors run()'s seed rule.
        let mut dominant3 = vec![1u8; n];
        dominant3[wu / 2 + wu / 2] = 0; // a single player-0 cell
        let mut safe3 = vec![NONE; n];
        safe3[wu / 2 + wu / 2] = 1; // mark it safe as player 1 (city-style)
        let mut q3 = seed_queue(&safe3, &dominant3, w, h);
        // The marked cell: safe=1, dominant=0 → NOT seeded.
        flood_serial(&mut safe3, &dominant3, w, h, &mut q3);
        assert_eq!(safe3[wu / 2 + wu / 2], 1, "city anchor stays safe");
    }

    /// Chunking the BFS across many small time-budgeted `flood_step` calls must
    /// produce a bit-identical `safe` grid to one un-bounded `flood_serial`
    /// run, and the tiny budget must actually chunk the work (steps > 1).
    /// This is the correctness guarantee behind the work-slicing fast path.
    #[test]
    fn flood_step_chunked_matches_serial() {
        use std::time::Duration;
        let (w, h) = (500i32, 500);
        let n = (w * h) as usize;
        let mut dominant = vec![0u8; n];
        build_bench_dominant(w, h, &mut dominant);

        // Seeded (pre-flood) state, shared by both runs.
        let mut seeded = vec![NONE; n];
        mark_safe_edges_multi(&mut seeded, w as usize, h as usize, &dominant);

        // Reference: one un-bounded serial flood.
        let mut safe_ref = seeded.clone();
        let mut q_ref = seed_queue(&safe_ref, &dominant, w, h);
        flood_serial(&mut safe_ref, &dominant, w, h, &mut q_ref);
        assert!(q_ref.is_empty(), "flood_serial clears its queue");

        // Chunked: same seed, many tiny-budget steps until the frontier drains.
        let mut safe_chunk = seeded.clone();
        let mut q_chunk = seed_queue(&safe_chunk, &dominant, w, h);
        let mut qhead = 0usize;
        let mut steps = 0u32;
        loop {
            steps += 1;
            let done = flood_step(
                &mut safe_chunk,
                &dominant,
                w,
                h,
                &mut q_chunk,
                &mut qhead,
                Some(Duration::from_micros(200)),
            );
            if done {
                break;
            }
            assert!(steps < 10_000, "chunked flood must terminate");
        }
        assert!(steps > 1, "a 200 µs budget must actually chunk the work");
        assert_eq!(
            safe_ref, safe_chunk,
            "chunked flood must be bit-identical to serial"
        );
    }

    /// The skip decision (`inputs_unchanged`): no fill yet → false; same `Arc` →
    /// true; byte-identical different `Arc` → true; different territory /
    /// players / cities → false. This is the Option-B skip gate.
    #[test]
    fn inputs_unchanged_skip_logic() {
        let mut s = EncirclementScratch::new();
        let terr: Arc<Vec<u8>> = Arc::new(vec![0u8; 100]);

        // No fill in flight yet → nothing to skip.
        assert!(!s.inputs_unchanged(&terr, &[], &vec![]));

        // Populate the fill snapshot, then exercise the gate.
        s.fill_territory = terr.as_slice().to_vec();
        s.fill_active = true;
        s.fill_players = vec!["player_0".to_string()];
        s.fill_cities = vec![vec![(3, 4)]];
        s.fill_complete = true;
        let players: Vec<String> = vec!["player_0".to_string()];
        let cities: Vec<Vec<(i32, i32)>> = vec![vec![(3, 4)]];

        assert!(
            s.inputs_unchanged(&terr, &players, &cities),
            "same Arc → skip"
        );
        // Byte-identical contents in a different Arc → still skip.
        let terr_copy: Arc<Vec<u8>> = Arc::new(vec![0u8; 100]);
        assert!(
            s.inputs_unchanged(&terr_copy, &players, &cities),
            "equal bytes → skip"
        );
        // Different territory bytes → recompute.
        let terr_diff: Arc<Vec<u8>> = Arc::new(vec![1u8; 100]);
        assert!(
            !s.inputs_unchanged(&terr_diff, &players, &cities),
            "diff bytes → no skip"
        );
        // Different player list (indexing would shift) → recompute.
        let players2: Vec<String> = vec!["player_1".to_string()];
        assert!(
            !s.inputs_unchanged(&terr, &players2, &cities),
            "diff players → no skip"
        );
        // Different city anchors → recompute.
        let cities2: Vec<Vec<(i32, i32)>> = vec![vec![(9, 9)]];
        assert!(
            !s.inputs_unchanged(&terr, &players, &cities2),
            "diff cities → no skip"
        );
    }
}
