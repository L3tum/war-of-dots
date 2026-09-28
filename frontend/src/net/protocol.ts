// CBOR wire codec + wire-format decode helpers for the War of Dots protocol.
//
// Owns every byte-level detail of the wire format: CBOR encode/decode (with
// the undefined-stripping rule serde_cbor requires), the I3 packed
// unit-position form, and the I4 sparse changed-cell grid diff. The wire
// bytes are byte-pinned against the server — see protocol_golden*.test.ts
// and the Rust-side contract tests in src/server/protocol.rs. This module
// must stay free of WebSocket/DOM/UI concerns (those live in
// net/connection.ts and ui/ respectively).

import { decode, encode } from 'cbor-x';

/**
 * Encode a protocol message to CBOR wire bytes.
 *
 * Strips undefined values before encoding — cbor-x encodes JS `undefined`
 * as CBOR undefined (0xf7), which serde_cbor on the server does NOT treat
 * as null. Removing them ensures missing fields use #[serde(default)].
 */
export function cborEncode(obj: unknown): Uint8Array {
  const cleaned = stripUndefined(obj);
  return encode(cleaned);
}

/** Decode CBOR wire bytes into a plain JS object. */
export function cborDecode(data: Uint8Array): unknown {
  return decode(data);
}

/** Convert a flat row-major array to a 2-D array (one row per width). */
export function flatTo2D(flat: number[], width: number): number[][] {
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
export function flatTo2DBools(flat: number[], width: number): boolean[][] {
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

/** Recursively remove `undefined` values from plain objects and arrays. */
export function stripUndefined(value: unknown): unknown {
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
