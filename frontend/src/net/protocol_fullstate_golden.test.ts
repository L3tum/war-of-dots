/**
 * Golden-CBOR contract test — FullState half of the Rust↔JS wire lockstep.
 *
 * Decodes `tests/fixtures/protocol_fullstate_golden.bin` (generated and
 * byte-locked by the Rust `protocol_fullstate_golden_contract` test in
 * `src/server/protocol.rs`) with plain cbor-x and asserts the same field
 * values as the Rust side.
 *
 * What this locks: the *server→client* wire form of the full-state
 * (units_added) payload. The server encodes with ciborium (deterministic —
 * the Rust test re-encodes and byte-compares), the client decodes with
 * cbor-x — so any byte-level drift on either side fails exactly one of the
 * two tests. Note the direction: this does NOT exercise the client→server
 * `float_deser` integer-tolerance (that path is `LobbySettings` only and is
 * locked by `float_deser_accepts_cbor_x_integer_form_of_whole_floats`).
 *
 * Wire types are imported from `generated/protocol.ts` (the schema-derived
 * source of truth) rather than re-declared here, so a schema change that
 * breaks this fixture fails at the type level too.
 *
 * Floats are whole numbers (1000, 100, 10, 20, …) for readable assertions;
 * the single non-whole float (`cost_per_item: 2.5`) is the control that real
 * floats still round-trip as f64.
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
import type { GameStateMessage, ServerMessage } from '../generated/protocol';

const here = dirname(fileURLToPath(import.meta.url));
const fixture = readFileSync(join(here, '../../../tests/fixtures/protocol_fullstate_golden.bin'));

function fullStateOf(msg: ServerMessage): GameStateMessage | undefined {
  return typeof msg === 'object' && msg !== null && 'FullState' in msg ? msg.FullState : undefined;
}

describe('protocol_fullstate_golden.bin (Rust↔JS CBOR contract)', () => {
  it('decodes as a FullState with the locked fields', () => {
    const msg = decode(fixture) as ServerMessage;
    const state = fullStateOf(msg);
    expect(state, 'golden fixture must decode to a FullState envelope').toBeDefined();
    if (!state) return;

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

    // Two units — whole floats decode to exact JS numbers.
    expect(state.units).toHaveLength(2);
    const [u1, u2] = state.units;
    expect(u1.id).toBe('u1');
    expect(u1.owner).toBe('player_0');
    expect(u1.position).toEqual([10, 20]);
    expect(u1.men).toBe(1000);
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
