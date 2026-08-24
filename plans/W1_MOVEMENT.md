# W1 — Movement: kill the per-entity `query_one_mut` overhead

**Status:** plan ready, not started.
**Baseline (run `11detailed`, 2026-08-21):** movement **1.278 ms/tick** = `p1_setup`
0.007 + `p2_query` 0.437 + `p3_apply` **0.619** + `p4_spatial` 0.215.
~9 825 units alive, **2 630 movers/tick**, 1000×1000 map, seed 42.
**Success metric:** movement ≤ **0.9 ms** after ①+②; ≤ 0.6 ms if ③ lands. `make test`
green, determinism intact.

## Diagnosis (locked by data)

`p3_apply`'s apply loop does, **per unit with a non-empty path** (~9.8 k, of which
only 2 630 moved):

- up to 4 × `world.query_one_mut(...)` — `UnitPosition` (always), `UnitPath`
  (on waypoint reach), `UnitStats` (when org drained), `UnitFlags` (movers)
- a `for_each_within` occupancy scan (movers only)

`query_one_mut` is a per-entity archetype hash lookup; a *full*
`query::<(&UnitPosition,)>` scan over 10 k entities costs **2.11 µs**
(`ecs/query_positions_10000` microbench). The ~10.5 k lookups/tick therefore cost
hundreds of µs against a 2 µs scan. Additionally ~7.2 k/tick of those are **no-ops**:
`moved == false` ⇒ `new_pos == old_pos` exactly, so the position write-back stores the
identical value.

This is an ECS access-pattern problem, not a SIMD problem.

## Steps (each independently shippable; re-stress after each)

Collect after each step:
```
./run-stresstest.sh 11w1s<N> --bench-detailed movement,encirclement,influence,supply,combat \
  --bench-prev=./reports/stress_11detailed.html
```

### ① Skip no-op changes — trivial, zero risk

In the apply loop (`src/game/systems/movement.rs`), before any write:

```rust
if !change.moved && change.new_path.is_none() && !change.pop_head && change.new_org.is_none()
{ continue; }
```

Invariant already documented in the code: `moved == false` ⇒ `new_pos == old_pos`
("Reaching a waypoint the unit is already at produces new_pos == pos and must NOT be
flaged as movement"). So skipping writes *and* lookups for no-ops changes nothing
observable.

**Expected:** removes ~7.2 k lookups + ~7.2 k position writes/tick.
`p3_apply` 0.619 → ~0.35–0.45 ms.

### ② Merge 4 `query_one_mut` → 1 multi-mut (remaining ~2.6 k movers)

```rust
if let Ok((mpos, mpath, mstats, mflags)) =
    world.query_one_mut::<(&mut UnitPosition, &mut UnitPath, &mut UnitStats, &mut UnitFlags)>(entity)
{
    mpos.x = ...; mpos.y = ...;
    if let Some(np) = &change.new_path { *mpath = np.clone(); }
    else if change.pop_head { debug_assert!(...); if !mpath.path.is_empty() { mpath.path.remove(0); } }
    if let Some(o) = change.new_org { mstats.organisation = o; }
    if change.moved { mflags.moved = true; }
}
```

One archetype lookup + pointer chase instead of up to four. Same component set the
unit always has (spawned together; the query in `compute_movement` proves presence).

**Expected:** `p3_apply` → ~0.25–0.30 ms (the remainder should be mostly the
`for_each_within` gate: 2 630 scans × ~60–100 ns ≈ 0.16–0.26 ms).
Movement total → **~0.9 ms** ✅ success metric met.

**Safety note:** hecs `query_one_mut` with multiple `&mut` of *different* component
types on one entity is permitted (same rule the two-phase comment in the file cites).
All existing `run_*` tests (`run_move_into_empty_space_succeeds`,
`run_move_blocked_within_min_distance`, `run_blocked_unit_moves_after_neighbour_leaves`)
must stay green — they exercise pop/org/flag paths.

### ③ (only if `p3_apply` still > 0.3 ms) Single-pass apply

Eliminate `query_one_mut` entirely:

- after phase 1, build `HashMap<Entity, usize>` → index into `scratch.changes`
  (~2.6 k inserts ≈ 30 µs);
- run **one** `world.query::<(&mut UnitPosition, &mut UnitPath, &mut UnitStats,
  &mut UnitFlags)>()` scan (~2 µs) and for each entity, `hash.get(entity)` → apply.

Trade ~100 µs of map work for ~0 lookups. Only worth it if ② left the loop hot
(e.g., if ② shows the gate scan is *not* the remainder).

### ④ (only if gate scan confirmed hot) Occupancy `for_each_within` SoA

If after ①–③ the mover gate is ≥ ~0.15 ms: build a SoA
(`Vec<f32> xs, ys` parallel to `grid_scratch.units`) once per tick in
`populate_grid_units` (cheap, shared with W7) and vectorize the 3×3-cell
`dist_sq < MIN_UNIT_DISTANCE_SQ` filter (f32 ops auto-vectorize; the
callback-free form is what blocks `for_each_within` today). Keep the scalar
`for_each_within` for combat (candidate-driven, different shape) or adapt it.
Randomized equivalence test: SoA gate vs current gate on a seeded cluster of units.

### ⑤ (separate decision) `p4_spatial` 0.215 ms

Sub-split first (2 `record_detailed` keys: `movement/p4a_populate`,
`p4b_build`). `GridIndex::build` is already vectorized (24 wide ops) and processes
1 M cells; the question is whether the *second* grid build per tick is avoidable:

- **Incremental patch:** post-grid = pre-grid with mover cells removed/re-inserted
  (2 630 units × 2 cells). Feasible if cell buckets tolerate re-sorted insertion
  (they're sorted by unit id? check `build`'s invariant);
- **Or** move the post-grid build into combat (only combat consumes it — verify) and
  skip it when movers == 0 this tick.

Do not start ⑤ in the same change as ①–③; re-measure first.

## `p2_query` (0.437 ms) — evaluate after ①–③

Contains: 9.8 k-iter 7-component query (~10–20 µs) + per-unit `compute_movement`
(terrain load, `unit_types_index` HashMap lookup, sqrt, SmallVec `.first()`).
~45 ns/unit. Candidates: replace the per-unit **HashMap** name lookup with a
dense `UnitTypeId { index: usize }` (component change, touches spawn sites —
moderate), or precompute speed per unit-type into a `Vec<f32>` indexed by
`unit_types_index` (removes the `&settings.unit_types[i]` chase). Cheapest first:
the `Vec<f32>` speed table (~10 lines). Judge from the new split — do not do it
speculatively.

## Verification per step

1. `cargo build --release` clean.
2. `cargo clippy --profile release --features debug-overlay,microbenchmarks,bench-alloc --all-targets` — 0 warnings.
3. `cargo test` — 421 green (movement `run_*` occupancy tests are the guard).
4. `make check-vectorization` — PASS (canary unchanged).
5. Stress run → compare `movement/*` rows vs baseline table below; record in this file.

## Before/after log

| step | movement total | p2_query | p3_apply | p4_spatial |
|---|---|---|---|---|
| baseline (11detailed) | 1.278 | 0.437 | 0.619 | 0.215 |
| ①+② (run 11w1) | 1.298 | 0.423 | 0.652 | 0.215 |
| ④ backoff (run 11w3m) | **1.195** | 0.430 | 0.538 (p3a **0.203** + p3b 0.110) | 0.219 |
| ③ | skipped by decision rule | | | |

**11w1 result — diagnosis correction:** ①+② were a **wash** (0.619 → 0.652, noise). Why:
- The phase-1 filter (`path.path.is_empty()`) means `scratch.changes` contains **only
  units with a non-empty path**, not all 9 825 alive units as the original estimate
  assumed. `compute_movement` returns an actual change for *every* such unit
  (a true no-op — `moved=false`, no pop, no org — is effectively unreachable,
  because `moved=false` ⇒ dist≈0 ⇒ `pop_head=true`). So **① filters ~0 entries**
  (11w2m: `noop-skipped` = 0.0 — confirmed); the "~7.2 k no-ops" estimate was wrong.
- ② (4→1 `query_one_mut`) is real but apparently worth < ~0.05 ms ⇒ per-entry
  lookups are **not** the dominant `p3_apply` cost.

**11w2m result — gate confirmed dominant:** `p3a_gate` **0.315** vs `p3b_write`
**0.111**; Movement diagnostics: `changes 9748 · gate scans 9747 · gate-blocked
7117 · movers 2630` /tick. **73 % of gate scans are blocked units re-scanning the
same jam** (city clusters are impassable at unit scale; the blocked unit retries
every tick and is blocked every tick). Decision rule fired: skip ③, go to ④.

**④ landed (2026-08-21): blocked-unit backoff.** A gate-blocked mover is put on a
3-tick cooldown (`GATE_BACKOFF`), during which the gate scan *and* the apply are
skipped; the countdown ticks down once per tick the unit is seen, and a passing
scan clears it. Backoff slots are a `Vec<u64>` indexed by entity id (generation in
the high 56 bits, so a reused id is automatically clear), capped at 2²⁰ ids so a
pathological id (`Entity::DANGLING`) cannot resize the vec to gigabytes. Expected
win: ~73 % of the 0.315 ms gate work ≈ **0.23 ms/tick**. **Measured (11w3m):
≈ half the estimate, but real** — see below. **Behavior change
(documented):** a unit jammed behind an impassable front now retries every 4th
tick instead of every tick — at most 3 ticks of extra delay in the one case that
previously made zero progress. New counter: `cooldown-skipped` in the Movement
diagnostics line + JSON.

**11w3m result — ④ measured: win ≈ half the estimate, but real.** Gate scans
9 747 → **4 411** (−55 %), gate-blocked 7 117 → 1 780, new `cooldown-skipped` =
5 334/tick — scans + skipped = 9 745 ≈ the old scan count, so the backoff is
doing exactly what it was designed to. `p3a_gate` 0.315 → **0.203 ms** (−0.112,
not the estimated −0.23) because the *remaining* scans skew expensive: 40 % are
now blocked (vs 73 % before) and a blocked scan walks the full cell before
failing — per-scan cost rose 32 → 46 ns. Movement total 1.278 → **1.195 ms**;
wall tick 4.140 ms. `p3b_write` unchanged (0.110 — correct, blocked units never
reached it), `p2_query` unchanged (0.430).

**③ skipped** (decision rule: `p3a_gate ≫ p3b_write` + large `gate-blocked`).

**W1 status after ④: substantially done.** Remaining movement sub-parts:
`p2_query` **0.430** (per-unit `compute_movement` + query — the speed-table /
HashMap-lookup item is the next candidate), `p3a_gate` 0.203 (further wins need
W7's candidate-driven spatial-scan shape), `p4_spatial` 0.219 (pre/post double
grid build — measure build-vs-populate before touching), `p3b_write` 0.110
(already one mut-query per unit). The ≤ 0.9 ms success metric is not met, but
the remaining ~0.3 ms sits in three sub-0.25 ms parts; the bigger prize moved to
**W6 (city_capture)** after the 100-city run.
