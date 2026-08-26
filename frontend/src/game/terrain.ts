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

  // W2: half-pixel overlap to eliminate 4-way corner gaps.
  // 1 physical screen pixel = 1/zoom world units, so half a pixel = 0.5/zoom.
  const hpX = 0.5 / zoom;
  const hpY = 0.5 / zoom;

  // W3: Pass 1 — compute per-cell final RGB, expanded by one cell on each side
  // for corner lookups. OOB cells clamp to the nearest valid edge cell.
  const expandedStartCol = Math.max(0, startCol - 1);
  const expandedEndCol = Math.min(width, endCol + 1);
  const expandedStartRow = Math.max(0, startRow - 1);
  const expandedEndRow = Math.min(height, endRow + 1);

  // cellColor[localRow][localCol] = [r, g, b] in 0..255
  const cellColor: number[][][] = [];
  const localRows = expandedEndRow - expandedStartRow;
  const localCols = expandedEndCol - expandedStartCol;

  for (let ly = 0; ly < localRows; ly++) {
    const row: number[][] = [];
    for (let lx = 0; lx < localCols; lx++) {
      // Clamp to valid grid coords
      const gx = Math.min(width - 1, Math.max(0, expandedStartCol + lx));
      const gy = Math.min(height - 1, Math.max(0, expandedStartRow + ly));
      const cell = grid[gy][gx];
      const palette = TERRAIN_PALETTES[cell] ?? TERRAIN_PALETTES.Plains;

      const shade = cellShade(gx, gy);
      const factor = 1 + (shade - 0.5) * 2 * palette.variance;
      let r = Math.min(255, Math.max(0, palette.r * factor));
      let g = Math.min(255, Math.max(0, palette.g * factor));
      let b = Math.min(255, Math.max(0, palette.b * factor));

      if (cell === 'Water' && isCoast(grid, gx, gy, width, height)) {
        r = Math.min(255, r * 1.18 + 20);
        g = Math.min(255, g * 1.15 + 26);
        b = Math.min(255, b * 1.05 + 18);
      }

      row.push([r, g, b]);
    }
    cellColor.push(row);
  }

  // W3: Pass 2 — build vertex color lattice. Each vertex at grid intersection
  // (x, y) is the average of the up-to-4 cells that own that corner.
  // Lattice dimensions: (localCols + 1) × (localRows + 1)
  function cornerColor(cx: number, cy: number): [number, number, number] {
    // (cx, cy) in lattice coords maps to cells (cx-1,cy-1),(cx,cy-1),(cx-1,cy),(cx,cy)
    // in local grid coords (each cell covers [i, i+1) in lattice space).
    const cells: Array<[number, number]> = [
      [Math.max(0, cx - 1), Math.max(0, cy - 1)],
      [Math.min(localCols - 1, cx), Math.max(0, cy - 1)],
      [Math.max(0, cx - 1), Math.min(localRows - 1, cy)],
      [Math.min(localCols - 1, cx), Math.min(localRows - 1, cy)],
    ];
    // Deduplicate: at edges/corners, some cells collapse to the same index.
    const seen = new Set<string>();
    let sumR = 0,
      sumG = 0,
      sumB = 0,
      count = 0;
    for (const [lx, ly] of cells) {
      const key = `${lx},${ly}`;
      if (seen.has(key)) continue;
      seen.add(key);
      const [r, g, b] = cellColor[ly][lx];
      sumR += r;
      sumG += g;
      sumB += b;
      count++;
    }
    return [sumR / count, sumG / count, sumB / count];
  }

  // Determine which cells are Mountain (for shading)
  function isMountain(gx: number, gy: number): boolean {
    const cx = Math.min(width - 1, Math.max(0, gx));
    const cy = Math.min(height - 1, Math.max(0, gy));
    return grid[cy][cx] === 'Mountain';
  }

  // W3: Mountain shading factors applied to corner colors
  const MOUNTAIN_TOP_FACTOR = 0.68;
  const MOUNTAIN_BOT_FACTOR = 1.18;

  for (let y = startRow; y < endRow && y < height; y++) {
    for (let x = startCol; x < endCol && x < width; x++) {
      // W2: expanded quad corners (half-pixel overlap)
      const wx = x * cellSize;
      const wy = y * cellSize;
      const x0 = wx - hpX;
      const x1 = wx + cellSize + hpX;
      const y0 = wy - hpY;
      const y1 = wy + cellSize + hpY;

      // Lattice corner indices for this cell in the local grid
      const localX = x - expandedStartCol;
      const localY = y - expandedStartRow;

      // Get the 4 corner colors (TL, TR, BR, BL)
      let [tlR, tlG, tlB] = cornerColor(localX, localY); // top-left
      let [trR, trG, trB] = cornerColor(localX + 1, localY); // top-right
      let [brR, brG, brB] = cornerColor(localX + 1, localY + 1); // bottom-right
      let [blR, blG, blB] = cornerColor(localX, localY + 1); // bottom-left

      // Mountain shading: darken top row, lighten bottom row
      // Only apply if ALL 4 surrounding cells are mountain (or at least the
      // cell itself is mountain) — blend shading when at mountain border.
      if (isMountain(x, y)) {
        // Apply mountain shading to top and bottom corners
        tlR *= MOUNTAIN_TOP_FACTOR;
        tlG *= MOUNTAIN_TOP_FACTOR;
        tlB *= MOUNTAIN_TOP_FACTOR;
        trR *= MOUNTAIN_TOP_FACTOR;
        trG *= MOUNTAIN_TOP_FACTOR;
        trB *= MOUNTAIN_TOP_FACTOR;
        brR = Math.min(255, brR * MOUNTAIN_BOT_FACTOR);
        brG = Math.min(255, brG * MOUNTAIN_BOT_FACTOR);
        brB = Math.min(255, brB * MOUNTAIN_BOT_FACTOR);
        blR = Math.min(255, blR * MOUNTAIN_BOT_FACTOR);
        blG = Math.min(255, blG * MOUNTAIN_BOT_FACTOR);
        blB = Math.min(255, blB * MOUNTAIN_BOT_FACTOR);
      }

      // W2: expanded quad positions
      positions.push(x0, y0, x1, y0, x1, y1, x0, y1);

      // W3: per-vertex colors (blended corners)
      colors.push(tlR / 255, tlG / 255, tlB / 255);
      colors.push(trR / 255, trG / 255, trB / 255);
      colors.push(brR / 255, brG / 255, brB / 255);
      colors.push(blR / 255, blG / 255, blB / 255);
    }
  }

  return { positions, colors };
}
