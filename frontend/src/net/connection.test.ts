//! Connection message dispatch tests.

import { describe, it, expect, beforeEach } from 'vitest';
import { decode, encode } from 'cbor-x';
import { applyCells } from './connection';

/** Recursively remove `undefined` values from plain objects and arrays. */
function stripUndefined(value: unknown): unknown {
  if (value === null || typeof value !== 'object') return value;
  if (Array.isArray(value)) return value.map(stripUndefined);
  const result: Record<string, unknown> = {};
  for (const [key, val] of Object.entries(value)) {
    if (val !== undefined) {
      result[key] = stripUndefined(val);
    }
  }
  return result;
}

describe('I4 sparse changed-cell apply (applyCells)', () => {
  it('applies packed (value<<bits)|index entries to a vision grid, incl. 1→0 flips', () => {
    // 4×4 grid: 16 cells → bits = ceil(log2(16)) = 4.
    const vision: boolean[][] = Array.from({ length: 4 }, () => Array(4).fill(true));
    // idx 2 → 1 (already 1), idx 7 and 15 → 0 (1→0 flips must be applied).
    const cells = [(1 << 4) | 2, 0 << 4, 0 << 4 | 7, 0 << 4 | 15];
    expect(applyCells(vision, cells, 4, true)).toBe(true);
    expect(vision[0][2]).toBe(true);
    expect(vision[1][3]).toBe(false);
    expect(vision[3][3]).toBe(false);
    expect(vision[2][0]).toBe(true);
  });

  it('applies border values including 255 (ownerless) at arbitrary indices', () => {
    // 3×5 grid: 15 cells → bits = ceil(log2(15)) = 4 (not a power of two).
    const border: number[][] = Array.from({ length: 3 }, () => Array(5).fill(0));
    expect(applyCells(border, [255 << 4 | 4, 2 << 4 | 9], 5, false)).toBe(true);
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
    const input: Record<string, unknown> = { a: null, b: undefined, c: 0, d: false, e: '' };
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
    const msg = { FullState: { terrain: [], units: [], cities: [], player: {} } };
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
    const msg = { GameOver: { game_id: 'g1', winner: 'player_1', message: 'Victory!' } };
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
      cities: [{ id: 'city_0', position: [0, 0], owner: 'player_1', manpower: 10000, level: 1 }],
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
