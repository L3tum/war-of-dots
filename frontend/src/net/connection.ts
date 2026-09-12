//! WebSocket connection — transport and message dispatch only.

import { decode, encode } from 'cbor-x';
import { CELL_SIZE } from '../game/config';
import { TerrainType } from '../game/terrain';
import {
  GameDeltaMessage,
  GameStateMessage,
  LeaderboardEntry,
  ServerMessage,
} from '../generated/protocol';
import { GameState } from '../game/game_state';
import { Renderer } from '../game/renderer';
import { Unit } from '../game/units';
import { Spectator } from '../ui/spectator';
import { Advisor } from '../ui/advisor';
import { LobbyUI } from '../ui/lobby';

const CBOR = {
  encode(obj: unknown): Uint8Array {
    // Strip undefined values before encoding — cbor-x encodes JS `undefined`
    // as CBOR undefined (0xf7), which serde_cbor on the server does NOT treat
    // as null.  Removing them ensures missing fields use #[serde(default)].
    const cleaned = stripUndefined(obj);
    return encode(cleaned);
  },
  decode(data: Uint8Array): unknown {
    return decode(data);
  },
};

/** Convert a flat row-major array to a 2-D array (one row per width). */
function flatTo2D(flat: number[], width: number): number[][] {
  const h = Math.ceil(flat.length / width);
  const grid: number[][] = new Array(h);
  for (let y = 0; y < h; y++) {
    const row = new Array(width);
    for (let x = 0; x < width; x++) row[x] = flat[y * width + x];
    grid[y] = row;
  }
  return grid;
}

/** Convert a flat row-major byte array to a 2-D boolean array. */
function flatTo2DBools(flat: number[], width: number): boolean[][] {
  const h = Math.ceil(flat.length / width);
  const grid: boolean[][] = new Array(h);
  for (let y = 0; y < h; y++) {
    const row = new Array(width);
    for (let x = 0; x < width; x++) row[x] = flat[y * width + x] !== 0;
    grid[y] = row;
  }
  return grid;
}

/**
 * I4: number of index bits needed to pack a row-major cell index for a grid
 * of `cells` cells — must match the server's `grid_index_bits`
 * (`ceil(log2(cells))`). Returns `null` when the grid is too large to pack
 * (an 8-bit value and the index must share a u32, i.e. at most 2^24 cells;
 * such grids always ship in the full-grid form).
 */
function gridIndexBits(cells: number): number | null {
  if (cells <= 1) return 0;
  let bits = 1;
  while (1 << bits < cells) bits += 1;
  return bits <= 24 ? bits : null;
}

/**
 * I4: apply a packed changed-cell list (sparse grid diff) to an existing 2-D
 * grid in place. Each entry packs the NEW value and the row-major cell index
 * into one u32: `(value << bits) | index`, with `bits` derived from the grid
 * dimensions — the client-side inverse of the server's `diff_grid`. Mutates
 * `grid`; returns false (with no mutation) when there is no usable baseline
 * grid — the sparse form always requires the previous grid, which the client
 * holds from the last full state/diff.
 */
export function applyCells(
  grid: number[][] | boolean[][],
  cells: number[],
  width: number,
  toBool: boolean,
): boolean {
  if (width === 0 || grid.length === 0 || grid[0] === undefined) return false;
  const bits = gridIndexBits(grid.length * width);
  if (bits === null) return false;
  const mask = bits === 0 ? 0 : (1 << bits) - 1;
  for (const packed of cells) {
    const index = bits === 0 ? 0 : packed & mask;
    const value = (bits === 0 ? packed : packed >> bits) & 0xff;
    const row = grid[Math.floor(index / width)];
    const col = index % width;
    if (row === undefined || col >= row.length) continue;
    if (toBool) (row as boolean[])[col] = value !== 0;
    else (row as number[])[col] = value;
  }
  return true;
}

/**
 * I3: quantization steps per map axis for packed unit positions — each axis
 * is a `u16` quantized to 65,536 steps ACROSS THE FULL MAP (see the server's
 * `UNIT_POSITION_STEPS` / `pack_unit_position`). This constant MUST stay in
 * lockstep with `src/server/protocol.rs`.
 */
export const UNIT_POSITION_STEPS = 65_536;

/**
 * I3: decode a packed unit position (single `u32`: `x_q << 16 | y_q`, each
 * axis quantized to `u16` over the full map) back to cell coordinates. This is
 * the client-side inverse of the server's `pack_unit_position` — pass the
 * same `map_size` (terrain width, height in cells) the server packed with.
 */
export function unpackUnitPosition(
  packed: number,
  mapSize: { width: number; height: number },
): { x: number; y: number } {
  const xq = packed >>> 16;
  const yq = packed & 0xffff;
  return {
    x: (xq * mapSize.width) / UNIT_POSITION_STEPS,
    y: (yq * mapSize.height) / UNIT_POSITION_STEPS,
  };
}

/**
 * I3: map size in cells from the client's 2-D terrain grid (rows × cols), or
 * `null` when the grid is not ready — packed unit positions cannot be decoded
 * until the full state has arrived.
 */
export function mapSizeFromTerrain(terrain: unknown[][]): { width: number; height: number } | null {
  const height = terrain.length;
  const first = height > 0 ? terrain[0] : undefined;
  const width = first !== undefined ? first.length : 0;
  return width > 0 && height > 0 ? { width, height } : null;
}

/**
 * I3: apply a per-slot equipment COUNTS update (in `UnitState.equipment` slot
 * order) to a unit, keeping slot names and `cost_per_item` from the
 * once-per-lifetime full state. Returns `false` when the counts do not line up
 * with the full-state slot table (different length) — the caller should keep
 * the previous equipment rather than corrupt slot order.
 */
export function applyEquipmentCounts(
  unit: { equipment: import('../generated/protocol').EquipmentState[] },
  counts: number[],
): boolean {
  if (unit.equipment.length !== counts.length) return false;
  unit.equipment = unit.equipment.map((slot, i) => ({ ...slot, count: counts[i] }));
  return true;
}

/** Re-export for replay player. */
export { flatTo2D, flatTo2DBools };

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

/**
 * WebSocket connection manager.
 *
 * Handles the transport layer (WebSocket open/close/reconnect) and
 * message dispatch. Does NOT handle game logic — that is delegated
 * to GameState, Renderer, LobbyUI, etc.
 *
 * Message format: CBOR-encoded protocol buffers.
 * - Client → Server: ClientMessage variants
 * - Server → Client: ServerMessage variants
 */
export class Connection {
  private ws: WebSocket | null = null;
  private gameState: GameState;
  private renderer: Renderer;
  private reconnectTimer: number | null = null;
  private reconnectDelay = 1000; // start at 1 second, doubles each attempt
  private maxReconnectDelay = 30000; // cap at 30 seconds

  // WP3: handshake — the server requires a Hello as the first message and
  // answers HelloAck before any lobby/game traffic. Sends that arrive
  // before the ack are queued (bounded); a rejected handshake is fatal
  // (no reconnect storm against a server that will never accept us).
  private static readonly PROTOCOL_VERSION = 1;
  private static readonly FEATURES = 0; // no feature bits requested in v1
  private helloAcked = false;
  private handshakeFailed = false;
  private pendingSends: Array<Record<string, unknown>> = [];

  // UI references — delegates all display logic to UI modules
  private spectator: Spectator;
  private advisor: Advisor;
  private lobbyUI: LobbyUI | null;

  constructor(
    gameState: GameState,
    renderer: Renderer,
    spectator: Spectator,
    advisor: Advisor,
    lobbyUI: LobbyUI | null,
    spectatorMode = false,
  ) {
    this.gameState = gameState;
    this.renderer = renderer;
    this.spectator = spectator;
    this.advisor = advisor;
    this.lobbyUI = lobbyUI;
    this.spectatorMode = spectatorMode;
  }

  setLobbyUI(lobbyUI: LobbyUI): void {
    this.lobbyUI = lobbyUI;
  }

  connect() {
    this.connectTo('/ws');
  }

  /** Connect to an explicit socket path (used for spectator / replay mode). */
  connectTo(path: string) {
    const protocol = location.protocol === 'https:' ? 'wss' : 'ws';
    this.ws = new WebSocket(`${protocol}://${location.host}${path}`);
    this.ws.binaryType = 'arraybuffer';

    this.ws.onopen = () => {
      console.log('Connected to server');
      this.reconnectDelay = 1000; // reset delay on success
      this.showDisconnected(false);
      // WP3: Hello MUST be the first message (spectator connections are
      // read-only and use a different server endpoint that has no
      // handshake).
      if (!this.spectatorMode) {
        this.helloAcked = false;
        this.sendRaw({
          Hello: {
            protocol_version: Connection.PROTOCOL_VERSION,
            features: Connection.FEATURES,
          },
        });
      }
    };

    this.ws.onmessage = (event: MessageEvent) => {
      const data = event.data;
      try {
        if (data instanceof ArrayBuffer) {
          const uint8 = new Uint8Array(data);
          // eslint-disable-next-line @typescript-eslint/no-explicit-any
          const msg = CBOR.decode(uint8) as ServerMessage & Record<string, any>;
          // Debug: log received message in dev mode
          if (location.hostname === 'localhost' || location.hostname === '127.0.0.1') {
            console.debug('[WS] ←', msg);
          }
          this.handleServerMessage(msg);
        } else if (typeof data === 'string') {
          // eslint-disable-next-line @typescript-eslint/no-explicit-any
          const msg = JSON.parse(data) as ServerMessage & Record<string, any>;
          this.handleServerMessage(msg);
        }
      } catch (err) {
        console.error('Failed to decode message:', err);
      }
    };

    this.ws.onclose = () => {
      try {
        // Debug-only log (avoid noisy prod logging)
        if (location.hostname === 'localhost' || location.hostname === '127.0.0.1') {
          console.log('Disconnected');
        }
        this.showDisconnected(true);
        this.reconnect();
      } catch (err) {
        console.error('Error in onclose handler:', err);
      }
    };

    this.ws.onerror = (err) => {
      try {
        console.error('WebSocket error', err);
        this.showDisconnected(true);
      } catch (handlerErr) {
        console.error('Error in onerror handler:', handlerErr);
      }
    };
  }

  private spectatorMode = false;

  send(message: Record<string, unknown>) {
    // Spectator / replay connections are read-only — never send control messages.
    if (this.spectatorMode) return;
    // WP3: the server rejects lobby/game traffic until the Hello handshake
    // completes. Queue briefly (bounded) instead of dropping — a
    // user-initiated send can race the ack in the first moments of a
    // connection.
    if (!this.helloAcked) {
      if (this.pendingSends.length >= 16) {
        console.warn('[WS] dropping send before HelloAck (queue full)');
        return;
      }
      this.pendingSends.push(message);
      return;
    }
    this.sendRaw(message);
  }

  private sendRaw(message: Record<string, unknown>) {
    if (this.ws && this.ws.readyState === WebSocket.OPEN) {
      const encoded = CBOR.encode(message);
      // Debug: log sent message in dev mode
      if (location.hostname === 'localhost' || location.hostname === '127.0.0.1') {
        console.debug('[WS] →', message);
      }
      this.ws.send(encoded);
    }
  }

  private showDisconnected(isDisconnected: boolean) {
    const overlay = document.getElementById('disconnected-overlay');
    if (overlay) {
      overlay.style.display = isDisconnected ? 'flex' : 'none';
    }
  }

  private reconnect() {
    // WP3: a rejected handshake is fatal — retrying cannot succeed against
    // this server, so don't hammer it with reconnect attempts.
    if (this.handshakeFailed) return;
    if (this.reconnectTimer) clearTimeout(this.reconnectTimer);
    this.reconnectTimer = setTimeout(() => {
      this.connect();
    }, this.reconnectDelay) as unknown as number;
    // Exponential backoff
    this.reconnectDelay = Math.min(this.reconnectDelay * 2, this.maxReconnectDelay);
  }

  // eslint-disable-next-line @typescript-eslint/no-explicit-any
  private handleServerMessage(msg: ServerMessage & Record<string, any>) {
    try {
      // WP3: handshake completion — the server accepted our protocol
      // version. Flush any sends that raced the ack.
      if (msg.HelloAck) {
        this.helloAcked = true;
        if (msg.HelloAck.features) {
          console.log('[WS] negotiated features:', msg.HelloAck.features);
        }
        const pending = this.pendingSends;
        this.pendingSends = [];
        for (const m of pending) this.sendRaw(m);
        return;
      }
      if (!this.helloAcked && 'Error' in msg) {
        // Any Error before HelloAck is a handshake rejection (unknown
        // protocol version / Hello not first).
        console.error('Handshake rejected by server:', msg.Error?.message);
        this.handshakeFailed = true;
        this.showDisconnected(true);
        return;
      }
      // The server uses Rust enum serialization: { FullState: {...} }, { DeltaState: {...} }
      // E-5: Use typed ServerMessage from generated protocol. The & Record<string, any>
      // allows accessing runtime fields that the generated types may not fully cover
      // (e.g., optional vs required field mismatches between Rust and TypeScript).
      if (msg.FullState) {
        this.applyFullState(msg.FullState);
      } else if (msg.DeltaState) {
        this.applyDelta(msg.DeltaState);
      } else if ('ClearSpectator' in msg) {
        // Unit variant — the server serializes it as { ClearSpectator: null },
        // so a truthy check would never match.
        this.spectator.clearAll();
      } else if (msg.GameOver) {
        this.spectator.gameOver(msg.GameOver.winner, msg.GameOver.game_id, msg.GameOver.message);
        // Re-enable Start Game so a new lobby/game can be started.
        if (this.lobbyUI) {
          this.lobbyUI.enableStartButton();
        }
      } else if (msg.LobbyCreated) {
        // Hide game-over overlay if showing
        const overlay = document.getElementById('game-over-overlay');
        if (overlay) overlay.classList.add('hidden');

        // Set lobby code and show waiting room
        const codeDisplay = document.getElementById('lobby-code-display');
        if (codeDisplay) codeDisplay.textContent = msg.LobbyCreated.lobby_code;

        // Delegate to LobbyUI for tab/panel management
        if (this.lobbyUI) {
          this.lobbyUI.showTab('info');
        }
      } else if (msg.LobbyJoined) {
        // Hide game-over overlay if showing
        const overlay = document.getElementById('game-over-overlay');
        if (overlay) overlay.classList.add('hidden');

        // Mirror LobbyCreated: set the code display so a joining player can
        // read/share the lobby code (previously the guest's display stayed
        // blank after a successful join).
        const codeDisplay = document.getElementById('lobby-code-display');
        if (codeDisplay) codeDisplay.textContent = msg.LobbyJoined.lobby_code;

        // Delegate to LobbyUI for tab/panel management
        if (this.lobbyUI) {
          this.lobbyUI.showTab('info');
        }
      } else if (msg.LobbyUpdate) {
        if (this.lobbyUI) {
          this.lobbyUI.updatePlayers(msg.LobbyUpdate.players);
        }
        // Build a stable owner-id -> slot-index map from lobby slot order.
        const ownerIdToIndex = new Map<string, number>();
        const playerList: Array<{ id: string; name: string; slot: number }> = [];
        const players: Array<{ id: string; name: string }> = msg.LobbyUpdate.players ?? [];
        for (let i = 0; i < players.length; i++) {
          ownerIdToIndex.set(players[i].id, i);
          playerList.push({ id: players[i].id, name: players[i].name, slot: i });
        }
        this.gameState.playerList = playerList;
        this.gameState.ownerIdToIndex = ownerIdToIndex;
      } else if (msg.LlmReasonLog) {
        this.spectator.addReason(
          msg.LlmReasonLog.player_name,
          msg.LlmReasonLog.action,
          msg.LlmReasonLog.reason,
        );
      } else if (msg.Advice) {
        this.advisor.show(msg.Advice.text);
      } else if (msg.Leaderboard) {
        if (this.lobbyUI) {
          this.lobbyUI.showLeaderboard(msg.Leaderboard.entries as LeaderboardEntry[]);
        }
      } else if (msg.Error) {
        console.error('Server error:', msg.Error.message);
        // If StartGame was rejected (e.g. a game already running for this lobby),
        // re-enable the button so the player can try again.
        if (this.lobbyUI) {
          this.lobbyUI.enableStartButton();
        }
      }
    } catch (err) {
      console.error('Error handling server message:', err);
    }
  }

  private applyFullState(state: GameStateMessage) {
    try {
      this.renderer.markDataDirty();
      this.gameState.terrain = state.terrain as TerrainType[][];
      this.gameState.visionGrid = state.vision_grid ?? [];
      this.gameState.borderGrid = state.border_grid ?? [];
      this.renderer.markFogDirty();
      this.renderer.markBorderDirty();
      for (const unitData of state.units) {
        const unit = new Unit(unitData);
        this.gameState.units.set(unitData.id, unit);
      }
      for (const cityData of state.cities) {
        this.gameState.cities.set(cityData.id, cityData);
      }
      this.gameState.player.funds = state.player.funds;
      this.gameState.player.units_owned = state.player.units_owned;
      if (state.player.player_id) {
        this.gameState.ownPlayerId = state.player.player_id;
      }

      // Center camera on the player's own territory (or map center for spectators)
      this.centerCamera();

      // Hide lobby and show game UI
      this.transitionToGameUI();
    } catch (err) {
      console.error('Error applying full state:', err);
    }
  }

  /** Center camera: on own territory when playing, on map center for spectators. */
  private centerCamera(): void {
    const ownId = this.gameState.ownPlayerId;
    if (ownId) {
      let sumX = 0;
      let sumY = 0;
      let count = 0;
      for (const unit of this.gameState.units.values()) {
        if (unit.owner === ownId) {
          sumX += unit.position.x * CELL_SIZE;
          sumY += unit.position.y * CELL_SIZE;
          count++;
        }
      }
      for (const [, city] of this.gameState.cities) {
        if (city.owner === ownId) {
          sumX += city.position[0] * CELL_SIZE;
          sumY += city.position[1] * CELL_SIZE;
          count++;
        }
      }
      if (count > 0) {
        this.renderer.camera.x = sumX / count;
        this.renderer.camera.y = sumY / count;
        this.fitMapZoom();
        return;
      }
    }
    this.centerCameraOnTerrain();
  }

  /** Zoom out so the whole map fits the viewport (with a 10% margin). */
  private fitMapZoom(): void {
    const terrain = this.gameState.terrain;
    if (!terrain || terrain.length === 0 || terrain[0].length === 0) return;
    let maxCol = 0;
    for (const row of terrain) {
      if (row.length > 0) maxCol = Math.max(maxCol, row.length - 1);
    }
    const mapW = (maxCol + 1) * CELL_SIZE;
    const mapH = terrain.length * CELL_SIZE;
    this.renderer.fitMapZoom(mapW, mapH);
  }

  /** Center the camera on the terrain map */
  private centerCameraOnTerrain(): void {
    const terrain = this.gameState.terrain;
    if (!terrain || terrain.length === 0 || terrain[0].length === 0) return;

    // Find the bounding box of non-empty terrain rows (handle ragged grids)
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

    this.renderer.camera.x = centerX;
    this.renderer.camera.y = centerY;
    this.fitMapZoom();
  }

  /** Hide lobby panel and show game UI elements */
  private transitionToGameUI(): void {
    // Hide lobby panel
    this.lobbyUI?.hide();

    // Show stats panel
    const statsPanel = document.getElementById('stats-panel');
    if (statsPanel) statsPanel.classList.remove('hidden');

    // Show city labels container
    const cityLabels = document.getElementById('city-labels');
    if (cityLabels) cityLabels.classList.remove('hidden');
  }

  private applyDelta(delta: GameDeltaMessage) {
    try {
      this.renderer.markDataDirty();
      if (delta.vision_grid_diff && delta.grid_width) {
        this.gameState.visionGrid = flatTo2DBools(delta.vision_grid_diff, delta.grid_width);
        this.renderer.markFogDirty();
      } else if (delta.vision_cells_changed && delta.grid_width) {
        // I4: sparse changed-cell list (packed u32) — patch the current grid.
        if (
          applyCells(this.gameState.visionGrid, delta.vision_cells_changed, delta.grid_width, true)
        ) {
          this.renderer.markFogDirty();
        }
      }
      if (delta.border_grid_diff && delta.grid_width) {
        this.gameState.borderGrid = flatTo2D(delta.border_grid_diff, delta.grid_width);
        this.renderer.markBorderDirty();
      } else if (delta.border_cells_changed && delta.grid_width) {
        // I4: sparse changed-cell list (packed u32) — patch the current grid.
        if (
          applyCells(this.gameState.borderGrid, delta.border_cells_changed, delta.grid_width, false)
        ) {
          this.renderer.markBorderDirty();
        }
      }
      for (const unit of delta.units_added || []) {
        const newUnit = new Unit(unit);
        this.gameState.units.set(unit.id, newUnit);
      }
      for (const id of delta.units_removed || []) {
        this.gameState.units.delete(id);
      }
      // I3: packed unit positions are decoded against the terrain map size
      // (cell dims), and equipment updates carry per-slot COUNTS only —
      // decoded against the unit's full-state slot table (names/costs come
      // from the once-per-lifetime full state).
      const mapSize = mapSizeFromTerrain(this.gameState.terrain);
      for (const update of delta.unit_updates || []) {
        const unit = this.gameState.units.get(update.id);
        if (unit) {
          // Packed position 0 (the origin) is a valid value — use `!= null`.
          if (update.position != null && mapSize) {
            unit.position = unpackUnitPosition(update.position, mapSize);
          }
          if (update.men != null) unit.men = update.men;
          if (update.organisation != null) unit.organisation = update.organisation;
          if (update.equipment) {
            if (!applyEquipmentCounts(unit, update.equipment)) {
              console.warn(`equipment counts for unit ${update.id} do not match its slot table`);
            }
          }
          if (update.path) unit.path = update.path;
          if (update.healing != null) unit.healing = update.healing;
          if (update.resupply_enabled != null) unit.resupplyEnabled = update.resupply_enabled;
          if (update.supply_percent != null) unit.supplyPercent = update.supply_percent;
          if (update.encircled != null) unit.encircled = update.encircled;
          if (update.attacking != null) unit.attacking = update.attacking;
          // Water travel fields
          if (update.on_water != null) unit.onWater = update.on_water;
          if (update.boat_supply_percent != null)
            unit.boatSupplyPercent = update.boat_supply_percent;
          if (update.embarking != null) unit.embarking = update.embarking;
          if (update.disembarking != null) unit.disembarking = update.disembarking;
        }
      }
      for (const update of delta.city_updates || []) {
        const city = this.gameState.cities.get(update.id);
        if (city) {
          if (update.manpower != null) city.manpower = update.manpower;
          if (update.owner != null) city.owner = update.owner;
          if (update.level != null) city.level = update.level;
        }
      }
      if (delta.player) {
        this.gameState.player.funds = delta.player.funds;
        if (delta.player.units_owned !== undefined) {
          this.gameState.player.units_owned = delta.player.units_owned;
        }
        if (delta.player.player_id) {
          this.gameState.ownPlayerId = delta.player.player_id;
        }
      }
    } catch (err) {
      console.error('Error applying delta:', err);
    }
  }
}
