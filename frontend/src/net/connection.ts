// WebSocket connection — transport and message dispatch only.

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
import { setDisconnectedOverlay, showLobbyInfo, transitionToGameUI } from '../ui/game_ui';
import {
  applyCells,
  applyEquipmentCounts,
  cborDecode,
  cborEncode,
  flatTo2D,
  flatTo2DBools,
  mapSizeFromTerrain,
  unpackUnitPosition,
} from './protocol';

/**
 * WebSocket connection manager — transport + dispatch only.
 *
 * Handles the transport layer (WebSocket open/close/reconnect, Hello
 * handshake) and routes decoded ServerMessage variants to the game/UI
 * modules. Wire encode/decode lives in ./protocol, page-level DOM chrome in
 * ../ui/game_ui, and camera setup in Renderer.
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
      setDisconnectedOverlay(false);
      // WP3: Hello MUST be the first message (spectator connections are
      // read-only and use a different server endpoint that has no
      // handshake).
      if (!this.spectatorMode) {
        this.helloAcked = false;
        this.sendRaw({
          Hello: {
            protocol_version: Connection.PROTOCOL_VERSION,
            // v1 requests no feature bits; the field stays on the wire (byte-pinned).
            features: 0,
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
          const msg = cborDecode(uint8) as ServerMessage & Record<string, any>;
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
        setDisconnectedOverlay(true);
        this.reconnect();
      } catch (err) {
        console.error('Error in onclose handler:', err);
      }
    };

    this.ws.onerror = (err) => {
      try {
        console.error('WebSocket error', err);
        setDisconnectedOverlay(true);
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
      const encoded = cborEncode(message);
      this.ws.send(encoded);
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
        setDisconnectedOverlay(true);
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
        // Enter the waiting room: hide any game-over overlay, show the lobby
        // code, switch to the info tab.
        showLobbyInfo(msg.LobbyCreated.lobby_code, this.lobbyUI);
      } else if (msg.LobbyJoined) {
        // Mirror LobbyCreated: a joining player gets the same waiting-room
        // treatment (previously the guest's code display stayed blank after a
        // successful join).
        showLobbyInfo(msg.LobbyJoined.lobby_code, this.lobbyUI);
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
      this.renderer.centerCameraOn(this.gameState);

      // Hide lobby and show game UI
      transitionToGameUI(this.lobbyUI);
    } catch (err) {
      console.error('Error applying full state:', err);
    }
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
