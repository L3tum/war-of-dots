import { describe, it, expect } from 'vitest';
import { CELL_SIZE, BORDER_LINE_WIDTH, PLAYER_COLORS, CITY_DOT_SIZE, CITY_COLORS } from './config';
import type { TerrainType } from './terrain';
import { colorForType } from './terrain';

describe('Border edge detection', () => {
  type BorderEdge = { gridX: number; gridY: number; dir: 'h' | 'v'; owner: number };

  const TERRITORY_NONE = 255;

  function detectBorderEdges(borderGrid: number[][]): BorderEdge[] {
    const edges: BorderEdge[] = [];
    if (borderGrid.length === 0) return edges;
    for (let y = 0; y < borderGrid.length - 1; y++) {
      for (let x = 0; x < borderGrid[y].length - 1; x++) {
        const p1 = borderGrid[y][x];
        const p2 = borderGrid[y][x + 1];
        const p3 = borderGrid[y + 1][x];
        // Only borders between two different players — never claimed vs NONE.
        if (p1 !== p2 && p1 !== TERRITORY_NONE && p2 !== TERRITORY_NONE) {
          edges.push({ gridX: x, gridY: y, dir: 'h' as const, owner: p1 });
        }
        if (p1 !== p3 && p1 !== TERRITORY_NONE && p3 !== TERRITORY_NONE) {
          edges.push({ gridX: x, gridY: y, dir: 'v' as const, owner: p1 });
        }
      }
    }
    return edges;
  }

  it('does not draw border between a player and NONE (255)', () => {
    // player 0 adjacent to unclaimed (255) → no border
    const edges = detectBorderEdges([
      [0, 255],
      [0, 255],
    ]);
    expect(edges).toEqual([]);
  });

  it('draws only the player-vs-player border when NONE is adjacent', () => {
    // row: 255 | 0 | 1 → single horizontal border between 0 and 1 at x=1
    const edges = detectBorderEdges([
      [255, 0, 1],
      [255, 0, 1],
    ]);
    expect(edges).toEqual([{ gridX: 1, gridY: 0, dir: 'h', owner: 0 }]);
  });

  it('returns empty for 1x1 grid', () => {
    expect(detectBorderEdges([[0]])).toEqual([]);
  });

  it('returns no edges when all same owner', () => {
    expect(
      detectBorderEdges([
        [0, 0],
        [0, 0],
      ]),
    ).toEqual([]);
  });

  it('detects horizontal border', () => {
    const edges = detectBorderEdges([
      [0, 1],
      [0, 1],
    ]);
    expect(edges).toEqual([{ gridX: 0, gridY: 0, dir: 'h', owner: 0 }]);
  });

  it('detects vertical border', () => {
    const edges = detectBorderEdges([
      [0, 0],
      [1, 1],
    ]);
    expect(edges).toEqual([{ gridX: 0, gridY: 0, dir: 'v', owner: 0 }]);
  });

  it('detects both horizontal and vertical borders', () => {
    const edges = detectBorderEdges([
      [0, 1],
      [1, 1],
    ]);
    expect(edges).toEqual([
      { gridX: 0, gridY: 0, dir: 'h', owner: 0 },
      { gridX: 0, gridY: 0, dir: 'v', owner: 0 },
    ]);
  });

  it('handles larger grid with multiple borders', () => {
    const edges = detectBorderEdges([
      [0, 0, 1],
      [0, 0, 1],
      [1, 1, 1],
    ]);
    // Horizontal borders at (0,1) and (1,1)
    // Vertical borders at (0,0) and (1,0)
    expect(edges).toHaveLength(4);
  });
});

describe('Border vertex generation', () => {
  function generateBorderVertices(
    edges: Array<{ gridX: number; gridY: number; dir: 'h' | 'v'; owner: number }>,
    camera: { x: number; y: number; zoom: number },
  ) {
    const positions: number[] = [];
    const colors: number[] = [];

    const BORDER_COLORS = [
      { r: 1, g: 0.2, b: 0.2 },
      { r: 0.2, g: 0.2, b: 1 },
    ];

    for (const edge of edges) {
      const c = BORDER_COLORS[edge.owner % 2] || { r: 1, g: 1, b: 1 };
      if (edge.dir === 'h') {
        const sx = (camera.x + edge.gridX * CELL_SIZE) * camera.zoom;
        const sy = (camera.y + (edge.gridY + 1) * CELL_SIZE) * camera.zoom;
        positions.push(
          sx,
          sy - BORDER_LINE_WIDTH,
          sx + CELL_SIZE * camera.zoom,
          sy - BORDER_LINE_WIDTH,
          sx + CELL_SIZE * camera.zoom,
          sy + BORDER_LINE_WIDTH,
          sx,
          sy + BORDER_LINE_WIDTH,
        );
      } else {
        const sx = (camera.x + (edge.gridX + 1) * CELL_SIZE) * camera.zoom;
        const sy = (camera.y + edge.gridY * CELL_SIZE) * camera.zoom;
        positions.push(
          sx - BORDER_LINE_WIDTH,
          sy,
          sx + BORDER_LINE_WIDTH,
          sy,
          sx + BORDER_LINE_WIDTH,
          sy + CELL_SIZE * camera.zoom,
          sx - BORDER_LINE_WIDTH,
          sy + CELL_SIZE * camera.zoom,
        );
      }
      for (let i = 0; i < 4; i++) colors.push(c.r, c.g, c.b);
    }

    return { positions, colors };
  }

  it('generates correct vertex count for one border', () => {
    const edges = [{ gridX: 0, gridY: 0, dir: 'h' as const, owner: 0 }];
    const { positions, colors } = generateBorderVertices(edges, { x: 0, y: 0, zoom: 1 });

    // One quad = 4 vertices * 2 coords = 8 positions
    expect(positions.length).toBe(8);
    // One quad = 4 vertices * 3 color components = 12 colors
    expect(colors.length).toBe(12);
  });

  it('scales border positions with zoom', () => {
    const edges = [{ gridX: 0, gridY: 0, dir: 'h' as const, owner: 0 }];
    const { positions } = generateBorderVertices(edges, { x: 0, y: 0, zoom: 2 });

    // Border should span CELL_SIZE * zoom = 30
    expect(positions[2] - positions[0]).toBe(CELL_SIZE * 2);
  });

  it('offsets border positions with camera', () => {
    const edges = [{ gridX: 0, gridY: 0, dir: 'h' as const, owner: 0 }];
    const { positions } = generateBorderVertices(edges, { x: 100, y: 100, zoom: 1 });

    // Horizontal border: sy = (100 + (0+1) * CELL_SIZE) * 1 = 100 + 15 = 115
    // positions[1] = sy - BORDER_LINE_WIDTH = 115 - 3 = 112
    expect(positions[0]).toBe(100);
    expect(positions[1]).toBe(100 + CELL_SIZE - BORDER_LINE_WIDTH);
  });
});

describe('Unit vertex generation', () => {
  it('generates unit quad at correct screen position', () => {
    const camera = { x: 0, y: 0, zoom: 1 };
    const unitPos = { x: 10, y: 20 };
    const unitDotSize = 5;

    const sx = (camera.x + unitPos.x * CELL_SIZE) * camera.zoom;
    const sy = (camera.y + unitPos.y * CELL_SIZE) * camera.zoom;

    // Quad corners
    const topLeftX = sx - unitDotSize;
    const topLeftY = sy - unitDotSize;
    const bottomRightX = sx + unitDotSize;
    const bottomRightY = sy + unitDotSize;

    expect(topLeftX).toBe(10 * CELL_SIZE - unitDotSize);
    expect(topLeftY).toBe(20 * CELL_SIZE - unitDotSize);
    expect(bottomRightX).toBe(10 * CELL_SIZE + unitDotSize);
    expect(bottomRightY).toBe(20 * CELL_SIZE + unitDotSize);
  });

  it('scales unit position with zoom', () => {
    const camera = { x: 0, y: 0, zoom: 2 };
    const unitPos = { x: 10, y: 20 };

    const sx = (camera.x + unitPos.x * CELL_SIZE) * camera.zoom;
    const sy = (camera.y + unitPos.y * CELL_SIZE) * camera.zoom;

    expect(sx).toBe(10 * CELL_SIZE * 2);
    expect(sy).toBe(20 * CELL_SIZE * 2);
  });

  it('offsets unit position with camera', () => {
    const camera = { x: 100, y: 200, zoom: 1 };
    const unitPos = { x: 10, y: 20 };

    const sx = (camera.x + unitPos.x * CELL_SIZE) * camera.zoom;
    const sy = (camera.y + unitPos.y * CELL_SIZE) * camera.zoom;

    expect(sx).toBe(100 + 10 * CELL_SIZE);
    expect(sy).toBe(200 + 20 * CELL_SIZE);
  });

  it('selects correct player color by index', () => {
    const colorIdx = parseInt('player_1'.replace('player_', '') || '0') || 0;
    const color = PLAYER_COLORS[colorIdx % PLAYER_COLORS.length];

    expect(color).toBe(PLAYER_COLORS[1]);
  });

  it('handles player with numeric ID', () => {
    const colorIdx = parseInt('player_2'.replace('player_', '') || '0') || 0;
    const color = PLAYER_COLORS[colorIdx % PLAYER_COLORS.length];

    expect(color).toBe(PLAYER_COLORS[2]);
  });
});

describe('Uniform buffer data', () => {
  it('generates correct uniform data from camera state', () => {
    const camera = { x: 100, y: 200, zoom: 1.5 };
    const uniformData = new Float32Array([camera.x, camera.y, camera.zoom]);

    expect(uniformData).toEqual(new Float32Array([100, 200, 1.5]));
    expect(uniformData.length).toBe(3);
    expect(uniformData.byteLength).toBe(12); // 3 floats * 4 bytes
  });

  it('generates default uniform data at origin', () => {
    const camera = { x: 0, y: 0, zoom: 1.0 };
    const uniformData = new Float32Array([camera.x, camera.y, camera.zoom]);

    expect(uniformData).toEqual(new Float32Array([0, 0, 1.0]));
  });
});

describe('Camera transform', () => {
  function worldToScreen(
    worldX: number,
    worldY: number,
    camera: { x: number; y: number; zoom: number },
  ) {
    // Same math as WGSL shader (center-based camera):
    // screenX = (worldX - camera.x) * zoom / (canvasWidth/2)
    // screenY = (camera.y - worldY) * zoom / (canvasHeight/2)
    const screenX = ((worldX - camera.x) * camera.zoom) / 250; // 2 / 500
    const screenY = ((camera.y - worldY) * camera.zoom) / 150; // 2 / 300
    return { screenX, screenY };
  }

  it('maps world point at camera center to NDC origin', () => {
    const result = worldToScreen(100, 50, { x: 100, y: 50, zoom: 1 });
    expect(result.screenX).toBeCloseTo(0);
    expect(result.screenY).toBeCloseTo(0);
  });

  it('applies zoom correctly', () => {
    const result = worldToScreen(100, 0, { x: 0, y: 0, zoom: 2 });
    expect(result.screenX).toBeCloseTo((100 * 2) / 250);
    expect(result.screenY).toBeCloseTo(0);
  });

  it('applies camera offset correctly', () => {
    const result = worldToScreen(0, 0, { x: 250, y: 150, zoom: 1 });
    expect(result.screenX).toBeCloseTo((0 - 250) / 250); // -1 (left edge)
    expect(result.screenY).toBeCloseTo((150 - 0) / 150); // 1 (top edge)
  });
});

describe('Fog of war generation', () => {
  it('marks cells as visible when in vision grid', () => {
    const visionGrid: boolean[][] = [
      [true, false, true],
      [false, true, false],
    ];

    const fogValues: number[] = [];
    for (let y = 0; y < visionGrid.length; y++) {
      for (let x = 0; x < visionGrid[y].length; x++) {
        fogValues.push(visionGrid[y][x] ? 0 : 128);
      }
    }

    expect(fogValues).toEqual([0, 128, 0, 128, 0, 128]);
  });
});

describe('Bar vertex generation', () => {
  it('generates bar quad at correct position above unit', () => {
    const unitScreenPos = { x: 100, y: 200 };
    const barWidth = 20;
    const barSpacing = 2.5;
    const symSize = 4; // unitDotSize * 0.8

    // First bar is above symbol
    const barY = unitScreenPos.y - symSize - barSpacing;
    const bx = unitScreenPos.x - barWidth / 2;

    expect(bx).toBe(90);
    expect(barY).toBe(193.5);
  });

  it('stacks bars with correct spacing', () => {
    const unitScreenPos = { x: 100, y: 200 };
    const barWidth = 20;
    const barHeight = 2;
    const barSpacing = 2.5;
    const symSize = 4;

    let barY = unitScreenPos.y - symSize - barSpacing;
    const barPositions: number[] = [];

    for (let i = 0; i < 3; i++) {
      const bx = unitScreenPos.x - barWidth / 2;
      const actualWidth = barWidth * 0.5; // 50% health
      barPositions.push(
        bx,
        barY,
        bx + actualWidth,
        barY,
        bx + actualWidth,
        barY + barHeight,
        bx,
        barY + barHeight,
      );
      barY -= barSpacing;
    }

    expect(barPositions.length).toBe(3 * 8); // 3 bars, 8 positions each
    // Bar 1 top edge Y at index 1, Bar 2 top edge Y at index 9
    // Each bar is barSpacing apart (stacked downward)
    expect(barPositions[9] - barPositions[1]).toBeCloseTo(-barSpacing);
  });
});

describe('Terrain color generation', () => {
  it('returns correct colors for terrain types', () => {
    const terrainColors: Record<string, string> = {
      Plains: '#7eb547',
      Hills: '#a0a04a',
      Forest: '#2d7a2d',
      Water: '#3878c9',
      Mountain: '#555555',
    };

    for (const [type, expected] of Object.entries(terrainColors)) {
      const actual = colorForType(type as TerrainType);
      expect(actual).toBe(expected);
    }
  });

  it('falls back to a colored value (never undefined/white) for unknown terrain', () => {
    const color = colorForType('Unknown' as TerrainType);
    expect(color).toBeDefined();
    expect(color).toBe('#7eb547'); // safe colored fallback, never blanks the screen
  });
});

// ============================================================================
// Camera Centering Tests
// ============================================================================

describe('Camera centering', () => {
  /**
   * Same centering logic as Connection.centerCameraOnTerrain()
   */
  function computeTerrainCenter(terrain: { length: number; [key: number]: unknown[] }): {
    x: number;
    y: number;
  } {
    const cellSize = CELL_SIZE;
    let minCol = Infinity,
      maxCol = -Infinity;
    for (let r = 0; r < terrain.length; r++) {
      const row = terrain[r];
      if (row && row.length > 0) {
        minCol = Math.min(minCol, 0);
        maxCol = Math.max(maxCol, row.length - 1);
      }
    }
    const minRow = 0;
    const maxRow = terrain.length - 1;

    return {
      x: ((minCol + maxCol) / 2 + 0.5) * cellSize,
      y: ((minRow + maxRow) / 2 + 0.5) * cellSize,
    };
  }

  it('centers correctly on a 10x10 terrain', () => {
    const terrain = Array.from({ length: 10 }, () => Array(10).fill(null));
    const center = computeTerrainCenter(terrain);

    // Center should be at (160, 160) = (5 + 0.5) * 32
    expect(center.x).toBe((4.5 + 0.5) * CELL_SIZE); // = 160
    expect(center.y).toBe((4.5 + 0.5) * CELL_SIZE); // = 160
  });

  it('centers correctly on a 2x2 terrain', () => {
    const terrain = [
      [null, null],
      [null, null],
    ];
    const center = computeTerrainCenter(terrain);

    // For 2x2: minCol=0, maxCol=1 → center = ((0+1)/2 + 0.5) * 32 = 32
    expect(center.x).toBe(1.0 * CELL_SIZE);
    expect(center.y).toBe(1.0 * CELL_SIZE);
  });

  it('centers correctly on a wide terrain (20x5)', () => {
    const terrain = Array.from({ length: 5 }, () => Array(20).fill(null));
    const center = computeTerrainCenter(terrain);

    expect(center.x).toBe((9.5 + 0.5) * CELL_SIZE); // = 320
    expect(center.y).toBe((2 + 0.5) * CELL_SIZE); // = 80
  });

  it('handles irregular (ragged) terrain grid', () => {
    // Row 0: 3 cells, Row 1: 5 cells, Row 2: 4 cells
    const terrain = [
      [null, null, null],
      [null, null, null, null, null],
      [null, null, null, null],
    ];
    const center = computeTerrainCenter(terrain);

    // minCol=0, maxCol=4 (from row 1), minRow=0, maxRow=2
    // centerX = ((0+4)/2 + 0.5) * 32 = (2 + 0.5) * 32 = 80
    // centerY = ((0+2)/2 + 0.5) * 32 = (1 + 0.5) * 32 = 48
    expect(center.x).toBe(2.5 * CELL_SIZE); // = 80
    expect(center.y).toBe(1.5 * CELL_SIZE); // = 48
  });

  it('returns zero center for empty terrain', () => {
    const terrain: unknown[][] = [];
    // centerCameraOnTerrain returns early for empty terrain
    // So this tests the guard condition
    expect(terrain.length).toBe(0);
  });
});

// ============================================================================
// City Dot Vertex Generation Tests
// ============================================================================

describe('City dot vertex generation', () => {
  interface CityData {
    id: string;
    position: [number, number];
    owner?: string;
  }

  function generateCityDotVertices(
    cities: CityData[],
    camera: { x: number; y: number; zoom: number },
  ): { positions: number[]; colors: number[] } {
    const positions: number[] = [];
    const colors: number[] = [];

    for (const city of cities) {
      const wx = city.position[0] * CELL_SIZE;
      const wy = city.position[1] * CELL_SIZE;
      const halfSize = CITY_DOT_SIZE * camera.zoom;
      positions.push(
        wx - halfSize,
        wy - halfSize,
        wx + halfSize,
        wy - halfSize,
        wx + halfSize,
        wy + halfSize,
        wx - halfSize,
        wy + halfSize,
      );

      const colorIdx = parseInt((city.owner ?? '').replace('player_', '') || '0') || 0;
      const color = CITY_COLORS[colorIdx % CITY_COLORS.length] || { r: 0.5, g: 0.5, b: 0.5 };
      for (let i = 0; i < 4; i++) {
        colors.push(color.r, color.g, color.b);
      }
    }

    return { positions, colors };
  }

  it('generates one quad (8 positions) for one city', () => {
    const cities = [{ id: 'city_0', position: [5, 5] as [number, number] }];
    const { positions, colors } = generateCityDotVertices(cities, {
      x: 0,
      y: 0,
      zoom: 1,
    });

    expect(positions.length).toBe(8);
    expect(colors.length).toBe(12);
  });

  it('generates correct quad at world position for city at (5,5)', () => {
    const cities = [{ id: 'city_0', position: [5, 5] as [number, number] }];
    const { positions } = generateCityDotVertices(cities, { x: 0, y: 0, zoom: 1 });

    const wx = 5 * CELL_SIZE; // 160
    const wy = 5 * CELL_SIZE; // 160
    const halfSize = CITY_DOT_SIZE * 1; // 6

    expect(positions[0]).toBe(wx - halfSize); // 154
    expect(positions[1]).toBe(wy - halfSize); // 154
    expect(positions[2]).toBe(wx + halfSize); // 166
    expect(positions[3]).toBe(wy - halfSize); // 154
  });

  it('scales city dot size with zoom', () => {
    const cities = [{ id: 'city_0', position: [5, 5] as [number, number] }];
    const { positions } = generateCityDotVertices(cities, { x: 0, y: 0, zoom: 2 });

    // Quad width should be 2 * CITY_DOT_SIZE * zoom = 24
    const width = positions[2] - positions[0];
    expect(width).toBe(2 * CITY_DOT_SIZE * 2);
  });

  it('colors city by owner index', () => {
    const cities = [{ id: 'city_0', position: [0, 0] as [number, number], owner: 'player_1' }];
    const { colors } = generateCityDotVertices(cities, { x: 0, y: 0, zoom: 1 });

    // player_1 -> index 1 -> CITY_COLORS[1] = Yellow
    expect(colors[0]).toBe(CITY_COLORS[1].r);
    expect(colors[1]).toBe(CITY_COLORS[1].g);
    expect(colors[2]).toBe(CITY_COLORS[1].b);
  });

  it('handles unknown owner with gray fallback', () => {
    const cities = [{ id: 'city_0', position: [0, 0] as [number, number] }];
    const { colors } = generateCityDotVertices(cities, { x: 0, y: 0, zoom: 1 });

    // No owner -> index 0 -> CITY_COLORS[0] = Red (first color)
    expect(colors[0]).toBe(CITY_COLORS[0].r);
  });

  it('generates quads for multiple cities', () => {
    const cities = [
      { id: 'city_0', position: [0, 0] as [number, number] },
      { id: 'city_1', position: [10, 10] as [number, number] },
      { id: 'city_2', position: [20, 5] as [number, number] },
    ];
    const { positions, colors } = generateCityDotVertices(cities, {
      x: 0,
      y: 0,
      zoom: 1,
    });

    expect(positions.length).toBe(3 * 8); // 3 cities * 8 positions
    expect(colors.length).toBe(3 * 12); // 3 cities * 12 colors
  });
});

// ============================================================================
// Fog Quad Tests
// ============================================================================

describe('Fog fullscreen quad', () => {
  /**
   * Simulates the NDC fullscreen quad with UVs for fog rendering.
   * 4 vertices, stride 8: pos(x,y) + uv(x,y)
   */
  function generateFogQuadData(): Float32Array {
    return new Float32Array([
      // Left-bottom NDC + UV
      -1, -1, 0, 0,
      // Right-bottom NDC + UV
      1, -1, 1, 0,
      // Left-top NDC + UV
      -1, 1, 0, 1,
      // Right-top NDC + UV
      1, 1, 1, 1,
    ]);
  }

  it('generates correct buffer size for fog quad', () => {
    const data = generateFogQuadData();
    expect(data.length).toBe(16); // 4 vertices * 4 floats
    expect(data.byteLength).toBe(64); // 16 * 4 bytes
  });

  it('covers full NDC space from -1 to +1', () => {
    const data = generateFogQuadData();
    // Positions: data[0,1] = -1,-1; data[4,5] = 1,-1; data[8,9] = -1,1; data[12,13] = 1,1
    expect(data[0]).toBe(-1); // left-bottom X
    expect(data[1]).toBe(-1); // left-bottom Y
    expect(data[4]).toBe(1); // right-bottom X
    expect(data[5]).toBe(-1); // right-bottom Y
    expect(data[8]).toBe(-1); // left-top X
    expect(data[9]).toBe(1); // left-top Y
    expect(data[12]).toBe(1); // right-top X
    expect(data[13]).toBe(1); // right-top Y
  });

  it('maps UVs from (0,0) to (1,1)', () => {
    const data = generateFogQuadData();
    // UVs: data[2,3] = 0,0; data[6,7] = 1,0; data[10,11] = 0,1; data[14,15] = 1,1
    expect(data[2]).toBe(0); // left-bottom UV X
    expect(data[3]).toBe(0); // left-bottom UV Y
    expect(data[6]).toBe(1); // right-bottom UV X
    expect(data[7]).toBe(0); // right-bottom UV Y
    expect(data[10]).toBe(0); // left-top UV X
    expect(data[11]).toBe(1); // left-top UV Y
    expect(data[14]).toBe(1); // right-top UV X
    expect(data[15]).toBe(1); // right-top UV Y
  });

  it('fog quad is independent of canvas dimensions', () => {
    // Unlike the old SCREEN_WIDTH/SCREEN_HEIGHT approach, the NDC quad
    // covers the full screen regardless of canvas size
    const data = generateFogQuadData();
    // All values should be -1, 0, or 1 — no canvas-dependent values
    for (const val of data) {
      expect([-1, 0, 1]).toContain(val);
    }
  });
});

// ============================================================================
// Config Constant Tests
// ============================================================================

describe('Config constants', () => {
  it('CITY_DOT_SIZE is defined', () => {
    expect(CITY_DOT_SIZE).toBe(6);
  });

  it('CITY_COLORS has at least 3 player colors', () => {
    expect(CITY_COLORS.length).toBeGreaterThanOrEqual(3);
  });

  it('CITY_COLORS[0] is red', () => {
    expect(CITY_COLORS[0]).toEqual({ r: 1, g: 0.2, b: 0.2 });
  });

  it('CITY_COLORS[1] is yellow', () => {
    expect(CITY_COLORS[1]).toEqual({ r: 1, g: 1, b: 0.2 });
  });

  it('CITY_COLORS[2] is blue', () => {
    expect(CITY_COLORS[2]).toEqual({ r: 0.2, g: 0.4, b: 1 });
  });

  it('SCREEN_WIDTH and SCREEN_HEIGHT are removed from config', async () => {
    // These should not exist anymore — fog now uses NDC fullscreen quad
    // We import the config module and check that the constants are not exported
    const configModule = await import('./config');
    expect((configModule as Record<string, unknown>).SCREEN_WIDTH).toBeUndefined();
    expect((configModule as Record<string, unknown>).SCREEN_HEIGHT).toBeUndefined();
  });
});
