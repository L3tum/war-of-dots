import { describe, it, expect } from 'vitest';
import { GameState } from './game_state';
import type { TerrainType } from './terrain';
import { Unit } from './units';

function makeMockUnit(id: string): Unit {
  return new Unit({
    id,
    owner: 'p1',
    position: [0, 0],
    unit_type: 'infantry',
    symbol: 'I',
    men: 1000,
    max_men: 1000,
    organisation: 100,
    equipment: [],
    max_equipment: [],
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
  });
}

describe('GameState', () => {
  it('initializes with empty state', () => {
    const state = new GameState();

    expect(state.terrain).toEqual([]);
    expect(state.visionGrid).toEqual([]);
    expect(state.borderGrid).toEqual([]);
    expect(state.units.size).toBe(0);
    expect(state.cities.size).toBe(0);
    expect(state.player.funds).toBe(0);
    expect(state.player.units_owned).toBe(0);
    expect(state.camera).toEqual({ x: 0, y: 0, zoom: 1.0 });
  });

  it('allows setting camera position', () => {
    const state = new GameState();
    state.camera = { x: 100, y: 200, zoom: 2.0 };

    expect(state.camera.x).toBe(100);
    expect(state.camera.y).toBe(200);
    expect(state.camera.zoom).toBe(2.0);
  });

  it('manages units map', () => {
    const state = new GameState();
    expect(state.units.size).toBe(0);

    state.units.set('unit_1', makeMockUnit('unit_1'));
    expect(state.units.size).toBe(1);
    expect(state.units.has('unit_1')).toBe(true);
    expect(state.units.has('unit_2')).toBe(false);

    state.units.set('unit_2', makeMockUnit('unit_2'));
    expect(state.units.size).toBe(2);

    state.units.delete('unit_1');
    expect(state.units.size).toBe(1);
    expect(state.units.has('unit_1')).toBe(false);
  });

  it('manages cities map', () => {
    const state = new GameState();
    expect(state.cities.size).toBe(0);

    state.cities.set('city_0', {
      id: 'city_0',
      position: [5, 5],
      owner: 'p1',
      manpower: 10000,
      level: 1,
    });

    expect(state.cities.size).toBe(1);
    const city = state.cities.get('city_0');
    expect(city).toBeDefined();
    expect(city?.id).toBe('city_0');
  });

  it('sets terrain grid', () => {
    const state = new GameState();
    const terrain: TerrainType[][] = [
      ['Plains', 'Water'],
      ['Forest', 'Hills'],
    ];
    state.terrain = terrain;

    expect(state.terrain.length).toBe(2);
    expect(state.terrain[0].length).toBe(2);
    expect(state.terrain[0][0]).toBe('Plains');
    expect(state.terrain[0][1]).toBe('Water');
  });

  it('sets vision grid', () => {
    const state = new GameState();
    state.visionGrid = [
      [true, false, true],
      [false, true, false],
    ];

    expect(state.visionGrid.length).toBe(2);
    expect(state.visionGrid[0][0]).toBe(true);
    expect(state.visionGrid[0][1]).toBe(false);
  });

  describe('player identity', () => {
    it('defaults ownPlayerId to null and builds an empty owner map', () => {
      const state = new GameState();
      expect(state.ownPlayerId).toBeNull();
      expect(state.playerList).toEqual([]);
      expect(state.ownerIdToIndex.size).toBe(0);
    });

    it('resolves color index from the owner map', () => {
      const state = new GameState();
      state.ownerIdToIndex = new Map([
        ['player_a3f2b1c0', 0],
        ['player_f0e1d2c3', 1],
      ]);
      expect(state.colorIndexForOwner('player_a3f2b1c0')).toBe(0);
      expect(state.colorIndexForOwner('player_f0e1d2c3')).toBe(1);
    });

    it('falls back to numeric parsing for benchmark ids', () => {
      const state = new GameState();
      expect(state.colorIndexForOwner('player_0')).toBe(0);
      expect(state.colorIndexForOwner('player_2')).toBe(2);
    });

    it('returns 0 for unknown or missing owners', () => {
      const state = new GameState();
      expect(state.colorIndexForOwner(null)).toBe(0);
      expect(state.colorIndexForOwner(undefined)).toBe(0);
      expect(state.colorIndexForOwner('player_zzzz')).toBe(0);
    });
  });
});
