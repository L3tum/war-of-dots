//! Client-side territory fill for display purposes.
//!
//! The server sends a border grid where most cells are 255 (unclaimed) and only
//! cells within a small influence radius of a city/unit carry a player index.
//! For visual purposes, we fill the gaps via multi-source BFS so that borders
//! appear as contiguous territory regions rather than disconnected blobs.
//!
//! Hot path (the renderer, Perf #3): `TerritoryFiller.fill()` — flat row-major
//! `Uint8Array`s with grow-only reusable scratch and zero per-cell allocation,
//! so the fill can run at update rate without GC pressure. `fillTerritory`
//! keeps the historical 2-D array-of-arrays signature for tests and one-off
//! callers (it delegates to the same flat core).

export const TERRITORY_NONE = 255;

export interface CityAnchor {
  x: number; // grid column
  y: number; // grid row
  ownerIdx: number;
}

/**
 * Multi-source BFS territory fill on flat row-major grids — the
 * allocation-free hot path used by the renderer.
 *
 * - Seeds: all cells already holding a player index (not TERRITORY_NONE),
 *   scanned in row-major order, plus each city anchor (in list order). An
 *   anchor on an already-seeded cell is enqueued a second time (a no-op on
 *   pop — kept for exact queue-order parity with the historical 2-D BFS).
 * - Expansion: 4-neighbor BFS (up, down, left, right — the historical
 *   neighbour order, which is part of the tie-breaking behaviour) into
 *   TERRITORY_NONE cells, but never into water-mask cells.
 *
 * Scratch is owned and reused: buffers grow to fit the largest grid seen and
 * are then stable (no steady-state allocation). The returned buffer is valid
 * until the next `fill()` call.
 */
export class TerritoryFiller {
  private out = new Uint8Array(0);
  private queue = new Uint32Array(0);

  fill(
    border: Uint8Array,
    waterMask: Uint8Array,
    rows: number,
    cols: number,
    anchorX: Int32Array,
    anchorY: Int32Array,
    anchorOwner: Uint8Array,
    anchorCount: number,
  ): Uint8Array {
    const cells = rows * cols;
    if (this.out.length < cells) this.out = new Uint8Array(cells);
    // Enqueue bound: every cell once as a seed + every anchor + every claim
    // (an unclaimed cell is claimed at most once), i.e. ≤ 2*cells + anchors.
    if (this.queue.length < cells * 2 + anchorCount) {
      this.queue = new Uint32Array(cells * 2 + anchorCount);
    }

    const grid = this.out;
    grid.set(border.subarray(0, cells), 0);
    const queue = this.queue;
    let head = 0;
    let tail = 0;

    // Seed the queue with all known-ownership cells (row-major)
    for (let i = 0; i < cells; i++) {
      if (grid[i] !== TERRITORY_NONE) queue[tail++] = i;
    }

    // Place city anchors — a city claims its own cell even if the grid says 255
    for (let c = 0; c < anchorCount; c++) {
      const x = anchorX[c];
      const y = anchorY[c];
      if (y >= 0 && y < rows && x >= 0 && x < cols) {
        const idx = y * cols + x;
        if (grid[idx] === TERRITORY_NONE) grid[idx] = anchorOwner[c];
        queue[tail++] = idx;
      }
    }

    if (tail > queue.length) {
      throw new Error(`territory fill queue overflow (anchors=${anchorCount})`);
    }

    // BFS expansion — unrolled 4-neighbour walk, no per-cell allocations
    while (head < tail) {
      const idx = queue[head++];
      const owner = grid[idx];
      const cy = (idx / cols) | 0;
      const cx = idx - cy * cols;

      if (cy > 0) {
        const j = idx - cols;
        if (grid[j] === TERRITORY_NONE && waterMask[j] === 0) {
          grid[j] = owner;
          queue[tail++] = j;
        }
      }
      if (cy + 1 < rows) {
        const j = idx + cols;
        if (grid[j] === TERRITORY_NONE && waterMask[j] === 0) {
          grid[j] = owner;
          queue[tail++] = j;
        }
      }
      if (cx > 0) {
        const j = idx - 1;
        if (grid[j] === TERRITORY_NONE && waterMask[j] === 0) {
          grid[j] = owner;
          queue[tail++] = j;
        }
      }
      if (cx + 1 < cols) {
        const j = idx + 1;
        if (grid[j] === TERRITORY_NONE && waterMask[j] === 0) {
          grid[j] = owner;
          queue[tail++] = j;
        }
      }
    }

    return grid;
  }
}

/**
 * Fill a border grid with territory ownership using multi-source BFS.
 *
 * Historical 2-D API (tests / one-off callers); the renderer drives the flat
 * `TerritoryFiller` directly on reused typed arrays.
 *
 * - Seeds: all cells already holding a player index (not TERRITORY_NONE),
 *   plus each city anchor cell (set to its owner index).
 * - Expansion: 4-neighbor BFS into TERRITORY_NONE cells, but never into
 *   cells whose terrain is 'Water'.
 * - Existing player-indexed cells take priority (they are the seeds).
 *
 * Returns a new grid; the input is not mutated.
 */
export function fillTerritory(
  borderGrid: number[][],
  terrain: string[][],
  cities: CityAnchor[],
): number[][] {
  if (borderGrid.length === 0) return borderGrid;

  const rows = borderGrid.length;
  const cols = borderGrid[0].length;
  const cells = rows * cols;

  const border = new Uint8Array(cells);
  const waterMask = new Uint8Array(cells);
  for (let y = 0; y < rows; y++) {
    const row = borderGrid[y];
    const tRow = terrain[y];
    for (let x = 0; x < cols; x++) {
      const i = y * cols + x;
      // Ragged rows can't occur for server data (uniform grids); treat a
      // missing cell as unclaimed rather than seeding it with `undefined`.
      border[i] = row[x] ?? TERRITORY_NONE;
      waterMask[i] = tRow && tRow[x] === 'Water' ? 1 : 0;
    }
  }

  const anchorX = new Int32Array(cities.length);
  const anchorY = new Int32Array(cities.length);
  const anchorOwner = new Uint8Array(cities.length);
  for (let c = 0; c < cities.length; c++) {
    anchorX[c] = cities[c].x;
    anchorY[c] = cities[c].y;
    anchorOwner[c] = cities[c].ownerIdx;
  }

  const out = new TerritoryFiller().fill(
    border,
    waterMask,
    rows,
    cols,
    anchorX,
    anchorY,
    anchorOwner,
    cities.length,
  );

  return Array.from({ length: rows }, (_, y) =>
    Array.from(out.subarray(y * cols, y * cols + cols)),
  );
}
