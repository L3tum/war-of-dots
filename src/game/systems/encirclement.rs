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
use crate::game::world_ext::WorldExt;
use hecs::World;
use std::collections::HashMap;
use std::sync::Arc;

/// No dominant player sentinel (used in dominant_player grid).
const NONE: u8 = 255;

pub fn run(world: &mut World, ctx: &TickContext, _tick: u64) {
    // Scoped Arc extraction for border grids (24 bytes pointer bump)
    let border_grids: Arc<HashMap<String, Vec<u32>>> = match world.world_data(ctx.world_entity) {
        Some(wb) => Arc::clone(&wb.borders.grids),
        None => return,
    };
    // wb dropped — borrow released

    let (w, h) = (ctx.width as i32, ctx.height as i32);
    let wusize = w as usize;
    let grid_size = wusize * (h as usize);

    // Map player IDs to indices 0..N for dominant player grid.
    // Sort for deterministic index assignment + tie-breaking (the underlying
    // units_by_player HashMap has process-random iteration order).
    let mut players: Vec<&String> = ctx.units_by_player.keys().collect();
    players.sort();
    let nplayers = players.len();
    let player_to_idx: HashMap<&str, u8> = players
        .iter()
        .enumerate()
        .map(|(i, p)| (p.as_str(), i as u8))
        .collect();

    // Single-pass dominant player grid: which player dominates each cell?
    let dominant = build_dominant_player_grid(&border_grids, &player_to_idx, nplayers, grid_size);

    // Collect all cities by player index for seeding
    let mut cities_by_player: Vec<Vec<(i32, i32)>> = vec![Vec::new(); nplayers];
    for c in &ctx.cities {
        if let Some(owner) = &c.owner {
            if let Some(&idx) = player_to_idx.get(owner.as_str()) {
                cities_by_player[idx as usize].push((c.position.0 as i32, c.position.1 as i32));
            }
        }
    }

    // Combined multi-player flood-fill: one BFS pass for ALL players simultaneously.
    // safe[i] == NONE → unvisited, safe[i] == player_idx → claimed by that player.
    let mut safe = vec![NONE; grid_size];
    // Mark all edge cells as safe for ALL players (edges are universally safe)
    mark_safe_edges_multi(&mut safe, wusize, h as usize, &dominant);
    // Mark cells near cities as safe (each city's radius marks cells for that player)
    for (pidx, cities) in cities_by_player.iter().take(nplayers).enumerate() {
        mark_safe_near_cities_multi(&mut safe, cities, wusize, h as usize, pidx as u8);
    }

    // Single combined BFS: queue stores (x, y, player_idx)
    let mut queue_buf: Vec<(i32, i32, u8)> = Vec::with_capacity(grid_size);
    let mut qhead = 0;
    // Seed queue with all safe anchor cells that have a dominant player
    for idx in 0..grid_size {
        if safe[idx] != NONE && dominant[idx] != NONE && safe[idx] == dominant[idx] {
            let x = idx % wusize;
            let y = idx / wusize;
            queue_buf.push((x as i32, y as i32, dominant[idx]));
        }
    }
    // BFS outward — each cell propagates to neighbors dominated by the SAME player
    while qhead < queue_buf.len() {
        let (cx, cy, pidx) = queue_buf[qhead];
        qhead += 1;
        for (nx, ny) in [(cx + 1, cy), (cx - 1, cy), (cx, cy + 1), (cx, cy - 1)] {
            if nx >= 0 && ny >= 0 && nx < w && ny < h {
                let nidx = (ny as usize) * wusize + (nx as usize);
                if safe[nidx] == NONE && dominant[nidx] == pidx {
                    safe[nidx] = pidx;
                    queue_buf.push((nx, ny, pidx));
                }
            }
        }
    }

    // Now mark unit encirclement flags: dominated but not safe
    for player_id in &players {
        let player_idx = *player_to_idx.get(player_id.as_str()).unwrap();
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

/// Single-pass dominant player grid: for each cell, which player has the highest influence?
/// Returns Vec<u8> where value is player index (0..N) or NONE (255) if no influence.
fn build_dominant_player_grid(
    border_grids: &HashMap<String, Vec<u32>>,
    player_to_idx: &HashMap<&str, u8>,
    _nplayers: usize,
    grid_size: usize,
) -> Vec<u8> {
    let mut dominant = vec![NONE; grid_size];
    let mut max_inf = vec![0u32; grid_size];

    // Iterate players in sorted order so equal-influence ties break the same
    // way every run (strict `>` keeps the first player encountered).
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

    // Only keep cells where influence > 0
    for i in 0..grid_size {
        if max_inf[i] == 0 {
            dominant[i] = NONE;
        }
    }

    dominant
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
