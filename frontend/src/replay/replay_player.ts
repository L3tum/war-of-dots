//! Replay playback on client — loads JSONL replay files and renders frame-by-frame

import { GameState } from '../game/game_state';
import { Renderer } from '../game/renderer';
import { Unit } from '../game/units';
import type { TerrainType } from '../game/terrain';
import {
  applyCells,
  applyEquipmentCounts,
  flatTo2D,
  flatTo2DBools,
  mapSizeFromTerrain,
  unpackUnitPosition,
} from '../net/connection';

export class ReplayPlayer {
  private canvas: HTMLCanvasElement;
  private gameState: GameState;
  private renderer: Renderer | null = null;
  private events: GameEvent[] = [];
  private currentIndex: number = 0;
  private appliedEvents: number = 0;
  private loaded: boolean = false;
  private paused: boolean = false;

  constructor(canvas: HTMLCanvasElement) {
    this.canvas = canvas;
    this.gameState = new GameState();
  }

  async initializeDevice(): Promise<GPUDevice> {
    const adapter = await navigator.gpu!.requestAdapter({ powerPreference: 'high-performance' });
    if (!adapter) {
      throw new Error('No WebGPU adapter available');
    }
    return adapter.requestDevice();
  }

  async loadReplay(file: string): Promise<void> {
    // `file` comes from the URL query (?replay=...) and is appended to a
    // relative fetch path — restrict it to a plain basename (no path separator,
    // ends in .jsonl) so the query param can't be used to fetch arbitrary files
    // from the static server (e.g. ?replay=../secrets.jsonl).
    if (!/^[A-Za-z0-9._-]+\.jsonl$/.test(file) || file.length > 128) {
      throw new Error(`Invalid replay file name: ${file}`);
    }
    try {
      // Served by the server's `/replays/:name` route (live-recorded games)
      // or statically by the no-server E2E harness (committed fixtures).
      const response = await fetch(`replays/${file}`);
      if (!response.ok) {
        throw new Error(`Failed to load replay: ${response.status} ${response.statusText}`);
      }
      const text = await response.text();
      const lines = text.split('\n');
      for (const line of lines) {
        const trimmed = line.trim();
        if (!trimmed) continue;
        try {
          const event = JSON.parse(trimmed);
          this.events.push(event);
        } catch {
          // Skip non-JSON lines (headers, comments)
        }
      }
      this.loaded = true;
    } catch (err) {
      console.error('Failed to load replay:', err);
      throw err;
    }
  }

  async start(): Promise<void> {
    // Initialize WebGPU device
    const device = await this.initializeDevice();
    this.renderer = new Renderer(this.canvas, device);

    this.renderer.setUpdateCallback(() => {
      this.tick();
    });
    this.renderer.startRenderLoop();
  }

  tick(): void {
    if (this.paused || !this.loaded) return;

    if (this.currentIndex < this.events.length) {
      const event = this.events[this.currentIndex];
      this.applyEvent(event);
      this.currentIndex++;
    }
  }

  pause(): void {
    this.paused = !this.paused;
  }

  /**
   * Test hook (frontend/e2e/webgpu.spec.ts): last render error from the active
   * renderer, if any. Exposed so the no-server replay path can assert render
   * health without a game server.
   */
  get renderError(): string | null {
    return this.renderer?.lastRenderError ?? null;
  }

  /**
   * Test hook (frontend/e2e/webgpu.spec.ts): render-pass submissions by the
   * active renderer. Lets the E2E settle poll for real work instead of a fixed
   * timeout that could elapse with zero frames submitted.
   */
  get framesSubmitted(): number {
    return this.renderer?.framesSubmitted ?? 0;
  }

  /**
   * Test hook (frontend/e2e/webgpu.spec.ts): replay events applied so far.
   * Each applied event marks the data dirty and therefore drives at least one
   * rendered frame — the E2E asserts frames >= events, which is the exact
   * invariant (a flat frame minimum could be met by the pre-replay frame).
   */
  get eventsApplied(): number {
    return this.appliedEvents;
  }

  private applyEvent(event: GameEvent): void {
    this.appliedEvents++;
    // Bump the renderer's data version so the frame dirty gate keeps the
    // render loop advancing while replay events are still being applied.
    this.renderer?.markDataDirty();
    // Full game state snapshot
    if (isFullStateEvent(event)) {
      const fs = event.full_state;
      if (fs.terrain) {
        // Legacy JSONL replays store { type, height } per cell; live-recorded
        // replays (written by the server) carry the wire shape — plain enum-
        // name strings. Only `type` is used either way.
        this.gameState.terrain = (fs.terrain as (string | { type: string })[][]).map((row) =>
          row.map((t) => (typeof t === 'string' ? (t as TerrainType) : (t.type as TerrainType))),
        );
      }
      if (fs.vision_grid) {
        this.gameState.visionGrid = fs.vision_grid;
      }
      if (fs.border_grid) {
        this.gameState.borderGrid = fs.border_grid;
      }
      if (fs.units) {
        this.gameState.units.clear();
        for (const raw of fs.units) {
          const unitData = raw as { id: string } & import('../generated/protocol').UnitState;
          const unit = new Unit(unitData);
          this.gameState.units.set(unitData.id, unit);
        }
      }
      if (fs.cities) {
        this.gameState.cities.clear();
        for (const raw of fs.cities) {
          const cityData = raw as import('../generated/protocol').CityState;
          this.gameState.cities.set(cityData.id, cityData);
        }
      }
      if (fs.player) {
        this.gameState.player.funds = fs.player.funds;
        this.gameState.player.units_owned = fs.player.units_owned;
      }
      return;
    }

    // Delta state
    if (isDeltaEvent(event)) {
      const delta = event.delta;
      if (delta.vision_grid_diff && delta.grid_width) {
        this.gameState.visionGrid = flatTo2DBools(delta.vision_grid_diff, delta.grid_width);
      } else if (delta.vision_cells_changed && delta.grid_width) {
        // I4: sparse changed-cell list (packed u32) — patch the current grid.
        applyCells(this.gameState.visionGrid, delta.vision_cells_changed, delta.grid_width, true);
      }
      if (delta.border_grid_diff && delta.grid_width) {
        this.gameState.borderGrid = flatTo2D(delta.border_grid_diff, delta.grid_width);
      } else if (delta.border_cells_changed && delta.grid_width) {
        // I4: sparse changed-cell list (packed u32) — patch the current grid.
        applyCells(this.gameState.borderGrid, delta.border_cells_changed, delta.grid_width, false);
      }
      // Units added
      for (const unit of delta.units_added || []) {
        const u = unit as { id: string };
        const newUnit = new Unit(unit as import('../generated/protocol').UnitState);
        this.gameState.units.set(u.id, newUnit);
      }
      // Units removed
      for (const id of delta.units_removed || []) {
        this.gameState.units.delete(id);
      }
      // Unit updates — I3: packed position decoded against the terrain map
      // size, equipment decoded as per-slot counts against the unit's
      // full-state slot table (same rules as the live connection).
      const mapSize = mapSizeFromTerrain(this.gameState.terrain);
      for (const rawUpdate of delta.unit_updates || []) {
        const update = rawUpdate as Record<string, unknown>;
        const unit = this.gameState.units.get(update.id as string);
        if (unit) {
          // Packed position 0 (the origin) is valid — use explicit null checks.
          if (update.position !== undefined && update.position !== null) {
            if (mapSize) unit.position = unpackUnitPosition(update.position as number, mapSize);
          }
          if (update.men !== undefined) unit.men = update.men as number;
          if (update.organisation !== undefined) unit.organisation = update.organisation as number;
          if (update.equipment !== undefined && update.equipment !== null) {
            const counts = update.equipment as number[];
            if (!applyEquipmentCounts(unit, counts)) {
              console.warn(`equipment counts for unit ${update.id} do not match its slot table`);
            }
          }
          if (update.path) unit.path = update.path as [number, number][];
          if (update.healing !== undefined) unit.healing = update.healing as boolean;
          if (update.resupply_enabled !== undefined) {
            unit.resupplyEnabled = update.resupply_enabled as boolean;
          }
          if (update.supply_percent !== undefined)
            unit.supplyPercent = update.supply_percent as number;
          if (update.encircled !== undefined) unit.encircled = update.encircled as boolean;
          if (update.attacking !== undefined) unit.attacking = update.attacking as boolean;
          if (update.on_water !== undefined) unit.onWater = update.on_water as boolean;
          if (update.boat_supply_percent !== undefined) {
            unit.boatSupplyPercent = update.boat_supply_percent as number;
          }
          if (update.embarking !== undefined) unit.embarking = update.embarking as boolean;
          if (update.disembarking !== undefined) {
            unit.disembarking = update.disembarking as boolean;
          }
        }
      }
      // City updates
      for (const rawCityUpdate of delta.city_updates || []) {
        const cityUpdate = rawCityUpdate as {
          id: string;
          manpower?: number;
          owner?: string;
          level?: number;
        };
        const city = this.gameState.cities.get(cityUpdate.id);
        if (city) {
          if (cityUpdate.manpower !== undefined) city.manpower = cityUpdate.manpower;
          if (cityUpdate.owner !== undefined) city.owner = cityUpdate.owner;
          if (cityUpdate.level !== undefined) city.level = cityUpdate.level;
        }
      }
      // Player state
      if (delta.player) {
        this.gameState.player.funds = delta.player.funds;
        this.gameState.player.units_owned = delta.player.units_owned;
      }
      return;
    }

    // Player action events (from JSONL action log)
    // These are informational — state changes come via delta/full_state events
    // No additional processing needed here
  }
}

// ============================================================================
// Type helpers for replay events (discriminated union)
// ============================================================================

interface FullStateEvent {
  full_state: {
    // Live-recorded files carry plain enum strings (wire shape); legacy
    // fixtures carry { type, height } objects — both accepted (see applyEvent).
    terrain?: (string | { type: string; height?: number })[][];
    vision_grid?: boolean[][];
    border_grid?: number[][];
    units?: unknown[];
    cities?: unknown[];
    player?: { funds: number; units_owned: number };
  };
  delta?: never;
  action_type?: never;
  tick?: number;
  timestamp?: number;
}

interface DeltaEvent {
  full_state?: never;
  delta: {
    vision_grid_diff?: number[];
    vision_cells_changed?: number[];
    border_grid_diff?: number[];
    border_cells_changed?: number[];
    grid_width?: number;
    units_added?: unknown[];
    units_removed?: string[];
    unit_updates?: unknown[];
    city_updates?: unknown[];
    player?: { funds: number; units_owned: number };
  };
  action_type?: never;
  tick?: number;
  timestamp?: number;
}

interface ActionEvent {
  full_state?: never;
  delta?: never;
  action_type: string;
  tick?: number;
  timestamp?: number;
  [key: string]: unknown;
}

type GameEvent = FullStateEvent | DeltaEvent | ActionEvent;

/** Type guard for FullStateEvent */
function isFullStateEvent(event: GameEvent): event is FullStateEvent {
  return 'full_state' in event && event.full_state !== undefined;
}

/** Type guard for DeltaEvent */
function isDeltaEvent(event: GameEvent): event is DeltaEvent {
  return 'delta' in event && event.delta !== undefined;
}
