//! InfluenceSystem — merged vision + border influence computation.
//!
//! Uses a single Euclidean circle per source (unit or city), precomputed as
//! circle spans. One span pass per source writes both binary visibility and
//! weighted influence into the same cells — vision and influence always
//! share a radius. Hill units use a larger circle (radius × the hill
//! multiplier), which extends both vision and influence together.
//!
//! After the per-player grids, influence is compared across players to build
//! the territory grid (dominant player per cell, or NONE).
//!
//! Sequential by design: Rayon was tried at multiple granularities and
//! rejected — per-player work (~0.1 ms) is below dispatch breakeven.
//!
//! `run()` takes a `dirty` flag: when the game state has not changed since
//! the last run, the stored grids are still correct and the whole compute
//! (including all per-player grid allocations) is skipped.

use crate::game::economy::City;
use crate::game::game::{InfluenceSettings, TickContext, UnitHandle};
use crate::game::unit::Terrain;
use crate::game::world_ext::WorldExt;
use hecs::World;
use std::collections::HashMap;
use std::sync::{Arc, OnceLock};

/// Max radius we need spans for (hill unit = 5 × 2.0 = 10 with default settings).
/// Radii are configurable via `InfluenceSettings`; the span lookup clamps with
/// `.min(MAX_RADIUS)` so a playtest that pushes a radius past this degrades to
/// the largest precomputed span instead of panicking.
const MAX_RADIUS: i32 = 10;

/// Precomputed circle spans: for each radius r, array of (dy, dx_start, dx_end).
/// Each row tells you which columns to touch at that row offset from center.
/// Eliminates `sqrt()` calls in the inner loop.
/// Stored as flat array of i32: [dy0, dx_start0, dx_end0, dy1, dx_start1, dx_end1, ...]
/// Indexed as CIRCLE_SPANS[r][row*3..row*3+3].
static CIRCLE_SPANS: OnceLock<Vec<Vec<(i32, i32, i32)>>> = OnceLock::new();

fn circle_spans() -> &'static Vec<Vec<(i32, i32, i32)>> {
    CIRCLE_SPANS.get_or_init(|| {
        (0..=MAX_RADIUS)
            .map(|r| {
                let rr = r;
                (-rr..=rr)
                    .filter_map(|dy| {
                        let discriminant = rr * rr - dy * dy;
                        if discriminant < 0 {
                            return None;
                        }
                        let dx_max = (discriminant as f64).sqrt() as i32;
                        Some((dy, -dx_max, dx_max))
                    })
                    .collect()
            })
            .collect()
    })
}

/// Pre-allocated scratch for influence, owned by `Game` (SupplyScratch pattern).
/// Buffers are sized once (see `ensure`) and reused across dirty ticks; the
/// per-tick reset is a cheap `fill()` memset instead of a fresh allocation.
/// - `city_positions`: per-player filtered city list (typically ≤ 20 entries).
/// - `player_influence`: one reused per-player influence buffer (folded straight
///   into the running territory, never stored per player).
/// - `territory` / `territory_max`: running dominant-player + max-influence
///   accumulators for the per-player territory fold.
pub struct InfluenceScratch {
    pub city_positions: Vec<(f32, f32)>,
    pub player_influence: Vec<u16>,
    pub territory: Vec<u8>,
    pub territory_max: Vec<u16>,
    /// Reused per-player vision stamp buffer; `fill(false)`'d before each
    /// player and cloned into the published Arc (no fresh alloc + zero-init).
    pub vision: Vec<bool>,
}
impl InfluenceScratch {
    pub fn new() -> Self {
        Self {
            city_positions: Vec::new(),
            player_influence: Vec::new(),
            territory: Vec::new(),
            territory_max: Vec::new(),
            vision: Vec::new(),
        }
    }
    /// Grow the per-tick buffers to `grid_size` once (no-op when already sized).
    fn ensure(&mut self, grid_size: usize) {
        if self.player_influence.len() != grid_size {
            self.player_influence.resize(grid_size, 0);
        }
        if self.territory.len() != grid_size {
            self.territory.resize(grid_size, NONE);
        }
        if self.territory_max.len() != grid_size {
            self.territory_max.resize(grid_size, 0);
        }
        if self.vision.len() != grid_size {
            self.vision.resize(grid_size, false);
        }
    }
}
impl Default for InfluenceScratch {
    fn default() -> Self {
        Self::new()
    }
}

/// Saturating influence write — adds `delta` (the configured per-source
/// weight). `u16` caps absurd stacks at 65,535; only strict-`>` ordering is
/// consumed in-game.
#[inline]
fn add_influence(grid: &mut [u16], idx: usize, delta: u16) {
    grid[idx] = grid[idx].saturating_add(delta);
}

/// Stamp one Euclidean circle (from the precomputed `circle_spans` table) into
/// the vision and influence grids centred on cell `(cx, cy)`. The per-cell
/// x-bounds check is hoisted: each row's valid x-range is clamped to the map
/// once per row, so the inner loop is straight-line index math. The written cell
/// set is identical to the per-cell `(0..w).contains(&x)` test it replaces
/// (`x ∈ [cx+dx_start, cx+dx_end] ∧ x ∈ [0, w)` = `x ∈ [max(lo), min(hi)]`).
#[inline]
#[allow(clippy::too_many_arguments)] // circle params + 2 output buffers + dims
fn stamp_circle(
    vision: &mut [bool],
    influence: &mut [u16],
    cx: i32,
    cy: i32,
    radius: i32,
    weight: u16,
    w: usize,
    h: usize,
) {
    let spans = circle_spans().get(radius as usize).unwrap();
    for &(dy, dx_start, dx_end) in spans {
        let y = cy + dy;
        if y < 0 || y >= h as i32 {
            continue;
        }
        let y_off = (y as usize) * w;
        // Clamp the row's x-range to the map, in i32 space (so a zero-width map
        // yields lo > hi rather than a bogus usize range). lo ≥ 0 and hi ≤ w-1,
        // so every written idx is in bounds.
        let lo = (cx + dx_start).max(0);
        let hi = (cx + dx_end).min(w as i32 - 1);
        if lo > hi {
            continue;
        }
        for x in lo..=hi {
            let idx = y_off + (x as usize);
            vision[idx] = true;
            add_influence(influence, idx, weight);
        }
    }
}

/// Compute vision and influence for a single player.
/// Pure (no world access); takes only references so it stays unit-testable.
/// Vision and influence share one circle per source (unit or city); `settings`
/// supplies the per-source weight and the circle radii.
///
/// Both vision and influence are written into the caller's reused buffers
/// (reset here); the caller clones vision into the published Arc. Influence is
/// folded into the territory by the caller.
#[allow(clippy::too_many_arguments)] // Pure per-player projection: sources/settings/dims + 2 output buffers
fn compute_player(
    city_positions: &mut Vec<(f32, f32)>,
    terrain: &Arc<Vec<Terrain>>,
    player_id: &str,
    units: &[UnitHandle],
    cities: &[City],
    settings: &InfluenceSettings,
    w: i32,
    h: i32,
    influence: &mut [u16],
    vision: &mut [bool],
) {
    let w_usize = w as usize;

    // Vision and influence both reuse caller buffers, reset here (no alloc).
    vision.fill(false);
    influence.fill(0);

    // Unit loop — a single circle pass writes both vision and influence.
    for handle in units {
        let (ux_f, uy_f) = handle.pos;

        // Hills extend the circle radius (both vision and influence). The lower
        // `.clamp(0, …)` guard defends against a negative configured radius (would
        // otherwise wrap `as usize` and panic on the span lookup); the upper bound
        // caps to the precomputed span table.
        let radius = ((settings.unit_radius
            * match terrain
                .get((uy_f as usize) * w_usize + (ux_f as usize))
                .copied()
            {
                Some(Terrain::Hills) => settings.hill_radius_multiplier,
                _ => 1.0,
            }) as i32)
            .clamp(0, MAX_RADIUS);

        stamp_circle(
            vision,
            influence,
            ux_f as i32,
            uy_f as i32,
            radius,
            settings.unit_weight,
            w_usize,
            h as usize,
        );
    }

    // City loop — same single-circle shape, configured radius/weight, no
    // terrain modifier.
    let city_radius =
        ((settings.unit_radius * settings.city_radius_multiplier) as i32).clamp(0, MAX_RADIUS);
    city_positions.clear();
    for c in cities {
        if c.owner.as_deref() == Some(player_id) {
            city_positions.push(c.position);
        }
    }
    for &(cx, cy) in city_positions.iter() {
        stamp_circle(
            vision,
            influence,
            cx as i32,
            cy as i32,
            city_radius,
            settings.city_weight,
            w_usize,
            h as usize,
        );
    }
}

/// Sentinel for "no player has influence at this cell."
pub(crate) const NONE: u8 = 255;

/// Fold one player's influence grid into the running territory accumulators:
/// a cell is claimed by this player only on a strict influence `>` (a running
/// max), so the lowest-sorted player wins ties. Shared by `build_territory_grid`
/// (unit tests) and the fused per-player fold in `run()` — single source of the
/// fold logic.
#[inline]
fn fold_influence_into(dominant: &mut [u8], max_inf: &mut [u16], grid: &[u16], idx: u8) {
    for i in 0..dominant.len() {
        if grid[i] > max_inf[i] {
            max_inf[i] = grid[i];
            dominant[i] = idx;
        }
    }
}

/// Compute the territory grid: which player has the highest influence at each
/// cell. Players iterated in sorted order; strict `>` means the lowest-sorted
/// ID wins ties. Returns `Vec<u8>` — player index or `NONE` (255) where no
/// player has influence.
///
/// Production `run()` folds each player straight into the running territory
/// (see `fold_influence_into`); this standalone batch version exists for the
/// unit tests that exercise the fold on a ready-made set of per-player grids.
#[cfg(test)]
pub(crate) fn build_territory_grid(
    border_grids: &HashMap<String, Vec<u16>>,
    player_to_idx: &HashMap<&str, u8>,
    grid_size: usize,
) -> Vec<u8> {
    let mut dominant = vec![NONE; grid_size];
    let mut max_inf = vec![0u16; grid_size];

    let mut player_ids: Vec<&str> = player_to_idx.keys().copied().collect();
    player_ids.sort();
    for player_id in player_ids {
        let idx = *player_to_idx.get(player_id).unwrap();
        if let Some(grid) = border_grids.get(player_id) {
            fold_influence_into(&mut dominant, &mut max_inf, grid, idx);
        }
    }
    // Cells no player reached stay `NONE`: `max_inf` only ever increases, so
    // `max_inf[i] == 0` ⟹ `dominant[i]` was never written (its init value).
    dominant
}

pub fn run(
    world: &mut World,
    ctx: &TickContext,
    _tick: u64,
    scratch: &mut InfluenceScratch,
    dirty: bool,
    settings: &InfluenceSettings,
) {
    // Skip path: game state unchanged since the last run → stored grids are
    // still correct. Avoids the compute AND all per-player grid allocations.
    if !dirty {
        return;
    }

    // Scoped Arc extraction for terrain grid (24 bytes pointer bump, not a clone)
    let terrain_grid: Arc<Vec<Terrain>> = match world.world_data(ctx.world_entity) {
        Some(wb) => Arc::clone(&wb.terrain.grid),
        None => return,
    };
    // wb dropped — borrow released

    let w = ctx.width as i32;
    let h = ctx.height as i32;
    let grid_size = (w * h) as usize;

    // Sorted player order: deterministic tie-break, and the enumeration index
    // MUST match `encirclement.rs` (which sorts `units_by_player` keys and
    // enumerates) or encirclement would flag the wrong player.
    let mut sorted_players: Vec<&String> = ctx.units_by_player.keys().collect();
    sorted_players.sort();

    // Size scratch buffers once; reset the territory accumulators for this run.
    scratch.ensure(grid_size);
    scratch.territory.fill(NONE);
    scratch.territory_max.fill(0);

    let mut updated_vision: HashMap<String, Vec<bool>> =
        HashMap::with_capacity(sorted_players.len());

    for (idx, &player_id) in sorted_players.iter().enumerate() {
        let units = ctx.units_by_player.get(player_id).unwrap();
        // Compute into the reused scratch buffers (disjoint field borrows).
        // Vision is stamped into `scratch.vision`, then cloned into the
        // published HashMap (one clone per player per dirty tick, no fresh
        // alloc + zero-init).
        compute_player(
            &mut scratch.city_positions,
            &terrain_grid,
            player_id,
            units,
            &ctx.cities,
            settings,
            w,
            h,
            &mut scratch.player_influence,
            &mut scratch.vision[..],
        );
        // A1: fold this player's influence straight into the running territory
        // (index = sorted position), then discard the buffer.
        fold_influence_into(
            &mut scratch.territory,
            &mut scratch.territory_max,
            &scratch.player_influence,
            idx as u8,
        );
        updated_vision.insert(player_id.clone(), scratch.vision.clone());
    }

    // Publish. `borders.grids` is production-dead (only `#[cfg(test)]` reads it)
    // and is no longer written; the per-player influence grids are folded above
    // and discarded. Territory is cloned into a fresh Arc since the world owns it.
    if let Some(wb) = world.world_data_mut(ctx.world_entity) {
        wb.vision.grids = Arc::new(updated_vision);
        wb.territory.grid = Arc::new(scratch.territory.clone());
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::CityUpgradeSettings;
    use crate::game::game::{TickContext, UnitHandle};
    use crate::game::world_ext::WorldExt;
    use std::collections::HashMap;

    const W: i32 = 40;
    const H: i32 = 40;

    fn ctx_with(
        units_by_player: HashMap<String, Vec<UnitHandle>>,
        cities: Vec<City>,
    ) -> TickContext {
        TickContext {
            world_entity: hecs::Entity::from_bits(1u64 << 32).unwrap(),
            cities,
            width: W as u32,
            height: H as u32,
            units_by_player,
            owner_id_by_entity: HashMap::new(),
            player_id_to_index: HashMap::new(),
            next_player_idx: 0,
        }
    }

    fn terrain() -> Arc<Vec<Terrain>> {
        Arc::new(vec![Terrain::Plains; (W * H) as usize])
    }

    fn terrain_with_hills() -> Arc<Vec<Terrain>> {
        let mut t = vec![Terrain::Plains; (W * H) as usize];
        t[20 * (W as usize) + 20] = Terrain::Hills;
        Arc::new(t)
    }

    fn unit(pos: (f32, f32)) -> UnitHandle {
        // Dummy entity (gen 1, id 0) — compute_player only reads `pos`.
        UnitHandle {
            entity: hecs::Entity::from_bits(1u64 << 32).unwrap(),
            pos,
        }
    }

    fn city(pos: (f32, f32), owner: &str) -> City {
        let mut c = City::new("city", pos);
        c.owner = Some(owner.to_string());
        c
    }

    fn compute(
        units: &[(f32, f32)],
        cities: &[City],
        terrain: &Arc<Vec<Terrain>>,
    ) -> (Vec<bool>, Vec<u16>) {
        let mut city_positions = Vec::new();
        let mut influence = vec![0u16; (W * H) as usize];
        let mut vision = vec![false; (W * H) as usize];
        let units: Vec<UnitHandle> = units.iter().map(|&p| unit(p)).collect();
        compute_player(
            &mut city_positions,
            terrain,
            "player_0",
            &units,
            cities,
            &InfluenceSettings::default(),
            W,
            H,
            &mut influence,
            &mut vision[..],
        );
        (vision, influence)
    }

    fn idx(x: i32, y: i32) -> usize {
        (y as usize) * (W as usize) + (x as usize)
    }

    #[test]
    fn flat_unit_vision_and_influence_shape() {
        let (vision, inf) = compute(&[(20.0, 20.0)], &[], &terrain());
        // Vision = circle r5: dist 3 < 5 → visible
        assert!(vision[idx(23, 20)]);
        // dist 4 < 5 → visible (was outside the old r3 square)
        assert!(vision[idx(24, 20)]);
        // dist 5 = r5 edge → visible
        assert!(vision[idx(25, 20)]);
        // dist 6 > 5 → not visible
        assert!(!vision[idx(26, 20)]);
        // (3,3) corner: dist √18 ≈ 4.24 < 5 → visible
        assert!(vision[idx(23, 23)]);
        // (4,4) corner: dist √32 ≈ 5.66 > 5 → NOT visible
        assert!(!vision[idx(24, 24)]);
        // Influence: same circle, weight 1
        assert_eq!(inf[idx(25, 20)], 1);
        assert_eq!(inf[idx(24, 24)], 0);
        assert_eq!(inf[idx(20, 20)], 1);
    }

    #[test]
    fn city_vision_and_influence_shape() {
        let (vision, inf) = compute(&[], &[city((20.0, 20.0), "player_0")], &terrain());
        // City circle r7: dist 7 = edge → visible
        assert!(vision[idx(27, 20)]);
        // (4,4) corner: dist 5.66 < 7 → visible
        assert!(vision[idx(24, 24)]);
        // dist 8 > 7 → not visible
        assert!(!vision[idx(28, 20)]);
        // Influence: city weight is 1 (equal to units, per design decision)
        assert_eq!(inf[idx(27, 20)], 1);
        assert_eq!(inf[idx(20, 20)], 1);
    }

    #[test]
    fn hill_unit_extends_both_vision_and_influence() {
        // Hill at (20,20): circle r10 (5×2) — extends both vision and influence
        let (vision, inf) = compute(&[(20.0, 20.0)], &[], &terrain_with_hills());
        // Vision: dist 6 < 10 → visible
        assert!(vision[idx(26, 20)]);
        // (4,4) corner: dist 5.66 < 10 → visible (was not under r5)
        assert!(vision[idx(24, 24)]);
        // dist 10 = r10 edge → visible
        assert!(vision[idx(30, 20)]);
        // dist 11 > 10 → not visible
        assert!(!vision[idx(31, 20)]);
        // Influence: also r10 now
        assert_eq!(inf[idx(26, 20)], 1); // was 0 under r5
        assert_eq!(inf[idx(24, 24)], 1); // was 0 under r5 (dist 5.66 < 10)
        assert_eq!(inf[idx(30, 20)], 1); // r10 edge
        assert_eq!(inf[idx(31, 20)], 0); // outside r10
    }

    #[test]
    fn u16_exact_at_realistic_density_and_saturating() {
        // 7000 units stacked at one cell — exact, well under u16::MAX.
        let dense: Vec<(f32, f32)> = vec![(20.0, 20.0); 7000];
        let (_, inf) = compute(&dense, &[], &terrain());
        assert_eq!(inf[idx(20, 20)], 7000);

        // Saturating boundary: from u16::MAX - 3, adding 5 caps at u16::MAX.
        let mut g = vec![u16::MAX - 3];
        add_influence(&mut g, 0, 5);
        assert_eq!(g[0], u16::MAX);
        add_influence(&mut g, 0, 5);
        assert_eq!(g[0], u16::MAX, "must not wrap");
    }

    #[test]
    fn run_skip_keeps_grids_untouched() {
        let mut world = World::new();
        let mut funds = HashMap::new();
        funds.insert("player_0".to_string(), 0);
        let cities = vec![city((10.0, 10.0), "player_0")];
        let terrain = vec![vec![Terrain::Plains; H as usize]; W as usize];
        let world_entity = world.spawn_world(
            terrain,
            W as u32,
            H as u32,
            CityUpgradeSettings::default(),
            funds,
            cities,
        );

        let mut ctx = ctx_with(
            HashMap::from([("player_0".to_string(), vec![unit((20.0, 20.0))])]),
            vec![city((10.0, 10.0), "player_0")],
        );
        ctx.world_entity = world_entity;

        let settings = InfluenceSettings::default();
        let mut scratch = InfluenceScratch::new();
        run(&mut world, &ctx, 1, &mut scratch, true, &settings);
        let (vision_ptr, vision_snapshot, territory_ptr) = {
            let wb = world.world_data(world_entity).unwrap();
            (
                Arc::as_ptr(&wb.vision.grids) as *const (),
                wb.vision.grids.clone(),
                Arc::as_ptr(&wb.territory.grid) as *const (),
            )
        };
        // Sanity: the dirty run actually produced non-empty vision and a
        // non-trivial territory grid (a unit's r5 circle is not all NONE).
        assert!(vision_snapshot["player_0"].iter().any(|&v| v));
        assert!(world
            .world_data(world_entity)
            .unwrap()
            .territory
            .grid
            .iter()
            .any(|&v| v != NONE));

        // Clean tick: must early-return without touching the stored Arcs.
        run(&mut world, &ctx, 2, &mut scratch, false, &settings);
        let wb = world.world_data(world_entity).unwrap();
        assert_eq!(
            Arc::as_ptr(&wb.vision.grids) as *const (),
            vision_ptr,
            "skip must not replace the vision Arc"
        );
        assert_eq!(
            Arc::as_ptr(&wb.territory.grid) as *const (),
            territory_ptr,
            "skip must not replace the territory grid Arc"
        );
        assert_eq!(&*wb.vision.grids, &*vision_snapshot);
    }

    #[test]
    fn territory_grid_single_player() {
        // One unit → territory = 0 within the r5 circle, 255 outside
        let (_, inf) = compute(&[(20.0, 20.0)], &[], &terrain());
        let player_to_idx: HashMap<&str, u8> = [("player_0", 0u8)].into_iter().collect();
        let grids: HashMap<String, Vec<u16>> =
            [("player_0".to_string(), inf)].into_iter().collect();
        let territory = build_territory_grid(&grids, &player_to_idx, (W * H) as usize);
        assert_eq!(territory[idx(20, 20)], 0); // center
        assert_eq!(territory[idx(25, 20)], 0); // r5 edge
        assert_eq!(territory[idx(26, 20)], NONE); // outside
    }

    #[test]
    fn territory_grid_two_players_boundary() {
        // Two units 10 apart: (15,20) player_a and (25,20) player_b.
        // Cell (20,20): dist 5 from both → tie → "player_a" wins (sorted).
        let (_, inf_a) = compute(&[(15.0, 20.0)], &[], &terrain());
        let (_, inf_b) = compute(&[(25.0, 20.0)], &[], &terrain());
        let player_to_idx: HashMap<&str, u8> =
            [("player_a", 0u8), ("player_b", 1u8)].into_iter().collect();
        let grids: HashMap<String, Vec<u16>> = [
            ("player_a".to_string(), inf_a),
            ("player_b".to_string(), inf_b),
        ]
        .into_iter()
        .collect();
        let territory = build_territory_grid(&grids, &player_to_idx, (W * H) as usize);
        // Center cell (20,20): dist 5 from both, tie → player_a (index 0)
        assert_eq!(territory[idx(20, 20)], 0);
        // (15,20): dist 0 from a, dist 10 from b → player_a
        assert_eq!(territory[idx(15, 20)], 0);
        // (25,20): dist 10 from a, dist 0 from b → player_b
        assert_eq!(territory[idx(25, 20)], 1);
    }

    #[test]
    fn territory_grid_no_units() {
        let player_to_idx: HashMap<&str, u8> = [("player_a", 0u8)].into_iter().collect();
        let grids: HashMap<String, Vec<u16>> =
            [("player_a".to_string(), vec![0u16; (W * H) as usize])]
                .into_iter()
                .collect();
        let territory = build_territory_grid(&grids, &player_to_idx, (W * H) as usize);
        assert!(territory.iter().all(|&v| v == NONE));
    }

    #[test]
    fn build_territory_grid_missing_player_key() {
        // A player present in the index map but absent from the grids map must
        // not panic and contributes nothing.
        let player_to_idx: HashMap<&str, u8> =
            [("player_a", 0u8), ("player_b", 1u8)].into_iter().collect();
        let grids: HashMap<String, Vec<u16>> =
            [("player_a".to_string(), vec![1u16; (W * H) as usize])]
                .into_iter()
                .collect();
        let territory = build_territory_grid(&grids, &player_to_idx, (W * H) as usize);
        assert!(territory.iter().all(|&v| v == 0));
    }

    /// Independent u32 reference implementation (wider accumulator type) used
    /// to cross-check `build_territory_grid`'s u16 arithmetic. Logic mirrors
    /// the production function but never truncates, so any divergence flags a
    /// u16 overflow/mis-ordering bug.
    fn reference_dominant(
        border_grids: &HashMap<String, Vec<u32>>,
        player_to_idx: &HashMap<&str, u8>,
        grid_size: usize,
    ) -> Vec<u8> {
        let mut dominant = vec![NONE; grid_size];
        let mut max_inf = vec![0u32; grid_size];
        let mut players: Vec<&str> = player_to_idx.keys().copied().collect();
        players.sort();
        for player_id in players {
            let idx = *player_to_idx.get(player_id).unwrap();
            if let Some(grid) = border_grids.get(player_id) {
                for i in 0..grid_size {
                    if grid[i] > max_inf[i] {
                        max_inf[i] = grid[i];
                        dominant[i] = idx;
                    }
                }
            }
        }
        for i in 0..grid_size {
            if max_inf[i] == 0 {
                dominant[i] = NONE;
            }
        }
        dominant
    }

    #[test]
    fn build_territory_grid_u16_matches_u32_reference() {
        let grid_size = 900; // 30x30
        let players = ["player_a", "player_b", "player_c"];
        let player_to_idx: HashMap<&str, u8> = players
            .iter()
            .enumerate()
            .map(|(i, p)| (*p, i as u8))
            .collect();

        // Deterministic LCG
        let mut rng = 0x9e37_79b9u32;
        let mut next = move || {
            rng = rng.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
            rng
        };

        let mut grids_u16: HashMap<String, Vec<u16>> = HashMap::new();
        let mut grids_u32: HashMap<String, Vec<u32>> = HashMap::new();
        for p in players {
            let mut g16 = vec![0u16; grid_size];
            let mut g32 = vec![0u32; grid_size];
            for i in 0..grid_size {
                // Sparse values including exact-tie points: 5, 10, 65530, 0
                let v = match next() % 10 {
                    0 => 0u32,
                    1 => 5,
                    2 => 10,
                    3 => 65530,
                    _ => next() % 65536,
                };
                g16[i] = v as u16;
                g32[i] = v;
            }
            grids_u16.insert(p.to_string(), g16);
            grids_u32.insert(p.to_string(), g32);
        }

        let d16 = build_territory_grid(&grids_u16, &player_to_idx, grid_size);
        let d32 = reference_dominant(&grids_u32, &player_to_idx, grid_size);
        assert_eq!(d16, d32, "u16 territory grid must match u32 reference");

        // Sanity: the grid is non-trivial (multiple players + NONE cells)
        let seen: std::collections::HashSet<u8> = d16.iter().copied().collect();
        assert!(seen.contains(&0));
        assert!(seen.contains(&1));
        assert!(seen.contains(&NONE));
    }

    #[test]
    fn circle_pass_edge_cells() {
        // A unit at the map corner (0,0) is clipped by the map bounds. Compare
        // the stamped circle against a naive per-cell `dx^2+dy^2 <= r^2` reference
        // (the `circle_spans` table and this test use the same integer rule, so
        // any off-by-one in the hoisted per-row x-clamp in `stamp_circle` shows up).
        let (vision, _) = compute(&[(0.0, 0.0)], &[], &terrain());
        let r: i32 = 5;
        for y in 0..H {
            for x in 0..W {
                let dx = x;
                let dy = y;
                let expected = (dx * dx + dy * dy) <= r * r;
                assert_eq!(vision[idx(x, y)], expected, "cell ({x},{y})");
            }
        }
    }

    #[test]
    fn territory_fold_zero_stays_none() {
        // Folding an all-zero influence grid into a fresh accumulator must leave
        // every cell NONE (guards the removed "reset to NONE" second pass in the
        // fused fold path). A real grid then claims exactly its own cells.
        let grid_size = (W * H) as usize;
        let zero = vec![0u16; grid_size];
        let mut dominant = vec![NONE; grid_size];
        let mut max_inf = vec![0u16; grid_size];
        fold_influence_into(&mut dominant, &mut max_inf, &zero, 0);
        assert!(dominant.iter().all(|&v| v == NONE));

        let (_, inf) = compute(&[(20.0, 20.0)], &[], &terrain());
        let mut dominant = vec![NONE; grid_size];
        let mut max_inf = vec![0u16; grid_size];
        fold_influence_into(&mut dominant, &mut max_inf, &inf, 7);
        assert_eq!(dominant[idx(20, 20)], 7);
        assert_eq!(dominant[idx(26, 20)], NONE);
    }
}
