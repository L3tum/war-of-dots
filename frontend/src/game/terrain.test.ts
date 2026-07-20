import { describe, it, expect } from 'vitest';
import { colorForType, marchingSquares, TERRAIN_PALETTES, TerrainType } from './terrain';

describe('colorForType', () => {
  it('returns green for Plains', () => {
    expect(colorForType('Plains')).toBe('#7eb547');
  });

  it('returns brownish for Hills', () => {
    expect(colorForType('Hills')).toBe('#a0a04a');
  });

  it('returns dark green for Forest', () => {
    expect(colorForType('Forest')).toBe('#2d7a2d');
  });

  it('returns blue for Water', () => {
    expect(colorForType('Water')).toBe('#3878c9');
  });

  it('returns gray for Mountain', () => {
    expect(colorForType('Mountain')).toBe('#555555');
  });

  it('falls back to a colored value (never white) for unknown input', () => {
    // A bad wire value must not blank the screen — colored fallback required.
    expect(colorForType('Bogus' as TerrainType)).toBe('#7eb547');
  });
});

describe('marchingSquares', () => {
  function makeGrid(width: number, height: number, type: TerrainType = 'Plains'): TerrainType[][] {
    const grid: TerrainType[][] = [];
    for (let y = 0; y < height; y++) {
      const row: TerrainType[] = [];
      for (let x = 0; x < width; x++) {
        row.push(type);
      }
      grid.push(row);
    }
    return grid;
  }

  it('returns empty for empty grid', () => {
    const grid: TerrainType[][] = [];
    const result = marchingSquares(grid, 1, { x: 0, y: 0 }, 1, 500, 300);
    expect(result.positions).toEqual([]);
    expect(result.colors).toEqual([]);
  });

  it('renders a quad for a 1x1 cell grid', () => {
    const grid = makeGrid(1, 1);
    const result = marchingSquares(grid, 1, { x: 0, y: 0 }, 1, 500, 300);
    // One quad = 4 vertices * 2 coords = 8 positions, 4 * 3 = 12 colors.
    expect(result.positions.length).toBe(8);
    expect(result.colors.length).toBe(12);
  });

  it('renders every cell (4 quads) in a 2x2 grid', () => {
    const grid = makeGrid(2, 2);
    const result = marchingSquares(grid, 1, { x: 0, y: 0 }, 1, 500, 300);
    expect(result.positions.length).toBe(4 * 8);
    expect(result.colors.length).toBe(4 * 12);
  });

  it('scales positions with cellSize', () => {
    const grid = makeGrid(2, 2);
    const result = marchingSquares(grid, 10, { x: 0, y: 0 }, 1, 500, 300);
    expect(result.positions[0]).toBe(0);
    expect(result.positions[2]).toBe(10);
  });

  it('sends world-space positions (zoom applied by shader)', () => {
    const grid = makeGrid(2, 2);
    const result = marchingSquares(grid, 1, { x: 0, y: 0 }, 2, 500, 300);
    expect(result.positions[0]).toBe(0);
    expect(result.positions[2]).toBe(1);
  });

  it('generates a quad per cell (16 quads) for a 4x4 grid', () => {
    const grid = makeGrid(4, 4);
    const result = marchingSquares(grid, 1, { x: 0, y: 0 }, 1, 500, 300);
    expect(result.positions.length).toBe(4 * 4 * 8);
    expect(result.colors.length).toBe(4 * 4 * 12);
  });

  it('keeps colors within the palette variance range for a uniform grid', () => {
    const grid = makeGrid(4, 4, 'Water');
    const result = marchingSquares(grid, 1, { x: 0, y: 0 }, 1, 500, 300);
    const p = TERRAIN_PALETTES.Water;
    let interior = 0;
    for (let i = 0; i < result.colors.length; i += 3) {
      const r = result.colors[i] * 255;
      const g = result.colors[i + 1] * 255;
      const bScaled = result.colors[i + 2] * 255;
      // Interior water cells (no land neighbor): within base +/- variance.
      if (g > p.g * (1 - p.variance) - 0.001 && g < p.g * (1 + p.variance) + 0.001) {
        expect(r).toBeGreaterThanOrEqual(Math.max(0, p.r * (1 - p.variance) - 0.001));
        expect(r).toBeLessThanOrEqual(Math.min(255, p.r * (1 + p.variance) + 0.001));
        expect(bScaled).toBeGreaterThanOrEqual(Math.max(0, p.b * (1 - p.variance) - 0.001));
        expect(bScaled).toBeLessThanOrEqual(Math.min(255, p.b * (1 + p.variance) + 0.001));
        interior++;
      }
    }
    expect(interior).toBeGreaterThan(0);
  });

  it('is deterministic for the same cell', () => {
    const grid = makeGrid(2, 2);
    const a = marchingSquares(grid, 1, { x: 0, y: 0 }, 1, 500, 300);
    const b = marchingSquares(grid, 1, { x: 0, y: 0 }, 1, 500, 300);
    expect(a.colors).toEqual(b.colors);
  });

  it('brightens coastal water cells that touch land', () => {
    const grid: TerrainType[][] = [
      ['Plains', 'Water'],
      ['Water', 'Water'],
    ];
    const result = marchingSquares(grid, 1, { x: 0, y: 0 }, 0.2, 500, 300);
    // The water cell at (0,1) touches Plains -> coastline brightening raises its red channel
    // beyond the base palette red, which interior water never reaches.
    const p = TERRAIN_PALETTES.Water;
    const baseRed = p.r / 255;
    let foundCoast = false;
    for (let i = 0; i < result.colors.length; i += 3) {
      if (result.colors[i] > baseRed + 0.01) foundCoast = true;
    }
    expect(foundCoast).toBe(true);
  });

  it('handles single-cell-wide grid', () => {
    const grid: TerrainType[][] = [['Plains'], ['Plains'], ['Plains']];
    const result = marchingSquares(grid, 1, { x: 0, y: 0 }, 1, 500, 300);
    // 3 cells exported, one quad each.
    expect(result.positions.length).toBe(3 * 8);
  });

  it('uses correct base color for terrain type', () => {
    const grid = makeGrid(2, 2, 'Water');
    const result = marchingSquares(grid, 1, { x: 0, y: 0 }, 1, 500, 300);
    const p = TERRAIN_PALETTES.Water;
    const shade = (x: number, y: number): number => {
      let n = x * 374761393 + y * 668265263;
      n = (n ^ (n >> 13)) * 1274126177;
      n = n ^ (n >> 16);
      return ((n >>> 0) % 1000) / 1000;
    };
    const factor = 1 + (shade(0, 0) - 0.5) * 2 * p.variance;
    expect(result.colors[0]).toBeCloseTo(Math.min(1, (p.r * factor) / 255), 2);
    expect(result.colors[1]).toBeCloseTo(Math.min(1, (p.g * factor) / 255), 2);
    expect(result.colors[2]).toBeCloseTo(Math.min(1, (p.b * factor) / 255), 2);
  });
});
