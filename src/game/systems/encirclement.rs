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
use std::sync::Arc;

/// Reused scratch buffers for the encirclement flood-fill. `safe` is the
/// per-cell claim grid (NONE = unvisited, else the owning player idx); `queue`
/// is the BFS frontier storing cell indices (`usize`) instead of `(x, y, idx)`
/// tuples — 8 bytes/entry vs 16, better cache locality. Both grow only on map
/// resize and are reused each run (no per-run alloc).
pub struct EncirclementScratch {
    pub safe: Vec<u8>,
    pub queue: Vec<usize>,
}

impl EncirclementScratch {
    pub fn new() -> Self {
        Self {
            safe: Vec::new(),
            queue: Vec::new(),
        }
    }

    /// Grow `safe` to `grid_size` once (no-op when already sized). `queue`
    /// grows on demand and keeps its capacity for reuse.
    fn ensure(&mut self, grid_size: usize) {
        if self.safe.len() != grid_size {
            self.safe.resize(grid_size, NONE);
        }
    }
}

impl Default for EncirclementScratch {
    fn default() -> Self {
        Self::new()
    }
}

pub fn run(world: &mut World, ctx: &TickContext, _tick: u64, scratch: &mut EncirclementScratch) {
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

    // Combined multi-player flood-fill: one BFS pass for ALL players simultaneously.
    // safe[i] == NONE → unvisited, safe[i] == player_idx → claimed by that player.
    // Reused from scratch (grown only on map resize); reset for this run.
    scratch.ensure(grid_size);
    scratch.safe.fill(NONE);
    let safe = &mut scratch.safe;
    // Mark all edge cells as safe for ALL players (edges are universally safe)
    mark_safe_edges_multi(safe, wusize, h as usize, dominant);
    // Mark cells near cities as safe (each city's radius marks cells for that player)
    for (pidx, cities) in cities_by_player.iter().take(nplayers).enumerate() {
        mark_safe_near_cities_multi(safe, cities, wusize, h as usize, pidx as u8);
    }

    // Single combined BFS: queue stores cell indices (usize) — reused from
    // scratch and cleared each run.
    let queue_buf = &mut scratch.queue;
    queue_buf.clear();
    let mut qhead = 0;
    // Seed queue with all safe anchor cells that have a dominant player
    for idx in 0..grid_size {
        if safe[idx] != NONE && dominant[idx] != NONE && safe[idx] == dominant[idx] {
            queue_buf.push(idx);
        }
    }
    // BFS outward — each cell propagates to neighbors dominated by the SAME player
    while qhead < queue_buf.len() {
        let idx = queue_buf[qhead];
        qhead += 1;
        let pidx = safe[idx];
        let cx = (idx % wusize) as i32;
        let cy = (idx / wusize) as i32;
        for (nx, ny) in [(cx + 1, cy), (cx - 1, cy), (cx, cy + 1), (cx, cy - 1)] {
            if nx >= 0 && ny >= 0 && nx < w && ny < h {
                let nidx = (ny as usize) * wusize + (nx as usize);
                if safe[nidx] == NONE && dominant[nidx] == pidx {
                    safe[nidx] = pidx;
                    queue_buf.push(nidx);
                }
            }
        }
    }

    // Now mark unit encirclement flags: dominated but not safe
    for (player_idx, player_id) in players.iter().enumerate() {
        let player_idx = player_idx as u8;
        let entities = &ctx.units_by_player[*player_id];
        for handle in entities {
            let xi = handle.pos.0 as i32;
            let yi = handle.pos.1 as i32;
            let idx = (yi as usize) * wusize + (xi as usize);
            let encircled =
                idx < grid_size && dominant[idx] == player_idx && safe[idx] != player_idx;
            if let Ok((flags,)) = world.query_one_mut::<(&mut UnitFlags,)>(handle.entity) {
                flags.encircled = encircled;
            }
        }
    }
}

/// Mark all edge cells as safe for ALL players (edges are universally safe).
/// safe[i] is set to dominant[i] if the edge cell has a dominant player.
fn mark_safe_edges_multi(safe: &mut [u8], w: usize, h: usize, dominant: &[u8]) {
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

// Combined BFS is inline in run() — no separate function needed.
//
// The dominant-player (now territory) grid logic lives in influence.rs and is
// unit-tested there as `build_territory_grid_*`.
