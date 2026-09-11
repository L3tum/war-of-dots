/**
 * Golden-CBOR contract test — the frontend half of the Rust↔JS wire lockstep.
 *
 * Decodes `tests/fixtures/protocol_delta_golden.bin` (generated and
 * byte-locked by the Rust `protocol_delta_golden_contract` test in
 * `src/server/protocol.rs`) with plain cbor-x and asserts the same field
 * values. Any schema rename/reorder or I3/I4 constant change breaks one side
 * (or both). Regenerate the fixture (Rust side only):
 *
 *   cargo test --lib dump_protocol_delta_golden -- --ignored
 */
import { describe, it, expect } from 'vitest';
import { readFileSync } from 'node:fs';
import { fileURLToPath } from 'node:url';
import { dirname, join } from 'node:path';
import { decode, encode } from 'cbor-x';
import { unpackUnitPosition } from './connection';

const here = dirname(fileURLToPath(import.meta.url));
const fixture = readFileSync(join(here, '../../../tests/fixtures/protocol_delta_golden.bin'));

// I4 packed cells for a 4x4 grid (bits = 4): vision (0<<4)|5, (1<<4)|1;
// border (255<<4)|3 (no owner), (2<<4)|7 (player 2).
const GOLDEN_VISION_CELLS = [5, 17];
const GOLDEN_BORDER_CELLS = [4083, 39];
// I3: pack_unit_position((11, 5), (32, 16)) → (22528 << 16) | 20480.
const GOLDEN_PACKED_POS = (22528 << 16) | 20480;

interface EquipmentWire {
  name: string;
  count: number;
  cost_per_item: number;
}
interface UnitUpdateWire {
  id: string;
  position?: number;
  men?: number;
  equipment?: number[];
}
interface DeltaWire {
  vision_grid_diff?: number[];
  border_grid_diff?: number[];
  vision_cells_changed?: number[];
  border_cells_changed?: number[];
  grid_width?: number;
  units_added?: Array<{
    id: string;
    owner: string;
    position: [number, number];
    men: number;
    supply_percent: number;
    equipment: EquipmentWire[];
  }>;
  units_removed?: string[];
  unit_updates?: UnitUpdateWire[];
  city_updates?: Array<{ id: string; manpower?: number; level?: number }>;
  player: { funds: number; units_owned: number; player_id: string };
}

describe('protocol_delta_golden.bin (Rust↔JS CBOR contract)', () => {
  it('decodes as a DeltaState with the locked I3/I4 fields', () => {
    const msg = decode(fixture) as { DeltaState: DeltaWire };
    const delta = msg.DeltaState;

    expect(delta.grid_width).toBe(4);
    expect(delta.vision_cells_changed).toEqual(GOLDEN_VISION_CELLS);
    expect(delta.border_cells_changed).toEqual(GOLDEN_BORDER_CELLS);
    // The sparse forms replace the full-grid forms — neither is present.
    expect(delta.vision_grid_diff).toBeUndefined();
    expect(delta.border_grid_diff).toBeUndefined();

    const update = delta.unit_updates![0];
    expect(update.id).toBe('u2');
    expect(update.position).toBe(GOLDEN_PACKED_POS);
    expect(update.men).toBe(1000);
    expect(update.equipment).toEqual([25]);

    const added = delta.units_added![0];
    expect(added.id).toBe('u1');
    expect(added.owner).toBe('player_0');
    expect(added.position).toEqual([2.5, 5]);
    expect(added.men).toBe(1000);
    expect(added.supply_percent).toBe(75);
    expect(added.equipment[0].name).toBe('rifle');
    expect(added.equipment[0].count).toBe(100);
    expect(added.equipment[0].cost_per_item).toBe(2);

    expect(delta.units_removed).toEqual(['u0']);
    expect(delta.city_updates![0].id).toBe('c1');
    expect(delta.city_updates![0].manpower).toBe(42);
    expect(delta.city_updates![0].level).toBe(3);
    expect(delta.player.funds).toBe(500);
    expect(delta.player.units_owned).toBe(2);
    expect(delta.player.player_id).toBe('player_0');
  });

  it('I3: the packed position unpacks to (11, 5) against the 32x16 map', () => {
    // Runs the real frontend unpack (connection.ts) against the golden
    // constant set {65536 steps per axis, (x_q << 16) | y_q}.
    const pos = unpackUnitPosition(GOLDEN_PACKED_POS, {
      width: 32,
      height: 16,
    });
    expect(pos).toEqual({ x: 11, y: 5 });
  });

  it('cbor-x encodes whole-number floats as CBOR integers (the float_deser asymmetry)', () => {
    // The Rust server's `float_deser` exists because cbor-x emits a JS
    // whole-number float (e.g. `game_speed: 1000`) as a CBOR integer
    // (major type 0) instead of a CBOR float — which plain ciborium rejects
    // for f32/f64 fields. Pin the client half of that asymmetry:
    const bytes = encode({ v: 1000.0 });
    // map(1) "v" uint16(1000)
    expect(Array.from(bytes)).toEqual([0xb9, 0x00, 0x01, 0x61, 0x76, 0x19, 0x03, 0xe8]);
    // Non-integer values still go out as floats (f64 by default).
    const half = encode({ v: 0.5 });
    expect(Array.from(half)).toEqual([
      0xb9, 0x00, 0x01, 0x61, 0x76, 0xfb, 0x3f, 0xe0, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
    ]);
  });
});
