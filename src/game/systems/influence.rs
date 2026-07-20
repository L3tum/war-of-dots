//! InfluenceSystem — merged vision + border influence computation.
//!
//! Uses circle projection from units and cities. Computes both binary
//! visibility and weighted influence in one inner loop per source.
//!
//! Parallelized across players using Rayon — each player's grids are
//! independent, so they can be computed in parallel.
//!
//! Accepts pre-grouped TickContext to avoid redundant player enumeration
//! and per-player unit filtering.

use crate::game::game::TickContext;
use crate::game::unit::Terrain;
use crate::game::world_ext::WorldExt;
use hecs::World;
use std::collections::HashMap;
use std::sync::Arc;
use std::sync::OnceLock;

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

/// Input data for a single player's influence computation.
/// All fields are Send + Sync (owned data, no world references).
struct PlayerInput<'a> {
    player_id: &'a str,
    unit_positions: Vec<(f32, f32)>,
    city_positions: Vec<(f32, f32)>,
    terrain: &'a Arc<Vec<Vec<Terrain>>>,
    width: i32,
    height: i32,
}

/// Output grids for a single player.
struct PlayerOutput {
    player_id: String,
    vision_grid: Vec<bool>,   // flat: [y * width + x]
    influence_grid: Vec<u32>, // flat: [y * width + x]
}

/// Compute vision and influence grids for a single player.
/// This function is pure (no world access) and can run in parallel.
fn compute_player_influence(input: &PlayerInput) -> PlayerOutput {
    let w = input.width;
    let h = input.height;
    let w_usize = w as usize;
    let h_usize = h as usize;
    let grid_size = w_usize * h_usize;

    // Flat 1D arrays for cache efficiency — contiguous memory, no pointer chasing
    let mut vision_grid = vec![false; grid_size];
    let mut influence_grid = vec![0u32; grid_size];

    // Precomputed circle spans for influence radius
    let i_radius = UNIT_INFLUENCE_RADIUS as i32;

    // Combined unit loop — vision + influence in one pass
    for (ux, uy) in &input.unit_positions {
        // Terrain modifier for vision (hills extend range)
        let terrain_mod = match input
            .terrain
            .get(*ux as usize)
            .and_then(|r| r.get(*uy as usize))
        {
            Some(Terrain::Hills) => 2.0,
            _ => 1.0,
        };

        let v_radius = (crate::game::constants::VISION_RADIUS * terrain_mod) as i32;
        let _max_radius = if v_radius > i_radius {
            v_radius
        } else {
            i_radius
        };

        // Circle projection with precomputed spans
        // Vision: Chebyshev distance (square), influence: Manhattan-ish (diamond via circle spans)
        let ux_i = *ux as i32;
        let uy_i = *uy as i32;

        // Vision (square): iterate bounding box, Chebyshev check
        for dy in -v_radius..=v_radius {
            for dx in -v_radius..=v_radius {
                let x = ux_i + dx;
                let y = uy_i + dy;
                if x >= 0 && y >= 0 && x < w && y < h {
                    vision_grid[(y as usize) * w_usize + (x as usize)] = true;
                }
            }
        }

        // Influence (circle): use precomputed spans to skip corners
        for &(dy, dx_start, dx_end) in circle_spans().get(i_radius as usize).unwrap() {
            let x_base = ux_i;
            let y = (uy_i + dy) as usize;
            for dx in dx_start..=dx_end {
                let x = (x_base + dx) as usize;
                if x < w_usize && y < h_usize {
                    influence_grid[y * w_usize + x] += 1;
                }
            }
        }
    }

    // Combined city loop — vision + influence in one pass
    let city_v_radius = (crate::game::constants::VISION_RADIUS * CITY_RADIUS_MULTIPLIER) as i32;
    let city_i_radius = ((UNIT_INFLUENCE_RADIUS * CITY_RADIUS_MULTIPLIER) as i32).min(MAX_RADIUS);

    for (cx, cy) in &input.city_positions {
        let cx_i = *cx as i32;
        let cy_i = *cy as i32;

        // Vision (square)
        for dy in -city_v_radius..=city_v_radius {
            for dx in -city_v_radius..=city_v_radius {
                let x = cx_i + dx;
                let y = cy_i + dy;
                if x >= 0 && y >= 0 && x < w && y < h {
                    vision_grid[(y as usize) * w_usize + (x as usize)] = true;
                }
            }
        }

        // Influence (circle) — radius clamped to MAX_RADIUS for safety
        if let Some(spans) = circle_spans().get(city_i_radius as usize) {
            for &(dy, dx_start, dx_end) in spans {
                let y = (cy_i + dy) as usize;
                for dx in dx_start..=dx_end {
                    let x = (cx_i + dx) as usize;
                    if x < w_usize && y < h_usize {
                        influence_grid[y * w_usize + x] += 5;
                    }
                }
            }
        }
    }

    PlayerOutput {
        player_id: input.player_id.to_string(),
        vision_grid,
        influence_grid,
    }
}

pub fn run(world: &mut World, ctx: &TickContext, _tick: u64) {
    // Scoped Arc extraction for terrain grid (24 bytes pointer bump, not 10KB clone)
    let terrain_grid: Arc<Vec<Vec<Terrain>>> = match world.world_data(ctx.world_entity) {
        Some(wb) => Arc::clone(&wb.terrain.grid),
        None => return,
    };
    // wb dropped — borrow released

    let w = ctx.width as i32;
    let h = ctx.height as i32;

    // Phase 1: Prepare per-player input data (sequential, main thread)
    // This extracts only the data each player needs — positions and cities.
    let player_inputs: Vec<PlayerInput<'_>> = ctx
        .units_by_player
        .iter()
        .map(|(player_id, entities)| {
            let unit_positions: Vec<(f32, f32)> = entities.iter().map(|(_, pos)| *pos).collect();
            let city_positions: Vec<(f32, f32)> = ctx
                .cities
                .iter()
                .filter(|c| c.owner.as_deref() == Some(player_id.as_str()))
                .map(|c| c.position)
                .collect();

            PlayerInput {
                player_id: player_id.as_str(),
                unit_positions,
                city_positions,
                terrain: &terrain_grid,
                width: w,
                height: h,
            }
        })
        .collect();

    // Phase 2: Compute grids per player (sequential)
    // Rayon was tried at multiple granularities (per-player, per-row) but all
    // showed regressions due to dispatch overhead on 32-core systems with small tasks.
    // The influence computation is ~0.1ms per player — below Rayon's breakeven point.
    let results: Vec<PlayerOutput> = player_inputs
        .iter()
        .map(|input| compute_player_influence(input))
        .collect();

    // Phase 3: Collect results into HashMaps (sequential, main thread)
    let updated_vision: HashMap<String, Vec<bool>> = results
        .iter()
        .map(|r| (r.player_id.clone(), r.vision_grid.clone()))
        .collect();

    let updated_border: HashMap<String, Vec<u32>> = results
        .into_iter()
        .map(|r| (r.player_id, r.influence_grid))
        .collect();

    // Phase 4: Write to world (sequential, main thread)
    if let Some(wb) = world.world_data_mut(ctx.world_entity) {
        wb.vision.grids = Arc::new(updated_vision);
        wb.borders.grids = Arc::new(updated_border);
    }
}
