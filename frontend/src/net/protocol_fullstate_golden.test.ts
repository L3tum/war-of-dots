/**
 * Golden-CBOR contract test — FullState half of the Rust↔JS wire lockstep.
 *
 * Decodes `tests/fixtures/protocol_fullstate_golden.bin` (generated and
 * byte-locked by the Rust `protocol_fullstate_golden_contract` test in
 * `src/server/protocol.rs`) with plain cbor-x and asserts the same field
 * values as the Rust side.
 *
 * Every float in the fixture is a WHOLE number (1000, 100, 10, 20, …) on
 * purpose: cbor-x encodes those as CBOR *integers*, which plain ciborium
 * rejects for f32 fields — so this fixture byte-locks the `float_decoding`
 * path on the FullState (units) payload, not just the delta path. The single
 * non-whole float (`cost_per_item: 2.5`) is the control that real floats still
 * round-trip as f64.
 *
 * Regenerate the fixture (Rust side only):
 *
 *   cargo test --lib dump_protocol_fullstate_golden -- --ignored
 */
import { describe, it, expect } from 'vitest';
import { readFileSync } from 'node:fs';
import { fileURLToPath } from 'node:url';
import { dirname, join } from 'node:path';
import { decode } from 'cbor-x';

const here = dirname(fileURLToPath(import.meta.url));
const fixture = readFileSync(join(here, '../../../tests/fixtures/protocol_fullstate_golden.bin'));

interface EquipmentWire {
  name: string;
  count: number;
  cost_per_item: number;
}
interface UnitWire {
  id: string;
  owner: string;
  position: [number, number];
  unit_type: string;
  symbol: string;
  men: number;
  max_men: number;
  organisation: number;
  equipment: EquipmentWire[];
  max_equipment: EquipmentWire[];
  path: Array<[number, number]>;
  healing: boolean;
  resupply_enabled: boolean;
  supply_percent: number;
  encircled: boolean;
  attacking: boolean;
  on_water: boolean;
  boat_supply_percent: number;
  embarking: boolean;
  disembarking: boolean;
}
interface CityWire {
  id: string;
  position: [number, number];
  level: number;
  manpower: number;
  owner: string | null;
}
interface FullStateWire {
  vision_grid?: boolean[][] | null;
  border_grid?: number[][] | null;
  terrain: string[][];
  units: UnitWire[];
  cities: CityWire[];
  player: { funds: number; units_owned: number; player_id: string };
}

describe('protocol_fullstate_golden.bin (Rust↔JS CBOR contract)', () => {
  it('decodes as a FullState with the locked fields', () => {
    const msg = decode(fixture) as { FullState: FullStateWire };
    const state = msg.FullState;
    expect(state).toBeDefined();

    expect(state.terrain).toEqual([
      ['Plains', 'Forest'],
      ['Plains', 'Forest'],
    ]);
    expect(state.vision_grid).toEqual([
      [true, true],
      [true, false],
    ]);
    expect(state.border_grid).toEqual([
      [0, 1],
      [2, 255],
    ]);

    // Two units — the float_decoding canary (all-whole floats decode to numbers).
    expect(state.units).toHaveLength(2);
    const [u1, u2] = state.units;
    expect(u1.id).toBe('u1');
    expect(u1.owner).toBe('player_0');
    expect(u1.position).toEqual([10, 20]);
    expect(u1.men).toBe(1000); // whole float → CBOR integer → JS number
    expect(u1.max_men).toBe(1000);
    expect(u1.organisation).toBe(100);
    expect(u1.equipment).toHaveLength(1);
    expect(u1.equipment[0].name).toBe('rifle');
    expect(u1.equipment[0].count).toBe(100);
    expect(u1.equipment[0].cost_per_item).toBe(2.5); // non-whole control
    expect(u1.on_water).toBe(false);

    expect(u2.id).toBe('u2');
    expect(u2.owner).toBe('player_1');
    expect(u2.position).toEqual([5, 7]);
    expect(u2.men).toBe(800);
    expect(u2.on_water).toBe(true);
    expect(u2.boat_supply_percent).toBe(60);
    expect(u2.encircled).toBe(true);
    expect(u2.attacking).toBe(true);

    expect(state.cities).toHaveLength(1);
    expect(state.cities[0].id).toBe('c1');
    expect(state.cities[0].position).toEqual([3, 4]);
    expect(state.cities[0].level).toBe(2);
    expect(state.cities[0].manpower).toBe(5000);
    expect(state.cities[0].owner).toBe('player_0');

    expect(state.player.funds).toBe(1000);
    expect(state.player.units_owned).toBe(2);
    expect(state.player.player_id).toBe('player_0');
  });
});
