//! Terrain rendering — per-cell flat-standing terrain with deterministic color
//! variation, water coastlines, and mountain shading for a richer map.

export type TerrainType = 'Plains' | 'Hills' | 'Forest' | 'Water' | 'Mountain';

// Enum-backed alias so existing `Terrain[][]` references keep type-checking.
export type Terrain = TerrainType;

export interface TerrainPalette {
  r: number;
  g: number;
  b: number;
  /** How much (0..1) per-cell luminance may vary from the base — richer map, no seams. */
  variance: number;
}

/** Base palette per terrain type (RGB 0..255). */
export const TERRAIN_PALETTES: Record<TerrainType, TerrainPalette> = {
  Plains: { r: 0x7e, g: 0xb5, b: 0x47, variance: 0.07 },
  Hills: { r: 0xa0, g: 0xa0, b: 0x4a, variance: 0.1 },
  Forest: { r: 0x2d, g: 0x7a, b: 0x2d, variance: 0.11 },
  Water: { r: 0x38, g: 0x78, b: 0xc9, variance: 0.05 },
  Mountain: { r: 0x55, g: 0x55, b: 0x55, variance: 0.15 },
};

export function colorForType(type: TerrainType): string {
  const p = TERRAIN_PALETTES[type] ?? TERRAIN_PALETTES.Plains;
  return `#${p.r.toString(16).padStart(2, '0')}${p.g
    .toString(16)
    .padStart(2, '0')}${p.b.toString(16).padStart(2, '0')}`;
}

function isLand(t: TerrainType): boolean {
  return t !== 'Water';
}

/** Deterministic per-cell shade in [0,1] — stable across frames. */
function cellShade(x: number, y: number): number {
  let n = x * 374761393 + y * 668265263;
  n = (n ^ (n >> 13)) * 1274126177;
  n = n ^ (n >> 16);
  return ((n >>> 0) % 1000) / 1000;
}

/** Does this cell touch a land cell orthogonally? (for coastlines) */
function isCoast(grid: TerrainType[][], x: number, y: number, w: number, h: number): boolean {
  const neighbors: Array<[number, number]> = [
    [x + 1, y],
    [x - 1, y],
    [x, y + 1],
    [x, y - 1],
  ];
  for (const [nx, ny] of neighbors) {
    if (nx < 0 || ny < 0 || nx >= w || ny >= h) continue;
    if (isLand(grid[ny]?.[nx])) return true;
  }
  return false;
}

export function marchingSquares(
  grid: TerrainType[][],
  cellSize: number,
  position: { x: number; y: number },
  zoom: number,
  canvasWidth: number,
  canvasHeight: number,
): { positions: number[]; colors: number[] } {
  const positions: number[] = [];
  const colors: number[] = [];

  if (grid.length === 0 || grid[0].length === 0) {
    return { positions, colors };
  }

  const height = grid.length;
  const width = grid[0].length;

  // Visible cell range based on camera viewport
  const camWorldX = position.x - canvasWidth / (2 * zoom);
  const camWorldY = position.y - canvasHeight / (2 * zoom);
  const visibleWorldW = canvasWidth / zoom;
  const visibleWorldH = canvasHeight / zoom;

  const startCol = Math.max(0, Math.floor(camWorldX / cellSize) - 1);
  const endCol = Math.min(width, Math.ceil((camWorldX + visibleWorldW) / cellSize) + 1);
  const startRow = Math.max(0, Math.floor(camWorldY / cellSize) - 1);
  const endRow = Math.min(height, Math.ceil((camWorldY + visibleWorldH) / cellSize) + 1);

  for (let y = startRow; y < endRow && y < height; y++) {
    for (let x = startCol; x < endCol && x < width; x++) {
      const cell = grid[y][x];
      const palette = TERRAIN_PALETTES[cell] ?? TERRAIN_PALETTES.Plains;

      const shade = cellShade(x, y);
      // factor in [1-variance, 1+variance]
      const factor = 1 + (shade - 0.5) * 2 * palette.variance;
      let r = Math.min(255, Math.max(0, palette.r * factor));
      let g = Math.min(255, Math.max(0, palette.g * factor));
      let b = Math.min(255, Math.max(0, palette.b * factor));

      const wx = x * cellSize;
      const wy = y * cellSize;

      if (cell === 'Water' && isCoast(grid, x, y, width, height)) {
        // Shoreline: brighten coastal water toward teal for a visible coastline.
        r = Math.min(255, r * 1.18 + 20);
        g = Math.min(255, g * 1.15 + 26);
        b = Math.min(255, b * 1.05 + 18);
      }

      if (cell === 'Mountain') {
        // North-facing light: darker top edge, lighter bottom edge.
        const topR = Math.max(0, r * 0.68);
        const topG = Math.max(0, g * 0.68);
        const topB = Math.max(0, b * 0.68);
        const botR = Math.min(255, r * 1.18);
        const botG = Math.min(255, g * 1.18);
        const botB = Math.min(255, b * 1.18);
        // Quad verts: TL, TR, BR, BL
        positions.push(wx, wy, wx + cellSize, wy, wx + cellSize, wy + cellSize, wx, wy + cellSize);
        colors.push(topR / 255, topG / 255, topB / 255);
        colors.push(topR / 255, topG / 255, topB / 255);
        colors.push(botR / 255, botG / 255, botB / 255);
        colors.push(botR / 255, botG / 255, botB / 255);
        continue;
      }

      const cr = r / 255;
      const cg = g / 255;
      const cb = b / 255;
      positions.push(wx, wy, wx + cellSize, wy, wx + cellSize, wy + cellSize, wx, wy + cellSize);
      for (let i = 0; i < 4; i++) {
        colors.push(cr, cg, cb);
      }
    }
  }

  return { positions, colors };
}
