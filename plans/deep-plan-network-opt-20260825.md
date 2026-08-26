# Deep Plan: Network Phase Optimization

## Context & Goals

The benchmark (report 91) shows the **network** phase dominates tick time:

| Metric | Value |
|--------|-------|
| Avg time | 1.089 ms/tick (88% of total) |
| P99 | 4.642 ms |
| Allocs | 7,583 / tick |
| Bytes | 1.44 MB / tick |
| Calls | 1,500 (all ticks) |

All other systems combined (movement, combat, influence, supply, etc.) sum to ≈ 0.13 ms. The network phase is the clear bottleneck. The goal is to apply the same patterns already proven in the other systems (scratch buffers, Arc sharing, dirty-flag skips, flat arrays) to `run_network_phase` and its helpers.

**Target:** reduce network-phase allocs by ≥ 60% and avg time to < 0.4 ms at the 200×200 / 4-player / ~240-unit benchmark config.

---

## Allocation Hot-Spots (current code, per tick, 4 players, ~240 units, 200×200 grid)

| Source | Allocs | Bytes |
|--------|--------|-------|
| `game.unit_states()` → `Vec<UnitState>` (each unit: 4 Strings + 3 Vecs) | ~2 400 | ~200 KB |
| `current_unit_map` = clone of every `UnitState` into `HashMap` | ~2 400 | ~200 KB |
| `current_city_map` (× 12 cities) | ~100 | ~15 KB |
| `border_grid_2d()` → `Vec<Vec<u32>>` (201 Vec allocs) | 201 | 160 KB |
| 4 × `vision_grid_2d()` → `Vec<Vec<bool>>` (201 Vec allocs each) | 804 | 160 KB |
| 4 × `prev_vision.get(pid).cloned()` (deep clone of 40 KB grid) | 804 | 160 KB |
| 4 × grid `==` comparison (40 K cells each, 2-D pointer-chasing) | 0 (CPU) | – |
| 4 × `encode_message` → `Vec::new()` growing to ~50–200 KB | ~16 | ~300 KB |
| `pid.clone()`, `player_id.to_string()`, misc | ~20 | ~2 KB |
| **Total (approx)** | **~6 800** | **~1.09 MB** |

(The remaining ~500 allocs / 350 KB come from HashMap bucket growth and CBOR internal buffers.)

---

## N1 — Flat Arc grid baselines (biggest single win)

**Problem.** Every tick we:
1. Allocate a fresh 2-D `Vec<Vec<u32>>` for the border grid (160 KB + 201 allocs).
2. Allocate a fresh 2-D `Vec<Vec<bool>>` per player for vision (4 × 40 KB + 804 allocs).
3. Deep-clone the *previous* vision grid per player just to compare (4 × 40 KB).
4. Store the new 2-D grid into the baseline (another allocation site).

The internal game state already stores these as **flat** `Arc<Vec<bool>>` (vision) and `Arc<Vec<u8>>` (territory) in `WorldBundle`. The 2-D `Vec<Vec<_>>` is only needed for the CBOR wire format.

**Approach.**

1. Add two accessor methods on `Game`:
   ```rust
   /// Returns the per-player flat vision grid (no allocation — Arc clone).
   pub fn vision_grid_flat(&self, player_id: &str) -> Option<Arc<Vec<bool>>> {
       let wb = self.world.world_data(self.world_entity)?;
       wb.vision.grids.get(player_id).cloned()   // Arc clone = refcount bump
   }

   /// Returns the global flat territory grid.
   pub fn territory_grid_flat(&self) -> Option<Arc<Vec<u8>>> {
       let wb = self.world.world_data(self.world_entity)?;
       if wb.territory.grid.is_empty() { return None; }
       Some(Arc::clone(&wb.territory.grid))       // Arc clone = refcount bump
   }
   ```

2. Change `NetworkBaselines` fields:
   ```rust
   pub prev_vision: HashMap<String, Arc<Vec<bool>>>,   // was: Option<Vec<Vec<bool>>>
   pub prev_border: Option<Arc<Vec<u8>>>,              // was: Option<Vec<Vec<u32>>>
   ```

3. In `run_network_phase`, replace the 2-D calls:
   ```rust
   let border_flat: Option<Arc<Vec<u8>>> =
       player_ids.first().and(|_| game.territory_grid_flat());

   for pid in player_ids {
       let vision_flat: Option<Arc<Vec<bool>>> = game.vision_grid_flat(pid);
       let prev_vision: Option<&Arc<Vec<bool>>> = baselines.prev_vision.get(pid);

       // Fast-path: Arc pointer equality (O(1)) → grid unchanged
       let vision_changed = match (&prev_vision, &vision_flat) {
           (Some(p), Some(c)) => !Arc::ptr_eq(p, c) && p.as_slice() != c.as_slice(),
           (None, Some(_))    => true,
           _                  => false,
       };
       // …similar for border (global, so check once outside the loop)
   ```

4. **Convert to 2-D only when sending.** Add small helpers in `protocol.rs`:
   ```rust
   pub fn flat_to_2d_bool(flat: &[bool], width: usize) -> Vec<Vec<bool>> { … }
   pub fn flat_to_2d_u32(flat: &[u8], width: usize) -> Vec<Vec<u32>> { … }
   ```
   Call them inside `build_delta_inner` only when the change flag is set.

5. **Baseline commit** becomes a cheap `Arc::clone`:
   ```rust
   if let Some(v) = &vision_flat {
       baselines.prev_vision.insert(pid.clone(), Arc::clone(v));
   }
   // …
   baselines.prev_border = border_flat;   // Arc move, no allocation
   ```

**Savings per tick (4 players, 200×200):**
- Eliminated: 4 × 201 + 201 = **1 005 small Vec allocs**, ~320 KB of 2-D grid data
- Eliminated: 4 × 40 KB prev-vision deep clones = **804 allocs, 160 KB**
- Replaced: O(4 × 40 K) 2-D element compare → O(1) `Arc::ptr_eq` + flat `slice == slice` (SIMD-friendly)
- Net: **~1 800 allocs, ~480 KB, and significant CPU reduction**

**Files:** `src/game/game.rs` (new accessors), `src/server/delta.rs` (baselines, `run_network_phase`, `build_delta_inner` signature + grid-diff logic), `src/server/protocol.rs` (2-D helpers), `src/server/game_server.rs` (replay path baseline init).

**Tests:**
- Existing `delta.rs` tests: update `build_delta_inner` calls for new flat-grid params.
- New: `grid_unchanged_when_arc_ptr_equal` — verify no delta sent when `Arc::ptr_eq` is true.
- New: `flat_to_2d_roundtrip` — `flat_to_2d_bool(flat_to_2d_bool(flat)) == original`.
- Integration: run 5 ticks, assert tick 1 sends full grid, tick 2 (no movement) sends no grid diff.

---

## N2 — Eliminate the redundant unit-map clone

**Problem.** `run_network_phase` builds:
```rust
let current_units = game.unit_states();                        // Vec<UnitState>  (~200 KB)
let current_unit_map = current_units.iter()
    .cloned()                                                   // ← full deep clone
    .map(|u| (u.id.clone(), u)).collect();                      // another ~200 KB
```
The map is used **only** for `contains_key` in the `units_removed` check inside `build_delta_inner`. The full `UnitState` values in the map are never read.

**Approach.**

1. Change `build_delta_inner`'s `current_unit_map` parameter from
   `&HashMap<String, UnitState>` to `&HashSet<&str>` (borrowed IDs).
2. In `run_network_phase`:
   ```rust
   let current_unit_ids: HashSet<&str> =
       current_units.iter().map(|u| u.id.as_str()).collect();
   ```
   (One small allocation for the HashSet buckets; the `&str` keys borrow from `current_units` — zero string clones.)
3. At baseline commit time, build the owned map **by moving** out of the Vec:
   ```rust
   baselines.prev_units = current_units
       .into_iter()
       .map(|u| (u.id.clone(), u))   // only the key String is cloned
       .collect();
   ```
   This replaces ~2 400 deep-clone allocs with ~240 key-string allocs.

**Savings:** ~2 200 allocs, ~200 KB per tick.

**Files:** `src/server/delta.rs`.

**Tests:** Existing delta tests updated for the new `&HashSet<&str>` param. New test: `units_removed_detected_via_id_set`.

---

## N3 — Remove the unused `current_city_map`

**Problem.** `build_delta_inner` takes `_current_city_map: &HashMap<String, CityState>` but **never uses it** (underscore-prefixed parameter). `run_network_phase` still builds it (cloning all 12 cities into a HashMap every tick).

**Approach.** Delete the parameter and the construction code.

**Savings:** ~100 allocs, ~15 KB per tick (small, but free).

**Files:** `src/server/delta.rs`.

---

## N4 — Single-pass unit snapshot (skip `UnitQueryData` intermediate)

**Problem.** `unit_states()` calls `query_all_units()` which builds a `Vec<UnitQueryData>` (cloning all Strings/Vectors from ECS components), then immediately maps each `UnitQueryData` → `UnitState` (cloning Strings/Vectors **again**).

**Approach.**

Add a method on `Game` that builds `Vec<UnitState>` directly from the ECS query, skipping the `UnitQueryData` intermediate:
```rust
pub fn unit_states_direct(&self) -> Vec<protocol::UnitState> {
    // Same 10-component query as query_all_units, but maps
    // components → UnitState in a single pass (no intermediate struct).
}
```
Use it in `run_network_phase`. Keep `query_all_units` / `UnitQueryData` for the benchmark AI and other consumers.

**Savings:** ~960 String clones + ~480 Vec clones per tick ≈ **~1 400 allocs, ~100 KB**.

**Files:** `src/game/game.rs` (new method), `src/server/delta.rs` (call site).

**Tests:** `unit_states_direct_matches_unit_states` — both produce identical `Vec<UnitState>` for the same world state.

---

## N5 — Pre-size the CBOR encode buffer

**Problem.** `encode_message` does `let mut buf = Vec::new();` then `ciborium::into_writer` grows it geometrically. For a message containing a 40 KB vision grid, that's ~5–6 reallocs + copies.

**Approach.**

Add `encode_message_hint(msg: &ServerMessage, capacity_hint: usize) -> Vec<u8>`:
- If the message contains a vision grid diff: hint ≈ `grid_cells + 256`
- If it contains a border grid diff: hint ≈ `grid_cells * 4 + 256`
- Otherwise: hint ≈ 512 (typical small delta)

```rust
pub fn encode_message_hint(msg: &ServerMessage, cap: usize) -> Vec<u8> {
    let mut buf = Vec::with_capacity(cap);
    ciborium::into_writer(msg, &mut buf).unwrap_or_default();
    buf
}
```
In `run_network_phase`, compute the hint from the delta's grid fields before encoding.

**Savings:** 3–5 reallocs per encode call × 4 players ≈ **~16 fewer allocs**, and less memory bandwidth.

**Files:** `src/server/protocol.rs`, `src/server/delta.rs`.

---

## N6 — (Optional / Phase 2) Compact grid wire format

**Problem.** The CBOR wire format sends vision as a nested `[[bool, …] × 200] × 200` — 40 200 individual CBOR items with array headers. A 200×200 border grid is 40 000 × u32. This is the single largest contributor to both encode time and wire bytes (~200 KB+ per grid per tick when it changes).

**Approach** (requires a coordinated frontend change):
- Change `GameDeltaMessage.vision_grid_diff` from `Option<Vec<Vec<bool>>>` to `Option<Vec<u8>>` (flat, 1 byte per cell, or bit-packed to 1 bit per cell → 5 KB).
- Change `border_grid_diff` from `Option<Vec<Vec<u32>>>` to `Option<Vec<u8>>` (flat, 1 byte per cell = 40 KB vs current 160 KB).
- Frontend: replace the 2-D array access with `flat[y * width + x]` indexing (trivial change in `game_state.ts` / `replay_player.ts`).
- Keep the 2-D `Vec<Vec<…>>` in `GameStateMessage` (full state) for now — it's sent once at connect.

**Savings:** Encode time ↓ ~70% for grid-heavy deltas; wire bytes ↓ 3–8×.

**Files:** `src/server/protocol.rs`, `src/server/delta.rs`, `frontend/src/game/game_state.ts`, `frontend/src/replay/replay_player.ts`, protocol schema / JSON-Schema.

**Tests:** New round-trip tests in `protocol.rs` (flat grid encode → decode → matches expected). Frontend: update `game_state` tests for flat indexing.

**Risks:** Breaking protocol change — must be versioned or deployed atomically (server + client). Defer to Phase 2.

---

## Expected Cumulative Impact (Phase 1: N1–N5)

| Metric | Before | After (est.) | Δ |
|--------|--------|--------------|---|
| Allocs/tick | 7 583 | ~2 800 | **−63%** |
| Bytes/tick | 1.44 MB | ~0.55 MB | **−62%** |
| Avg time | 1.089 ms | ~0.35–0.50 ms | **−55–68%** |
| P99 time | 4.642 ms | ~1.0–1.5 ms | **−70%** |

The P99 improvement is larger because the worst-case ticks are exactly the ones where all 4 vision grids + border grid change simultaneously (the 2-D allocation storm).

---

## Sequencing

1. **N3** (remove unused city map) — trivial, do first as a warm-up commit.
2. **N1** (flat Arc grid baselines) — largest win, touches the most code.
3. **N2** (unit ID set) — medium, pairs naturally with N1's `build_delta_inner` signature change.
4. **N4** (direct unit snapshot) — independent, clean method addition.
5. **N5** (encode buffer pre-size) — small, do last.
6. **N6** (compact wire format) — **separate branch / PR**, requires frontend + protocol coordination.

N1 + N2 can be done in one PR (both change `build_delta_inner`'s signature). N3 is a one-line deletion that can ride along.

---

## Verification

```bash
# Unit + integration tests
cargo test

# Benchmark A/B (same seed, same config)
./run-benchmark.sh 92 --bench-prev=./reports/report_91.html
# Compare "network" row:
#   before: 1.089 ms avg,  P99 4.642 ms,  7583 allocs,  1.44 MB
#   after:  target < 0.50 ms avg, P99 < 1.5 ms, < 3000 allocs, < 0.6 MB

# Frontend (only if N6 is done)
cd frontend && npm run typecheck && npm run lint && npm run test && npm run build
```

**Manual check:** 2+ player lobby, watch the debug overlay's network row — confirm per-tick ms drops and the alloc counter drops.
