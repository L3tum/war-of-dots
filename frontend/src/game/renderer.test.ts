// Renderer tests — run the REAL Renderer against the recording fake GPU
// (frontend/src/test/fake_gpu.ts). Geometry assertions read the recorded
// `queue.writeBuffer` payloads (world-space, as emitted — the camera transform
// lives in WGSL and is not re-implemented here).

import { describe, it, expect, vi } from 'vitest';
import { Renderer } from './renderer';
import {
  createFakeGpu,
  FakeGPUDevice,
  FakeGPURenderPassEncoder,
  Fake2DContext,
  FakeOffscreenCanvas,
  type FakeGPUBuffer,
  type WriteBufferCall,
} from '../test/fake_gpu';
import { GameState } from './game_state';
import { Unit } from './units';
import { marchingSquares, colorForType, type TerrainType } from './terrain';
import { fillTerritory, TERRITORY_NONE } from './territory';
import { detectBorderEdges } from './geometry';
import type { CityState, EquipmentState, UnitState } from '../generated/protocol';
import {
  CELL_SIZE,
  UNIT_DOT_SIZE,
  BORDER_LINE_WIDTH,
  PATH_LINE_WIDTH,
  BAR_HEIGHT,
  BAR_SPACING,
  BAR_WIDTH_MULTIPLIER,
  PLAYER_COLORS,
  CITY_COLORS,
  CITY_DOT_SIZE,
  OUTLINE_EXPAND,
  OUTLINE_COLOR,
  SYMBOL_CELL_SIZE,
  SYMBOL_ATLAS_SIZE,
} from './config';

// ============================================================================
// Fixtures & helpers
// ============================================================================

function makeTerrain(w = 32, h = 16): TerrainType[][] {
  return Array.from({ length: h }, () => Array.from({ length: w }, () => 'Plains' as TerrainType));
}

function makeUnitState(overrides: Partial<UnitState> = {}): UnitState {
  const equipment: EquipmentState[] = [];
  return {
    id: 'u1',
    owner: 'player_0',
    position: [10, 8],
    unit_type: 'infantry',
    symbol: 'S',
    men: 80,
    max_men: 100,
    organisation: 60,
    equipment,
    max_equipment: equipment,
    path: [],
    healing: false,
    resupply_enabled: true,
    supply_percent: 100,
    encircled: false,
    attacking: false,
    on_water: false,
    boat_supply_percent: 0,
    embarking: false,
    disembarking: false,
    ...overrides,
  };
}

function makeCityState(overrides: Partial<CityState> = {}): CityState {
  return {
    id: 'c1',
    position: [6, 4],
    owner: 'player_0',
    manpower: 1000,
    level: 1,
    ...overrides,
  };
}

/** A full game state: 32×16 map, one own unit, two cities, split borders, partial fog. */
function makeGameState(): GameState {
  const state = new GameState();
  state.terrain = makeTerrain();
  const w = state.terrain[0].length;
  const h = state.terrain.length;
  state.borderGrid = Array.from({ length: h }, () =>
    Array.from({ length: w }, (_, x) => (x < w / 2 ? 0 : 1)),
  );
  state.visionGrid = Array.from({ length: h }, (_, y) =>
    Array.from({ length: w }, (_, x) => (x + y) % 2 === 0),
  );
  state.units.set('u1', new Unit(makeUnitState()));
  state.cities.set('c1', makeCityState());
  state.cities.set('c2', makeCityState({ id: 'c2', position: [20, 4], owner: 'player_1' }));
  state.player = { funds: 1000, units_owned: 1, player_id: 'player_0' };
  state.ownPlayerId = 'player_0';
  return state;
}

interface Harness {
  renderer: Renderer;
  device: FakeGPUDevice;
  canvas: HTMLCanvasElement;
  unknownGpuCalls: string[];
}

/**
 * Construct the real Renderer on a fake device. The constructor's
 * setCanvasDimensions() zeroes the canvas under happy-dom (0×0 bounding rect),
 * so the pixel size is restored AFTER construction.
 */
function makeRenderer(width = 1280, height = 720): Harness {
  const { device, canvas, unknownGpuCalls } = createFakeGpu(width, height);
  const renderer = new Renderer(canvas, device as unknown as GPUDevice);
  canvas.width = width;
  canvas.height = height;
  return { renderer, device, canvas, unknownGpuCalls };
}

/** Private buffer fields the assertions read. */
function buffersOf(renderer: Renderer) {
  return renderer as unknown as {
    terrainPositionsBuffer: FakeGPUBuffer;
    terrainColorBuffer: FakeGPUBuffer;
    unitPositionsBuffer: FakeGPUBuffer;
    unitColorBuffer: FakeGPUBuffer;
    symbolPositionBuffer: FakeGPUBuffer;
    symbolColorBuffer: FakeGPUBuffer;
    borderPositionsBuffer: FakeGPUBuffer;
    borderColorBuffer: FakeGPUBuffer;
    pathPositionsBuffer: FakeGPUBuffer;
    pathColorBuffer: FakeGPUBuffer;
    barPositionsBuffer: FakeGPUBuffer;
    barColorBuffer: FakeGPUBuffer;
    cityPositionsBuffer: FakeGPUBuffer;
    cityColorBuffer: FakeGPUBuffer;
    outlinePositionsBuffer: FakeGPUBuffer;
    outlineColorBuffer: FakeGPUBuffer;
    uniformBuffer: FakeGPUBuffer;
    fogUniformBuffer: FakeGPUBuffer;
    fogPositionsBuffer: FakeGPUBuffer;
    fogTexture: { width: number; height: number } | null;
    symbolBindGroup: unknown;
    fogBindGroup: unknown;
  };
}

function writeCallsTo(queue: FakeGPUDevice['queue'], buffer: FakeGPUBuffer): WriteBufferCall[] {
  return queue.writeBufferCalls.filter((c) => c.buffer === buffer);
}

function latestPayload(queue: FakeGPUDevice['queue'], buffer: FakeGPUBuffer): Float32Array {
  const calls = writeCallsTo(queue, buffer);
  expect(calls.length).toBeGreaterThan(0);
  return calls[calls.length - 1].data as Float32Array;
}

function lastPass(device: FakeGPUDevice): FakeGPURenderPassEncoder {
  const submitted = device.queue.submitted;
  expect(submitted.length).toBeGreaterThan(0);
  const last = submitted[submitted.length - 1] as {
    renderPasses: FakeGPURenderPassEncoder[];
  };
  return last.renderPasses[0];
}

/** Buffer data is Float32 — round expected values the same way before comparing. */
function f32(values: number[]): Float32Array {
  return new Float32Array(values);
}

/** One full data-update + draw cycle, mirroring the rAF loop body. */
function frame(renderer: Renderer, device: FakeGPUDevice, state: GameState): void {
  renderer.render(state);
  renderer.renderFrame();
}

// ============================================================================
// detectBorderEdges — the REAL extracted function
// ============================================================================

describe('detectBorderEdges (real geometry.ts, filled grids)', () => {
  const plains = (w: number, h: number): TerrainType[][] =>
    Array.from({ length: h }, () => Array.from({ length: w }, () => 'Plains' as TerrainType));

  it('returns no edges for an unclaimed map', () => {
    const grid = Array.from({ length: 4 }, () => Array(4).fill(TERRITORY_NONE));
    const display = fillTerritory(grid, plains(4, 4), []);
    expect(detectBorderEdges(display)).toEqual([]);
  });

  it('draws no border between a player and unclaimed (255) land', () => {
    // The raw-grid variant excludes player-vs-NONE edges...
    const raw = [
      [0, 255],
      [0, 255],
    ];
    expect(detectBorderEdges(raw)).toEqual([]);
    // ...and fillTerritory expands the player across unclaimed land, so the
    // display grid also yields no border.
    const display = fillTerritory(
      [
        [0, 255],
        [0, 255],
      ],
      plains(2, 2),
      [],
    );
    expect(display.every((row) => row.every((c) => c === 0))).toBe(true);
    expect(detectBorderEdges(display)).toEqual([]);
  });

  it('detects a full vertical border between two players (left/right columns)', () => {
    const grid = [
      [0, 1],
      [0, 1],
    ];
    const display = fillTerritory(grid, plains(2, 2), []);
    // 'v' = edge to the right-hand neighbour → drawn as a vertical line
    expect(detectBorderEdges(display)).toEqual([
      { gridX: 0, gridY: 0, dir: 'v', ownerA: 0, ownerB: 1 },
      { gridX: 0, gridY: 1, dir: 'v', ownerA: 0, ownerB: 1 },
    ]);
  });

  it('detects a full horizontal border between two players (top/bottom rows)', () => {
    const grid = [
      [0, 0],
      [1, 1],
    ];
    const display = fillTerritory(grid, plains(2, 2), []);
    // 'h' = edge to the lower neighbour → drawn as a horizontal line
    expect(detectBorderEdges(display)).toEqual([
      { gridX: 0, gridY: 0, dir: 'h', ownerA: 0, ownerB: 1 },
      { gridX: 1, gridY: 0, dir: 'h', ownerA: 0, ownerB: 1 },
    ]);
  });

  it('detects both edge orientations at a corner', () => {
    const grid = [
      [0, 1],
      [1, 1],
    ];
    // Right-neighbour ('v') edge is pushed before the lower-neighbour ('h') edge
    expect(detectBorderEdges(fillTerritory(grid, plains(2, 2), []))).toEqual([
      { gridX: 0, gridY: 0, dir: 'v', ownerA: 0, ownerB: 1 },
      { gridX: 0, gridY: 0, dir: 'h', ownerA: 0, ownerB: 1 },
    ]);
  });

  it('returns empty for a 1×1 grid and for a single-owner map', () => {
    expect(detectBorderEdges([[0]])).toEqual([]);
    const grid = [
      [0, 0],
      [0, 0],
    ];
    expect(detectBorderEdges(fillTerritory(grid, plains(2, 2), []))).toEqual([]);
  });

  it('keeps the single player-vs-player border when NONE is adjacent', () => {
    // row: 255 | 0 | 1 → one horizontal border between 0 and 1 at x=1
    const raw = [
      [255, 0, 1],
      [255, 0, 1],
    ];
    expect(detectBorderEdges(raw)).toEqual([
      { gridX: 1, gridY: 0, dir: 'v', ownerA: 0, ownerB: 1 },
      { gridX: 1, gridY: 1, dir: 'v', ownerA: 0, ownerB: 1 },
    ]);
    const grid = Array.from({ length: 8 }, () => Array(8).fill(TERRITORY_NONE));
    const display = fillTerritory(grid, plains(8, 8), [
      { x: 0, y: 0, ownerIdx: 0 },
      { x: 7, y: 7, ownerIdx: 1 },
    ]);
    // Both territories claim land and meet somewhere
    expect(display[0][0]).toBe(0);
    expect(display[7][7]).toBe(1);
    const edges = detectBorderEdges(display);
    expect(edges.length).toBeGreaterThan(0);
    for (const e of edges) {
      expect([e.ownerA, e.ownerB].sort()).toEqual([0, 1]);
    }
  });

  it('does not expand territories through water', () => {
    // 0 0 W 1 — water column separates the players; no 0-vs-1 border exists.
    const terrain = [
      ['Plains', 'Plains', 'Water', 'Plains'],
      ['Plains', 'Plains', 'Water', 'Plains'],
    ] as TerrainType[][];
    const grid = [
      [0, 0, 255, 1],
      [0, 0, 255, 1],
    ];
    const display = fillTerritory(grid, terrain, []);
    expect(display[0]).toEqual([0, 0, 255, 1]);
    expect(detectBorderEdges(display)).toEqual([]);
  });
});

// ============================================================================
// Renderer construction on the fake device
// ============================================================================

describe('Renderer construction (fake GPU)', () => {
  it('creates the five pipelines and configures the canvas context', () => {
    const { device, canvas } = makeRenderer();
    expect(device.createdPipelines).toHaveLength(5);
    const ctx = canvas.getContext('webgpu') as unknown as {
      configured: { device: unknown; format: string } | null;
    };
    expect(ctx.configured?.format).toBe('bgra8unorm');
    expect(ctx.configured?.device).toBe(device);
  });

  it('uploads the initial uniform buffer (camera 0,0,1)', () => {
    const { device } = makeRenderer();
    expect(device.queue.writeBufferCalls.length).toBeGreaterThan(0);
  });

  it('keeps an explicit canvas size after construction', () => {
    const { canvas } = makeRenderer(640, 480);
    expect(canvas.width).toBe(640);
    expect(canvas.height).toBe(480);
  });
});

// ============================================================================
// Geometry payloads — recorded writeBuffer content (world space)
// ============================================================================

describe('Renderer geometry payloads (recorded writeBuffer)', () => {
  const CAM = { x: 512, y: 256, zoom: 1 };

  function setup(): Harness {
    const harness = makeRenderer();
    harness.renderer.camera = { ...CAM };
    return harness;
  }

  it('only ever calls WebGPU/2D methods the fake implements (no silent no-ops)', () => {
    const harness = setup();
    harness.renderer.render(makeGameState());
    // The device proxy, canvas-context proxy, and symbol-canvas 2D proxy all
    // record any call to a method the fakes don't implement; a non-empty list
    // means the renderer started using an un-faked API and the suite would
    // otherwise stay green.
    expect(harness.unknownGpuCalls, `un-faked device calls: ${harness.unknownGpuCalls}`).toEqual(
      [],
    );
    const symbolCanvas = (harness.renderer as unknown as { symbolCanvas: FakeOffscreenCanvas })
      .symbolCanvas;
    expect(symbolCanvas.unknown2dCalls, 'un-faked 2d context calls').toEqual([]);
  });

  it('terrain buffer carries the marchingSquares mesh verbatim', () => {
    const { renderer, device } = setup();
    const state = makeGameState();
    renderer.render(state);
    const b = buffersOf(renderer);
    const expected = marchingSquares(state.terrain, CELL_SIZE, CAM, CAM.zoom, 1280, 720);
    expect(expected.positions.length).toBeGreaterThan(0);
    expect(latestPayload(device.queue, b.terrainPositionsBuffer)).toEqual(
      new Float32Array(expected.positions),
    );
    expect(latestPayload(device.queue, b.terrainColorBuffer)).toEqual(
      new Float32Array(expected.colors),
    );
    expect(renderer.terrainVertexCount).toBe(expected.positions.length / 2);
  });

  it('uploads unit dots at world-space cell centers with owner color', () => {
    const { renderer, device } = setup();
    renderer.render(makeGameState());
    const b = buffersOf(renderer);
    const p = latestPayload(device.queue, b.unitPositionsBuffer);
    // Unit at (10, 8) → world (320, 256), half-size = UNIT_DOT_SIZE/2/zoom
    const wx = 10 * CELL_SIZE;
    const wy = 8 * CELL_SIZE;
    const half = UNIT_DOT_SIZE / 2 / CAM.zoom;
    expect(p).toEqual(
      f32([wx - half, wy - half, wx + half, wy - half, wx + half, wy + half, wx - half, wy + half]),
    );
    const c = latestPayload(device.queue, b.unitColorBuffer);
    expect(c).toEqual(
      f32([
        PLAYER_COLORS[0].r,
        PLAYER_COLORS[0].g,
        PLAYER_COLORS[0].b,
        PLAYER_COLORS[0].r,
        PLAYER_COLORS[0].g,
        PLAYER_COLORS[0].b,
        PLAYER_COLORS[0].r,
        PLAYER_COLORS[0].g,
        PLAYER_COLORS[0].b,
        PLAYER_COLORS[0].r,
        PLAYER_COLORS[0].g,
        PLAYER_COLORS[0].b,
      ]),
    );
    expect(renderer.unitVertexCount).toBe(4);
  });

  it('interleaves symbol positions and UVs (stride 16) at 0.8× dot size', () => {
    const { renderer, device } = setup();
    renderer.render(makeGameState());
    const b = buffersOf(renderer);
    const p = latestPayload(device.queue, b.symbolPositionBuffer);
    const wx = 10 * CELL_SIZE;
    const wy = 8 * CELL_SIZE;
    const half = (UNIT_DOT_SIZE / 2 / CAM.zoom) * 0.8;
    const uv = SYMBOL_CELL_SIZE / SYMBOL_ATLAS_SIZE;
    expect(p.length).toBe(16); // 4 vertices × 4 floats
    expect(p).toEqual(
      f32([
        wx - half,
        wy - half,
        0,
        0,
        wx + half,
        wy - half,
        uv,
        0,
        wx + half,
        wy + half,
        uv,
        uv,
        wx - half,
        wy + half,
        0,
        uv,
      ]),
    );
  });

  it('emits three status bars per unit with percent widths', () => {
    const { renderer, device } = setup();
    renderer.render(makeGameState());
    const b = buffersOf(renderer);
    const p = latestPayload(device.queue, b.barPositionsBuffer);
    const wx = 10 * CELL_SIZE;
    const wy = 8 * CELL_SIZE;
    const symHalf = (UNIT_DOT_SIZE / 2 / CAM.zoom) * 0.8;
    const barWidth = symHalf * BAR_WIDTH_MULTIPLIER;
    const bx = wx - barWidth / 2;
    let barY = wy - symHalf - BAR_SPACING / CAM.zoom;
    const percents = [80, 60, 100]; // men%, org%, supply%
    for (let i = 0; i < 3; i++) {
      const actual = (barWidth * percents[i]) / 100;
      const expected = [
        bx,
        barY,
        bx + actual,
        barY,
        bx + actual,
        barY + BAR_HEIGHT,
        bx,
        barY + BAR_HEIGHT,
      ];
      expect(new Float32Array(p.slice(i * 8, i * 8 + 8))).toEqual(f32(expected));
      barY -= BAR_SPACING / CAM.zoom;
    }
    expect(renderer.barVertexCount).toBe(12);
  });

  it('renders each border as two owner-coloured half-width quads (world space)', () => {
    const { renderer, device } = setup();
    renderer.render(makeGameState());
    const b = buffersOf(renderer);
    // 32×16 split at col 16 → 16 vertical edges at gridX=15
    expect(renderer.borderVertexCount).toBe(16 * 8);
    const p = latestPayload(device.queue, b.borderPositionsBuffer);
    const c = latestPayload(device.queue, b.borderColorBuffer);
    const lw = BORDER_LINE_WIDTH / CAM.zoom;
    const wx = 16 * CELL_SIZE;
    const wy = 0 * CELL_SIZE;
    // First edge (gridY=0): quad A (owner 0) then quad B (owner 1)
    expect(new Float32Array(p.slice(0, 16))).toEqual(
      f32([
        wx - lw,
        wy,
        wx,
        wy,
        wx,
        wy + CELL_SIZE,
        wx - lw,
        wy + CELL_SIZE,
        wx,
        wy,
        wx + lw,
        wy,
        wx + lw,
        wy + CELL_SIZE,
        wx,
        wy + CELL_SIZE,
      ]),
    );
    expect(new Float32Array(c.slice(0, 24))).toEqual(
      f32([
        PLAYER_COLORS[0].r,
        PLAYER_COLORS[0].g,
        PLAYER_COLORS[0].b,
        PLAYER_COLORS[0].r,
        PLAYER_COLORS[0].g,
        PLAYER_COLORS[0].b,
        PLAYER_COLORS[0].r,
        PLAYER_COLORS[0].g,
        PLAYER_COLORS[0].b,
        PLAYER_COLORS[0].r,
        PLAYER_COLORS[0].g,
        PLAYER_COLORS[0].b,
        PLAYER_COLORS[1].r,
        PLAYER_COLORS[1].g,
        PLAYER_COLORS[1].b,
        PLAYER_COLORS[1].r,
        PLAYER_COLORS[1].g,
        PLAYER_COLORS[1].b,
        PLAYER_COLORS[1].r,
        PLAYER_COLORS[1].g,
        PLAYER_COLORS[1].b,
        PLAYER_COLORS[1].r,
        PLAYER_COLORS[1].g,
        PLAYER_COLORS[1].b,
      ]),
    );
  });

  it('border quads scale inversely with zoom (constant screen width)', () => {
    const h = setup();
    h.renderer.camera.zoom = 2;
    h.renderer.render(makeGameState());
    const b = buffersOf(h.renderer);
    const p = latestPayload(h.device.queue, b.borderPositionsBuffer);
    const lw = BORDER_LINE_WIDTH / 2;
    const wx = 16 * CELL_SIZE;
    expect(p[0]).toBe(wx - lw);
    expect(p[1]).toBe(0); // wy = 0 for first edge
    expect(p[8]).toBe(wx);
    expect(p[10] - p[8]).toBe(lw);
  });

  it('uploads path preview quads in world space', () => {
    const { renderer, device } = setup();
    renderer.currentPath = [
      { x: 5, y: 4 },
      { x: 6, y: 4 },
    ];
    renderer.render(makeGameState());
    const b = buffersOf(renderer);
    const p = latestPayload(device.queue, b.pathPositionsBuffer);
    expect(p).toEqual(
      f32([
        5 * CELL_SIZE - PATH_LINE_WIDTH,
        4 * CELL_SIZE,
        5 * CELL_SIZE + PATH_LINE_WIDTH,
        4 * CELL_SIZE,
        6 * CELL_SIZE + PATH_LINE_WIDTH,
        4 * CELL_SIZE,
        6 * CELL_SIZE - PATH_LINE_WIDTH,
        4 * CELL_SIZE,
      ]),
    );
    expect(renderer.pathVertexCount).toBe(4);
  });

  it('city dots: own city 1.5× with 50% white mix, other city 1× flat color', () => {
    const { renderer, device } = setup();
    renderer.render(makeGameState());
    const b = buffersOf(renderer);
    const p = latestPayload(device.queue, b.cityPositionsBuffer);
    const c = latestPayload(device.queue, b.cityColorBuffer);
    // City A (own) at (6,4): dot 18, half 9. City B at (20,4): dot 12, half 6.
    const ax = 6 * CELL_SIZE,
      ay = 4 * CELL_SIZE;
    const bx = 20 * CELL_SIZE,
      by = 4 * CELL_SIZE;
    expect(p).toEqual(
      f32([
        ax - 9,
        ay - 9,
        ax + 9,
        ay - 9,
        ax + 9,
        ay + 9,
        ax - 9,
        ay + 9,
        bx - 6,
        by - 6,
        bx + 6,
        by - 6,
        bx + 6,
        by + 6,
        bx - 6,
        by + 6,
      ]),
    );
    // Own city color: CITY_COLORS[0] mixed 50% white
    expect(c).toEqual(
      f32([
        CITY_COLORS[0].r * 0.5 + 0.5,
        CITY_COLORS[0].g * 0.5 + 0.5,
        CITY_COLORS[0].b * 0.5 + 0.5,
        CITY_COLORS[0].r * 0.5 + 0.5,
        CITY_COLORS[0].g * 0.5 + 0.5,
        CITY_COLORS[0].b * 0.5 + 0.5,
        CITY_COLORS[0].r * 0.5 + 0.5,
        CITY_COLORS[0].g * 0.5 + 0.5,
        CITY_COLORS[0].b * 0.5 + 0.5,
        CITY_COLORS[0].r * 0.5 + 0.5,
        CITY_COLORS[0].g * 0.5 + 0.5,
        CITY_COLORS[0].b * 0.5 + 0.5,
        CITY_COLORS[1].r,
        CITY_COLORS[1].g,
        CITY_COLORS[1].b,
        CITY_COLORS[1].r,
        CITY_COLORS[1].g,
        CITY_COLORS[1].b,
        CITY_COLORS[1].r,
        CITY_COLORS[1].g,
        CITY_COLORS[1].b,
        CITY_COLORS[1].r,
        CITY_COLORS[1].g,
        CITY_COLORS[1].b,
      ]),
    );
    expect(renderer.cityVertexCount).toBe(8);
  });

  it('city dots keep constant screen size across zoom', () => {
    const h1 = setup();
    h1.renderer.render(makeGameState());
    const b1 = buffersOf(h1.renderer);
    const p1 = latestPayload(h1.device.queue, b1.cityPositionsBuffer);
    const widthAtZoom1 = p1[2] - p1[0];

    const h2 = makeRenderer();
    h2.renderer.camera = { ...CAM, zoom: 2 };
    h2.renderer.render(makeGameState());
    const b2 = buffersOf(h2.renderer);
    const p2 = latestPayload(h2.device.queue, b2.cityPositionsBuffer);
    const widthAtZoom2 = p2[2] - p2[0];

    // Screen-space constancy: worldWidth * zoom = constant dot screen size
    expect(widthAtZoom1 * 1).toBeCloseTo(CITY_DOT_SIZE * 1.5, 5);
    expect(widthAtZoom2 * 2).toBeCloseTo(CITY_DOT_SIZE * 1.5, 5);
  });

  it('outline halo covers own units and cities only', () => {
    const { renderer, device } = setup();
    renderer.render(makeGameState());
    const b = buffersOf(renderer);
    const p = latestPayload(device.queue, b.outlinePositionsBuffer);
    const c = latestPayload(device.queue, b.outlineColorBuffer);
    // Own unit outline first (half = UNIT_DOT_SIZE/2 + OUTLINE_EXPAND = 7)
    const wx = 10 * CELL_SIZE,
      wy = 8 * CELL_SIZE;
    const uHalf = UNIT_DOT_SIZE / 2 + OUTLINE_EXPAND;
    expect(Array.from(p.slice(0, 8))).toEqual([
      wx - uHalf,
      wy - uHalf,
      wx + uHalf,
      wy - uHalf,
      wx + uHalf,
      wy + uHalf,
      wx - uHalf,
      wy + uHalf,
    ]);
    // Then own city (half = CITY_DOT_SIZE/2 + OUTLINE_EXPAND = 9)
    const cx = 6 * CELL_SIZE,
      cy = 4 * CELL_SIZE;
    const cHalf = CITY_DOT_SIZE / 2 + OUTLINE_EXPAND;
    expect(Array.from(p.slice(8, 16))).toEqual([
      cx - cHalf,
      cy - cHalf,
      cx + cHalf,
      cy - cHalf,
      cx + cHalf,
      cy + cHalf,
      cx - cHalf,
      cy + cHalf,
    ]);
    expect(p.length).toBe(16); // opponent city NOT outlined
    for (let i = 0; i < 8; i++) {
      expect([c[i * 3], c[i * 3 + 1], c[i * 3 + 2]]).toEqual([
        OUTLINE_COLOR.r,
        OUTLINE_COLOR.g,
        OUTLINE_COLOR.b,
      ]);
    }
    expect(renderer.outlineVertexCount).toBe(8);
  });

  it('uploads fog texture with alpha 255 where invisible', () => {
    const { renderer, device } = setup();
    const state = makeGameState();
    renderer.render(state);
    const calls = device.queue.writeTextureCalls;
    expect(calls.length).toBe(1);
    const call = calls[0];
    const w = state.terrain[0].length,
      h = state.terrain.length;
    expect(call.size).toEqual([w, h, 1]);
    expect(call.layout.bytesPerRow).toBe(w * 4);
    expect(call.data.length).toBe(w * h * 4);
    for (let y = 0; y < h; y++) {
      for (let x = 0; x < w; x++) {
        const visible = state.visionGrid[y][x];
        const alpha = call.data[(y * w + x) * 4 + 3];
        expect(alpha).toBe(visible ? 0 : 255);
      }
    }
  });

  it('uploads the symbol atlas texture when the unit composition changes', () => {
    const { renderer, device } = setup();
    const state = makeGameState();
    renderer.render(state);
    expect(device.queue.copyExternalImageCalls).toHaveLength(1);
    const call = device.queue.copyExternalImageCalls[0];
    expect(call.size).toEqual([SYMBOL_ATLAS_SIZE, SYMBOL_ATLAS_SIZE, 1]);
    // The atlas texture is the first texture the constructor creates
    expect(call.texture).toBe(device.createdTextures[0]);
    // Unit composition unchanged → second render must NOT re-upload the atlas
    renderer.render(state);
    expect(device.queue.copyExternalImageCalls).toHaveLength(1);
    // New unit type → re-upload
    state.units.set('u2', new Unit(makeUnitState({ id: 'u2', symbol: 'N', position: [11, 8] })));
    renderer.render(state);
    expect(device.queue.copyExternalImageCalls).toHaveLength(2);
  });

  it('paints unit symbols onto the 2D atlas when the composition changes', () => {
    const { renderer } = setup();
    const ctx = (renderer as unknown as { symbolCtx: Fake2DContext }).symbolCtx;
    const state = makeGameState();
    renderer.render(state);
    // Initial render must actually paint the glyph (regression: the draw gate
    // compared against an already-committed cache key and never fired).
    const texts = ctx.fillTextCalls.map((c) => c.text);
    expect(texts).toContain('S');
    // Composition unchanged → no repaint on the next frame
    renderer.render(state);
    expect(ctx.fillTextCalls).toHaveLength(texts.length);
    // Composition changed → repaints, including the new symbol
    state.units.set('u2', new Unit(makeUnitState({ id: 'u2', symbol: 'N', position: [11, 8] })));
    renderer.render(state);
    expect(ctx.fillTextCalls.map((c) => c.text)).toContain('N');
    expect(ctx.fillTextCalls.length).toBeGreaterThan(texts.length);
  });

  it('writes camera + canvas uniforms each frame', () => {
    const { renderer, device } = setup();
    renderer.renderFrame();
    const b = buffersOf(renderer);
    const p = latestPayload(device.queue, b.uniformBuffer);
    expect(Array.from(p)).toEqual([CAM.x, CAM.y, CAM.zoom, 1280, 720]);
  });

  it('writes fog uniforms including CELL_SIZE once fog exists', () => {
    const { renderer, device } = setup();
    renderer.render(makeGameState());
    renderer.renderFrame();
    const b = buffersOf(renderer);
    const p = latestPayload(device.queue, b.fogUniformBuffer);
    expect(Array.from(p)).toEqual([CAM.x, CAM.y, CAM.zoom, 1280, 720, CELL_SIZE]);
  });

  it('renders only terrain + fog for a unit-less state', () => {
    const { renderer, device } = setup();
    const empty = new GameState();
    empty.terrain = makeTerrain();
    empty.visionGrid = Array.from({ length: 16 }, () => Array(32).fill(true));
    frame(renderer, device, empty);
    expect(renderer.unitVertexCount).toBe(0);
    expect(renderer.borderVertexCount).toBe(0);
    expect(renderer.outlineVertexCount).toBe(0);
    expect(renderer.cityVertexCount).toBe(0);
    expect(renderer.lastRenderError).toBeNull();
    const pass = lastPass(device);
    const [main, , , , fog] = device.createdPipelines;
    expect(pass.draws.map((d) => d.pipeline)).toEqual([main, fog]);
    expect(pass.draws[1].vertexCount).toBe(4);
  });

  it('ignores a state without terrain (lobby) and uploads nothing', () => {
    const { renderer, device } = setup();
    const before = device.queue.writeBufferCalls.length;
    renderer.render(new GameState());
    expect(device.queue.writeBufferCalls.length).toBe(before);
  });
});

// ============================================================================
// Draw sequence
// ============================================================================

describe('renderFrame draw sequence', () => {
  it('draws passes in z-order with the cached vertex counts', () => {
    const { renderer, device } = makeRenderer();
    renderer.camera = { x: 512, y: 256, zoom: 1 };
    const state = makeGameState();
    frame(renderer, device, state);

    const pass = lastPass(device);
    expect(pass.clearValue).toEqual({ r: 0.05, g: 0.05, b: 0.1, a: 1 });

    const [main, symbol, line, outline, fog] = device.createdPipelines;
    const draws = pass.draws.map((d) => [d.pipeline, d.vertexCount] as const);
    const expectedTerrainCount =
      marchingSquares(state.terrain, CELL_SIZE, renderer.camera, 1, 1280, 720).positions.length / 2;
    // z-order: terrain → borders → outlines → cities → units → bars → symbols → fog
    // (path draw skipped — currentPath empty)
    expect(draws).toEqual([
      [main, expectedTerrainCount],
      [line, renderer.borderVertexCount],
      [outline, renderer.outlineVertexCount],
      [main, renderer.cityVertexCount],
      [main, renderer.unitVertexCount],
      [main, renderer.barVertexCount],
      [symbol, renderer.unitVertexCount],
      [fog, 4],
    ]);

    // The recorded vertex-buffer / bind-group bindings (previously captured
    // but never asserted) form a net against the "section binds nothing / wrong
    // buffer" regression class. Identity assertions, not slot checks: a slot-
    // only check (`some(g => g.index === 1)`) is satisfied by the fog section,
    // which binds slot 1 AFTER symbols and would mask a missing symbol bind
    // (blank glyphs in a real browser).
    const b = buffersOf(renderer);
    const boundBuffers = pass.vertexBuffers.map((v) => v.buffer);
    expect(boundBuffers).toContain(b.symbolPositionBuffer);
    expect(boundBuffers).toContain(b.symbolColorBuffer);
    expect(boundBuffers).toContain(b.fogPositionsBuffer);
    expect(b.symbolBindGroup, 'symbol bind group must exist when symbols draw').not.toBeNull();
    expect(b.fogBindGroup, 'fog bind group must exist when fog draws').not.toBeNull();
    expect(
      pass.bindGroups.some((g) => g.index === 1 && g.group === b.symbolBindGroup),
      'symbol texture/sampler bind group must be bound on slot 1',
    ).toBe(true);
    expect(
      pass.bindGroups.some((g) => g.index === 1 && g.group === b.fogBindGroup),
      'fog texture bind group must be bound on slot 1',
    ).toBe(true);
  });
});

// ============================================================================
// Cache / dirty-flag gating
// ============================================================================

describe('dirty-flag and cache gating', () => {
  it('terrain cache: no re-upload while camera+terrain unchanged, re-upload after camera move', () => {
    const { renderer, device } = makeRenderer();
    renderer.camera = { x: 512, y: 256, zoom: 1 };
    const state = makeGameState();
    const b = buffersOf(renderer);

    renderer.render(state);
    const afterFirst = writeCallsTo(device.queue, b.terrainPositionsBuffer).length;
    expect(afterFirst).toBe(1);

    renderer.render(state);
    expect(writeCallsTo(device.queue, b.terrainPositionsBuffer).length).toBe(afterFirst);

    // Camera pan changes the visible range → recompute + re-upload
    // (512 px moves the clamped tStartCol; smaller pans clamp at the map edge)
    renderer.camera.x += 512;
    renderer.render(state);
    expect(writeCallsTo(device.queue, b.terrainPositionsBuffer).length).toBe(afterFirst + 1);
  });

  it('terrain cache: a different terrain reference invalidates the cache', () => {
    const { renderer, device } = makeRenderer();
    renderer.camera = { x: 512, y: 256, zoom: 1 };
    const state = makeGameState();
    const b = buffersOf(renderer);
    renderer.render(state);
    const afterFirst = writeCallsTo(device.queue, b.terrainPositionsBuffer).length;
    state.terrain = makeTerrain(); // new reference, same content
    renderer.render(state);
    expect(writeCallsTo(device.queue, b.terrainPositionsBuffer).length).toBe(afterFirst + 1);
  });

  it('fog: uploaded once, re-uploaded only after markFogDirty or size change', () => {
    const { renderer, device } = makeRenderer();
    renderer.camera = { x: 512, y: 256, zoom: 1 };
    const state = makeGameState();

    renderer.render(state);
    expect(device.queue.writeTextureCalls.length).toBe(1);

    renderer.render(state);
    expect(device.queue.writeTextureCalls.length).toBe(1);

    renderer.markFogDirty();
    renderer.render(state);
    expect(device.queue.writeTextureCalls.length).toBe(2);

    renderer.render(state);
    expect(device.queue.writeTextureCalls.length).toBe(2);
  });

  it('fog: a differently sized vision grid forces a re-upload', () => {
    const { renderer, device } = makeRenderer();
    renderer.camera = { x: 512, y: 256, zoom: 1 };
    const state = makeGameState();
    renderer.render(state);
    renderer.render(state);
    expect(device.queue.writeTextureCalls.length).toBe(1);
    state.visionGrid = Array.from({ length: 8 }, () => Array(8).fill(true));
    renderer.render(state);
    expect(device.queue.writeTextureCalls.length).toBe(2);
  });

  it('rAF frame gate: updateCallback fires on key change, suppressed while idle', async () => {
    const { renderer } = makeRenderer();
    const state = makeGameState();

    let callbacks = 0;
    renderer.setUpdateCallback(() => {
      callbacks++;
      renderer.render(state);
    });

    // Pre-consume the initial fog/border dirty flags — they flip on first
    // render and would otherwise change the frame key once more.
    renderer.render(state);

    vi.stubGlobal('requestAnimationFrame', (cb: FrameRequestCallback) => setTimeout(cb, 16));
    vi.useFakeTimers();
    try {
      renderer.startRenderLoop();
      await vi.advanceTimersByTimeAsync(16);
      expect(callbacks).toBe(1);

      // Same camera/canvas/data → key unchanged → no callback
      await vi.advanceTimersByTimeAsync(16);
      expect(callbacks).toBe(1);

      // markDataDirty changes the frame key → next frame runs
      renderer.markDataDirty();
      await vi.advanceTimersByTimeAsync(16);
      expect(callbacks).toBe(2);
    } finally {
      vi.useRealTimers();
      vi.unstubAllGlobals();
    }
  });
});

// ============================================================================
// Error surfacing
// ============================================================================

describe('render error surfacing', () => {
  it('records lastRenderError when the queue fails mid-frame', () => {
    const { renderer, device } = makeRenderer();
    device.queue.submit = () => {
      throw new Error('swiftshader exploded');
    };
    renderer.renderFrame();
    expect(renderer.lastRenderError).toBe('swiftshader exploded');
    // A healthy queue must not throw further
    device.queue.submit = () => {};
    expect(() => renderer.renderFrame()).not.toThrow();
  });
});

// ============================================================================
// Terrain colors (real terrain.ts)
// ============================================================================

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
      expect(colorForType(type as TerrainType)).toBe(expected);
    }
  });

  it('falls back to Plains for unknown terrain', () => {
    expect(colorForType('Unknown' as TerrainType)).toBe('#7eb547');
  });
});

// ============================================================================
// Config constants
// ============================================================================

describe('Config constants', () => {
  it('CITY_DOT_SIZE is defined', () => {
    expect(CITY_DOT_SIZE).toBe(12);
  });

  it('CITY_COLORS has 6 distinct colors', () => {
    expect(CITY_COLORS.length).toBe(6);
    for (let i = 0; i < CITY_COLORS.length; i++) {
      for (let j = i + 1; j < CITY_COLORS.length; j++) {
        expect(CITY_COLORS[i]).not.toEqual(CITY_COLORS[j]);
      }
    }
  });

  it('PLAYER_COLORS has 6 distinct colors', () => {
    expect(PLAYER_COLORS.length).toBe(6);
    for (let i = 0; i < PLAYER_COLORS.length; i++) {
      for (let j = i + 1; j < PLAYER_COLORS.length; j++) {
        expect(PLAYER_COLORS[i]).not.toEqual(PLAYER_COLORS[j]);
      }
    }
  });
});

// ============================================================================
// Perf #3 — territory/fog change detection
// (the dirty flags fire on every delta; the pipeline must skip the full
//  40k-cell recompute when its inputs actually changed is false)
// ============================================================================

describe('Perf #3 — territory + fog change detection', () => {
  const CAM = { x: 512, y: 256, zoom: 1 };

  function setup(): Harness {
    const harness = makeRenderer();
    harness.renderer.camera = { ...CAM };
    return harness;
  }

  it('skips the territory BFS when grid + cities are unchanged', () => {
    const { renderer } = setup();
    const state = makeGameState();
    renderer.render(state);
    expect(renderer.territoryRecomputeCount).toBe(1);

    // Dirty flag alone (no content change) must not trigger a BFS
    renderer.markBorderDirty();
    renderer.render(state);
    expect(renderer.territoryRecomputeCount).toBe(1);

    // A single-cell grid change → recompute
    state.borderGrid[2][3] = state.borderGrid[2][3] === 0 ? 1 : 0;
    renderer.markBorderDirty();
    renderer.render(state);
    expect(renderer.territoryRecomputeCount).toBe(2);

    // A city owner change is an input too (anchors feed the BFS)
    state.cities.get('c1')!.owner = 'player_1';
    renderer.markBorderDirty();
    renderer.render(state);
    expect(renderer.territoryRecomputeCount).toBe(3);
  });

  it('a city position change triggers a recompute (anchor key)', () => {
    const { renderer } = setup();
    const state = makeGameState();
    renderer.render(state);
    expect(renderer.territoryRecomputeCount).toBe(1);

    state.cities.get('c1')!.position = [7, 5];
    renderer.markBorderDirty();
    renderer.render(state);
    expect(renderer.territoryRecomputeCount).toBe(2);
  });

  it('keeps the border upload identical across a skipped pass', () => {
    const { renderer, device } = setup();
    const state = makeGameState();
    renderer.render(state);
    const b = buffersOf(renderer);
    const first = new Float32Array(latestPayload(device.queue, b.borderPositionsBuffer));

    // Same content, dirty flag set again → no BFS, same uploaded positions
    renderer.markBorderDirty();
    renderer.render(state);
    const again = latestPayload(device.queue, b.borderPositionsBuffer);
    expect(again).toEqual(first);
  });

  it('skips the border position upload when edges + zoom are unchanged; re-uploads on zoom change', () => {
    const { renderer, device } = setup();
    const state = makeGameState();
    renderer.render(state);
    const b = buffersOf(renderer);
    const writes1 = writeCallsTo(device.queue, b.borderPositionsBuffer).length;
    expect(writes1).toBe(1);

    // Same edges + same zoom (e.g. a unit-movement frame) → no re-upload
    renderer.markBorderDirty();
    renderer.render(state);
    expect(writeCallsTo(device.queue, b.borderPositionsBuffer).length).toBe(writes1);

    // Zoom change → line width changes → re-upload
    renderer.camera.zoom = 2;
    renderer.render(state);
    expect(writeCallsTo(device.queue, b.borderPositionsBuffer).length).toBe(writes1 + 1);
    expect(latestPayload(device.queue, b.borderPositionsBuffer).length).toBeGreaterThan(0);
  });

  it('clearing the border grid drops the borders; re-sending identical content restores them', () => {
    const { renderer, device } = setup();
    const state = makeGameState();
    const original = state.borderGrid;
    renderer.render(state);
    const b = buffersOf(renderer);
    expect(renderer.territoryRecomputeCount).toBe(1);

    // Grid cleared → no borders, no recompute
    state.borderGrid = [];
    renderer.markBorderDirty();
    renderer.render(state);
    expect(renderer.territoryRecomputeCount).toBe(1);

    // Re-send the exact same grid content → must recompute + re-upload
    // (the cleared pass must have reset the committed snapshot)
    state.borderGrid = original;
    renderer.markBorderDirty();
    renderer.render(state);
    expect(renderer.territoryRecomputeCount).toBe(2);
    expect(writeCallsTo(device.queue, b.borderPositionsBuffer).length).toBeGreaterThanOrEqual(2);
  });

  it('fog: flag forces a rewrite, flagless renders stay stable, uploads mirror content', () => {
    const { renderer, device } = setup();
    const state = makeGameState();
    renderer.render(state);
    expect(device.queue.writeTextureCalls.length).toBe(1);

    // No flag, no content change → no rewrite
    renderer.render(state);
    expect(device.queue.writeTextureCalls.length).toBe(1);

    // Flag → rewrite (existing contract) — even when content is identical
    renderer.markFogDirty();
    renderer.render(state);
    expect(device.queue.writeTextureCalls.length).toBe(2);

    // Flag cleared → stable again
    renderer.render(state);
    expect(device.queue.writeTextureCalls.length).toBe(2);

    // A real content flip (flagged) rewrites with the flipped alpha
    state.visionGrid[0][0] = !state.visionGrid[0][0];
    renderer.markFogDirty();
    renderer.render(state);
    expect(device.queue.writeTextureCalls.length).toBe(3);
    const call = device.queue.writeTextureCalls[2];
    expect(call.data.length).toBe(state.terrain.length * state.terrain[0].length * 4);
    expect(call.data[0 * 4 + 3]).toBe(state.visionGrid[0][0] ? 0 : 255);
    expect(call.data[1 * 4 + 3]).toBe(state.visionGrid[0][1] ? 0 : 255);
  });

  it('fog alpha still matches vision content after repeated content flips', () => {
    const { renderer, device } = setup();
    const state = makeGameState();
    const w = state.terrain[0].length;

    renderer.render(state); // initial upload

    for (let i = 0; i < 5; i++) {
      state.visionGrid[i % 2][i % w] = !state.visionGrid[i % 2][i % w];
      renderer.markFogDirty();
      renderer.render(state);
    }
    expect(device.queue.writeTextureCalls.length).toBe(6); // 1 initial + 5 flips
    const data = new Uint8Array(device.queue.writeTextureCalls[5].data);
    for (let y = 0; y < state.visionGrid.length; y++) {
      for (let x = 0; x < w; x++) {
        expect(data[(y * w + x) * 4 + 3]).toBe(state.visionGrid[y][x] ? 0 : 255);
      }
    }
  });
});
