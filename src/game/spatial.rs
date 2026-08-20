//! Shared spatial grid — a reusable, allocation-free neighbour index.
//!
//! A flat-buffer 1D grid (prefix-sum offsets + sizes, no per-cell Vecs) mapping
//! unit positions to coarse cells. **Built/updated in place by `movement`** and
//! consumed downstream (`nearby_enemies`, `combat` via the `NearbyEnemies` lists
//! movement's grid feeds). The dense buffers (`entries`/`cell_offsets`/
//! `cell_sizes`) are resized only on growth and overwritten in place each build
//! — **no `::clear()` of the grid structure, no steady-state heap allocation**.
//!
//! The cell side (`SPATIAL_CELL_SIZE`) is a *locality tuning knob*, decoupled
//! from the combat/occupancy rules: it is set to the combat distance, so a 3×3
//! cell scan is a **complete** neighbourhood for any query radius up to
//! `COMBAT_DISTANCE` (see `constants` const-safety tests).

use crate::game::components::UnitPosition;
use crate::game::constants::SPATIAL_CELL_SIZE;
use hecs::Entity;

/// Data stored per entity in a grid cell.
/// `owner_idx` is the compact owner ID from `TickContext` (u8) — enables O(1)
/// owner comparison with a single-byte integer equality in the consumers.
#[derive(Clone, Copy)]
pub struct CellEntry {
    entity: Entity,
    pos: UnitPosition,
    owner_idx: u8,
}

/// Reused flat-buffer spatial grid buffers, owned by `Game` and passed by `&mut`
/// into `GridIndex::build`. `entries`/`cell_offsets`/`cell_sizes` are resized
/// only on growth, so a steady-state build does no heap allocation for the grid
/// (the per-cell buffers are overwritten in place). `units` is the per-build
/// input record list (its capacity is preserved across builds).
#[derive(Default)]
pub struct GridIndexScratch {
    pub entries: Vec<CellEntry>,
    pub cell_offsets: Vec<usize>,
    pub cell_sizes: Vec<usize>,
    /// All-unit records for this build: `(entity, position, owner_idx)`.
    /// Cleared and repopulated by the builder; the dense buffers above are the
    /// reusable state that is deliberately *not* cleared.
    pub units: Vec<(Entity, UnitPosition, u8)>,
    /// The most recently built index metadata (bounds + dimensions). Written by
    /// the builder (`movement`'s post-move build) and read by consumers
    /// (`nearby_enemies`) so the grid is built once and reused, not rebuilt.
    pub index: GridIndex,
}

/// Coarse spatial grid metadata: the cell bounds + dimensions. The dense
/// buffers (entries/offsets/sizes) live in `GridIndexScratch` and are read
/// directly by the caller after `build`.
#[derive(Clone, Copy, Default)]
pub struct GridIndex {
    min_x: i32,
    min_y: i32,
    width: usize,
    height: usize,
}

impl GridIndex {
    /// Build the grid from a slice of unit records, filling the reusable dense
    /// buffers passed by mutable reference. Returns the built index.
    ///
    /// `units` is borrowed disjointly from the dense buffers, so the caller can
    /// hold it (e.g. as `scratch.units`) while `build` mutates
    /// entries/offsets/sizes.
    ///
    /// The dense buffers are reused in place: `cell_offsets`/`cell_sizes` grow
    /// only on expansion (a no-op at steady state) and are zeroed over their
    /// live range, and `entries` is resized without a per-slot default write
    /// since every slot is overwritten in pass 2.
    pub fn build(
        units: &[(Entity, UnitPosition, u8)],
        entries: &mut Vec<CellEntry>,
        cell_offsets: &mut Vec<usize>,
        cell_sizes: &mut Vec<usize>,
    ) -> Self {
        let mut min_x = i32::MAX;
        let mut min_y = i32::MAX;
        let mut max_x = i32::MIN;
        let mut max_y = i32::MIN;
        for &(_, pos, _) in units {
            let c = cell_for(pos);
            if c.0 < min_x {
                min_x = c.0;
            }
            if c.1 < min_y {
                min_y = c.1;
            }
            if c.0 > max_x {
                max_x = c.0;
            }
            if c.1 > max_y {
                max_y = c.1;
            }
        }
        // No units: return a degenerate (empty) index. Callers guard `n == 0`.
        if min_x > max_x {
            return Self {
                min_x: 0,
                min_y: 0,
                width: 0,
                height: 0,
            };
        }
        let width = (max_x - min_x + 2) as usize;
        let height = (max_y - min_y + 2) as usize;
        let total = width * height;

        // Grow the per-cell buffers to at least `total` cells (no-op when
        // enough) — inlined so `build` can borrow the fields disjointly from
        // `units`.
        if cell_offsets.len() < total {
            cell_offsets.resize(total, 0);
        }
        if cell_sizes.len() < total {
            cell_sizes.resize(total, 0);
        }
        let cell_sizes = &mut cell_sizes[..total];
        let cell_offsets = &mut cell_offsets[..total];

        // Pass 1: count entries per cell (zeroed first for reuse).
        cell_sizes.fill(0);
        for &(_, pos, _) in units {
            let c = cell_for(pos);
            let cx = ((c.0 - min_x) as usize).min(width - 1);
            let cy = ((c.1 - min_y) as usize).min(height - 1);
            cell_sizes[cy * width + cx] += 1;
        }

        // Build prefix-sum offsets.
        for i in 1..total {
            cell_offsets[i] = cell_offsets[i - 1] + cell_sizes[i - 1];
        }

        // Pass 2: fill entries in place. `cell_sizes` doubles as a per-cell
        // cursor (reset to 0); each write advances it, so after the pass it
        // again holds the final per-cell counts — no `cell_offsets.clone()`.
        entries.resize(
            units.len(),
            CellEntry {
                entity: Entity::from_bits(1u64 << 32).unwrap(),
                pos: UnitPosition::default(),
                owner_idx: 0,
            },
        );
        cell_sizes.fill(0);
        for &(e, pos, owner_id) in units.iter() {
            let c = cell_for(pos);
            let cx = ((c.0 - min_x) as usize).min(width - 1);
            let cy = ((c.1 - min_y) as usize).min(height - 1);
            let idx = cy * width + cx;
            let slot = cell_offsets[idx] + cell_sizes[idx];
            entries[slot] = CellEntry {
                entity: e,
                pos,
                owner_idx: owner_id,
            };
            cell_sizes[idx] += 1;
        }

        Self {
            min_x,
            min_y,
            width,
            height,
        }
    }

    /// Map a (cx, cy) cell to a dense-buffer index, or `None` if out of bounds.
    #[inline]
    fn cell_idx(&self, cx: i32, cy: i32) -> Option<usize> {
        let x = (cx - self.min_x) as isize;
        let y = (cy - self.min_y) as isize;
        let w = self.width as isize;
        let h = self.height as isize;
        if (0..w).contains(&x) && (0..h).contains(&y) {
            Some((y * w + x) as usize)
        } else {
            None
        }
    }
}

/// Coarse cell containing `pos`: `(floor(x/S), floor(y/S))`.
#[inline]
fn cell_for(pos: UnitPosition) -> (i32, i32) {
    (
        (pos.x / SPATIAL_CELL_SIZE).floor() as i32,
        (pos.y / SPATIAL_CELL_SIZE).floor() as i32,
    )
}

/// For each unit within `radius_sq` of `pos` in a built grid, call
/// `f(entity, pos, owner_idx)`. A 3×3 cell scan plus a squared-distance filter
/// (FMA/SIMD-friendly, branch-free inner test). Completeness holds for
/// `radius_sq` up to `SPATIAL_CELL_SIZE²` (see `constants` tests).
pub fn for_each_within(
    grid: &GridIndex,
    scratch: &GridIndexScratch,
    pos: UnitPosition,
    radius_sq: f32,
    mut f: impl FnMut(Entity, UnitPosition, u8),
) {
    if grid.width == 0 || grid.height == 0 {
        return;
    }
    let c = cell_for(pos);
    for dx in -1..=1i32 {
        for dy in -1..=1i32 {
            if let Some(idx) = grid.cell_idx(c.0 + dx, c.1 + dy) {
                let start = scratch.cell_offsets[idx];
                let count = scratch.cell_sizes[idx];
                for entry in &scratch.entries[start..start + count] {
                    let ddx = pos.x - entry.pos.x;
                    let ddy = pos.y - entry.pos.y;
                    if ddx * ddx + ddy * ddy < radius_sq {
                        f(entry.entity, entry.pos, entry.owner_idx);
                    }
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Build a grid from a small hand-made set of records into fresh scratch.
    fn build(units: &[(Entity, UnitPosition, u8)]) -> (GridIndex, GridIndexScratch) {
        let mut scratch = GridIndexScratch {
            units: units.to_vec(),
            ..Default::default()
        };
        let grid = GridIndex::build(
            &scratch.units,
            &mut scratch.entries,
            &mut scratch.cell_offsets,
            &mut scratch.cell_sizes,
        );
        (grid, scratch)
    }

    /// Spawn `n` empty entities in a throwaway world and return their handles.
    /// Real handles (valid index+generation) — avoids the fragility of crafting
    /// `Entity` bit patterns by hand.
    fn entities(n: usize) -> Vec<Entity> {
        let mut world = hecs::World::new();
        (0..n).map(|_| world.spawn(())).collect()
    }
    fn p(x: f32, y: f32) -> UnitPosition {
        UnitPosition { x, y }
    }

    #[test]
    fn finds_neighbours_within_radius() {
        let ents = entities(3);
        let (a, b, far) = (ents[0], ents[1], ents[2]);
        let units = vec![
            (a, p(0.0, 0.0), 0),
            (b, p(9.0, 0.0), 1),     // within 10 of (0,0)
            (far, p(50.0, 50.0), 1), // far away
        ];
        let (grid, scratch) = build(&units);

        let mut found: Vec<Entity> = Vec::new();
        for_each_within(&grid, &scratch, p(0.0, 0.0), 10.0 * 10.0, |ent, _, _| {
            found.push(ent);
        });
        // Includes self (a) + b; not far.
        assert!(found.contains(&a), "self is within its own radius");
        assert!(found.contains(&b), "b at dist 9 < 10 must be found");
        assert!(!found.contains(&far), "far unit must not be found");
    }

    #[test]
    fn excludes_units_beyond_radius() {
        let ents = entities(2);
        let (a, b) = (ents[0], ents[1]);
        let units = vec![(a, p(0.0, 0.0), 0), (b, p(11.0, 0.0), 1)]; // 11 > 10
        let (grid, scratch) = build(&units);

        let mut found: Vec<Entity> = Vec::new();
        for_each_within(&grid, &scratch, p(0.0, 0.0), 100.0, |ent, _, _| {
            found.push(ent)
        });
        assert!(found.contains(&a));
        assert!(!found.contains(&b), "b at dist 11 > 10 must be excluded");
    }

    /// A unit at distance just under `COMBAT_DISTANCE` (10) but in an
    /// *adjacent* cell must still be found — this is the 3×3 completeness
    /// guarantee at the `SPATIAL_CELL_SIZE == COMBAT_DISTANCE == 10` boundary.
    #[test]
    fn three_by_three_is_complete_at_cell_boundary() {
        // a sits right on a cell edge (x=10.0 is the boundary between cell 0
        // and cell 1). b sits in the neighbouring cell at dist < 10.
        let ents = entities(2);
        let (a, b) = (ents[0], ents[1]);
        let units = vec![(a, p(10.0, 0.0), 0), (b, p(19.9, 0.0), 1)]; // dist 9.9
        let (grid, scratch) = build(&units);

        let mut found: Vec<Entity> = Vec::new();
        for_each_within(&grid, &scratch, p(10.0, 0.0), 100.0, |ent, _, _| {
            found.push(ent)
        });
        assert!(
            found.contains(&b),
            "adjacent-cell candidate at dist<10 must be found"
        );

        // Symmetric: from b's side.
        let mut found2: Vec<Entity> = Vec::new();
        for_each_within(&grid, &scratch, p(19.9, 0.0), 100.0, |ent, _, _| {
            found2.push(ent)
        });
        assert!(found2.contains(&a), "reverse lookup must also be complete");
    }

    #[test]
    fn empty_grid_is_safe() {
        let empty: Vec<(Entity, UnitPosition, u8)> = Vec::new();
        let (grid, scratch) = build(&empty);
        let mut count = 0;
        for_each_within(&grid, &scratch, p(0.0, 0.0), 100.0, |_, _, _| count += 1);
        assert_eq!(count, 0);
    }

    /// Steady-state rebuild overwrites the dense buffers in place: rebuilding
    /// a *smaller* population into an already-grown scratch must still report
    /// only the current units (no stale entries leaking from a prior build).
    #[test]
    fn rebuild_overwrites_stale_entries_in_place() {
        let mut scratch = GridIndexScratch::default();
        // Big population first.
        let big_handles = entities(50);
        let big: Vec<(Entity, UnitPosition, u8)> = big_handles
            .iter()
            .enumerate()
            .map(|(i, &e)| (e, p(i as f32 * 3.0, 0.0), 0))
            .collect();
        scratch.units = big.clone();
        let _g1 = GridIndex::build(
            &scratch.units,
            &mut scratch.entries,
            &mut scratch.cell_offsets,
            &mut scratch.cell_sizes,
        );
        // Now a single unit far away (a fresh handle, distinct from the big set).
        let one_ent = entities(1)[0];
        let one = vec![(one_ent, p(1000.0, 1000.0), 0)];
        scratch.units = one.clone();
        let g2 = GridIndex::build(
            &scratch.units,
            &mut scratch.entries,
            &mut scratch.cell_offsets,
            &mut scratch.cell_sizes,
        );
        let mut count = 0;
        for_each_within(&g2, &scratch, p(1000.0, 1000.0), 100.0, |_, _, _| {
            count += 1
        });
        assert_eq!(count, 1, "stale big-population entries must not leak");
    }
}
