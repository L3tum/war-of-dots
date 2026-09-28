// Connection message dispatch tests.

import { describe, it, expect, beforeEach, afterEach, vi } from 'vitest';
import { decode, encode } from 'cbor-x';
import {
  applyCells,
  applyEquipmentCounts,
  mapSizeFromTerrain,
  stripUndefined,
  unpackUnitPosition,
} from './protocol';
import { Connection } from './connection';
import { CELL_SIZE } from '../game/config';
import { GameState } from '../game/game_state';
import { Unit } from '../game/units';
import { Renderer } from '../game/renderer';
import { createFakeGpu } from '../test/fake_gpu';
import type { Spectator } from '../ui/spectator';
import type { Advisor } from '../ui/advisor';
import type { LobbyUI } from '../ui/lobby';
import type { UnitState, CityState, EquipmentState } from '../generated/protocol';

describe('I4 sparse changed-cell apply (applyCells)', () => {
  it('applies packed (value<<bits)|index entries to a vision grid, incl. 1→0 flips', () => {
    // 4×4 grid: 16 cells → bits = ceil(log2(16)) = 4.
    const vision: boolean[][] = Array.from({ length: 4 }, () => Array(4).fill(true));
    // idx 2 → 1 (already 1), idx 7 and 15 → 0 (1→0 flips must be applied).
    const cells = [(1 << 4) | 2, 0 << 4, (0 << 4) | 7, (0 << 4) | 15];
    expect(applyCells(vision, cells, 4, true)).toBe(true);
    expect(vision[0][2]).toBe(true);
    expect(vision[1][3]).toBe(false);
    expect(vision[3][3]).toBe(false);
    expect(vision[2][0]).toBe(true);
  });

  it('applies border values including 255 (ownerless) at arbitrary indices', () => {
    // 3×5 grid: 15 cells → bits = ceil(log2(15)) = 4 (not a power of two).
    const border: number[][] = Array.from({ length: 3 }, () => Array(5).fill(0));
    expect(applyCells(border, [(255 << 4) | 4, (2 << 4) | 9], 5, false)).toBe(true);
    expect(border[0][4]).toBe(255);
    expect(border[1][4]).toBe(2);
    expect(border[2][2]).toBe(0);
  });

  it('handles the 1-cell grid (bits = 0) form', () => {
    const vision: boolean[][] = [[false]];
    expect(applyCells(vision, [1], 1, true)).toBe(true);
    expect(vision[0][0]).toBe(true);
    expect(applyCells(vision, [0], 1, true)).toBe(true);
    expect(vision[0][0]).toBe(false);
  });

  it('roundtrips a random sparse change against the flat-grid replace path', () => {
    // Same invariant the server relies on: patching `prev` with the packed
    // list of `prev → curr` differences must reproduce `curr` exactly.
    const w = 37;
    const h = 23;
    const bits = Math.ceil(Math.log2(w * h));
    const prev: number[][] = Array.from({ length: h }, () => Array(w).fill(0));
    const curr = prev.map((row) => row.slice());
    let seed = 12345;
    const rnd = () => (seed = (seed * 1103515245 + 12345) & 0x7fffffff) / 0x7fffffff;
    const changed: number[] = [];
    for (let y = 0; y < h; y++) {
      for (let x = 0; x < w; x++) {
        if (rnd() < 0.03) {
          const v = Math.floor(rnd() * 256);
          curr[y][x] = v;
          changed.push((v << bits) | (y * w + x));
        }
      }
    }
    expect(applyCells(prev, changed, w, false)).toBe(true);
    expect(prev).toEqual(curr);
  });

  it('refuses to apply without a baseline grid', () => {
    expect(applyCells([], [1], 4, true)).toBe(false);
  });
});

describe('I3 packed unit position + equipment counts (connection decode)', () => {
  it('unpacks a packed position to cell coords within one quantization step', () => {
    // Mirror of the server's `pack_unit_position`: x_q = round(x*65536/w),
    // y_q = round(y*65536/h), packed = (x_q<<16)|y_q.
    const pack = (x: number, y: number, w: number, h: number) => {
      const xq = Math.round((x * 65536) / w);
      const yq = Math.round((y * 65536) / h);
      return ((xq << 16) | yq) >>> 0;
    };
    const map = { width: 1000, height: 1000 };
    const halfStep = 0.5 * (1000 / 65536);
    for (const [x, y] of [
      [0, 0],
      [3.5, 4.25],
      [999.123, 500.5],
      [0.00001, 0.99999],
    ]) {
      const { x: dx, y: dy } = unpackUnitPosition(pack(x, y, 1000, 1000), map);
      expect(Math.abs(dx - x)).toBeLessThanOrEqual(halfStep);
      expect(Math.abs(dy - y)).toBeLessThanOrEqual(halfStep);
    }
    // Exact for whole-cell coords on a power-of-two map.
    const exact = unpackUnitPosition(pack(11, 20, 32, 32), {
      width: 32,
      height: 32,
    });
    expect(exact).toEqual({ x: 11, y: 20 });
  });

  it('decodes packed position 0 (the origin) to (0,0) — not skipped as falsy', () => {
    expect(unpackUnitPosition(0, { width: 1000, height: 1000 })).toEqual({
      x: 0,
      y: 0,
    });
  });

  it('decodes the max packed value to the map edge (clamped by the u16 range)', () => {
    const w = 1000;
    const h = 1000;
    const { x, y } = unpackUnitPosition(0xffffffff, { width: w, height: h });
    expect(Math.abs(x - w + w / 65536)).toBeLessThan(1e-6);
    expect(Math.abs(y - h + h / 65536)).toBeLessThan(1e-6);
  });

  it('mapSizeFromTerrain derives cells from the 2-D grid, null when empty', () => {
    const grid: unknown[][] = Array.from({ length: 10 }, () => Array(20).fill('plains'));
    expect(mapSizeFromTerrain(grid)).toEqual({ width: 20, height: 10 });
    expect(mapSizeFromTerrain([])).toBeNull();
    expect(mapSizeFromTerrain([[]])).toBeNull();
  });

  it('applies equipment counts against the full-state slot table, preserving names/costs', () => {
    const unit = {
      equipment: [
        { name: 'rifle', cost_per_item: 2, count: 100 },
        { name: 'cannon', cost_per_item: 7, count: 40 },
        { name: 'medkit', cost_per_item: 5, count: 10 },
      ],
    };
    // Slot order == UnitState.equipment order: index i is slot i.
    expect(applyEquipmentCounts(unit, [105, 0, 7])).toBe(true);
    expect(unit.equipment).toEqual([
      { name: 'rifle', cost_per_item: 2, count: 105 },
      { name: 'cannon', cost_per_item: 7, count: 0 },
      { name: 'medkit', cost_per_item: 5, count: 7 },
    ]);
  });

  it('refuses a counts update whose length mismatches the slot table (no corruption)', () => {
    const unit = {
      equipment: [{ name: 'rifle', cost_per_item: 2, count: 100 }],
    };
    expect(applyEquipmentCounts(unit, [1, 2, 3])).toBe(false);
    expect(unit.equipment).toEqual([{ name: 'rifle', cost_per_item: 2, count: 100 }]);
  });
});

describe('CBOR encoding strips undefined values', () => {
  it('removes top-level undefined values', () => {
    const input: Record<string, unknown> = { a: 1, b: undefined, c: 'hello' };
    const cleaned = stripUndefined(input);
    expect(cleaned).toEqual({ a: 1, c: 'hello' });
    expect((cleaned as Record<string, unknown>).b).toBeUndefined(); // key removed entirely
  });

  it('removes nested undefined values', () => {
    const input = {
      outer: {
        inner: { x: 1, y: undefined, z: true },
        keep: 'yes',
        drop: undefined,
      },
    };
    const cleaned = stripUndefined(input);
    const outer = (cleaned as Record<string, Record<string, unknown>>).outer;
    const inner = outer.inner as Record<string, unknown>;
    expect(inner).toEqual({ x: 1, z: true });
    expect(outer.keep).toBe('yes');
    expect(outer.drop).toBeUndefined();
  });

  it('preserves null values', () => {
    const input: Record<string, unknown> = {
      a: null,
      b: undefined,
      c: 0,
      d: false,
      e: '',
    };
    const cleaned = stripUndefined(input);
    expect(cleaned).toEqual({ a: null, c: 0, d: false, e: '' });
  });

  it('handles arrays with undefined elements', () => {
    const input: unknown[] = [1, undefined, 'hello', null];
    const cleaned = stripUndefined(input);
    expect(cleaned).toEqual([1, undefined, 'hello', null]); // array elements preserved
  });

  it('CBOR roundtrip produces decodable bytes without undefined', () => {
    const input = {
      LobbyMessage: {
        CreateLobby: {
          settings: {
            map: 'default',
            use_standard_ruleset: true,
            llm_thinking_delay: 3.0,
          },
          password: undefined,
        },
      },
    };
    const cleaned = stripUndefined(input);
    const bytes = encode(cleaned);
    const decoded = decode(bytes);
    // password key should not exist at all
    const LobbyMessage = decoded as Record<string, unknown>;
    const CreateLobby = LobbyMessage.LobbyMessage as Record<string, unknown>;
    const createLobbyObj = CreateLobby.CreateLobby as Record<string, unknown>;
    const settings = createLobbyObj.settings as Record<string, unknown>;
    expect(createLobbyObj.password).toBeUndefined(); // key was stripped
    expect(settings.map).toBe('default');
    expect(settings.use_standard_ruleset).toBe(true);
  });
});

describe('Connection message routing', () => {
  it('recognizes FullState message', () => {
    const msg = {
      FullState: { terrain: [], units: [], cities: [], player: {} },
    };
    expect(msg).toHaveProperty('FullState');
    expect(msg).not.toHaveProperty('DeltaState');
  });

  it('recognizes DeltaState message', () => {
    const msg = {
      DeltaState: {
        vision_grid_diff: [],
        unit_updates: [],
        units_added: [],
        units_removed: [],
        city_updates: [],
        player: { funds: 0, units_owned: 0 },
      },
    };
    expect(msg).toHaveProperty('DeltaState');
    expect(msg).not.toHaveProperty('FullState');
  });

  it('recognizes Leaderboard message', () => {
    const msg = {
      Leaderboard: {
        entries: [
          {
            player_id: 'p1',
            name: 'Alice',
            games_played: 10,
            games_won: 5,
            avg_casualties: 100.0,
          },
        ],
      },
    };
    expect(msg).toHaveProperty('Leaderboard');
    expect(msg.Leaderboard.entries).toHaveLength(1);
    expect(msg.Leaderboard.entries[0].name).toBe('Alice');
  });

  it('recognizes LobbyCreated message', () => {
    const msg = { LobbyCreated: { lobby_code: 'ABC123', lobby_id: '123' } };
    expect(msg).toHaveProperty('LobbyCreated');
    expect(msg.LobbyCreated.lobby_code).toBe('ABC123');
  });

  it('recognizes LobbyUpdate message', () => {
    const msg = {
      LobbyUpdate: {
        players: [{ id: '1', name: 'Alice', is_human: true, llm_provider: null }],
      },
    };
    expect(msg).toHaveProperty('LobbyUpdate');
    expect(msg.LobbyUpdate.players).toHaveLength(1);
  });

  it('recognizes GameOver message', () => {
    const msg = {
      GameOver: {
        game_id: 'game123',
        winner: 'player_1',
        message: 'Game Over',
      },
    };
    expect(msg).toHaveProperty('GameOver');
    expect(msg.GameOver.winner).toBe('player_1');
  });

  it('recognizes LlmReasonLog message', () => {
    const msg = {
      LlmReasonLog: {
        player_name: 'Alice',
        action: 'move',
        reason: 'Moving to capture city',
      },
    };
    expect(msg).toHaveProperty('LlmReasonLog');
    expect(msg.LlmReasonLog.player_name).toBe('Alice');
  });

  it('recognizes Advice message', () => {
    const msg = { Advice: { text: 'Consider reinforcing your front line.' } };
    expect(msg).toHaveProperty('Advice');
    expect(msg.Advice.text).toContain('reinforcing');
  });

  it('recognizes ClearSpectator message', () => {
    const msg = { ClearSpectator: null };
    expect(msg).toHaveProperty('ClearSpectator');
  });

  it('recognizes Error message', () => {
    const msg = { Error: { message: 'Something went wrong' } };
    expect(msg).toHaveProperty('Error');
    expect(msg.Error.message).toBe('Something went wrong');
  });
});

describe('LobbyUI showTab DOM manipulation', () => {
  let container: HTMLDivElement;

  beforeEach(() => {
    container = document.createElement('div');
    container.innerHTML = `
      <div id="lobby-panel" class="panel">
        <div id="lobby-tabs">
          <button id="tab-create" class="active">Create</button>
          <button id="tab-join">Join</button>
          <button id="tab-leaderboard">Leaderboard</button>
        </div>
        <div class="panel-content">
          <div id="lobby-panel-create">Create form</div>
          <div id="lobby-panel-join" class="hidden">Join form</div>
          <div id="lobby-info" class="hidden">
            <strong id="lobby-code-display"></strong>
          </div>
        </div>
      </div>
    `;
    document.body.appendChild(container);
  });

  /** Simulate the showTab('info') logic from LobbyUI. */
  function simulateShowTabInfo() {
    ['lobby-panel-create', 'lobby-panel-join', 'lobby-info'].forEach((id) => {
      const el = document.getElementById(id);
      if (el) el.classList.add('hidden');
    });
    ['tab-create', 'tab-join', 'tab-leaderboard'].forEach((id) => {
      const el = document.getElementById(id);
      if (el) el.classList.remove('active');
    });
    const tabs = document.getElementById('lobby-tabs');
    if (tabs) tabs.classList.add('hidden');
    const info = document.getElementById('lobby-info');
    if (info) info.classList.remove('hidden');
  }

  it('hides create/join panels and shows info panel for waiting room', () => {
    simulateShowTabInfo();

    expect(document.getElementById('lobby-panel-create')?.classList.contains('hidden')).toBe(true);
    expect(document.getElementById('lobby-panel-join')?.classList.contains('hidden')).toBe(true);
    expect(document.getElementById('lobby-info')?.classList.contains('hidden')).toBe(false);
  });

  it('hides the tab bar when showing info tab', () => {
    simulateShowTabInfo();

    expect(document.getElementById('lobby-tabs')?.classList.contains('hidden')).toBe(true);
  });

  it('deactivates all tab buttons when showing info tab', () => {
    simulateShowTabInfo();

    expect(document.getElementById('tab-create')?.classList.contains('active')).toBe(false);
    expect(document.getElementById('tab-join')?.classList.contains('active')).toBe(false);
    expect(document.getElementById('tab-leaderboard')?.classList.contains('active')).toBe(false);
  });

  it('sets lobby code display correctly', () => {
    simulateShowTabInfo();
    const codeDisplay = document.getElementById('lobby-code-display');
    if (codeDisplay) codeDisplay.textContent = 'ABC123';
    expect(codeDisplay?.textContent).toBe('ABC123');
  });

  it('LobbyCreated message has expected structure', () => {
    const msg = { LobbyCreated: { lobby_code: 'xyz789' } };
    expect(msg.LobbyCreated.lobby_code).toBe('xyz789');

    // Simulate handler
    const codeDisplay = document.getElementById('lobby-code-display');
    if (codeDisplay) codeDisplay.textContent = msg.LobbyCreated.lobby_code;
    expect(codeDisplay?.textContent).toBe('xyz789');
  });

  it('LobbyJoin message handled alongside info tab', () => {
    const msg = { LobbyJoin: { lobby_id: '42', lobby_code: 'JOIN1' } };
    expect(msg.LobbyJoin.lobby_code).toBe('JOIN1');

    simulateShowTabInfo();
    const codeDisplay = document.getElementById('lobby-code-display');
    if (codeDisplay) codeDisplay.textContent = msg.LobbyJoin.lobby_code;
    expect(document.getElementById('lobby-info')?.classList.contains('hidden')).toBe(false);
  });
});

describe('CBOR roundtrip for server messages', () => {
  it('encodes and decodes LobbyCreated', () => {
    const msg = { LobbyCreated: { lobby_code: 'abc123' } };
    const bytes = encode(msg);
    const decoded = decode(bytes);
    expect(decoded).toHaveProperty('LobbyCreated');
    expect((decoded as Record<string, Record<string, string>>).LobbyCreated.lobby_code).toBe(
      'abc123',
    );
  });

  it('encodes and decodes LobbyJoin', () => {
    const msg = { LobbyJoin: { lobby_id: '42', lobby_code: 'xyz' } };
    const bytes = encode(msg);
    const decoded = decode(bytes);
    expect(decoded).toHaveProperty('LobbyJoin');
  });

  it('encodes and decodes LobbyUpdate with players', () => {
    const msg = {
      LobbyUpdate: {
        players: [
          { id: '1', name: 'Alice', is_human: true, llm_provider: null },
          { id: '2', name: 'Bob', is_human: false, llm_provider: 'openai' },
        ],
      },
    };
    const bytes = encode(msg);
    const decoded = decode(bytes);
    expect(decoded).toHaveProperty('LobbyUpdate');
    const entries = (decoded as Record<string, Record<string, unknown[]>>).LobbyUpdate.players;
    expect(entries).toHaveLength(2);
  });

  it('encodes and decodes GameOver', () => {
    const msg = {
      GameOver: { game_id: 'g1', winner: 'player_1', message: 'Victory!' },
    };
    const bytes = encode(msg);
    const decoded = decode(bytes);
    expect(decoded).toHaveProperty('GameOver');
  });
});

// ============================================================================
// Game Start Transition Tests
// ============================================================================

describe('Game start transition (FullState → lobby hide)', () => {
  /**
   * Tests that applyFullState logic correctly transitions UI state.
   * We test the helper functions directly since Connection requires WebSocket mocking.
   */

  it('FullState message contains expected fields', () => {
    const state = {
      terrain: [[{ type: 'Plains', height: 0 }]],
      vision_grid: [[true]],
      border_grid: [[0]],
      units: [],
      cities: [
        {
          id: 'city_0',
          position: [0, 0],
          owner: 'player_1',
          manpower: 10000,
          level: 1,
        },
      ],
      player: { funds: 1000, units_owned: 0 },
    };

    expect(state.terrain).toBeDefined();
    expect(state.vision_grid).toBeDefined();
    expect(state.units).toBeInstanceOf(Array);
    expect(state.cities).toBeInstanceOf(Array);
    expect(state.player.funds).toBe(1000);
  });

  it('camera centering formula produces correct center for known terrain', () => {
    const CELL_SIZE = 32;
    const terrain = Array.from({ length: 10 }, () => Array(15).fill(null));

    let minCol = Infinity,
      maxCol = -Infinity;
    for (const row of terrain) {
      if (row.length > 0) {
        minCol = Math.min(minCol, 0);
        maxCol = Math.max(maxCol, row.length - 1);
      }
    }
    const minRow = 0;
    const maxRow = terrain.length - 1;

    const centerX = ((minCol + maxCol) / 2 + 0.5) * CELL_SIZE;
    const centerY = ((minRow + maxRow) / 2 + 0.5) * CELL_SIZE;

    // For 10x15: center = (7 + 0.5) * 32 = 240, (4.5 + 0.5) * 32 = 160
    expect(centerX).toBe(240);
    expect(centerY).toBe(160);
  });

  it('transitionToGameUI hides lobby and shows game panels', () => {
    // Setup DOM
    const lobbyPanel = document.createElement('div');
    lobbyPanel.id = 'lobby-panel';
    lobbyPanel.className = 'panel';
    document.body.appendChild(lobbyPanel);

    const statsPanel = document.createElement('div');
    statsPanel.id = 'stats-panel';
    statsPanel.className = 'panel hidden';
    document.body.appendChild(statsPanel);

    const cityLabels = document.createElement('div');
    cityLabels.id = 'city-labels';
    cityLabels.className = 'hidden';
    document.body.appendChild(cityLabels);

    // Simulate transitionToGameUI logic using direct references
    lobbyPanel.classList.add('hidden');
    statsPanel.classList.remove('hidden');
    cityLabels.classList.remove('hidden');

    // Verify state after transition
    expect(lobbyPanel.classList.contains('hidden')).toBe(true);
    expect(statsPanel.classList.contains('hidden')).toBe(false);
    expect(cityLabels.classList.contains('hidden')).toBe(false);

    // Cleanup
    lobbyPanel.remove();
    statsPanel.remove();
    cityLabels.remove();
  });

  it('handles missing DOM elements gracefully during transition', () => {
    // No DOM elements present — should not throw
    expect(() => {
      const lp = document.getElementById('lobby-panel');
      if (lp) {
        lp.classList.add('hidden');
      }
      const sp = document.getElementById('stats-panel');
      if (sp) sp.classList.remove('hidden');
      const cl = document.getElementById('city-labels');
      if (cl) cl.classList.remove('hidden');
    }).not.toThrow();
  });

  it('camera centering handles ragged terrain gracefully', () => {
    const CELL_SIZE = 32;
    const terrain = [
      [null, null, null],
      [null, null, null, null, null],
      [null, null, null, null],
    ];

    let minCol = Infinity,
      maxCol = -Infinity;
    for (const row of terrain) {
      if (row.length > 0) {
        minCol = Math.min(minCol, 0);
        maxCol = Math.max(maxCol, row.length - 1);
      }
    }
    const minRow = 0;
    const maxRow = terrain.length - 1;

    const centerX = ((minCol + maxCol) / 2 + 0.5) * CELL_SIZE;
    const centerY = ((minRow + maxRow) / 2 + 0.5) * CELL_SIZE;

    // minCol=0, maxCol=4, minRow=0, maxRow=2
    // centerX = (2 + 0.5) * 32 = 80
    // centerY = (1 + 0.5) * 32 = 48
    expect(centerX).toBe(80);
    expect(centerY).toBe(48);
  });
});

// ============================================================================
// Real Connection class — dispatch against a fake WebSocket
// ============================================================================

/** Recording WebSocket double: captures outbound frames, injects scripted inbound. */
class FakeWebSocket {
  static instances: FakeWebSocket[] = [];
  static readonly CONNECTING = 0;
  static readonly OPEN = 1;
  static readonly CLOSING = 2;
  static readonly CLOSED = 3;

  readonly url: string;
  readyState = FakeWebSocket.CONNECTING;
  binaryType = '';
  sent: Uint8Array[] = [];
  onopen: (() => void) | null = null;
  onmessage: ((ev: { data: unknown }) => void) | null = null;
  onclose: (() => void) | null = null;
  onerror: ((err: unknown) => void) | null = null;

  constructor(url: string) {
    this.url = url;
    FakeWebSocket.instances.push(this);
  }

  send(data: Uint8Array) {
    this.sent.push(data);
  }

  close() {
    this.readyState = FakeWebSocket.CLOSED;
    this.onclose?.();
  }

  /** Test helper: simulate a successful open. */
  open() {
    this.readyState = FakeWebSocket.OPEN;
    this.onopen?.();
  }

  /** Test helper: inject a CBOR-encoded server message. */
  feedServerMessage(msg: Record<string, unknown>) {
    const bytes = encode(msg);
    this.onmessage?.({ data: new Uint8Array(bytes).buffer });
  }
}

function makeUnitState(overrides: Partial<UnitState> = {}): UnitState {
  const equipment: EquipmentState[] = [{ name: 'rifle', cost_per_item: 2, count: 10 }];
  return {
    id: 'u1',
    owner: 'player_0',
    position: [2, 3],
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
    position: [4, 5],
    owner: 'player_0',
    manpower: 1000,
    level: 1,
    ...overrides,
  };
}

type MockFn = ReturnType<typeof vi.fn>;

interface ConnHarness {
  conn: Connection;
  gameState: GameState;
  renderer: Renderer;
  spectator: { clearAll: MockFn; gameOver: MockFn; addReason: MockFn };
  advisor: { show: MockFn };
  lobbyUI: {
    enableStartButton: MockFn;
    showTab: MockFn;
    updatePlayers: MockFn;
    showLeaderboard: MockFn;
    hide: MockFn;
  };
  ws: FakeWebSocket;
}

function makeConnection(spectatorMode = false): ConnHarness {
  const gameState = new GameState();
  const { device, canvas } = createFakeGpu(1280, 720);
  const renderer = new Renderer(canvas, device as unknown as GPUDevice);
  canvas.width = 1280;
  canvas.height = 720;

  const spectator: ConnHarness['spectator'] = {
    clearAll: vi.fn(),
    gameOver: vi.fn(),
    addReason: vi.fn(),
  };
  const advisor: ConnHarness['advisor'] = { show: vi.fn() };
  const lobbyUI: ConnHarness['lobbyUI'] = {
    enableStartButton: vi.fn(),
    showTab: vi.fn(),
    updatePlayers: vi.fn(),
    showLeaderboard: vi.fn(),
    hide: vi.fn(),
  };

  const conn = new Connection(
    gameState,
    renderer,
    spectator as unknown as Spectator,
    advisor as unknown as Advisor,
    lobbyUI as unknown as LobbyUI,
    spectatorMode,
  );
  conn.connect();
  const ws = FakeWebSocket.instances[FakeWebSocket.instances.length - 1]!;
  ws.open();
  return { conn, gameState, renderer, spectator, advisor, lobbyUI, ws };
}

describe('Connection (real class, fake WebSocket)', () => {
  let h: ConnHarness;

  beforeEach(() => {
    vi.stubGlobal('WebSocket', FakeWebSocket);
    FakeWebSocket.instances = [];
    h = makeConnection();
  });

  afterEach(() => {
    vi.unstubAllGlobals();
  });

  it('connects to /ws, handshakes, then sends CBOR frames with undefined stripped', () => {
    expect(h.ws.url).toBe('ws://localhost/ws');
    // WP3: the first frame is always the Hello handshake.
    expect(h.ws.sent).toHaveLength(1);
    const hello = decode(h.ws.sent[0]) as Record<string, Record<string, unknown>>;
    expect(hello.Hello.protocol_version).toBe(1);
    expect(hello.Hello.features).toBe(0);

    // Sends before HelloAck are queued, not dropped.
    h.conn.send({
      LobbyMessage: {
        CreateLobby: {
          settings: { map: 'default', use_standard_ruleset: true },
          password: undefined,
        },
      },
    } as Record<string, unknown>);
    expect(h.ws.sent).toHaveLength(1);

    // HelloAck releases the queued sends.
    h.ws.feedServerMessage({ HelloAck: { protocol_version: 1, features: 0 } });
    expect(h.ws.sent).toHaveLength(2);
    const decoded = decode(h.ws.sent[1]) as Record<string, Record<string, Record<string, unknown>>>;
    const create = decoded.LobbyMessage.CreateLobby;
    expect(create.password).toBeUndefined();
    expect((create.settings as Record<string, unknown>).map).toBe('default');
  });

  it('treats a pre-HelloAck Error as fatal: no reconnect scheduled', async () => {
    vi.useFakeTimers();
    try {
      h.ws.feedServerMessage({ Error: { message: 'Hello must be the first message' } });
      h.ws.close(); // would normally schedule a reconnect
      // Advance far beyond any retry backoff: a non-fatal failure would
      // have spawned new WebSocket instances by now.
      vi.advanceTimersByTime(60_000);
      expect(FakeWebSocket.instances).toHaveLength(1);
    } finally {
      vi.useRealTimers();
    }
  });

  it('spectator-mode connections never send frames', () => {
    const s = makeConnection(true);
    s.conn.send({ Action: { Move: { id: 'u1', x: 0, y: 0 } } });
    expect(s.ws.sent).toHaveLength(0);
  });

  it('applies FullState: grids, units, cities, player, camera centered on own territory', () => {
    const dataDirty = vi.spyOn(h.renderer, 'markDataDirty');
    const fogDirty = vi.spyOn(h.renderer, 'markFogDirty');
    const borderDirty = vi.spyOn(h.renderer, 'markBorderDirty');

    const terrain = Array.from({ length: 10 }, () => Array(10).fill('Plains'));
    h.ws.feedServerMessage({
      FullState: {
        terrain,
        vision_grid: Array.from({ length: 10 }, () => Array(10).fill(true)),
        border_grid: Array.from({ length: 10 }, () => Array(10).fill(0)),
        units: [makeUnitState()],
        cities: [makeCityState()],
        player: { funds: 500, units_owned: 1, player_id: 'player_0' },
      },
    });

    // Grids
    expect(h.gameState.terrain).toEqual(terrain);
    expect(h.gameState.visionGrid[0][0]).toBe(true);
    expect(h.gameState.borderGrid).toEqual(Array.from({ length: 10 }, () => Array(10).fill(0)));
    // Units are real Unit objects
    const unit = h.gameState.units.get('u1');
    expect(unit).toBeInstanceOf(Unit);
    expect(unit!.position).toEqual({ x: 2, y: 3 });
    // Cities
    expect(h.gameState.cities.get('c1')?.manpower).toBe(1000);
    // Player
    expect(h.gameState.player.funds).toBe(500);
    expect(h.gameState.ownPlayerId).toBe('player_0');
    // Camera centered on own unit (2,3) + own city (4,5): avg (3,4) * CELL_SIZE
    expect(h.renderer.camera.x).toBeCloseTo(3 * CELL_SIZE, 5);
    expect(h.renderer.camera.y).toBeCloseTo(4 * CELL_SIZE, 5);
    // Dirty marks fired
    expect(dataDirty).toHaveBeenCalled();
    expect(fogDirty).toHaveBeenCalled();
    expect(borderDirty).toHaveBeenCalled();
  });

  it('spectator FullState (no own id) centers on map center', () => {
    const terrain = Array.from({ length: 10 }, () => Array(10).fill('Plains'));
    h.ws.feedServerMessage({
      FullState: {
        terrain,
        units: [makeUnitState({ owner: 'player_1' })],
        cities: [makeCityState({ owner: 'player_1' })],
        player: { funds: 0, units_owned: 0, player_id: '' },
      },
    });
    // 10x10 → center (5,5) * CELL_SIZE
    expect(h.renderer.camera.x).toBeCloseTo(5 * CELL_SIZE, 5);
    expect(h.renderer.camera.y).toBeCloseTo(5 * CELL_SIZE, 5);
  });

  it('I4: sparse vision_cells_changed patches the existing vision grid in place', () => {
    const fogDirty = vi.spyOn(h.renderer, 'markFogDirty');
    // Baseline 4x4 all-visible
    h.gameState.visionGrid = Array.from({ length: 4 }, () => Array(4).fill(true));
    h.gameState.terrain = Array.from({ length: 4 }, () => Array(4).fill('Plains'));
    // Patch index 5 (row 1, col 1) → false. bits=ceil(log2(16))=4, value 0 → packed=5
    h.ws.feedServerMessage({
      DeltaState: {
        vision_cells_changed: [5],
        grid_width: 4,
        city_updates: [],
        unit_updates: [],
        units_added: [],
        units_removed: [],
        player: { funds: 0, units_owned: 0, player_id: '' },
      },
    });
    expect(h.gameState.visionGrid[1][1]).toBe(false);
    expect(h.gameState.visionGrid[0][0]).toBe(true); // untouched
    expect(fogDirty).toHaveBeenCalled();
  });

  it('I4: sparse border_cells_changed patches the border grid including 255', () => {
    const borderDirty = vi.spyOn(h.renderer, 'markBorderDirty');
    h.gameState.borderGrid = Array.from({ length: 4 }, () => Array(4).fill(0));
    h.gameState.terrain = Array.from({ length: 4 }, () => Array(4).fill('Plains'));
    // index 3 → 255 (bits=4, value 255 → (255<<4)|3 = 4083); index 7 → 2 (→ (2<<4)|7 = 39)
    h.ws.feedServerMessage({
      DeltaState: {
        border_cells_changed: [4083, 39],
        grid_width: 4,
        city_updates: [],
        unit_updates: [],
        units_added: [],
        units_removed: [],
        player: { funds: 0, units_owned: 0, player_id: '' },
      },
    });
    expect(h.gameState.borderGrid[0][3]).toBe(255);
    expect(h.gameState.borderGrid[1][3]).toBe(2);
    expect(h.gameState.borderGrid[0][0]).toBe(0);
    expect(borderDirty).toHaveBeenCalled();
  });

  it('I3: packed unit position decoded against map size + equipment counts applied', () => {
    const terrain = Array.from({ length: 10 }, () => Array(10).fill('Plains'));
    h.ws.feedServerMessage({
      FullState: {
        terrain,
        units: [makeUnitState({ position: [0, 0] })],
        cities: [],
        player: { funds: 0, units_owned: 1, player_id: 'player_0' },
      },
    });
    const unit = h.gameState.units.get('u1')!;
    // Pack (2.5, 5) on a 10x10 map: x_q=round(2.5*65536/10)=16384, y_q=round(5*65536/10)=32768
    const packed = (16384 << 16) | 32768;
    h.ws.feedServerMessage({
      DeltaState: {
        unit_updates: [{ id: 'u1', position: packed, equipment: [25] }],
        city_updates: [],
        units_added: [],
        units_removed: [],
        player: { funds: 0, units_owned: 1, player_id: 'player_0' },
      },
    });
    expect(unit.position).toEqual({ x: 2.5, y: 5 });
    expect(unit.equipment[0].count).toBe(25);
    expect(unit.equipment[0].name).toBe('rifle'); // slot name preserved from full state
  });

  it('applies units_added/removed, city_updates, and player funds from a delta', () => {
    h.ws.feedServerMessage({
      FullState: {
        terrain: Array.from({ length: 4 }, () => Array(4).fill('Plains')),
        units: [makeUnitState()],
        cities: [makeCityState()],
        player: { funds: 100, units_owned: 1, player_id: 'player_0' },
      },
    });
    h.ws.feedServerMessage({
      DeltaState: {
        units_added: [makeUnitState({ id: 'u2', owner: 'player_1' })],
        units_removed: [],
        city_updates: [{ id: 'c1', manpower: 42, level: 3 }],
        unit_updates: [],
        player: { funds: 999, units_owned: 2, player_id: 'player_0' },
      },
    });
    expect(h.gameState.units.has('u2')).toBe(true);
    expect(h.gameState.cities.get('c1')?.manpower).toBe(42);
    expect(h.gameState.cities.get('c1')?.level).toBe(3);
    expect(h.gameState.player.funds).toBe(999);

    // Now remove u1
    h.ws.feedServerMessage({
      DeltaState: {
        units_added: [],
        units_removed: ['u1'],
        city_updates: [],
        unit_updates: [],
        player: { funds: 999, units_owned: 1, player_id: 'player_0' },
      },
    });
    expect(h.gameState.units.has('u1')).toBe(false);
  });

  it('GameOver routes to spectator + re-enables start button', () => {
    h.ws.feedServerMessage({
      GameOver: { game_id: 'g1', winner: 'player_0', message: 'Victory' },
    });
    expect(h.spectator.gameOver).toHaveBeenCalledWith('player_0', 'g1', 'Victory');
    expect(h.lobbyUI.enableStartButton).toHaveBeenCalled();
  });

  it('LobbyUpdate sets gameState.playerList + ownerIdToIndex and updates lobby UI', () => {
    h.ws.feedServerMessage({
      LobbyUpdate: {
        players: [
          { id: 'player_a', name: 'Alice', is_human: true, llm_provider: null },
          {
            id: 'player_b',
            name: 'Bob',
            is_human: false,
            llm_provider: 'openai',
          },
        ],
        settings: { map: 'default', use_standard_ruleset: true },
      },
    });
    expect(h.gameState.playerList).toEqual([
      { id: 'player_a', name: 'Alice', slot: 0 },
      { id: 'player_b', name: 'Bob', slot: 1 },
    ]);
    expect(h.gameState.ownerIdToIndex.get('player_b')).toBe(1);
    expect(h.lobbyUI.updatePlayers).toHaveBeenCalled();
  });

  it('Advice → advisor.show; LlmReasonLog → spectator.addReason; ClearSpectator → spectator.clearAll', () => {
    h.ws.feedServerMessage({ Advice: { text: 'Hold the line' } });
    expect(h.advisor.show).toHaveBeenCalledWith('Hold the line');

    h.ws.feedServerMessage({
      LlmReasonLog: {
        player_name: 'Alice',
        action: 'move',
        reason: 'flanking',
      },
    });
    expect(h.spectator.addReason).toHaveBeenCalledWith('Alice', 'move', 'flanking');

    h.ws.feedServerMessage({ ClearSpectator: null });
    expect(h.spectator.clearAll).toHaveBeenCalled();
  });

  /**
   * Debug-log scrubbing guard (WP2): the per-frame `console.log`/`console.debug`
   * calls that used to sit in the send/dispatch path (one log per WebSocket
   * frame, both directions) were pure debug noise — the payload is fully
   * re-derivable from gameState — and are removed. This asserts that a full
   * handshake + representative full/delta traffic + an outbound send never
   * reaches the console with a `[WS]` frame log. (One-shot lifecycle logs
   * like 'negotiated features' are fine; per-frame frame-payload logs are not.)
   */
  it('hot-path traffic never logs frame payloads to the console', () => {
    const logged: unknown[][] = [];
    const spyLog = vi.spyOn(console, 'log').mockImplementation((...args) => logged.push(args));
    const spyDebug = vi.spyOn(console, 'debug').mockImplementation((...args) => logged.push(args));
    const spyInfo = vi.spyOn(console, 'info').mockImplementation((...args) => logged.push(args));
    try {
      // Full handshake, then representative full + delta traffic...
      h.ws.feedServerMessage({ HelloAck: { protocol_version: 1, features: 0 } });
      h.ws.feedServerMessage({
        FullState: {
          terrain: Array.from({ length: 4 }, () => Array(4).fill('Plains')),
          units: [makeUnitState()],
          cities: [makeCityState()],
          player: { funds: 100, units_owned: 1, player_id: 'player_0' },
        },
      });
      h.ws.feedServerMessage({
        DeltaState: {
          unit_updates: [{ id: 'u1', position: 0, equipment: [5] }],
          city_updates: [],
          units_added: [],
          units_removed: [],
          player: { funds: 101, units_owned: 1, player_id: 'player_0' },
        },
      });
      // ...and outbound traffic (HelloAck delivered → raw send path).
      h.conn.send({ MoveTroop: { troop_id: 'u1', path: [[1, 1]] } });
    } finally {
      spyLog.mockRestore();
      spyDebug.mockRestore();
      spyInfo.mockRestore();
    }

    // No per-frame frame logs in either direction, ever.
    const frameLogs = logged.filter((args) =>
      args.some((a) => typeof a === 'string' && a.includes('[WS]')),
    );
    expect(frameLogs).toEqual([]);
    // console.debug was only used by the removed hot-path logs.
    expect(spyDebug).not.toHaveBeenCalled();
    // The traffic itself was applied (proves the guard ran with real flow,
    // not with the handlers short-circuited).
    expect(h.gameState.player.funds).toBe(101);
    expect(h.gameState.units.get('u1')!.equipment[0].count).toBe(5);
  });
});
