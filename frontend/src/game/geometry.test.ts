// Differential tests for the flat border-edge scan (Perf #3): the renderer
// hot path (`detectBorderEdgesFlat`, flat row-major + struct-of-arrays
// buffers) must emit exactly the edges of the historical 2-D
// `detectBorderEdges`, in the same order.

import { describe, it, expect } from 'vitest';
import { detectBorderEdges, detectBorderEdgesFlat, type EdgeBuffers } from './geometry';
import { TERRITORY_NONE } from './territory';

/** Deterministic PRNG (mulberry32) so failures are reproducible. */
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

/** Run the flat variant and convert its output back to the 2-D edge shape. */
function flatEdges(grid: number[][], rows: number, cols: number) {
  const flat = new Uint8Array(rows * cols);
  for (let y = 0; y < rows; y++) {
    for (let x = 0; x < cols; x++) flat[y * cols + x] = grid[y][x];
  }
  const n2 = rows * cols * 2;
  const buf: EdgeBuffers = {
    x: new Int32Array(n2),
    y: new Int32Array(n2),
    dir: new Uint8Array(n2),
    ownerA: new Uint8Array(n2),
    ownerB: new Uint8Array(n2),
  };
  const n = detectBorderEdgesFlat(flat, rows, cols, buf);
  return Array.from({ length: n }, (_, i) => ({
    gridX: buf.x[i],
    gridY: buf.y[i],
    dir: (buf.dir[i] === 1 ? 'v' : 'h') as 'v' | 'h',
    ownerA: buf.ownerA[i],
    ownerB: buf.ownerB[i],
  }));
}

function randomGrid(rng: () => number, rows: number, cols: number): number[][] {
  const grid: number[][] = [];
  for (let y = 0; y < rows; y++) {
    const row: number[] = [];
    for (let x = 0; x < cols; x++) {
      const r = rng();
      row.push(r < 0.5 ? TERRITORY_NONE : Math.floor(rng() * 3));
    }
    grid.push(row);
  }
  return grid;
}

describe('detectBorderEdgesFlat — differential vs the 2-D variant', () => {
  it('matches on 300 randomized grids (owners 0..2, unclaimed, incl. 1×1)', () => {
    const rng = mulberry32(0xfeed);
    for (let iter = 0; iter < 300; iter++) {
      const rows = 1 + Math.floor(rng() * 16);
      const cols = 1 + Math.floor(rng() * 16);
      const grid = randomGrid(rng, rows, cols);
      expect(flatEdges(grid, rows, cols), `mismatch on map ${iter}`).toEqual(
        detectBorderEdges(grid),
      );
    }
  });

  it('matches at production scale (200×200 = 40,000 cells)', () => {
    const grid = randomGrid(mulberry32(0xcafe), 200, 200);
    const ref = detectBorderEdges(grid);
    const flat = flatEdges(grid, 200, 200);
    expect(flat.length).toBe(ref.length);
    expect(flat).toEqual(ref);
  });

  it('never emits an edge to the map boundary (out-of-bounds = unclaimed)', () => {
    const grid = [
      [TERRITORY_NONE, TERRITORY_NONE],
      [TERRITORY_NONE, 0],
    ];
    expect(detectBorderEdges(grid)).toEqual([]);
    expect(flatEdges(grid, 2, 2)).toEqual([]);
  });

  it('per-cell order: right-neighbour edge precedes lower-neighbour edge', () => {
    // Cell (0,0) owns 0 with a right neighbour (owner 1) and a lower
    // neighbour (owner 1) — both edges, 'v' first.
    const grid = [
      [0, 1, 2],
      [1, 1, 1],
    ];
    expect(flatEdges(grid, 2, 3)).toEqual(detectBorderEdges(grid));
    expect(detectBorderEdges(grid).slice(0, 2)).toEqual([
      { gridX: 0, gridY: 0, dir: 'v', ownerA: 0, ownerB: 1 },
      { gridX: 0, gridY: 0, dir: 'h', ownerA: 0, ownerB: 1 },
    ]);
  });

  it('no edge between unclaimed cells, no edge into unclaimed territory', () => {
    const grid = [
      [TERRITORY_NONE, TERRITORY_NONE, 0],
      [TERRITORY_NONE, 1, TERRITORY_NONE],
    ];
    // Only real player-player boundary: (0,1)=1 vs (0,2)=0 — wait, (1,1)=1
    // has right neighbour (1,2)=255 (none, no edge) and upper (0,1)=255.
    // (0,2)=0 has lower neighbour (1,2)=255. So there are no edges at all.
    expect(detectBorderEdges(grid)).toEqual([]);
    expect(flatEdges(grid, 2, 3)).toEqual([]);
  });
});
