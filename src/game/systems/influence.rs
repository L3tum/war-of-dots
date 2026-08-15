//! InfluenceSystem — merged vision + border influence computation.
//!
//! Uses circle projection from units and cities. Computes both binary
//! visibility and weighted influence in one inner loop per source.
//!
//! Where the vision square is fully contained in the influence circle
//! (flat units, cities) both are written in the same span pass. Hill
//! units (extended vision radius) keep the original two-pass projection
//! so their (larger) vision shape is preserved exactly.
//!
//! Sequential by design: Rayon was tried at multiple granularities and
//! rejected — per-player work (~0.1 ms) is below dispatch breakeven.
//!
//! `run()` takes a `dirty` flag: when the game state has not changed since
//! the last run, the stored grids are still correct and the whole compute
//! (including all per-player grid allocations) is skipped.

use crate::game::economy::City;
use crate::game::game::{TickContext, UnitHandle};
use crate::game::unit::Terrain;
use crate::game::world_ext::WorldExt;
use hecs::World;
use std::collections::HashMap;
use std::sync::{Arc, OnceLock};

/// Radius for unit influence projection.
const UNIT_INFLUENCE_RADIUS: f32 = 5.0;
/// Multiplier for city radii (cities project farther).
const CITY_RADIUS_MULTIPLIER: f32 = 1.5;
/// Max radius we need spans for (city influence = 5 × 1.5 = 7.5, round up).
const MAX_RADIUS: i32 = 8;

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
/// `city_positions` is the per-player filtered city list (typically ≤ 20
/// entries), `.clear()`'d per player with capacity preserved.
pub struct InfluenceScratch {
    pub city_positions: Vec<(f32, f32)>,
}
impl InfluenceScratch {
    pub fn new() -> Self {
        Self {
            city_positions: Vec::new(),
        }
    }
    pub fn clear(&mut self) {
        self.city_positions.clear();
    }
}
impl Default for InfluenceScratch {
    fn default() -> Self {
        Self::new()
    }
}

/// Saturating influence write — units `+1`, cities `+5`. `u16` caps absurd
/// stacks at 65,535; only strict-`>` ordering is consumed in-game.
#[inline]
fn add_influence(grid: &mut [u16], idx: usize, delta: u16) {
    grid[idx] = grid[idx].saturating_add(delta);
}

/// Compute vision and influence grids for a single player.
/// Pure (no world access); takes only references so it stays unit-testable.
fn compute_player(
    scratch: &mut InfluenceScratch,
    terrain: &Arc<Vec<Vec<Terrain>>>,
    player_id: &str,
    units: &[UnitHandle],
    cities: &[City],
    w: i32,
    h: i32,
) -> (Vec<bool>, Vec<u16>) {
    let w_usize = w as usize;
    let grid_size = w_usize * (h as usize);

    // Fresh, zeroed per player — zeroing is guaranteed by the initializers.
    // (Recycled buffers would retain stale vision/influence cells.)
    let mut vision_grid = vec![false; grid_size];
    let mut influence_grid = vec![0u16; grid_size];

    // Precomputed circle spans for influence radius
    let i_radius = UNIT_INFLUENCE_RADIUS as i32;

    // Combined unit loop — vision + influence in one pass where the vision
    // square is contained in the influence circle (flat units: r3 ⊆ r5).
    for handle in units {
        let (ux_f, uy_f) = handle.pos;

        // Terrain modifier for vision (hills extend range)
        let terrain_mod = match terrain.get(ux_f as usize).and_then(|r| r.get(uy_f as usize)) {
            Some(Terrain::Hills) => 2.0,
            _ => 1.0,
        };

        let v_radius = (crate::game::constants::VISION_RADIUS * terrain_mod) as i32;
        let ux_i = ux_f as i32;
        let uy_i = uy_f as i32;

        if v_radius <= i_radius {
            // Fold: vision square ⊆ influence circle — one span pass writes
            // both. Vision keeps its original SQUARE shape: only cells within
            // Chebyshev `v_radius` (a subset of this circle span) get marked.
            for &(dy, dx_start, dx_end) in circle_spans().get(i_radius as usize).unwrap() {
                let y = uy_i + dy;
                if (0..h).contains(&y) {
                    let y_off = (y as usize) * w_usize;
                    let in_vision_row = dy.abs() <= v_radius;
                    for dx in dx_start..=dx_end {
                        let x = ux_i + dx;
                        if (0..w).contains(&x) {
                            let idx = y_off + (x as usize);
                            if in_vision_row && dx.abs() <= v_radius {
                                vision_grid[idx] = true;
                            }
                            add_influence(&mut influence_grid, idx, 1);
                        }
                    }
                }
            }
        } else {
            // Hill units: vision square (r6) exceeds the influence circle (r5) —
            // keep the original two passes so the hill vision shape is unchanged.
            // Vision (square): iterate bounding box, Chebyshev distance
            for dy in -v_radius..=v_radius {
                let y = uy_i + dy;
                if (0..h).contains(&y) {
                    let y_off = (y as usize) * w_usize;
                    for dx in -v_radius..=v_radius {
                        let x = ux_i + dx;
                        if (0..w).contains(&x) {
                            vision_grid[y_off + (x as usize)] = true;
                        }
                    }
                }
            }
            // Influence (circle): use precomputed spans to skip corners
            for &(dy, dx_start, dx_end) in circle_spans().get(i_radius as usize).unwrap() {
                let y = uy_i + dy;
                if (0..h).contains(&y) {
                    let y_off = (y as usize) * w_usize;
                    for dx in dx_start..=dx_end {
                        let x = ux_i + dx;
                        if (0..w).contains(&x) {
                            add_influence(&mut influence_grid, y_off + (x as usize), 1);
                        }
                    }
                }
            }
        }
    }

    // Combined city loop — city vision square (r4) ⊆ influence circle (r7),
    // so one span pass writes both. Vision keeps its SQUARE shape (Chebyshev
    // `city_v_radius`), influence stays circular with weight 5.
    let city_v_radius = (crate::game::constants::VISION_RADIUS * CITY_RADIUS_MULTIPLIER) as i32;
    let city_i_radius = ((UNIT_INFLUENCE_RADIUS * CITY_RADIUS_MULTIPLIER) as i32).min(MAX_RADIUS);

    scratch.city_positions.clear();
    for c in cities {
        if c.owner.as_deref() == Some(player_id) {
            scratch.city_positions.push(c.position);
        }
    }
    if let Some(spans) = circle_spans().get(city_i_radius as usize) {
        for &(cx, cy) in &scratch.city_positions {
            let cx_i = cx as i32;
            let cy_i = cy as i32;
            for &(dy, dx_start, dx_end) in spans {
                let y = cy_i + dy;
                if (0..h).contains(&y) {
                    let y_off = (y as usize) * w_usize;
                    let in_vision_row = dy.abs() <= city_v_radius;
                    for dx in dx_start..=dx_end {
                        let x = cx_i + dx;
                        if (0..w).contains(&x) {
                            let idx = y_off + (x as usize);
                            if in_vision_row && dx.abs() <= city_v_radius {
                                vision_grid[idx] = true;
                            }
                            add_influence(&mut influence_grid, idx, 5);
                        }
                    }
                }
            }
        }
    }

    (vision_grid, influence_grid)
}

pub fn run(
    world: &mut World,
    ctx: &TickContext,
    _tick: u64,
    scratch: &mut InfluenceScratch,
    dirty: bool,
) {
    // Skip path: game state unchanged since the last run → stored grids are
    // still correct. Avoids the compute AND all per-player grid allocations.
    if !dirty {
        return;
    }

    // Scoped Arc extraction for terrain grid (24 bytes pointer bump, not a clone)
    let terrain_grid: Arc<Vec<Vec<Terrain>>> = match world.world_data(ctx.world_entity) {
        Some(wb) => Arc::clone(&wb.terrain.grid),
        None => return,
    };
    // wb dropped — borrow released

    let w = ctx.width as i32;
    let h = ctx.height as i32;

    // Compute per player directly from ctx (positions carried by UnitHandle),
    // inserting finished grids into the output maps by value — no intermediate
    // position Vecs, no per-player grid clones, no id to_string().
    let mut updated_vision: HashMap<String, Vec<bool>> = HashMap::new();
    let mut updated_border: HashMap<String, Vec<u16>> = HashMap::new();

    for (player_id, units) in &ctx.units_by_player {
        let (vision, influence) =
            compute_player(scratch, &terrain_grid, player_id, units, &ctx.cities, w, h);
        updated_vision.insert(player_id.clone(), vision);
        updated_border.insert(player_id.clone(), influence);
    }
    scratch.clear(); // release the per-player city list contents (capacity kept)

    // Write to world
    if let Some(wb) = world.world_data_mut(ctx.world_entity) {
        wb.vision.grids = Arc::new(updated_vision);
        wb.borders.grids = Arc::new(updated_border);
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

    fn ctx_with(units_by_player: HashMap<String, Vec<UnitHandle>>, cities: Vec<City>) -> TickContext {
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

    fn terrain() -> Arc<Vec<Vec<Terrain>>> {
        Arc::new(vec![vec![Terrain::Plains; H as usize]; W as usize])
    }

    fn terrain_with_hills() -> Arc<Vec<Vec<Terrain>>> {
        let mut t = vec![vec![Terrain::Plains; H as usize]; W as usize];
        t[20][20] = Terrain::Hills;
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
        terrain: &Arc<Vec<Vec<Terrain>>>,
    ) -> (Vec<bool>, Vec<u16>) {
        let mut scratch = InfluenceScratch::new();
        let units: Vec<UnitHandle> = units.iter().map(|&p| unit(p)).collect();
        compute_player(&mut scratch, terrain, "player_0", &units, cities, W, H)
    }

    fn idx(x: i32, y: i32) -> usize {
        (y as usize) * (W as usize) + (x as usize)
    }

    /// Old (pre-fold) projection: separate square vision + circle influence
    /// passes for every source — the reference implementation for the fold.
    fn legacy_compute(
        units: &[(f32, f32)],
        cities: &[City],
        terrain: &Arc<Vec<Vec<Terrain>>>,
    ) -> (Vec<bool>, Vec<u16>) {
        let mut vision = vec![false; (W * H) as usize];
        let mut inf = vec![0u16; (W * H) as usize];
        let w_usize = W as usize;
        let i_radius = UNIT_INFLUENCE_RADIUS as i32;
        for &(ux, uy) in units {
            let mod_f = match terrain.get(ux as usize).and_then(|r| r.get(uy as usize)) {
                Some(Terrain::Hills) => 2.0,
                _ => 1.0,
            };
            let v_radius = (crate::game::constants::VISION_RADIUS * mod_f) as i32;
            let (ux_i, uy_i) = (ux as i32, uy as i32);
            for dy in -v_radius..=v_radius {
                for dx in -v_radius..=v_radius {
                    let x = ux_i + dx;
                    let y = uy_i + dy;
                    if x >= 0 && y >= 0 && x < W && y < H {
                        vision[(y as usize) * w_usize + (x as usize)] = true;
                    }
                }
            }
            for &(dy, dx_start, dx_end) in circle_spans().get(i_radius as usize).unwrap() {
                for dx in dx_start..=dx_end {
                    let x = ux_i + dx;
                    let y = uy_i + dy;
                    if x >= 0 && y >= 0 && x < W && y < H {
                        inf[(y as usize) * w_usize + (x as usize)] += 1;
                    }
                }
            }
        }
        let city_v_radius =
            (crate::game::constants::VISION_RADIUS * CITY_RADIUS_MULTIPLIER) as i32;
        let city_i_radius = ((UNIT_INFLUENCE_RADIUS * CITY_RADIUS_MULTIPLIER) as i32).min(MAX_RADIUS);
        for c in cities {
            if c.owner.as_deref() != Some("player_0") {
                continue;
            }
            let (cx_i, cy_i) = (c.position.0 as i32, c.position.1 as i32);
            for dy in -city_v_radius..=city_v_radius {
                for dx in -city_v_radius..=city_v_radius {
                    let x = cx_i + dx;
                    let y = cy_i + dy;
                    if x >= 0 && y >= 0 && x < W && y < H {
                        vision[(y as usize) * w_usize + (x as usize)] = true;
                    }
                }
            }
            if let Some(spans) = circle_spans().get(city_i_radius as usize) {
                for &(dy, dx_start, dx_end) in spans {
                    for dx in dx_start..=dx_end {
                        let x = cx_i + dx;
                        let y = cy_i + dy;
                        if x >= 0 && y >= 0 && x < W && y < H {
                            inf[(y as usize) * w_usize + (x as usize)] += 5;
                        }
                    }
                }
            }
        }
        (vision, inf)
    }

    #[test]
    fn flat_unit_vision_and_influence_shape() {
        let (vision, inf) = compute(&[(20.0, 20.0)], &[], &terrain());
        // Chebyshev-3 square corners visible
        assert!(vision[idx(23, 20)]);
        assert!(vision[idx(20, 23)]);
        assert!(vision[idx(23, 23)]);
        // Outside the r3 square, inside the r5 circle — not visible
        assert!(!vision[idx(24, 20)]);
        // Circle influence: r5 edge present, square corner (4,4) absent
        assert_eq!(inf[idx(25, 20)], 1);
        assert_eq!(inf[idx(24, 24)], 0);
        assert_eq!(inf[idx(20, 20)], 1);
    }

    #[test]
    fn city_vision_and_influence_shape() {
        let (vision, inf) = compute(&[], &[city((20.0, 20.0), "player_0")], &terrain());
        // Chebyshev-4 square corners visible (3*1.5 truncated = 4)
        assert!(vision[idx(24, 20)]);
        assert!(vision[idx(24, 24)]);
        // Chebyshev 5 — not visible
        assert!(!vision[idx(25, 20)]);
        // Circle r7 influence with weight 5
        assert_eq!(inf[idx(27, 20)], 5);
        assert_eq!(inf[idx(20, 20)], 5);
    }

    #[test]
    fn hill_unit_vision_preserved() {
        // Hill at (20,20): vision square r6 (3*2) — must include cells that
        // are OUTSIDE the influence circle r5, e.g. Cheb 6 and the (4,4) corner.
        let (vision, inf) = compute(&[(20.0, 20.0)], &[], &terrain_with_hills());
        assert!(vision[idx(26, 20)], "Chebyshev 6 must be visible on a hill");
        assert!(vision[idx(24, 24)], "square corner (4,4) must stay visible");
        // But influence stays circular: (4,4) at dist 5.66 > r5
        assert_eq!(inf[idx(24, 24)], 0);
        assert_eq!(inf[idx(25, 20)], 1);
    }

    #[test]
    fn fold_matches_legacy_projection() {
        // Differential: mixed plains + hill units and two cities (one owned,
        // one not) — folded output must be byte-identical to the legacy
        // square + circle passes.
        let mut units = Vec::new();
        let mut rng = 0x1234_5678u32;
        for _ in 0..200 {
            // deterministic LCG
            rng = rng.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
            let x = (rng % (W as u32 - 4)) as f32 + 2.0;
            rng = rng.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
            let y = ((rng >> 8) % (H as u32 - 4)) as f32 + 2.0;
            units.push((x, y));
        }
        let cities = vec![city((10.0, 10.0), "player_0"), city((30.0, 30.0), "player_1")];
        let t = terrain_with_hills();
        let (v_new, i_new) = compute(&units, &cities, &t);
        let (v_old, i_old) = legacy_compute(&units, &cities, &t);
        assert_eq!(v_new, v_old, "vision grids must be identical");
        assert_eq!(i_new, i_old, "influence grids must be identical");
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
            HashMap::from([(
                "player_0".to_string(),
                vec![unit((20.0, 20.0))],
            )]),
            vec![city((10.0, 10.0), "player_0")],
        );
        ctx.world_entity = world_entity;

        let mut scratch = InfluenceScratch::new();
        run(&mut world, &ctx, 1, &mut scratch, true);
        let (grids_ptr, vision_snapshot, border_snapshot) = {
            let wb = world.world_data(world_entity).unwrap();
            (
                Arc::as_ptr(&wb.borders.grids) as *const (),
                wb.vision.grids.clone(),
                wb.borders.grids.clone(),
            )
        };

        // Clean tick: must early-return without touching the stored Arcs.
        run(&mut world, &ctx, 2, &mut scratch, false);
        let wb = world.world_data(world_entity).unwrap();
        assert_eq!(
            Arc::as_ptr(&wb.borders.grids) as *const (),
            grids_ptr,
            "skip must not replace the grid Arc"
        );
        assert_eq!(&*wb.vision.grids, &*vision_snapshot);
        assert_eq!(&*wb.borders.grids, &*border_snapshot);
        // Sanity: the dirty run actually produced non-empty grids
        assert!(!border_snapshot["player_0"].iter().all(|&v| v == 0));
    }
}
