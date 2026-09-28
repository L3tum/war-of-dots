// Pure geometry helpers shared by the renderer (and its tests).

import { TERRITORY_NONE } from './territory';

/**
 * A single player-vs-player border edge on the display grid.
 *
 * `dir` follows the renderer's convention:
 *  - `'h'` — edge to the LOWER neighbour (rows y / y+1), drawn as a horizontal line.
 *  - `'v'` — edge to the RIGHT neighbour (cols x / x+1), drawn as a vertical line.
 */
export interface BorderEdge {
  gridX: number;
  gridY: number;
  dir: 'h' | 'v';
  ownerA: number;
  ownerB: number;
}

/**
 * Detect player-vs-player border edges on a territory display grid (the output
 * of `fillTerritory`). Out-of-bounds neighbours count as `TERRITORY_NONE`, and
 * borders are only ever drawn between two different players — a claimed cell
 * adjacent to unclaimed land (255) produces no edge.
 *
 * Pure: operates on the grid passed in, returns a fresh edge list.
 */
export function detectBorderEdges(displayGrid: number[][]): BorderEdge[] {
  const edges: BorderEdge[] = [];
  if (displayGrid.length === 0) return edges;

  for (let y = 0; y < displayGrid.length; y++) {
    for (let x = 0; x < displayGrid[y].length; x++) {
      const p1 = displayGrid[y][x];
      const p2 = x + 1 < displayGrid[y].length ? displayGrid[y][x + 1] : TERRITORY_NONE;
      const p3 = y + 1 < displayGrid.length ? displayGrid[y + 1][x] : TERRITORY_NONE;
      // Draw borders between two different players.
      // 'v' = right neighbour (vertical line), 'h' = lower neighbour (horizontal line)
      if (p1 !== p2 && p1 !== TERRITORY_NONE && p2 !== TERRITORY_NONE) {
        edges.push({ gridX: x, gridY: y, dir: 'v', ownerA: p1, ownerB: p2 });
      }
      if (p1 !== p3 && p1 !== TERRITORY_NONE && p3 !== TERRITORY_NONE) {
        edges.push({ gridX: x, gridY: y, dir: 'h', ownerA: p1, ownerB: p3 });
      }
    }
  }

  return edges;
}

/**
 * Reusable edge storage for `detectBorderEdgesFlat`. All five arrays must
 * share the same length ≥ 2*rows*cols (a cell contributes at most two edges:
 * to the right neighbour and to the lower neighbour).
 */
export interface EdgeBuffers {
  x: Int32Array;
  y: Int32Array;
  /** 0 = 'h' (edge to the LOWER neighbour), 1 = 'v' (edge to the RIGHT neighbour) */
  dir: Uint8Array;
  ownerA: Uint8Array;
  ownerB: Uint8Array;
}

/**
 * Allocation-free variant of `detectBorderEdges` for the renderer hot path
 * (Perf #3): scans a flat row-major display grid (the output of
 * `TerritoryFiller`) and writes edges into caller-provided buffers, in the
 * same order as the 2-D variant (row-major; per cell the 'v' edge is written
 * before the 'h' edge). Returns the edge count.
 */
export function detectBorderEdgesFlat(
  grid: Uint8Array,
  rows: number,
  cols: number,
  buf: EdgeBuffers,
): number {
  let n = 0;
  for (let y = 0; y < rows; y++) {
    for (let x = 0; x < cols; x++) {
      const i = y * cols + x;
      const p1 = grid[i];
      const p2 = x + 1 < cols ? grid[i + 1] : TERRITORY_NONE;
      const p3 = y + 1 < rows ? grid[i + cols] : TERRITORY_NONE;
      // Draw borders between two different players.
      // 'v' = right neighbour (vertical line), 'h' = lower neighbour (horizontal line)
      if (p1 !== p2 && p1 !== TERRITORY_NONE && p2 !== TERRITORY_NONE) {
        buf.x[n] = x;
        buf.y[n] = y;
        buf.dir[n] = 1;
        buf.ownerA[n] = p1;
        buf.ownerB[n] = p2;
        n++;
      }
      if (p1 !== p3 && p1 !== TERRITORY_NONE && p3 !== TERRITORY_NONE) {
        buf.x[n] = x;
        buf.y[n] = y;
        buf.dir[n] = 0;
        buf.ownerA[n] = p1;
        buf.ownerB[n] = p3;
        n++;
      }
    }
  }
  return n;
}
