//! Pure geometry helpers shared by the renderer (and its tests).

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
