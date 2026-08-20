# Deep Plan: Unit occupancy — unit radius (circle model)

*Follow-on to plans/deep-plan-20260819-000146.md (the reverse-lookup death_cleanup plan — shipped,
**kept as-is**). **Supersedes the cell-based version** (`deep-plan-20260820-003246.md`) — same
payoff (bounded `NearbyEnemies` lists → `nearby_enemies` + `death_cleanup` collapse) but the
abstraction is a **unit radius** with circle tests, not a cell grid / occupancy map.*

*User-locked decisions (this round):*
1. **One global `UNIT_RADIUS` const** (no per-unit-type field → no protocol/delta change; client
   draws the same size from the same constant).
2. **`UNIT_RADIUS = 5.0`** (a 10×10 dot).
3. **Combat = circle intersection, `dist < 10`** (the mover is the attacker). `10 = 2×UNIT_RADIUS`
   (the two dots tangent).
4. **Min unit distance = 8** (centers; 10px dots may overlap by 2px at closest approach).
5. **Spatial index is reused, not rebuilt per consumer** — built/updated by movement and consumed
   downstream; no `::clear()`.
6. Benchmark units **non-overlapping from tick 0** (lattice layout).
7. Benchmark map stays **1000×1000** (already the stress default); add a **fail-fast capacity
   check** that errors out if the requested unit count can't lay out non-overlapping.
8. **death_cleanup unchanged** (purging is load-bearing for the upcoming conditional rebuild);
   **large-list microbench coverage kept.**

## Context — what the measurement actually showed

The stress_6 run kept `death_cleanup` at **21 010 ms / 12.359 ms avg** (vs 10 905 ms pre-change)
even after the reverse-lookup rewrite. Rather than keep guessing, a faithful phase-level
micro-harness (`cargo test --release --features microbenchmarks dc_phase_measurement`) — 10 000-unit
world, a 3 000-unit dense cluster with **~1 000-entry enemy lists** (matching the benchmark's real
`total_nearby_entries / with-enemies ≈ 588 146 / 577 ≈ 1 019`) — times each phase of `death_cleanup`
against the **original flat retain**:

```
dead     flat_retain(old)   reverse_lookup   query+retain   new(reverse+retain)
100            22 µs              4.6 µs          26.1 µs           31 µs  (1.4x)
1000           44 µs              9.0 µs          51.6 µs           61 µs  (1.4x)
2000           84 µs             18.2 µs         102.5 µs          120 µs  (1.4x)
```

**Finding 1 — the reverse lookup is ~1.4× *slower* than the flat retain in dense spikes.** Dead
units in a dense symmetric cluster each list ~1 000 enemies, so the "targeted" reverse pass reads
~`dead × 1000` entries. It only wins in sparse terrain — the cheap ticks anyway.

**Finding 2 — the real cost driver is list *density*, not the cleanup algorithm.** The benchmark
converges units to one point (moves 30 % of units to the map centre every 20 ticks) and
`recruit_unit` spawns every unit at the *same* `city_pos`, building a cluster where 577 units hold
~1 019-entry lists. **Both** top systems are O(total entries):
- `nearby_enemies` full rebuild: writes all ~588k entries/tick → **4.3 ms/tick (7 751 ms total)**.
- `death_cleanup` purge: scans all ~588k entries on death ticks → **12.3 ms/tick (21 010 ms total)**.

Optimizing the cleanup algorithm is the wrong level. **Bounding how close units can get (a unit
radius with circle tests) bounds every list**, which fixes both systems at once. That is this
rework.

**Finding 3 — the `NearbyEnemies` purge is redundant *today*.** `nearby_enemies` does an
unconditional full rebuild every tick and runs before `combat`, building every list from currently
living units, so any dead reference is overwritten next tick. The only readers are `combat` (fresh
lists; tolerates a stale ref via `world.get` → `Err` → skip) and `combat_health()` (counts
`len()` only). **User decision (locked): keep the purge as-is.** The next `nearby_enemies` rework
switches to a **conditional rebuild** — after which stale references are *not* wiped by any
rebuild, so the death_cleanup purge becomes load-bearing. `death_cleanup` is therefore
**out of scope for this plan**.

## The rework — unit occupancy as a circle (unit radius)

### Model
A unit **is** `(x, y)` with a **global** radius. Two predicates, both the same branch-free circle
test `dx² + dy² < T²` (FMA/SIMD-friendly), used with different thresholds:
- **Combat:** `dist < COMBAT_DISTANCE (10)` → in combat; the mover is the attacker.
- **Occupancy:** a unit may not move so another unit's center comes within
  `MIN_UNIT_DISTANCE (8)`.

**No cell grid, no occupancy `HashMap`.** The only surviving grid is the spatial-lookup index
`nearby_enemies` already builds — now *shared*, built by movement, cell size tuned to the combat
reach (decoupled from the rule).

**Why this is the win (bounded density):** because centers can't approach closer than 8, units
pack at min spacing 8 → **any disk of combat radius 10 holds only ~O(1) enemies** (a 10-radius
disk at 8-spacing holds ~4–7 units). That is the exact bound the cell model was reaching for,
derived *geometrically* from the radius instead of an arbitrary cell size. So `total_nearby_entries`
stays bounded and the O(588k) cost in `nearby_enemies` + `death_cleanup` collapses — same payoff as
the cell plan, with the cell abstraction and the occupancy map **deleted**.

### Constants — `src/game/constants.rs` (replace `COMBAT_RANGE_SQ`)
```rust
/// Visual/physical radius of a unit's dot (a 10×10 dot). Global for now — no
/// per-unit-type field, so the client draws the same size with no protocol change.
pub const UNIT_RADIUS: f32 = 5.0;

/// Two units are in combat when their circles touch/intersect: center distance
/// < 2×UNIT_RADIUS (the two 5px dots tangent at distance 10). Mover is attacker.
pub const COMBAT_DISTANCE: f32 = UNIT_RADIUS * 2.0;            // = 10.0
pub const COMBAT_DISTANCE_SQ: f32 = COMBAT_DISTANCE * COMBAT_DISTANCE; // = 100.0

/// Closest two unit centers may approach. 8 < COMBAT_DISTANCE, so units can
/// close to 8–10 (fighting; 10px dots overlap by up to 2px) and stop at 8.
pub const MIN_UNIT_DISTANCE: f32 = 8.0;
pub const MIN_UNIT_DISTANCE_SQ: f32 = MIN_UNIT_DISTANCE * MIN_UNIT_DISTANCE; // = 64.0

/// Spatial-lookup cell side. Set to COMBAT_DISTANCE so the 3×3 neighbourhood is
/// complete for both the combat (radius 10) and occupancy (radius 8) queries.
/// A locality tuning knob, decoupled from the rule.
pub const SPATIAL_CELL_SIZE: f32 = COMBAT_DISTANCE;            // = 10.0
```
Const-safety unit tests pin: `MIN_UNIT_DISTANCE < COMBAT_DISTANCE` (units can close enough to
fight — otherwise they'd be blocked beyond combat range), `COMBAT_DISTANCE == 2.0 * UNIT_RADIUS`,
`SPATIAL_CELL_SIZE >= COMBAT_DISTANCE` (3×3 completeness).

### Verified structural facts (from code audit)
- `UnitPosition` is written by exactly **one** path outside spawn: the movement apply phase
  (movement.rs:130). `water.rs` only toggles `on_water`/timers → **one enforcement point**.
- City dots are plain `Economy.cities` data, **not ECS units** → a city never blocks; only a
  previous recruit still sitting on the city blocks the next.
- The grid `nearby_enemies` builds (`GridIndex`/`GridIndexScratch`) already **reuses flat buffers
  in place** (prefix-sum offsets, resize-only-on-growth — **no `::clear()`**). It is exactly the
  spatial index the model needs; we relocate it to a shared module and change *who* builds it.
- `combat.rs` iterates `NearbyEnemies` lists and re-checks `dist² >= COMBAT_RANGE_SQ`
  (combat.rs:284, test :651) → **constant-only** change there.
- The stress test **already runs 1000×1000 / 10 000 units** (`run-stresstest.sh` defaults
  `--bench-map-size 1000 --bench-initial-units 10000`); capacity at spacing 8 is
  `(1000/8)² = 15 625 ≥ 10 000` → fits.
- The only stacking consumer is the benchmark's initial `recruit_unit` population; all tests
  recruit ≤1 unit/city (audited).

### 1. Spatial grid → shared module `src/game/spatial.rs`
Move `GridIndex` / `GridIndexScratch` / `cell_for` out of `nearby_enemies.rs` into `spatial.rs`.
Cell size → `SPATIAL_CELL_SIZE`. Add a shared, allocation-free neighbour query:
```rust
// For each unit within `radius_sq` of `pos` in the built grid, call `f`.
// 3×3 cell scan + dist² filter (SIMD-friendly, branch-free inner test).
fn for_each_within(
    grid: &GridIndex, scratch: &GridIndexScratch,
    pos: UnitPosition, radius_sq: f32,
    mut f: impl FnMut(Entity, UnitPosition, u8),
)
```
Both movement (occupancy, `MIN_UNIT_DISTANCE_SQ`) and nearby_enemies (combat,
`COMBAT_DISTANCE_SQ`) call it. The build stays O(n), in-place, no `::clear()`.

### 2. Movement — build grid, gate moves, hand off (`movement.rs`)
`run` gains a `&mut grid_scratch` parameter (the `Game.grid_index_scratch` field; pass it at the
call site game.rs:728). Pipeline:
1. **Build pre-move grid** from all current unit positions (in-place, O(n)).
2. **Compute** (unchanged `compute_movement` per unit → new_pos / new_path / pop_head / org).
3. **Gate** in the apply loop, per `change.moved` unit: query the **pre-move** grid for any
   *other* unit within `MIN_UNIT_DISTANCE` of the clamped `new_pos` (`for_each_within` with
   `MIN_UNIT_DISTANCE_SQ`, **skipping the mover's own entity**). If a neighbour is within 8 →
   **block**: no `UnitPosition` write, **no `pop_head`** (the waypoint is *not* consumed — the
   unit didn't reach it), no `movers` push, no `moved` flag; the unit stays at `old_pos` and
   retries next tick (movement recomputes from the path every tick). Else apply as today.
4. **Build post-move grid** (in-place, O(n)) reflecting applied positions — this is the grid
   nearby_enemies + combat consume (fresh, post-move).
- **Cost:** 2 in-place O(n) builds/tick, no allocation at steady state, no `::clear()`. Far
  cheaper than the heavy per-cell work it replaces (the win is that 3×3 neighbourhoods now hold
  ~4–7 entries instead of ~1 000).
- **Soft equilibrium:** two units converging on the *same* point in the *same* tick both pass the
  pre-move check (neither is there yet) and may overlap for one tick; next tick one is blocked and
  resolves. Self-correcting; combat treats them as engaged regardless (dist < 10). Documented in
  the module doc.
- **Bounded-density equilibrium:** blocked units wait at their path head until a neighbour
  moves/dies → stable shells around contested points. No list growth.

### 3. nearby_enemies — consume the movement-built grid (`nearby_enemies.rs`)
`run` **no longer builds the grid**. It takes the **post-move** grid (built by movement, via the
`Game.grid_index_scratch` field) and runs the existing 3×3 enemy-collection loop with
`COMBAT_DISTANCE_SQ` (replacing `COMBAT_RANGE_SQ`) — via `spatial::for_each_within`. Output:
`NearbyEnemies` lists + `attacking` flag for movers — **unchanged semantics, fresh (post-move)
lists**, preserving the death_cleanup freshness invariant. `GridIndexScratch` ownership stays a
`Game` field; movement writes it, nearby_enemies reads it.

### 4. combat — constant only (`combat.rs`)
`COMBAT_RANGE_SQ` → `COMBAT_DISTANCE_SQ` at combat.rs:284 (the re-check) and the test at :651. No
structural change — combat still iterates `NearbyEnemies` lists.

### 5. Recruit gate + premade path — `src/game/game.rs`
- Factor the resource-check + deduct + `spawn_unit(pos)` + post-spawn bookkeeping tail of
  `recruit_unit` into a private helper `fn recruit_at(&mut self, unit_type, owner, pos)
  -> Option<String>`.
- `recruit_unit(city_id, ...)`: in **Phase 1 validation** (before any deduction — an occupied city
  must not spend resources) add the **circle gate**: on-demand O(n) scan over
  `(Entity, &UnitPosition)` for any center within `MIN_UNIT_DISTANCE` of `city_pos` → if found,
  `return None` (nothing spent). Recruits are event-rate in real play, so no grid is needed here.
  Else `recruit_at(..., city_pos)`.
- `pub fn recruit_unit_at(&mut self, unit_type, owner, pos) -> Option<String>`: the
  **premade/benchmark bypass** — identical resource checks + bookkeeping, **no circle gate**,
  explicit position.

### 6. Benchmark — non-overlapping lattice + fail-fast (`benchmark.rs`)
- **Safety check (fail-fast):** before initial placement, compute
  `capacity = (map_size / MIN_UNIT_DISTANCE).floor()²` (as u64). If `requested_total > capacity`,
  **error out immediately** with a clear message (e.g. "10 000 units cannot lay out
  non-overlapping on 200×200 (capacity 625); increase --bench-map-size"). At 1000×1000 →
  capacity 15 625, so 10 000 fits.
- **Initial population (:364-395):** replace the city-point `recruit_unit` stacking with a
  **non-overlapping lattice** via `recruit_unit_at`. `n = ceil(sqrt(target_total))`;
  `spacing = map_size / n` (≥ MIN_UNIT_DISTANCE, guaranteed by the capacity check); unit `k` →
  `((k % n + 0.5)·spacing, (k / n + 0.5)·spacing)`. Owner round-robin over cities and
  infantry/cavalry parity **preserved** — only the position changes (city point → lattice cell).
  **Non-overlapping from tick 0** → no warm-up drain.
- **Ongoing recruit events (:170-191):** keep `recruit_unit` (the real, **gated** path) — cities
  recruit on their dot and are naturally blocked while the previous recruit is still there.
  Realistic; exercises the gate.
- **Movement events** (30 % → centre every 20 ticks, :153-166) **unchanged** — now form bounded
  shells instead of a 1 000-stack.

### Expected impact (the payoff)
- `total_nearby_entries`: 588 146 → **bounded** (engaged units × ~4–7 in a 10-radius disk at
  8-spacing). ~100× drop, and **no warm-up spike** (lattice is non-overlapping from tick 0).
- `nearby_enemies`: 4.3 ms/tick → O(n × ~10), target ≲ 0.2 ms/tick.
- `death_cleanup`: 12.4 ms/tick → collapses with entry count (death spikes µs-scale).
- `combat`: fewer candidate pairs → slightly cheaper.
- **Gameplay (by design):** units keep ~8px spacing, form shells around contested points, fight at
  dist 8–10. All `units_remaining` / `total_nearby_entries` / `combats` / `movers` shift vs
  stress_6 → **stress_9 is the new baseline**; do not diff gameplay rows against stress_6.
- **Rendering:** the client draws a 10×10 dot (2×UNIT_RADIUS) at (x,y). No protocol change (radius
  is a global const; `delta.rs`/`protocol.rs` only carry `(x,y)` and are unaffected).

## death_cleanup — unchanged (user decision)
Keep the shipped reverse-lookup purge (`dead_set` + `seen` + `candidates`) exactly as-is. Under the
*upcoming conditional rebuild* of `nearby_enemies`, stale references are no longer wiped by any
rebuild, so the purge is load-bearing and its fresh-lists invariant is the contract that rebuild
must uphold. `dc_phase_measurement` is **kept** as the permanent dense-death-spike regression
harness — it builds 1 000-entry lists manually, independent of the game's occupancy rules.

## Large-list coverage — kept (user decision)
Unchanged fixtures: `systems/nearby_enemies_500` (500-entry lists), `systems/alloc_n640` (250-entry
alloc churn), `systems/death_cleanup_50` (with the `setup_nearby_enemies` wiring from the prior
plan), and the `dc_phase_measurement` harness. These exercise the O(entries) paths at scale even
though the in-game stress benchmark will now see bounded density.

## Files to modify
1. `src/game/constants.rs` — remove `COMBAT_RANGE_SQ`; add `UNIT_RADIUS`, `COMBAT_DISTANCE(_SQ)`,
   `MIN_UNIT_DISTANCE(_SQ)`, `SPATIAL_CELL_SIZE` + const-safety tests.
2. `src/game/spatial.rs` — **new module**: move `GridIndex`/`GridIndexScratch`/`cell_for` from
   nearby_enemies; use `SPATIAL_CELL_SIZE`; add `for_each_within`. Declared in the game `mod` tree.
3. `src/game/systems/movement.rs` — `run` +`&mut grid_scratch`; build pre-move grid; per-mover
   circle gate (self-excluding, block-skip: no pos write / no `pop_head` / no mover / no flag);
   build post-move grid; module doc (rule + soft-equilibrium).
4. `src/game/systems/nearby_enemies.rs` — stop building the grid; consume movement's post-move
   grid; `COMBAT_RANGE_SQ` → `COMBAT_DISTANCE_SQ`.
5. `src/game/systems/combat.rs` — `COMBAT_RANGE_SQ` → `COMBAT_DISTANCE_SQ` (:284, test :651).
6. `src/game/game.rs` — `recruit_unit` circle gate (Phase 1); factor `recruit_at` helper; new
   `recruit_unit_at`; pass `grid_index_scratch` to movement (call site :728).
7. `src/benchmark.rs` — fail-fast capacity check; initial population → non-overlapping lattice via
   `recruit_unit_at`; ongoing recruits keep `recruit_unit`; header comment.

**No changes:** death_cleanup.rs, components.rs (`UnitPosition` unchanged — radius is a global
const, **no new component field**), debug_stats.rs, world_ext.rs, microbenchmarks.rs, supply.rs,
water.rs, protocol.rs, delta.rs, tests/.

## Steps
1. **Constants + const-safety tests** (Constants section above).
2. **Spatial module** — move the grid to `spatial.rs`, add `for_each_within`; unit tests:
   neighbours within/without the radius; 3×3 completeness at the `SPATIAL_CELL_SIZE = 10` boundary
   (a unit at dist just under 10 is found); build reused in place (no realloc / no clear).
3. **Movement gating** — §2 + unit tests (module's existing test helper builds worlds): move into
   empty area succeeds (pos written, mover recorded); **blocked** when another unit within 8 (pos
   unchanged, **path head NOT popped**, not in `movers`, `moved` false); self-exclusion (moving
   within its own 8-disk is allowed); a blocked unit moves once the neighbour leaves/despawns;
   determinism (fixed ECS order).
4. **nearby_enemies consume grid** — §3; existing tests still pass (now reading the
   movement-built grid); confirm 3×3 completeness at cell size 10.
5. **combat constant** — §4; combat tests still pass.
6. **Recruit gate + bypass** — §5 + tests: first recruit OK; second (previous unmoved) returns
   `None` **and deducts nothing** (funds + manpower unchanged); after the previous unit moves away
   the next recruit succeeds; `recruit_unit_at` places two units at the same position (bypass),
   deducts resources and registers id/owner identically to `recruit_unit`.
7. **Benchmark lattice + fail-fast** — §6 + unit test for the capacity check (10 000 on 200×200 →
   error; on 1000×1000 → ok); smoke-run a short benchmark ("Recruited 10 000 units", non-overlapping).
8. **Build + full test pass** (Verification).
9. **Stress benchmark** — fresh circle-model baseline (Verification).

## Verification
```
cargo test                       # debug: constants + spatial + movement + nearby + combat + recruit + all
cargo test --all-targets -- --skip alloc_invariants::warm_tick_net_heap_growth_is_bounded
cargo test --lib alloc_invariants::warm_tick_net_heap_growth_is_bounded -- --exact   # dhat (separate)
cargo clippy --all-targets --all-features -- -D warnings
cargo build --release
```
Stress (do not run unattended — one long `cargo run`):
```
./run-stresstest.sh 9          # circle-model baseline — NO --bench-prev: behavior changed, this IS the new baseline
```
Subsequent tuning runs diff against `./reports/stress_9.html`; repeating stress_9 with the same
seed must produce identical gameplay rows (determinism check).

Expected vs stress_6 (**gameplay rows shift — compare health/timing shapes, not exact gameplay
values**):
| Row | stress_6 | Expected (stress_9) |
|---|---|---|
| `health_avg_total_nearby_entries` | 588 146 | **bounded** — engaged × ~4–7 (lattice is non-overlapping from tick 0 → no warm-up spike) |
| `nearby_enemies` total | 7 751 ms | collapses with entry count (target ≲ 500 ms) |
| `death_cleanup` total | 21 010 ms | collapses (target ≲ 1 000 ms; spikes µs-scale) |
| avg tick wall | 20.234 ms | drops by the two systems' reduction (≈ −10–15 ms) |
| `units_remaining` / deaths / combats | 8 775 / … | **will differ** (8px spacing; shells) — sanity: deterministic across repeats |
| microbench rows (nearby_enemies_500, death_cleanup_50, alloc_*, dc harness) | as shipped | **unchanged** (manual fixtures; occupancy doesn't touch them) |
| allocs | ~16 / 8.9 KB | ~same (grid scratch is grow-only, in-place) |

Must pass: all new constants/spatial/movement/recruit tests; all existing tests green (pre-audit:
no test recruits >1 unit/city; benchmarks route through the lattice/bypass); integration_game.rs
dead-unit test unaffected.

## Risks & mitigations
- **R1 — gameplay shift (expected, by design).** 8px spacing; convergence forms shells;
  `units_remaining` etc. change vs stress_6. Mitigation: stress_9 is the *new* baseline; repeat run
  confirms determinism (same seed → identical rows).
- **R2 — same-tick convergence overlap (soft, self-correcting).** Two convergers on the same point
  both pass the pre-move check → 1-tick overlap → next tick one is blocked. Not a hard invariant
  (it is a perf/density bound, not a physical law), so a transient overlap is harmless; combat is
  unaffected (dist < 10). Documented in movement's module doc.
- **R3 — two grid builds/tick (cost).** Both O(n), in-place, no allocation at steady state; ≪ the
  heavy per-cell work they replace. If profiling shows the 2nd build matters → incremental update
  (out-of-scope follow-up).
- **R4 — 3×3 completeness at `SPATIAL_CELL_SIZE = 10`.** A candidate at dist < 10 is always in the
  3×3 (cell side 10 → 3×3 covers radius 10). Pinned by a boundary unit test + the const-safety
  `SPATIAL_CELL_SIZE >= COMBAT_DISTANCE`.
- **R5 — traversal slower now.** Movement step is `speed/10` (tiny vs 8px spacing) → no tunneling
  (a step can't jump an 8-gap), but crossing the map is slower. Bump `speed` values if balance
  needs it — tuning, out of scope.
- **R6 — recruit on-demand O(n) scan (negligible).** Event-rate in real play; benchmark bypasses.
  No per-tick cost on this path.
- **R7 — determinism (low).** Fixed ECS order for builds + gate; no RNG / wall-clock. Stress
  repeat-run check in Verification.
- **R8 — capacity-check margin.** Uses `floor(map/D)²` (conservative lower bound); true packing
  fits slightly more, so the check may reject a config that *barely* fits. Acceptable — fail-fast
  is the intent. Documented on the check.
- **R9 — out of scope, unchanged:** death_cleanup structure (kept for the conditional-rebuild
  contract); the `nearby_enemies` conditional rebuild itself (next plan — its TODO already
  documents the death_cleanup freshness dependency); per-unit-type radius (when the client needs
  variable dot sizes → then a protocol field); `owner_id_by_entity`/`entity_registry` unbounded
  growth; `tick_durations_ms` ring.
