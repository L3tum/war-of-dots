//! Controls tests — unit/city selection, path drawing, zoom clamping, and the
//! MoveUnit send contract — against a fake canvas (happy-dom + fake GPU).
//!
//! NOTE (MoveUnit reachability): in the committed UI flow, a left-click on
//! empty ground both deselects the unit AND seeds the path, while the
//! pointerup handler only sends `MoveUnit` when a selection is still active —
//! so the full click→drag→click sequence can never deliver a MoveUnit from
//! the browser UI today. That pre-existing interaction gap is flagged for the
//! frontend owners; this suite therefore exercises the pointerup send
//! contract directly (selection + drawn path → send) rather than pretending
//! the end-to-end click flow sends.

import { describe, it, expect, beforeEach, afterEach, vi } from 'vitest';
import { createFakeGpu } from '../test/fake_gpu';
import { Renderer } from '../game/renderer';
import { GameState } from '../game/game_state';
import { Unit } from '../game/units';
import { Controls } from './controls';
import { CELL_SIZE } from '../game/config';
import type { Connection } from '../net/connection';
import type { UnitInfoUI } from './unit_info';
import type { UnitState } from '../generated/protocol';

function makeUnitState(overrides: Partial<UnitState> = {}): UnitState {
  const equipment = [{ name: 'rifle', cost_per_item: 2, count: 10 }];
  return {
    id: 'u1',
    owner: 'player_0',
    position: [10, 10],
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

interface Harness {
  controls: Controls;
  canvas: HTMLCanvasElement;
  camera: { x: number; y: number; zoom: number };
  renderer: Renderer;
  send: ReturnType<typeof vi.fn>;
  selectUnit: ReturnType<typeof vi.fn>;
  selectCity: ReturnType<typeof vi.fn>;
  gameState: GameState;
}

function makeControls(): Harness {
  const gameState = new GameState();
  const { device, canvas } = createFakeGpu(1280, 720);
  const renderer = new Renderer(canvas, device as unknown as GPUDevice);
  canvas.width = 1280;
  canvas.height = 720;

  // Fixed CSS size so screen↔world math is exact (see header note on
  // happy-dom's 0×0 getBoundingClientRect under the default harness).
  canvas.getBoundingClientRect = () =>
    ({
      left: 0,
      top: 0,
      width: 1280,
      height: 720,
      x: 0,
      y: 0,
      right: 1280,
      bottom: 720,
      toJSON: () => ({}),
    }) as DOMRect;

  const camera = { x: 0, y: 0, zoom: 1 };
  const send = vi.fn();
  const connection = { send } as unknown as Connection;
  const selectUnit = vi.fn();
  const selectCity = vi.fn();
  const unitInfo = { selectUnit, selectCity } as unknown as UnitInfoUI;

  const controls = new Controls(canvas, camera, renderer, connection, gameState, unitInfo);
  return { controls, canvas, camera, renderer, send, selectUnit, selectCity, gameState };
}

/** Dispatch a pointer event with the given props (happy-dom-safe). */
function dispatchPointer(target: EventTarget, type: string, props: Record<string, unknown>): void {
  const event = Object.assign(new Event(type, { bubbles: true, cancelable: true }), props);
  target.dispatchEvent(event as unknown as PointerEvent);
}

/**
 * Screen coords for a point at a cell's ORIGIN (entity hit-testing: units/
 * cities sit exactly at cell*CELL_SIZE in world space).
 * With canvas 1280×720 @ (0,0), camera (0,0), zoom 1:
 * world = (clientX - 640, clientY - 360).
 */
function clientForEntity(cellX: number, cellY: number): { x: number; y: number } {
  return {
    x: cellX * CELL_SIZE + 640,
    y: cellY * CELL_SIZE + 360,
  };
}

/**
 * Screen coords for a point at a cell's CENTER (path drawing).
 * Cell origins sit on the exact grid boundary, where float round-trip
 * (client→NDC→world) can land a hair below the edge and floor() into the
 * previous cell — the center is a half-cell away from any boundary.
 */
function clientForCellCenter(cellX: number, cellY: number): { x: number; y: number } {
  return {
    x: cellX * CELL_SIZE + CELL_SIZE / 2 + 640,
    y: cellY * CELL_SIZE + CELL_SIZE / 2 + 360,
  };
}

describe('Controls — selection and path drawing', () => {
  let h: Harness;

  beforeEach(() => {
    h = makeControls();
  });

  afterEach(() => {
    h.controls.destroy();
    vi.restoreAllMocks();
  });

  it('clicking a unit selects it and clears any drawn path', () => {
    const state = makeUnitState({ position: [10, 10] });
    h.gameState.units.set(state.id, new Unit(state));
    h.renderer.currentPath = [{ x: 1, y: 1 }];

    const c = clientForEntity(10, 10);
    dispatchPointer(h.canvas, 'pointerdown', {
      button: 0,
      clientX: c.x,
      clientY: c.y,
      pointerId: 1,
    });

    expect(h.selectUnit).toHaveBeenCalledWith('u1');
    expect(h.selectCity).not.toHaveBeenCalled();
    expect(h.renderer.currentPath).toEqual([]);
  });

  it('clicking a city selects it', () => {
    h.gameState.cities.set('c1', {
      id: 'c1',
      position: [15, 5],
      owner: 'player_0',
      manpower: 500,
      level: 1,
    });

    const c = clientForEntity(15, 5);
    dispatchPointer(h.canvas, 'pointerdown', {
      button: 0,
      clientX: c.x,
      clientY: c.y,
      pointerId: 1,
    });

    expect(h.selectCity).toHaveBeenCalledWith('c1');
    expect(h.selectUnit).not.toHaveBeenCalled();
  });

  it('clicking empty ground deselects and starts a new path at the clicked cell', () => {
    // Click at cell (1, 4) — far from any unit/city.
    const c = clientForCellCenter(1, 4);
    dispatchPointer(h.canvas, 'pointerdown', {
      button: 0,
      clientX: c.x,
      clientY: c.y,
      pointerId: 1,
    });

    expect(h.selectUnit).toHaveBeenCalledWith(null);
    expect(h.selectCity).toHaveBeenCalledWith(null);
    expect(h.renderer.currentPath).toEqual([{ x: 1, y: 4 }]);
  });

  it('pointermove extends the drawn path (one point per new cell)', () => {
    // Start a path at (1,4), then move the cursor to (3,5) and (3,6).
    let c = clientForCellCenter(1, 4);
    dispatchPointer(h.canvas, 'pointerdown', {
      button: 0,
      clientX: c.x,
      clientY: c.y,
      pointerId: 1,
    });

    c = clientForCellCenter(3, 5);
    dispatchPointer(h.canvas, 'pointermove', {
      button: 0,
      clientX: c.x,
      clientY: c.y,
      pointerId: 1,
    });
    c = clientForCellCenter(3, 6);
    dispatchPointer(h.canvas, 'pointermove', {
      button: 0,
      clientX: c.x,
      clientY: c.y,
      pointerId: 1,
    });

    expect(h.renderer.currentPath).toEqual([
      { x: 1, y: 4 },
      { x: 3, y: 5 },
      { x: 3, y: 6 },
    ]);
  });

  it('pointermove without a started path does nothing', () => {
    const c = clientForCellCenter(3, 5);
    dispatchPointer(h.canvas, 'pointermove', {
      button: 0,
      clientX: c.x,
      clientY: c.y,
      pointerId: 1,
    });
    expect(h.renderer.currentPath).toEqual([]);
  });

  it('pointerup with a selection and a drawn path sends MoveUnit and resets', () => {
    // Selection (via unit click) + a drawn path: the pointerup contract is
    // send MoveUnit{unitId, path}, clear the path, and deselect.
    const state = makeUnitState({ position: [10, 10] });
    h.gameState.units.set(state.id, new Unit(state));
    const c = clientForEntity(10, 10);
    dispatchPointer(h.canvas, 'pointerdown', {
      button: 0,
      clientX: c.x,
      clientY: c.y,
      pointerId: 1,
    });

    h.renderer.currentPath = [
      { x: 11, y: 10 },
      { x: 12, y: 10 },
    ];
    dispatchPointer(h.canvas, 'pointerup', { button: 0, clientX: c.x, clientY: c.y, pointerId: 1 });

    expect(h.send).toHaveBeenCalledWith({
      MoveUnit: {
        unitId: 'u1',
        path: [
          { x: 11, y: 10 },
          { x: 12, y: 10 },
        ],
      },
    });
    expect(h.renderer.currentPath).toEqual([]);
    expect(h.selectUnit).toHaveBeenLastCalledWith(null);
  });

  it('pointerup without a selection does not send', () => {
    h.renderer.currentPath = [{ x: 1, y: 1 }];
    const c = clientForCellCenter(1, 1);
    dispatchPointer(h.canvas, 'pointerup', { button: 0, clientX: c.x, clientY: c.y, pointerId: 1 });
    expect(h.send).not.toHaveBeenCalled();
  });

  it('deselect() clears the selection, the city, and the path', () => {
    const state = makeUnitState({ position: [10, 10] });
    h.gameState.units.set(state.id, new Unit(state));
    const c = clientForEntity(10, 10);
    dispatchPointer(h.canvas, 'pointerdown', {
      button: 0,
      clientX: c.x,
      clientY: c.y,
      pointerId: 1,
    });
    h.renderer.currentPath = [{ x: 2, y: 2 }];

    h.controls.deselect();

    expect(h.selectUnit).toHaveBeenLastCalledWith(null);
    expect(h.selectCity).toHaveBeenCalledWith(null);
    expect(h.renderer.currentPath).toEqual([]);
  });
});

describe('Controls — zoom', () => {
  let h: Harness;

  beforeEach(() => {
    h = makeControls();
  });

  afterEach(() => {
    h.controls.destroy();
    vi.restoreAllMocks();
  });

  it('zooming in past the limit clamps to ZOOM_MAX', () => {
    // Zoom-in (deltaY < 0) multiplies by 1.1; 20 steps overshoots the cap.
    for (let i = 0; i < 20; i++) {
      dispatchPointer(h.canvas, 'wheel', {
        clientX: 640,
        clientY: 360,
        deltaY: -120,
        pointerId: 1,
      });
    }
    expect(h.camera.zoom).toBe(3.0);
  });

  it('zooming out past the limit clamps to ZOOM_MIN', () => {
    for (let i = 0; i < 40; i++) {
      dispatchPointer(h.canvas, 'wheel', {
        clientX: 640,
        clientY: 360,
        deltaY: 120,
        pointerId: 1,
      });
    }
    expect(h.camera.zoom).toBe(0.3);
  });

  it('zooming at the canvas center keeps the center point fixed', () => {
    h.camera.x = 100;
    h.camera.y = 200;
    // Cursor at the exact canvas center (640, 360): NDC (0,0), so the camera
    // target is unchanged by the zoom.
    dispatchPointer(h.canvas, 'wheel', {
      clientX: 640,
      clientY: 360,
      deltaY: -120,
      pointerId: 1,
    });
    expect(h.camera.zoom).toBeCloseTo(1.1, 10);
    expect(h.camera.x).toBeCloseTo(100, 10);
    expect(h.camera.y).toBeCloseTo(200, 10);
  });
});
