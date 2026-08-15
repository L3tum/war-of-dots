# Implementation Report — Optimize `supply.rs`

Spec: `plans/deep-plan-20260814-090000.md`. Executed **Phase 1 (S1–S6)** and
**Phase 2 (S7)**; **Phase 3 (S8) deferred** (optional).

## What changed

### `src/game/systems/supply.rs`
- **`SupplyScratch`** added (mirrors `CombatScratch`): `mp_cost`, `fund_cost`
  (now dense `Vec<u32>` indexed by player index), `dead`, and a reusable
  `delivery` plan buffer. `new()`/`Default`/`clear()` (capacity-preserving).
- **`run` signature** → `(world, world_entity, settings, scratch: &mut SupplyScratch,
  ctx: &TickContext) -> Vec<Entity>`.
- **Single mutable query, in place.** The old two-phase design (immutable read →
  `Vec<Change>` staging → up to 3 `query_one_mut` writes per changed unit) is gone.
  One query now returns
  `(Entity, &UnitTypeId, &UnitOwner, &UnitFlags, &mut UnitStats, &mut UnitEquipment,
  &mut UnitWaterTransition)` and every write is applied immediately. Removed:
  `type Change`, the `Vec<Change>` buffer, the per-entity `query_one_mut` loop,
  `eq_copy = (*equipment).clone()`, and `(*wt).clone()`.
- **`plan_delivery_into(equipment, &mut scratch.delivery) -> u32`** replaces
  `plan_delivery` — reuses the scratch buffer instead of a per-tick per-unit `Vec` alloc.
- **Behavior preserved:** `ratio = equipment_ratio(equipment)` is computed from the
  **pre-wear** equipment (before the wear/delivery mutation), exactly as the original
  read the untouched immutable reference. The `f32::EPSILON` guards on men/org are kept,
  and the dead-check reads the *stored* `stats.men`.
- **Dense `u8` player indexing (Phase 2):** pools snapshot and cost accumulators are
  `Vec<u32>` indexed by `ctx.owner_id_by_entity[entity]`; per-unit `String` hashing and
  `pid.clone()` are eliminated. A small `id_by_index: Vec<String>` reverse map
  (built per tick from `ctx.player_id_to_index`, ≤ a handful of players) maps index →
  id for the final one-write-per-player deduction.

### `src/game/game.rs`
- `supply_scratch` field (near `combat_scratch`) + init; call site at the supply
  `tick_system!` now passes `&mut self.supply_scratch, &self.ctx`.

### `src/microbenchmarks.rs`
- `systems/supply_200` / `supply_500` fixtures build a local `SupplyScratch` and a
  `TickContext::build_full(...)`, passing both to `run`.

### `Cargo.toml`
- No change (no new dependency).

## Verification
- `cargo check --all-targets --features microbenchmarks` — clean, **0 warnings from supply.rs**.
- `cargo test supply` — all 11 `supply::tests` pass with **unchanged assertion values**
  (funds `100_000 - 3` / `100_000 - 1`, org malus thresholds, encircled/short-funds skips).
- `cargo test` (full) — green (unit + 21 integration, 0 failed).
- `cargo clippy --all-targets --features microbenchmarks` — clean.
- `cargo build --release` — OK.

## Notes / not done
- **`cargo fmt` / `rustfmt`** are blocked by the sandbox in this session; formatting was
  verified manually (≤100 cols, consistent 4-space style, matches surrounding code).
- **Benchmark delta** (`systems/supply_200`/`_500`) not captured: the harness is driven
  through the debug server (`debug.rs` → `run_microbenchmarks`), not a standalone CLI.
  Follow-up: `cargo run --release --features microbenchmarks` and compare before/after —
  Phase 1 removes the per-tick equipment/wt clones, the `Vec<Change>` + per-entity
  `query_one_mut` (≤3 per unit), and the per-unit delivery `Vec`; Phase 2 removes per-unit
  `String` hashing/`clone`.
- **Phase 3 (S8) deferred** — optional: `smallvec` for `delivery`/`dead`, and caching
  `unit_types_by_name` per `Game` instead of per tick. Marginal gain; would add a
  dependency and further signature/plumbing churn. Left out of scope to keep the change
  focused.

---

## Post-benchmark regression — FOUND & FIXED

After the first benchmark run, the report showed **`Units remaining: 2`** (a near-total
wipeout) and faster ticks. Investigation found a **real behavioral bug** introduced by the
dense cost accumulators (Phase 2), not a benign perf artifact.

### Root cause
`run()` sized the dense cost vectors with `scratch.mp_cost.resize(remaining_mp.len(), 0)` /
`scratch.fund_cost.resize(remaining_funds.len(), 0)`. **`Vec::resize(len, 0)` is a no-op when
the length is already `len`.** `Game` reuses one persistent `self.supply_scratch` every tick
with an unchanged player count, so after tick 1 the vectors were **never zeroed again** —
`mp_cost[oi] += gain` kept adding to the previous tick's total. The per-unit FCFS *gating*
stayed correct (fresh snapshot), but the end-of-tick **deduction compounded**: tick 1 deducts
C, tick 2 2C, tick 3 3C… Global pools (funds ~2000, manpower) exhausted within a few dozen
ticks → no supply → units drained to death. Tests/microbench never caught it (fresh scratch
each call = tick 1, where resize-from-empty is correct).

### Fix
Zero the vectors in `SupplyScratch::clear()` (matches `CombatScratch`'s reset-all pattern);
`resize` in `run()` then only handles growth:
```rust
pub fn clear(&mut self) {
    self.dead.clear();
    self.delivery.clear();
    self.mp_cost.fill(0);
    self.fund_cost.fill(0);
}
```

### Regression test
`supply_cost_accumulators_reset_between_ticks` runs `run()` **twice on the same scratch**
(the real-game pattern) with a unit drawing exactly 1 manpower/tick, asserting the pool drops
by 2 (not 3). Verified it **fails with `left: 3, right: 2`** when the `fill(0)` fix is removed,
and passes with it.

### Microbenchmark measurement artifact (fixed)
`systems/supply_200/500` initially showed only a ~26% gain because `TickContext::build_full`
was called **inside the timed closure**, masking the `run()` improvement. Fixed by building the
ctx once in `SupplyFixture::new()` (un-timed setup) and storing it on the fixture, so the
benchmark now measures `supply::run` alone. Expect a larger, honest delta on the next run.

### Verification
- `cargo test` — **176 passed, 0 failed** (155 unit incl. new regression test + 21 integration).
- `cargo clippy --all-targets --features microbenchmarks` — clean (no supply warnings).
- `cargo build --release` — OK.

**Action for the user:** re-run `./run-benchmark.sh 60 --bench-prev=./reports/report_58.html`.
Expected: units no longer wipe out (units-remaining back to baseline), and `systems/supply_200/500`
now show the true (larger) gain.

**Confirmed by user benchmark re-run:** `Units remaining: 194` (was 2 — wipeout gone),
supply system 0.013ms avg, and `systems/supply_200`/`_500` now report the honest gain
(10.15µs / 23.98µs, unmasked).

---

## S8 (Phase 3) — smallvec + cached unit-type index (DONE)

The final, previously-optional step, done to squeeze the remaining performance out.

### 1. `SmallVec` for the hot scratch buffers
- `SupplyScratch.dead` → `SmallVec<[hecs::Entity; 8]>`
- `SupplyScratch.delivery` → `SmallVec<[(usize, u32); 4]>`

Default unit types carry 0–2 equipment slots and most ticks see only a handful of
dead units, so both stay **inline on the stack** and do no heap allocation in the common
case (they spill to heap only past the inline capacity). Added `smallvec = "1"` to
`Cargo.toml` (already present transitively at 1.15.2, so no new lock churn). `run()` now
returns `SmallVec<[hecs::Entity; 8]>`; the caller's `.chain(supply_dead)` uses
`IntoIterator`, so **no change** was needed at the `all_dead` construction site.

### 2. Cached `unit_types_index` (name → index into `settings.unit_types`)
- Built **once** in `Game::new` into a new `Game.unit_types_index: HashMap<String, usize>`
  field (built before `settings` is moved into `Self`).
- `run()` takes it as a `&HashMap<String, usize>` parameter and indexes straight into
  `settings.unit_types[i]` per unit — replacing the per-tick
  `HashMap<&str, &UnitType>` build (the `UnitType` import is now unused and removed).
- The 11 unit tests and the two microbenchmark closures build the index once and pass it
  through (microbenchmark builds it in un-timed setup, mirroring `Game`).

### Why these are safe
- `Game` is constructed only in `Game::new`, so adding the struct field breaks nothing.
- `SmallVec` implements the same slice/iter API as `Vec` for all uses here
  (`push`, `iter`, `iter_mut`, `get_mut`, `clear`, `is_empty`, `len`, `fill`-free), so the
  per-unit loops are unchanged.
- Behavioral invariants untouched: pre-wear `ratio`, `f32::EPSILON` guards, FCFS gating,
  dense per-player accumulators + the `fill(0)` reset fix all still apply identically.

### Verification
- `cargo check --all-targets --features microbenchmarks` — clean.
- `cargo test` — **176 passed, 0 failed** (155 unit incl. regression test + 21 integration).
- `cargo clippy --all-targets --features microbenchmarks` — clean, no warnings in
  supply.rs / microbenchmarks.rs / game.rs (only the pre-existing
  `set_detailed_systems` dead-code stub at game.rs:1219).
- `cargo build --release` — OK (same 20 pre-existing warnings, none in touched files).

**Action for the user:** re-run `./run-benchmark.sh 60 --bench-prev=./reports/report_60.html`
to capture the final delta. Expect: identical game behavior (units remaining ~194), and a
small further drop in `systems/supply_200/500` + `supply` avg from removing the per-tick
HashMap build and the two small heap allocations. **Plan fully complete (S1–S8).**
