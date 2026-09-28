// Game state on the client side

import { TerrainType } from './terrain';
import { Unit } from './units';
import {
  PlayerState as ServerPlayerState,
  CityState as ServerCityState,
} from '../generated/protocol';

export class GameState {
  terrain: TerrainType[][] = [];
  visionGrid: boolean[][] = [];
  borderGrid: number[][] = [];
  units: Map<string, Unit> = new Map();
  cities: Map<string, ServerCityState> = new Map();
  player: ServerPlayerState = { funds: 0, units_owned: 0, player_id: '' };
  camera: { x: number; y: number; zoom: number } = { x: 0, y: 0, zoom: 1.0 };
  /** The connected player's own id (e.g. "player_a3f2b1c0"); null for spectators/replays. */
  ownPlayerId: string | null = null;
  /** Player list in lobby slot order — used to derive stable per-owner color indices. */
  playerList: Array<{ id: string; name: string; slot: number }> = [];
  /** owner id -> player slot index (color index). */
  ownerIdToIndex: Map<string, number> = new Map();

  /** Resolve a stable color index for an owner id, with numeric fallback for benchmark ids. */
  colorIndexForOwner(owner: string | null | undefined): number {
    if (!owner) return 0;
    const mapped = this.ownerIdToIndex.get(owner);
    if (mapped !== undefined) return mapped;
    // Fallback for numeric/benchmark ids like "player_0".
    const parsed = parseInt(owner.replace('player_', '') || '0', 10);
    return Number.isNaN(parsed) ? 0 : parsed;
  }
}
