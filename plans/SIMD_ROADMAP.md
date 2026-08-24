# SIMD / Structural Optimization — Future-Work Roadmap

**Status:** measurement complete; this is the forward plan. Each work item (W1–W5)
is self-contained enough to become its own execution plan when we start it.

**Basis:** two measurements taken after the `deep-plan-20260821-231145.md` changes
(R1 branchless `|=` in `union_vision_grids`, R3-Step-A branchless fold in
`influence.rs`) were landed:

- **Full game:** `./run-stresstest.sh 11` — 10 000 initial units, 1000×1000 map,
  seed 42, 1500 ticks × 3 runs. Avg wall tick **4.28 ms**; avg system time
  **3.75 ms/tick**.
- **Vectorization tripwire:** `make check-vectorization` (via `llvm-objdump
  --demangle`) — the first real run.
- **Realistic city density (added 2026-08-21):** `--bench-cities 100` (run
  `11w3cities`) — the user's city:unit-ratio hypothesis, which **reopened W6
  (city_capture as the #2 system)** and partially de-risked W4 (supply
  funds-blocked −37 %). `reports/stress_11w3m.html` (12c, post-W1-④) and
  `reports/stress_11w3cities.html` (100c) are the current comparison baselines.
- **Two follow-on fixes (added 2026-08-22):**
  - **`track_unit_losses` alloc spike (the flat “other/overhead” alloc).**
    `Game::track_unit_losses()` ran every tick *outside* any `tick_system!` block
    and did `entry(owner.player_id.clone())` — one heap `String` alloc per unit
    per tick (~9 600/tick at ~10 k units; the flat residual in `stress_14`:
    total 9 639/tick vs ~32 in the instrumented systems). Now counts per-player
    by matching each unit's borrowed owner id against the few stable player-id
    keys (str equality, no per-unit alloc). Measured: a **constant 6 allocs /
    232 B per call at 1 000 and 10 000 units**
    (`track_unit_losses_alloc_does_not_scale_with_units`). Also trims allocator
    churn from the “other/overhead” time slice.
  - **Movement `p2_query` name→index probe (the biggest p2_query sub-cost).**
    `compute_movement` did `unit_types_index.get(&utype.name)` (a SipHash
    name→index `HashMap` probe) **per unit** — micro-measured **14–16 ns of
    `compute_movement`'s 27 ns/unit (≈55 %)**; `sqrt` ≈0 ns. Now a linear
    str-scan over the few `settings.unit_types` (same value + 1.0 fallback — the
    map *was* name→position in `settings.unit_types`), dropping the now-dead
    `unit_types_index` param from `movement::run`/`compute_movement`.
    `compute_movement`: **27 → 19 ns/unit** (`movement_compute_cost_breakdown`),
    ≈ **−0.08 ms** off `p2_query` at ~9 800 units. The dense-`index: usize`
    -in-`UnitTypeId` variant (removes the per-unit `String` compare too, ≈−5 ns
    more) is the further lever if movement regains heat — it touches the
    component + `spawn_unit` + ~10 call sites, so not worth it for a ~4 % tick
    slice now. **End-to-end stress re-run pending** (see measurement log).
- **Measurement-harness per-tick allocs (added 2026-08-22, 2nd entry).**
  After the `track_unit_losses` fix, the *next*-largest source in the flat
  “other/overhead” alloc bucket was the timing/alloc harness itself: the
  `tick_system!` macro calls `record_system_timing` (**~9×/tick**, always on
  `debug-overlay`) and `record_system_alloc` (**~9×/tick**, on `bench-alloc`),
  and `World::record_detailed` does the same per sub-part of any
  `--bench-detailed` system. Each did `map.entry(key.to_string())` — a **throwaway
  `String` alloc per call** (the key is only used for the lookup, never stored),
  and these calls sit *outside* each system’s measured window, so every one of
  them lands in the unattributed “other/overhead” residual. Now the three maps
  (`system_timings`, `system_allocs`, `detailed_timings`) are written with the
  **get-first pattern**: `get_mut(key)` borrows via `String: Borrow<str>` (no
  alloc), and `entry(key.to_string()).or_default()` only runs the first time a
  name is recorded. No map-key-type / signature / JSON changes (the maps stay
  `HashMap<String, _>`); `detailed_systems` stays `HashSet<String>` (fed from owned
  CLI args). Net: the harness contributes **~0 allocs/tick** instead of ~18; the
  whole “other/overhead” residual is now just `track_unit_losses`’s constant 6.
  (The `SystemTimer`/`DetailedTimer::new` `to_string()` calls were left as-is —
  they run *inside* a system’s measured window, so they attribute to that
  system, not to “other/overhead”.)

- **One spatial grid per tick (added 2026-08-23).** `movement::run` built the
  spatial grid **twice**: a pre-move build (occupancy gate needs the frozen
  pre-move snapshot so blocking never depends on apply order) plus a post-move
  build (combat target-acquisition + city-capture read `grid_scratch.index`
  for freshness). Now it builds **once**, from pre-move positions, and the
  same grid is left in `grid_scratch.index` for combat/city-capture. Trade
  accepted: those consumers see a unit that moved this tick at its **old**
  position — combat rescans every mover every tick, so target
  acquire/release is at most one tick late (city capture the same). Saves the
  second `populate_grid_units` + `GridIndex::build`, ≈ **0.21 ms/tick**
  (stress_15: `p4_spatial` 0.212 ms gone + second populate; movement
  1.072 → ~0.86 ms expected; the `movement/p4_spatial` detailed key is
  retired). Build + clippy + 448 tests green; the first edit accidentally
  dropped the surviving `populate_grid_units` call (compiler caught it: `ctx`
  unused + `populate_grid_units` dead) — fixed before any run.
  **End-to-end confirmed (stress 16 vs 15):** movement **1.074 → 0.901 ms/tick
  (−0.173, −16 %)**; every other system flat in noise — combat 0.384 → 0.392,
  **target_acquisition 0.316 → 0.321 (the 1-tick-late trade is unmeasurable)**,
  influence 1.741 → 1.769, supply 0.497 → 0.507, city_capture 0.008 → 0.008.
  Residual −0.04 ms vs the 0.212 `p4_spatial` figure is sub-part noise
  (p2_query/p3a_gate each +0.019/+0.009).
**Priority rationale:** a system earns work if it is ≥ ~10 % of system time, or it
causes a real hitch. Below that: document and stop.

---

## 1. Measurement baseline

### 1.1 Per-system cost (full game, avg/tick)

| system | avg/call | cadence | % of system time | P99 | work item |
|---|---|---|---|---|---|
| movement | **1.314 ms** | every tick | **~35 %** | 2.154 ms | **W1** |
| influence | **1.730 ms** | every 3rd | ~15 % (amortized 0.58 ms/tick) | 2.154 ms | **W3** |
| supply | 0.532 ms | every tick | ~14 % | 1.000 ms | **W4** |
| water | 0.391 ms | every tick | ~10 % | 0.464 ms | — (low SIMD payoff) |
| combat | 0.365 ms | every tick | ~10 % | 1.000 ms | — |
| city_capture | 0.243 ms | every tick | ~6 % | 0.464 ms | W5 (closed) |
| encirclement | **6.263 ms** | ~every 30th (skipped if frontier unchanged) | ~3 % amortized | **10.000 ms** | **W2** |
| tickcontext | 0.362 ms | every 3rd | — | 0.464 ms | — |
| economy / death_cleanup / surrender | ≤ 0.012 ms | — | negligible | — | — |

Extra context from the same run: ~9 825 units alive avg, **2 630 movers/tick**
(⇒ movement ≈ **~500 ns/mover** — see W1), 96 engaged combat pairs/tick.

### 1.2 Microbenchmarks (isolated; `--features microbenchmarks`)

| id | value | note |
|---|---|---|
| `systems/movement_500` / `movement_2000` | 24.5 µs / 95.6 µs | full `movement::run` |
| `spatial/for_each_within_500` | med 100 ns/scan | isolated, sparse probe — lower bound only |
| `systems/influence_100` / `influence_500` / `influence_100_skip` | 9.3 µs / 18.1 µs / ~0 | skip path is free; Step A fold ≈ −3 % vs pre-Step-A (18.7→18.1) |
| `systems/supply_200` / `supply_500` | 10.7 µs / 26.1 µs | |
| `systems/combat_50pairs` / `combat_200pairs` | 73.0 µs / 379.0 µs | |
| `systems/city_capture_100` / `encirclement_100` | 2.1 µs / 6.8 µs | small maps, 100 units |
| `ecs/query_positions_10000` | **2.11 µs** | key reference: a full 10 k-entity query *scan* is 2 µs — per-entity `query_one_mut` is ~1000× costlier per entity |

### 1.3 Detailed sub-part splits (run `11detailed`, 2026-08-21) — the decision data

Per-phase avg/tick (from `--bench-detailed movement,encirclement,influence,supply,combat`):

| system (total) | sub-parts | reading |
|---|---|---|
| **movement** (1.278 ms) | `p3_apply` **0.619** · `p2_query` 0.437 · `p4_spatial` 0.215 · `p1_setup` 0.007 · (11w1: ①+② landed → p3_apply 0.652 ⇒ lookups were **not** the dominant cost; `p3a_gate`/`p3b_write` + Movement diagnostics line added) | Apply-phase cost; dominant sub-part TBD by the gate/write split (gate scans incl. blocked retries are the prime suspect). |
| **encirclement** (5.913 ms) | `p2_bfs` **5.311** (90 %, P99 10 ms) · `p1_seed` 0.477 · `p3_flags` 0.124 | The serial BFS flood-fill **is** the hitch. Wave/parallel flood-fill confirmed as the fix. |
| **influence** (1.646 ms) | `p2_compute` **1.589** (97 %) · `p3_fold` 0.251 (16 %) · `p1_seed` 0.056 · (11w1: `p2a_fill` **0.237** ⇒ fills are cheap, the **circle-stamping inner loop** ≈ 1.35 ms is the cost — the 12 MB/call memset hypothesis was wrong) | **R3-Step B = NO-GO** (fold is 16 %, already non-scalar, Step A already gave −3 % on `influence_500`). Re-target at the stamp loop. |
| **supply** (0.487 ms) | `p2_loop` 0.486 · `p3_deduct` ~0 · **funds-blocked ≈ 2 476/tick** (P99 10 000; counts, not ms — previously mislabeled in the ms table) · mp-blocked 0 | **Rollback/SIMD idea dead**: the funds pool is *chronically* tight (≈ a quarter of the army hits an unaffordable delivery every tick, on a small residue — `0 < avail < cost`, not `avail == 0`). Not a SIMD target. One cheap branch (measured ≈ 0), then close. |
| **combat** (0.352 ms) | `target_acquisition` **0.280** (80 %) · stats/resolution/validation ~0.05 each · retreat 0.012 | TA scans all 2 630 movers/tick via the same `for_each_within` grid pattern → benefits from W1's spatial work. New item **W7** (piggyback). |

Allocs (avg/tick) are healthy everywhere (≤ 26 allocs, city_capture largest) — not a target.

### 1.4 Vectorization verdicts (`make check-vectorization`)

| symbol | result | reading |
|---|---|---|
| `GridIndex::build` | ✓ 24 wide ops | Positive control — build targets AVX2. |
| `Game::union_vision_grids` | ✗ 0 wide ops | Never vectorized (`bool` element type). R1 was a no-op for SIMD. |
| `spatial::for_each_within` | scalar | Expected — the SoA/SIMD backlog (W1/W4). |
| `movement::run` | scalar | Expected — per-unit compute is sequential (W1). |
| `influence::run` | ✓ 6 wide ops | Non-scalar → **R3-Step-B gate not tripped**. Attribution to the fold unconfirmed. |

---

## 2. The R1 finding (union_vision_grids) — resolve first, it's a 5-min change

R1's premise ("branchless `|=` → auto-vectorize") was **disproven by measurement**:
`bool` is 1-bit in MIR, so LLVM will not vectorize the OR loop, and the tripwire
confirms 0 wide-register ops. Two consequences:

- **Not a regression** — it was never vectorized. The old check script's
  "REGRESSION" label was a false alarm; `union_vision_grids` is now in
  `REPORT_ONLY` and the repo is green.
- **It is a micro-regression** for the common (sparse) case: `result_flat[i] |=
  grid[i]` stores every cell, whereas the prior `if grid[i] { result_flat[i] = true;
  }` skipped empty cells. Vision grids are mostly empty.

**Recommendation:** ✅ **DONE (2026-08-21):** `union_vision_grids` (game.rs:1516) reverted to the
sparse-friendly branch form. Build/clippy/421 tests green. The inline comment states
the measurement and points here. The real fix (below) remains gated.

**When a real fix would be worth it:** `union_vision_grids` is only called on
`AddSpectator` (cold) and per-tick **only when `spectator_vision` is active** — it
is not in the core tick path and has no benchmark. If spectator-vision ever becomes
a measured bottleneck (large map, many players, spectators watching), the real fix
is to store vision grids as `[u8]` (0/1) so the OR auto-vectorizes, or a manual
256-bit byte-OR. That is **W5** and is gated on a spectator-vision benchmark.

---

## 3. Work items

### W1 — movement (R4) — **P1, plan written: `plans/W1_MOVEMENT.md`**

- **Measured split (11w3m/11w3cities):** `p3_apply` **0.538** (p3a_gate 0.203 +
  p3b_write 0.110) · `p2_query` **0.430** · `p4_spatial` 0.219 · `p1` 0.007.
  Movement total **1.195 ms** (11w3m, 12 cities) / 1.257 (100 cities — more
  movers: 3 610 vs 2 630/tick).
- **Diagnosis (locked by data):** ① no-op skip filtered ~0 entries (the change
  list holds only units with a non-empty path, and `compute_movement` yields a
  real change for all of them); ② (4→1 `query_one_mut`) was a wash ⇒ lookups are
  **not** the dominant cost. 11w2m showed the **occupancy gate** was: 73 % of
  gate scans were blocked units re-scanning the same jam. **④ blocked-unit
  3-tick backoff landed and measured (11w3m): p3a_gate 0.315 → 0.203 ms**
  (half the estimate — remaining scans skew blocked/expensive; see
  `plans/W1_MOVEMENT.md` log). ③ skipped by decision rule.
- **Status: substantially done.** Next candidate if movement is still hot:
  `p2_query` 0.430 (per-unit HashMap lookup in `compute_movement` / speed
  table). Further gate wins belong to **W7's** candidate-driven scan shape
  (shared spatial-scan pattern). `p4_spatial` 0.219: measure
  build-vs-populate before touching (incremental post-grid patch = ⑤).
- **Success metric:** movement 1.278 → **≤ 0.9 ms** at the stress point (①+② alone
  should reach ~0.9; ③ targets ~0.6); `make test` green; determinism intact.
- **Spillover:** ④'s SoA position buffer is shared by **W7** (combat TA) and W4's range
  work. `p2_query` (0.437) may also hold a cheap win (per-unit HashMap lookup in
  `compute_movement`) — evaluate after ①–③.

### W2 — encirclement hitch (R7) — **CLOSED (2026-08-23)** — full log: `plans/W2_ENCIRCLEMENT.md`

- **Shipped:** ① packed-coord serial BFS (queue `u32 = (cy<<16)|cx`, neighbors
  ±1/±w) — p2_bfs 5.311 → **4.822 ms** (run 13).
- **Rejected by measurement:** ② wave-parallel (flat 5 % across 1/4/32 threads —
  wasn't parallelizing), ②′ per-player parallel (run 14: **p2_bfs +18 %**,
  allocs 21 → 89/call — regions interleave, not partitionable), SIMD dilation
  probe (220× slower — O(diameter) full-plane passes), and a combined
  `(dominant<<8)|safe` u16 grid (**+9 %** local A/B — the two-`u8` layout's
  short-circuit already reads only 1 byte per interior neighbor check).
- **Conclusion:** the flood is serial memory-bound at ~3 ns/cell and every
  restructuring measured worse. `flood_serial_timing_probe` test kept as a
  local A/B tripwire. Remaining lever is the 30-tick **cadence** (gameplay
  decision, not engineering): at ~5.4 ms per call every 30 ticks it amortizes
  to 0.18 ms/tick; the P99 10 ms hitch is the price of the strategic flag.

### W3 — influence (R3) — **P2, DONE (stamp-loop vectorization landed 2026-08-23)**

- **Measured split:** `p2_compute` **1.589 ms** (97 %) · `p3_fold` 0.251 · `p1_seed`
  0.056 · `p2a_fill` 0.237. **Step-B (glam hand-SIMD of the fold) CLOSED NO-GO** —
  the fold is a side-show, already auto-vectorized (6 wide ops). The per-player
  full-grid clears are **not** the cost (12 MB zeroing costs only 0.24 ms).
- **Landed (2026-08-23):** rewrote the `stamp_circle` inner loop from an interleaved
  per-cell `vision[i]=true; influence[i]+=w` into **two contiguous row passes** —
  `vision[start..end].fill(true)` (a memset) + `for v in influence[start..end] { v.saturating_add(w) }`
  (a clean `u16` saturating-add the compiler auto-vectorizes). The interleaved
  form mixed a 1-byte store with a 2-byte RMW and defeated vectorization.
  **Probe (2 500 units @ 1000×1000, one player):** `compute_player` **0.328 → 0.142 ms
  (−57 %)**. ×4 players ⇒ `p2_compute` ≈ 0.57 ms; `influence` total ≈ **0.93 ms**
  (target ≤ 1.0 ✓). Behavior bit-identical (all influence shape tests +
  `run_skip_keeps_grids_untouched` green). `stamp_cost_probe` test kept as the A/B
  tripwire. `add_influence` is now test-only (`#[cfg(test)]`).
- **Remaining (documented, not built):** rayon *per-player* would need a separate
  `influence` buffer per player + a post-parallel fold, and at ~0.14 ms/player is
  below dispatch breakeven (see W2's ②′ for the same reasoning) — rejected. `Vision
  as [u8]` is a side-benefit only. **W3 CLOSED.**

### W4 — supply (R5) — **P4, downgraded: one cheap branch, then CLOSE**

- **Measured:** `p2_loop` 0.487 ms (≈50 ns/unit of scalar per-unit math) ·
  `obs_blocked_funds` ≈ **4 703/tick** (funds pool chronically exhausted) ·
  `obs_blocked_mp` = 0.
- **City-ratio check (11w3m vs 11w3cities, `--bench-cities` 12 vs 100):** user
  hypothesis **confirmed in direction, partial in magnitude.** Funds-blocked
  dropped 2 670 → **1 672/tick (−37 %)** — with 8× city income the pools no
  longer sit chronically exhausted — but FCFS gating is still ~17 % of units
  (vs 27 % at 12 cities). Note the 100-city run is a *harsher* test overall
  (mid-battle recruitment exploded 1 680 → 14 000 units, higher churn), so the
  real improvement is arguably larger than −37 %. Crucially, `p2_loop` itself is
  **unchanged** (0.521 → 0.528): the loop's per-unit cost dominates; the blocked
  units cost the same whether or not the pool gates them.
- **Verdict (locked by data):** the "optimistic vector + rare rollback" idea is **dead**
  — rollback would trigger on ~half the units every tick. SIMD is not the answer; this
  is ordinary per-unit compute.
- **The one cheap win (✅ landed):** skip `plan_delivery_into` when the owner's
  `remaining_funds == 0` **or the unit is encircled** (the plan feeds only the
  delivery, which can never fire in those cases — exactly equivalent). Saves the
  plan work for ~4.7 k units/tick (est. ~30–50 µs); `11w1` run measures it via
  `supply/p2_loop`. Tests green.
- **Then close.** 0.49 ms/tick of scalar per-unit math is not worth structural work at
  current map/unit densities; revisit if unit counts grow 10×.

### W7 — combat target acquisition — **P3, trigger fired (11w3m: 0.276 ms, still ≥ 0.2)**

- **Measured:** `target_acquisition` 0.280 ms/tick = **80 % of combat** (0.352 ms) and
  ~8 % of system time; scans all 2 630 movers/tick through `for_each_within` on the
  post-move grid. 11w3m: **0.276** (W1-④ did not move it — it scans candidates,
  not blocked retries) ⇒ the "write a focused plan" condition is met.
- **Plan (next session):** candidate-driven scan — the **146 candidates** scan their
  3×3 neighborhood cells for enemies instead of 2 630 movers each asking the grid
  (11w3m diagnostics: candidates 146, skipped-mutual 50). Check the current
  direction in code first; expect ~0.2 ms back.

### W5 — union_vision_grids real fix — **P5, gated** (unchanged)

### W6 — city_capture (R6) — **P1, REOPENED by the 100-city run (11w3cities)**

- **Why it was closed:** at the default 12-city stress point it cost 0.243 ms
  (6 %) — under the 10 % bar. **That measurement is config-dependent, not
  code-dependent.** With `--bench-cities 100` (realistic city:unit ratio, per
  user insight): **0.804 ms (18.6 % of system time, #2 system after movement)**
  and it became the **top allocator: 202 allocs/tick, 7.9 KB/tick** (was 26 /
  962 B at 12 cities — the clone of the city Vec scales 8.3×).
- **Root cause (read in code, `systems/city_capture.rs`):** the inner loop is
  `for unit { for city { dist² check } }` — **O(units × cities)**: 120 k checks
  at 12 cities, 1 M at 100. Plus `wb.cities.cities.clone()` every tick.
- **Fix (✅ implemented 2026-08-21, awaiting measurement):** city-outer spatial
  scan. `run` now takes the shared post-move `&GridIndexScratch` (built by
  `movement`, already reused by `combat` — both pass it as a shared borrow) and,
  per city, calls `spatial::for_each_within(&grid, scratch, city_pos,
  CAPTURE_RANGE_SQ, …)` so each city only touches units within 1.5 tiles. Cost
  O(cities × units_in_range) instead of O(units × cities) (1 M checks → a few
  hundred at 100 cities). `SPATIAL_CELL_SIZE` (10) ≫ capture radius (1.5) so the
  3×3 cell scan is complete.
- **Allocations:** the `wb.cities.cities.clone()` (the 202 allocs/tick at 100
  cities) is gone — cities are borrowed immutably (`world_data`) alongside the
  grid read (both shared borrows), and the mutation phase takes a separate
  `world_data_mut` borrow after. The winner's owner is recorded as a compact u8
  during the scan and resolved to a `player_id` String **only for cities that
  actually change hands** (rare) — so the steady-state per-tick allocation drops
  from ~200 to ~0 (plus a short `city_id` clone per qualifying insert on an
  occupied city, bounded by units-in-range).
- **Determinism:** same strict-`<` distance test, same last-qualifier-wins
  BTreeMap reduce. The per-city tie-break is now "last in spatial entry order"
  (was "last in ECS query order") — both deterministic for a given world state,
  diverging only when two *different* players' units contest one city in the
  same tick (documented; not gameplay-meaningful). **6 new tests** pin the
  behavior: neutral capture, no self-capture, enemy capture, out-of-range,
  multi-same-owner (counts once), and contested-city single deterministic winner.
- **Success metric (verify in next run):** city_capture ≤ 0.1 ms and ≤ ~10
  allocs/tick at **both** 12 and 100 cities; all 444 tests green (done); expect
  ~0.6 ms/tick back at realistic city counts (0.3 at the default config).

---

## 4. Instrumentation added (2026-08-21) — one stress run answers W1/W2/W3/W4

Per-phase timers via the existing `record_detailed` machinery (no-op without the
debug-overlay/benchmark feature; zero cost in normal play), plus cheap counters:

| system | keys | what it splits |
|---|---|---|
| movement | `movement/p1_setup`, `p2_query`, `p3_apply`, `p4_spatial` | (existing) moved-flag clear · per-unit `compute_movement` query+compute · apply loop incl. `for_each_within` occupancy gate · post-move grid build |
| encirclement | `encirclement/p1_seed`, `p2_bfs`, `p3_flags` | safe-fill + edge/city seeding + queue seed · the BFS flood-fill (the P99 suspect) · per-unit flag loop |
| influence | `influence/p1_seed`, `p2_compute`, `p3_fold` | setup + buffer resets · per-player circle stamping · territory running-max fold (accumulated; the Step-B target) |
| supply | `supply/p2_loop`, `p3_deduct` + `supply/obs_blocked_funds`, `supply/obs_blocked_mp` | per-unit pass · pool deduction; counters = how often the FCFS pool gate blocked a draw (W4 rollback feasibility: 0 ⇒ pools never constrain) |

**Collect everything with one run:**

```
./run-stresstest.sh 11detailed --bench-detailed movement,encirclement,influence,supply
```

(The script forwards the extra flag; the HTML report's detailed table shows
per-phase avg/P99. Keep `reports/stress_11detailed.html` as the W1–W4 baseline.)

Added 2026-08-21 (W1-④ + city-ratio work): `--bench-cities N` flag (CLI default 12 =
authored layout; other counts generate an even sqrt lattice). **The stress-test
script (`run-stresstest.sh`) defaults to `--bench-cities 100`** (10×10 lattice) since
2026-08-21 — a 10k-army stress run should use a realistic city:unit ratio so
per-player city income can keep up with the army's supply drain; pass
`--bench-cities 12` to a stress run to get the old authored-layout config.
`--bench-recruiting N` flag (default **2000**) caps units recruited
per recruit event (every 100 ticks) — each city recruits `bench_units/2` (min 1)
per event, capped at this total and spread evenly across cities; raise for heavier
recruitment/ECS-churn pressure, lower to hold the population roughly stable. The
cap is recorded in the JSON `config.max_recruits_per_event` and shown in the HTML
config table ("Max recruits/event"). Battle Activity HTML cards now show Gate
Scans/Tick, Gate-Blocked/Tick, Cooldown-Skipped/Tick, Supply-Blocked (Funds/MP)
with prev-diffs; detailed sub-part table shows prev-diffs; console prints the
counters next to the Movers/Combats lines. The five per-tick counters also flow
through the JSON `results` block for the HTML diff mechanism.

Added 2026-08-21 (W6 city_capture): `city_capture::run` now takes the shared
post-move `&GridIndexScratch` (city-outer spatial scan) — the system is no
longer O(units × cities) and no longer clones the city Vec each tick. The
microbenchmark fixture `CityCaptureFixture` builds the same grid via
`populate_grid_units` + `GridIndex::build`.

All green after this change: release build, `clippy -D warnings` (with
`debug-overlay,microbenchmarks,bench-alloc`), 444 tests (was 431: +6 city_capture,
+1 recruit_event_plan).

Added 2026-08-22 (three follow-on fixes): 
- **`track_unit_losses` alloc** (game.rs): the per-unit
  `entry(owner.player_id.clone())` String-clone (the flat ~9 600/tick
  “other/overhead” alloc in `stress_14`) is now a per-player count over the few
  stable player-id keys — no per-unit alloc. Regression guard
  `track_unit_losses_alloc_does_not_scale_with_units` measures a **constant
  6 allocs / 232 B per call at 1 000 and 10 000 units**.
- **movement `p2_query`** (movement.rs): the per-unit SipHash
  `unit_types_index.get(&utype.name)` probe (micro-measured **14–16 ns of the
  27 ns/unit** `compute_movement`) is now a linear str-scan over `settings.unit_types`;
  the dead `unit_types_index` param is dropped from `movement::run` /
  `compute_movement` (+ callers: game.rs, microbench). `compute_movement`: **27 →
  19 ns/unit** (`movement_compute_cost_breakdown`); ≈ −0.08 ms off `p2_query` at
  ~9 800 units. End-to-end re-stress pending.
- **measurement harness alloc** (debug_stats.rs, world_ext.rs): `record_system_timing`
  / `record_system_alloc` / `World::record_detailed` did `entry(key.to_string())`
  — a throwaway `String` alloc per call, all outside each system’s measured window
  ⇒ flat “other/overhead” (~9 timing + ~9 alloc per tick). Now get-first
  (`get_mut` via `Borrow<str>`; `entry` only on first record): **~0 allocs/tick**.
  No map-key-type / signature / JSON changes; `detailed_systems` stays
  `HashSet<String>` (owned CLI args).

All green after this change: release build, `clippy -D warnings` (with
`debug-overlay,microbenchmarks,bench-alloc`), 448 tests (was 444: +2 new
measurement tests).

## 5. Recommended sequence (updated after 11w3m + 11w3cities)

1. ~~§2 revert `union_vision_grids`~~ ✅ done.
2. **W1 (movement)** — ✅ **substantially done.** ④ blocked-unit 3-tick backoff
   landed + measured (11w3m): p3a_gate 0.315 → 0.203 ms, movement total 1.278 →
   1.195 ms, wall 4.140 ms. 2026-08-22: the `p2_query` 0.430 ms probe (the
   per-unit SipHash name→index lookup) is now a linear str-scan — see the basis
   note; end-to-end re-stress pending. Remaining sub-parts < 0.25 ms; next
   candidate (dense `UnitTypeId.index`) only if movement regains heat.
3. **W6 (city_capture)** — ✅ **implemented** (see item above): city-outer spatial
   scan + drop the city Vec clone. Expect 0.804 → ≤ 0.1 ms and 202 → ~0 allocs/tick
   at 100 cities (0.243 ms / 26 allocs at 12 cities). Verify in the next run.
4. **W2 (encirclement hitch)** — `plans/W2_ENCIRCLEMENT.md`. **① (packed-coord
   division fix) shipped — that is the final W2 state.** ②-wave (rayon frontier-
   waves) measured 5 % and was **rejected** (didn't parallelize — flat at 4 and 32
   threads). **②′ per-player parallel was implemented, wired into `run`, measured
   (run `14`), and REJECTED** — bit-identical to serial but **10 % slower** in the
   microbench (3.366 vs 3.048 ms) and **+18 %** on real-game `p2_bfs` (4.822 →
   5.710 ms), P99 unchanged at 10 ms, allocs 21 → 89/call. Reason: player
   dominance regions are disjoint as *writes* but **intermixed as work** (they
   interleave and together cover the whole map), so there's no clean parallel
   partition and rayon's dispatch overhead dominates. **Reverted to serial;
   ③ amortization (spread the flood over N ticks) was **also closed as designed**
   (2026-08-23): ②′ proved per-player flood work ≈ full-map work, so rotating
   one player per tick moves the ~5 ms hitch (every ~4 ticks) rather than
   shrinking it. A final micro-opt probe — combining `safe`+`dominant` into one
   `(dominant<<8)|safe` u16 grid (single 2-byte load per neighbor) — measured
   **+9 % slower** locally (3.32 vs 3.03 ms, `flood_serial_timing_probe`):
   the shipped two-`u8` test short-circuits and reads only 1 byte per interior
   neighbor. **W2 CLOSED** — the only remaining lever is the 30-tick cadence
   (gameplay decision), and the probe test stays as a local A/B tripwire.
5. **W3 (influence)** — circle-stamping inner loop is the target (~1.35 ms of the
   1.64 `p2_compute`; fills are 0.26). Unchanged by the new runs (1.671/1.711
   per call at 12/100 cities).
6. **W7 (combat TA)** — **trigger fired** (0.276 ms in 11w3m, ≥ 0.2 threshold);
   write the candidate-driven-scan focused plan.
7. **W4 (supply)** — stays closed/P4. City-ratio check (11w3cities) confirmed the
   hypothesis in direction: funds-blocked 2 670 → 1 672 (−37 %, and the 100-city
   run has 8× the mid-battle recruitment, so the true effect is better), but
   `p2_loop` is unchanged (0.528) — the per-unit cost, not the pool gate, is what
   costs. No new work.
8. **W5** — only if spectator-vision ever measures hot.

**Next single collection run (covers W6 + re-confirms W1/W3/W7):**
```
# 100 cities is now the stress-test default (see run-stresstest.sh), so the
# primary run is the realistic-config one and compares to 11w3cities:
./run-stresstest.sh 11w6 --bench-detailed movement,encirclement,influence,supply,combat \
  --bench-prev=./reports/stress_11w3cities.html
# The 12-city (authored-layout) config is now an explicit opt-out, for completeness:
./run-stresstest.sh 11w6c12 --bench-cities 12 --bench-detailed movement,encirclement,influence,supply,combat \
  --bench-prev=./reports/stress_11w3m.html
```

Rationale: W6 is the biggest *certain* win left at realistic city counts (~0.6 ms/tick, ~14 % of the tick)
and it's an access-pattern fix, not speculative SIMD. W2 is closed (see item 4).
W3/W4 are now bounded and cheap. (Note: the stress-test default moved from 12 to 100
cities on 2026-08-21, so bare `run-stresstest.sh N` now matches `11w3cities`-style
configs; pass `--bench-cities 12` to get the old authored-layout run.)

---

## 6. Measurement harness (all working now)

- **`make check-vectorization`** — vectorization tripwire (passing; sole
  `EXPECT_VECTOR` canary = `GridIndex::build`). Re-run after any change to the hot
  loops to catch a silent auto-vectorization regression or an SSE2 fallback.
- **`./run-stresstest.sh N`** — full game; per-system avg/P99 + allocs. Defaults to
  100 cities (pass `--bench-cities 12` for the authored layout). Use this for
  "is it a meaningful tick slice" and for the hitch (P99). Save the HTML report as a
  baseline per work item.
- **`cargo run --release --features microbenchmarks -- --benchmark`** — per-loop
  numbers (existing ids + `spatial/for_each_within_500`). Use for before/after on a
  specific loop.
- **`rust-objdump -d --demangle target/release/war-of-dots`** — manual per-loop asm
  inspection, for the "did *this* loop vectorize" attribution question (W1/W3).

**Baseline rule:** before starting a work item, record its stress + relevant
microbench numbers; after, record again. Keep the before/after in the item's plan.
