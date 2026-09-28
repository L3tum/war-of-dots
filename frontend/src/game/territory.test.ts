// Differential tests for the flat territory pipeline (Perf #3).
//
// The reference below is a verbatim copy of the pre-Perf-#3 2-D multi-source
// BFS (array-of-arrays grid, per-cell tuple queue). The flat core
// (`TerritoryFiller`) and the 2-D `fillTerritory` wrapper must both
// reproduce its output bit-for-bit — these tests lock that in on
// randomized maps, including production-scale (200×200) inputs.

import { describe, it, expect } from 'vitest';
import { fillTerritory, TerritoryFiller, TERRITORY_NONE, type CityAnchor } from './territory';

/** Verbatim copy of the original 2-D multi-source BFS (test reference only). */
function fillTerritoryReference(
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

/** Deterministic PRNG (mulberry32) so differential failures are reproducible. */
function mulberry32(seed: number): () => number {
  let a = seed >>> 0;
  return () => {
    a = (a + 0x6d2b79f5) >>> 0;
    let t = a;
    t = Math.imul(t ^ (t >>> 15), t | 1);
    t ^= t + Math.imul(t ^ (t >>> 7), t | 61);
    return ((t ^ (t >>> 14)) >>> 0) / 4294967296;
  };
}

interface RandomCase {
  rows: number;
  cols: number;
  border: number[][];
  terrain: string[][];
  cities: CityAnchor[];
}

/** Random map: 55% unclaimed, else owner 0..2; ~20% water; 0..3 city anchors
 * (coordinates may land out of bounds — the pipeline must ignore them). */
function randomCase(rng: () => number, rows: number, cols: number): RandomCase {
  const border: number[][] = [];
  const terrain: string[][] = [];
  for (let y = 0; y < rows; y++) {
    const row: number[] = [];
    const tRow: string[] = [];
    for (let x = 0; x < cols; x++) {
      const r = rng();
      row.push(r < 0.55 ? TERRITORY_NONE : Math.floor(rng() * 3));
      const t = rng();
      tRow.push(t < 0.65 ? 'Plains' : t < 0.85 ? 'Water' : 'Hills');
    }
    border.push(row);
    terrain.push(tRow);
  }
  const cities: CityAnchor[] = [];
  const nCities = Math.floor(rng() * 4);
  for (let c = 0; c < nCities; c++) {
    const x = Math.floor(rng() * (cols + 2)) - 1;
    const y = Math.floor(rng() * (rows + 2)) - 1;
    cities.push({ x, y, ownerIdx: Math.floor(rng() * 3) });
  }
  return { rows, cols, border, terrain, cities };
}

/** Run the flat core on a case and convert the output back to 2-D for comparison. */
function flatFill(c: RandomCase): number[][] {
  const { rows, cols, border, terrain, cities } = c;
  const flat = new Uint8Array(rows * cols);
  const water = new Uint8Array(rows * cols);
  for (let y = 0; y < rows; y++) {
    for (let x = 0; x < cols; x++) {
      const i = y * cols + x;
      flat[i] = border[y][x];
      water[i] = terrain[y][x] === 'Water' ? 1 : 0;
    }
  }
  const ax = new Int32Array(cities.length);
  const ay = new Int32Array(cities.length);
  const ao = new Uint8Array(cities.length);
  for (let i = 0; i < cities.length; i++) {
    ax[i] = cities[i].x;
    ay[i] = cities[i].y;
    ao[i] = cities[i].ownerIdx;
  }
  const out = new TerritoryFiller().fill(flat, water, rows, cols, ax, ay, ao, cities.length);
  return Array.from({ length: rows }, (_, y) =>
    Array.from(out.subarray(y * cols, y * cols + cols)),
  );
}

const plains = (rows: number, cols: number): string[][] =>
  Array.from({ length: rows }, () => Array<string>(cols).fill('Plains'));

describe('TerritoryFiller (flat core) — differential vs the original 2-D BFS', () => {
  it('matches on 300 randomized maps (seeds, water, anchors, out-of-bounds anchors)', () => {
    const rng = mulberry32(0x5eed);
    for (let iter = 0; iter < 300; iter++) {
      const rows = 1 + Math.floor(rng() * 24);
      const cols = 1 + Math.floor(rng() * 24);
      const c = randomCase(rng, rows, cols);
      const expected = fillTerritoryReference(c.border, c.terrain, c.cities);
      expect(flatFill(c), `mismatch on map ${iter} (${rows}x${cols})`).toEqual(expected);
    }
  });

  it('matches the public 2-D wrapper (same core, conversion round-trip)', () => {
    const rng = mulberry32(0xabc1);
    for (let iter = 0; iter < 50; iter++) {
      const rows = 1 + Math.floor(rng() * 16);
      const cols = 1 + Math.floor(rng() * 16);
      const c = randomCase(rng, rows, cols);
      expect(fillTerritory(c.border, c.terrain, c.cities)).toEqual(
        fillTerritoryReference(c.border, c.terrain, c.cities),
      );
    }
  });

  it('matches at production scale (200×200 = 40,000 cells)', () => {
    const c = randomCase(mulberry32(0xbeef), 200, 200);
    expect(flatFill(c)).toEqual(fillTerritoryReference(c.border, c.terrain, c.cities));
  });
});

describe('fillTerritory — behaviour edge cases (locked to the original semantics)', () => {
  it('returns an empty grid as-is', () => {
    const empty: number[][] = [];
    expect(fillTerritory(empty, plains(4, 4), [])).toBe(empty);
  });

  it('leaves an all-unclaimed grid unclaimed when there are no anchors', () => {
    const grid = Array.from({ length: 3 }, () => Array<number>(3).fill(TERRITORY_NONE));
    expect(fillTerritory(grid, plains(3, 3), [])).toEqual(
      Array.from({ length: 3 }, () => Array<number>(3).fill(TERRITORY_NONE)),
    );
  });

  it('the row-major earlier seed wins an equidistant contested cell (tie-break)', () => {
    // 0 ? 1 → the ? cell is distance 1 from both seeds; the left seed is
    // enqueued first, so its wave claims the cell first.
    expect(fillTerritory([[0, TERRITORY_NONE, 1]], plains(1, 3), [])).toEqual([[0, 0, 1]]);
    const c: RandomCase = {
      rows: 1,
      cols: 3,
      border: [[0, TERRITORY_NONE, 1]],
      terrain: plains(1, 3),
      cities: [],
    };
    expect(flatFill(c)).toEqual([[0, 0, 1]]);
  });

  it('a city anchor on an already-seeded cell does not change the owner', () => {
    const grid = [[0]];
    expect(fillTerritory(grid, plains(1, 1), [{ x: 0, y: 0, ownerIdx: 1 }])).toEqual([[0]]);
  });

  it('out-of-bounds city anchors are ignored', () => {
    const grid = Array.from({ length: 2 }, () => Array<number>(2).fill(TERRITORY_NONE));
    const cities = [
      { x: -1, y: 0, ownerIdx: 0 },
      { x: 2, y: 0, ownerIdx: 1 },
      { x: 0, y: 2, ownerIdx: 2 },
    ];
    expect(fillTerritory(grid, plains(2, 2), cities)).toEqual(
      Array.from({ length: 2 }, () => Array<number>(2).fill(TERRITORY_NONE)),
    );
  });

  it('never expands into water', () => {
    const terrain = [
      ['Plains', 'Water'],
      ['Plains', 'Water'],
    ];
    const grid = [
      [0, TERRITORY_NONE],
      [TERRITORY_NONE, TERRITORY_NONE],
    ];
    expect(fillTerritory(grid, terrain, [])).toEqual([
      [0, TERRITORY_NONE],
      [0, TERRITORY_NONE],
    ]);
  });

  it('does not mutate the input grid', () => {
    const grid = [
      [0, TERRITORY_NONE],
      [TERRITORY_NONE, 1],
    ];
    const before = grid.map((row) => [...row]);
    fillTerritory(grid, plains(2, 2), []);
    expect(grid).toEqual(before);
  });
});
