# Encirclement hitch — forward strategy plan

**Goal:** reduce the encirclement P99 10 ms hitch (a 2.4× frame hitch on the
ticks it fires) without changing gameplay semantics, and decide whether it is
even worth touching.

**Status (2026-08-23):** **Option B (skip) + work-slicing implemented and
locally verified** — see “Shipped: skip + work-slicing” below. Awaiting the
stress-test A/B (`run 18`) to confirm the P99 collapses from 10 ms to ~2.5 ms.
Options C (downsample) and D (local predicate) are **not** implemented and are
only needed if B+slicing are insufficient on churn-heavy maps.

**Companion doc:** `plans/W2_ENCIRCLEMENT.md` is the *exploration log* — it
records the micro-optimization and parallelism attempts that were **measured
and rejected** (packed-coord serial BFS shipped; wave-parallel, per-player
rayon, u16-combined grid, and SIMD dilation all rejected with numbers). This
doc is the *forward-looking decision plan*: what is actually left, ranked, with
a decision framework. **Read both.**

## Shipped: skip + work-slicing (2026-08-23)

The flood (`p2_bfs`, ~5–10 ms) was the whole hitch. It is now wrapped in two
correctness-preserving fast paths in `systems/encirclement.rs`:

1. **SKIP (Option B):** the `safe` grid is a pure function of
   `(territory grid, city anchors, player id list)`. `EncirclementScratch` now
   keeps the last fill’s input snapshot (the territory `Arc` by refcount, the
   city-anchor vec, the sorted player-id vec). When all three match the current
   inputs **and** the last fill completed, `run` reuses the cached `safe` grid
   and runs only the per-unit flag pass (bit-identical; ~0.12 ms instead of
   ~5 ms). The territory compare is an `Arc` pointer-equal fast path, else a
   byte compare (tens of µs on 1 MB).
2. **SLICE (work-slicing):** the BFS is now a resumable, budgeted step
   (`flood_step`), run for at most `FLOOD_TICK_BUDGET` (2 ms) per encirclement
tick. A 4.8 ms flood spreads over ~3 encirclement ticks instead of one 10 ms
   hitch. `flood_serial` is now a thin wrapper (`budget: None`) used by tests /
   the microbenchmark; the serial probe still reads ~3.0 ms (no regression).
   Chunking is bit-identical to a single run (the `safe` grid is
   visit-order-independent) — covered by the `flood_step_chunked_matches_serial`
   test.

**Key invariant (prevents a footgun):** if inputs change while a fill is
in-flight, we **finish the old fill on its own snapshot** rather than abort —
aborting under continuous churn would restart a fresh 2 ms chunk every tick and
never complete, stranding the `encircled` flag stale forever. The tradeoff is a
bounded flag staleness of ~one fill cycle (≤ ~3 encirclement intervals = ~90
game ticks in the worst churn case, vs ~30 ticks today). The flag is a strategic
supply/heal gate, not a per-tick tactical one, so this is acceptable. The
per-unit flag pass is updated **only on completion** (a partial `safe` would
over-report encirclement), and always pairs the `safe` grid with the same
snapshot it was built from.

**Local verification:** 456 tests green (incl. new `flood_step_chunked_matches_
serial` + `inputs_unchanged_skip_logic`), clippy `-D warnings` clean, serial
flood probe unchanged (~3.0 ms). **Pending:** `run 18` stress A/B (P99 target
10 → ~2.5 ms; `encirclement` amortized cost should fall, with `p2_bfs` now
bounded to ~2 ms/tick and ≈ 0 on skip ticks).

## Current measured state (stress_16, 1000×1000, 4 players)

| metric | value |
|---|---|
| `encirclement` avg | **5.417 ms** (runs ~every 30th tick, 56/1500 ticks) |
| `encirclement` P99 | **10.000 ms** (≈ 2.4× the 4.12 ms tick average) |
| `encirclement/p1_seed` (reset + seed) | 0.511 ms (9 %) |
| `encirclement/p2_bfs` (the flood) | **4.785 ms (88 %)** |
| `encirclement/p3_flags` (per-unit flag set) | 0.120 ms |
| amortized to every tick | **~0.18 ms/tick** |
| allocs | 38 B/call (0 at steady state) |

The flood is **serial-bound** (a dependent store→load chain over a 1 MB `u8`
`safe` grid; the neighbor test short-circuits so each interior check is one
byte-load). This is the floor of the current algorithm — no micro-opt removes
the ~1 M-cell single O(cells) sweep, and every parallelism/restructure variant
already measured worse (see W2 log). So the remaining levers are about
**how often and over how much area we run the flood**, not the flood loop itself.

## What the flag actually does (grounds the tradeoffs)

`UnitFlags::encircled` is a **strategic** signal, not a per-tick tactical one:
- `supply.rs`: `can_deliver = !flags.encircled && …` — encircled units get no
  equipment deliveries.
- `supply.rs`: `if !flags.encircled && men < max` — encircled units stop
  replenishing.
- `economy.rs`: `if unit.encircled { return; }` — encircled units don't heal.

It does **not** gate movement or combat. So the flag can lag by several ticks
without breaking the game; the cost is a bounded staleness in a
supply/heal/reinforce decision, not a per-tick correctness break. **This is
what makes the cadence and approximation options viable at all.**

The cadence today: `run` fires only on `tick % CITY_CAPTURE_TICK_INTERVAL == 0`
(= **30 ticks**) *and* `encirclement_dirty` (set whenever influence ran that
tick). So the flag is already up to 30 ticks stale.

## Options, ranked

### 0. First: decide if it even matters (measure before building)
The 10 ms P99 is a *hitch on ~1 in 30 ticks*. Before any engineering, confirm
it is actually user-visible:
- Does the 10 ms spike drop a rendered frame or push a server tick over its
  budget? Check the tick-time distribution in the stress report (P50 vs P99
  of the whole tick, not just the encirclement system).
- If the game is server-side / not real-time-smoothed, a 10 ms spike every
  1.5 s may be **acceptable → Option E (do nothing)**, and we just leave the
  `flood_serial_timing_probe` tripwire in place.

**Decision gate:** only proceed if the hitch is confirmed to hurt (visible
stutter, or tick budget blown on the target platform).

### Option A — raise the cadence (30 → 60+ ticks) — *cheapest, pure gameplay call*
**What:** bump `CITY_CAPTURE_TICK_INTERVAL`-independent encirclement cadence to
60 (or run encirclement on its own longer interval, decoupled from city capture).
**Cost:** one constant + decouple the `is_multiple_of(30)` gate from the
city-capture gate (currently entangled via the shared interval — small refactor
so city capture keeps its own cadence).
**Effect:** halves (or thirds) the hitch *frequency*; the per-run cost is
unchanged (still 5.4 ms when it fires), and the flag staleness grows to 60+
ticks. **Amortized cost halves: ~0.18 → ~0.09 ms/tick.**
**Accuracy:** flag up to 60 ticks stale. Fine for a strategic supply/heal gate;
a freshly encircled unit keeps receiving supply for up to ~1 extra interval.
**Effort:** ~30 min. **Risk:** none to determinism/correctness; purely a
staleness tradeoff. **This is the only option that needs user sign-off** (it
changes gameplay timing).

### Option B — skip the flood when the territory grid is unchanged — *safe, free when it fires*
**Why it's valid:** the `safe` grid is a pure function of `(dominant grid,
map dims, city anchors)`. The territory grid (`dominant`) only changes when
influence changes; city anchors only change when cities are captured/moved.
If the new territory grid is **byte-identical** to the previous run *and* the
city anchor set is unchanged, the `safe` grid is provably identical — so we can
**reuse the cached `safe` grid and skip `p1_seed` + `p2_bfs` entirely**, running
only the cheap `p3_flags` pass (0.12 ms) to re-read each unit's flag from the
reused grid (units may have moved into/out of a safe region, so flags are
recomputed even though the grid is reused).
**How:** add `last_territory: Vec<u8>` + `last_city_signature` to
`EncirclementScratch`. At the top of `run`, if
`territory == last_territory && cities_by_player == last_city_signature` →
skip straight to `p3_flags` on the cached `safe`. Else run the full flood and
store the new grid/signature.
**Effect:** the 4.785 ms flood disappears on every tick where territory is
stable. In the **stress test** (every unit moves every tick) territory churns
every dirty tick, so the skip fires rarely → little win there. In **real play**
(idle units, calm maps) territory is stable for many intervals → the hitch
mostly disappears. Cheap `==` on a 1 MB `u8` vec is tens of µs.
**Accuracy:** bit-identical (same `safe` grid, same flag computation). No
semantics change at all.
**Effort:** ~1 h. **Risk:** low — the skip is a pure equivalence (guarded by
byte-exact territory + city signature). Add one test: run the flood twice on an
unchanged territory, assert the second run skips (a counter) and produces the
same flags.
**This is the recommended first engineering win** if the hitch matters — it
cannot make the result wrong, and it removes the hitch exactly in the calm
situations that are most of real gameplay.

### Option C — downsampled flood grid — *bigger win, moderate effort, small accuracy loss*
**Why:** the encircled flag is a strategic, per-unit boolean. Sub-cell flood
precision is irrelevant. Running the flood on a **coarse grid** (e.g. 2×2 or 4×4
cell blocks) cuts cell count by 4× / 16×.
**How:** build a downsampled `dominant` grid (a block is `dominant` iff
**any** cell in it is — conservative, preserves "is there a friendly path"),
flood that, then for a unit at `(x,y)` read its coarse cell. 500×500 (2×2) →
~1.2 ms; 250×250 (4×4) → ~0.3 ms.
**Effect:** direct ~4–16× cut on `p2_bfs` (4.785 → ~1.2 / ~0.3 ms) on *every*
run, independent of cadence.
**Accuracy:** a unit adjacent to a 1-cell-wide enemy pocket may be flagged
~1 block differently. Acceptable for a strategic gate; **not** bit-identical to
the exact flood, so it changes (slightly) which units are flagged as encircled
— needs a gameplay sign-off and a randomized sanity test (coarse vs exact agree
within a bounded margin / on all non-boundary cells).
**Effort:** ~half a day (downsample + upsample + block-conservative rule + test).
**Risk:** low-moderate (semantic change, bounded).

### Option D — cheap local predicate instead of a global flood — *max win, changes semantics*
**Why:** "encircled" only matters as a supply/heal cutoff. A much cheaper
predicate is sufficient for that: *a unit is encircled if, within radius R of
it, every reachable path to a same-player safe anchor is blocked* — or, more
simply, *if no cell within radius R of the unit is same-player-dominant-and-safe*.
That is an O(units × R²) local check (a few hundred cells per unit), no global
1 MB flood at all.
**Effect:** replaces a 4.8 ms full-map flood with a ~0.3 ms per-unit local
check that runs cheaply every tick (the 30-tick cadence could then be dropped
entirely).
**Accuracy:** loses the "long thin corridor" encirclement case (a unit cut off by
a distant enemy ring far beyond R). Good enough for supply cutoff in practice,
but it is a **different definition** of encircled → gameplay sign-off required,
and it diverges from the current flood's exact result.
**Effort:** ~1 day (new predicate + the radius parameter + tests + parity
measurement against the current flood to size the divergence).
**Risk:** moderate — it is a model change, not an optimization.

### Option E — accept it
At ~0.18 ms/tick amortized the flood is a small slice of the 4.12 ms tick. If
the P99 hitch (Option 0) is confirmed acceptable for the target, **do nothing**
beyond keeping the `flood_serial_timing_probe` tripwire. This is a legitimate
outcome, not a cop-out — the remaining cost is real but small in the aggregate.

## Recommendation

1. **Run Option 0** — confirm the 10 ms P99 is an actual problem. Cheap, no
   code.
2. If it matters: **do Option B** (skip-flood-when-territory-unchanged) first.
   It is correctness-preserving, cheap, and removes the hitch in the calm maps
   that dominate real play. Keep it regardless — it's a pure win.
3. If the hitch *still* bites on churn-heavy maps, **combine with Option A**
   (raise cadence, needs sign-off) and/or **Option C** (downsample, needs
   sign-off + parity test).
4. Only reach for **Option D** if B+A+C together still aren't enough *and*
   accepting the coarser "encircled" definition is gameplay-acceptable.

## Verification (shared by B / C / D)
1. `cargo build --release` clean; `cargo clippy --profile release --features
   debug-overlay,microbenchmarks,bench-alloc --all-targets -- -D warnings` clean.
2. `cargo test` green (existing encirclement tests + new parity/skip tests).
3. `flood_serial_timing_probe` A/B (local) before/after.
4. `./run-stresstest.sh <tag> --bench-detailed encirclement
   --bench-prev=./reports/<prev>.html` — target: encirclement P99 ≤ 2 ms or
   the amortized cost / hitch frequency visibly reduced per the chosen option.
