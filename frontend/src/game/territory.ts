//! Client-side territory fill for display purposes.
//!
//! The server sends a border grid where most cells are 255 (unclaimed) and only
//! cells within a small influence radius of a city/unit carry a player index.
//! For visual purposes, we fill the gaps via multi-source BFS so that borders
//! appear as contiguous territory regions rather than disconnected blobs.

export const TERRITORY_NONE = 255;

export interface CityAnchor {
  x: number; // grid column
  y: number; // grid row
  ownerIdx: number;
}

/**
 * Fill a border grid with territory ownership using multi-source BFS.
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

  // Copy the grid
  const grid: number[][] = borderGrid.map((row) => [...row]);

  // Seed the BFS queue with all known-ownership cells and city anchors
  const queue: Array<[number, number]> = [];

  for (let y = 0; y < rows; y++) {
    for (let x = 0; x < cols; x++) {
      if (grid[y][x] !== TERRITORY_NONE) {
        queue.push([y, x]);
      }
    }
  }

  // Place city anchors — a city claims its own cell even if the grid says 255
  for (const c of cities) {
    if (c.y >= 0 && c.y < rows && c.x >= 0 && c.x < cols) {
      if (grid[c.y][c.x] === TERRITORY_NONE) {
        grid[c.y][c.x] = c.ownerIdx;
      }
      queue.push([c.y, c.x]);
    }
  }

  // BFS expansion
  const head = { x: 0 };
  while (head.x < queue.length) {
    const [cy, cx] = queue[head.x];
    head.x++;
    const owner = grid[cy][cx];

    const neighbors: Array<[number, number]> = [
      [cy - 1, cx],
      [cy + 1, cx],
      [cy, cx - 1],
      [cy, cx + 1],
    ];

    for (const [ny, nx] of neighbors) {
      if (ny < 0 || ny >= rows || nx < 0 || nx >= cols) continue;
      if (grid[ny][nx] !== TERRITORY_NONE) continue;
      // Don't expand into water
      if (terrain[ny] && terrain[ny][nx] === 'Water') continue;
      grid[ny][nx] = owner;
      queue.push([ny, nx]);
    }
  }

  return grid;
}
