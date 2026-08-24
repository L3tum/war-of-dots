# W2 — Encirclement: kill the 10 ms P99 flood-fill hitch

**Status:** **① shipped (packed-coord serial) — that is the final W2 state.** Both
parallel designs were **measured and rejected**; `run` is back to serial-only and
**rayon has been removed from the tree** (it was not a pre-existing dependency).
- **① packed-coord division fix — SHIPPED** (the `run` path): the queue is
  `u32 = (cy << 16) | cx`, neighbors are pure index math (`idx ± 1`, `idx ± w`).
  Free, correct, tighter inner loop. This is what stayed.
- **②-wave — rejected.** Bounded-depth waves over a *combined* frontier
  (re-partition + join-barrier every 32 depth-levels): **5 % faster than serial
  at 1000², and the same 5 % at 4 and 32 threads** — the flat line proved it
  wasn't parallelizing (per-wave barriers + the `safe[n]==NONE` claim starved
  late waves). Removed.
- **②′ per-player parallel — rejected (measured, run `14`).** Premise: each
  player's safe region is *disjoint* (a cell has one `dominant` value, so a flood
  only ever claims `dominant==p` cells), so the combined BFS was re-expressed as
  one single-player flood per player, run concurrently. That was **true for
  writes** (no overlapping writes → bit-identical, equivalence test passed) but
  **false for work**: the four dominance regions are large, intermixed, and
  together cover the whole map (influence frontiers tangle), so each "disjoint"
  region is a big scattered, cache-hostile set. Splitting them across threads
  doesn't reduce the ~1M cells visited; it adds a rayon task + a per-player queue
  per flood for zero speedup. **Measured (run `14`):** microbench `flood_per_player_1000`
  **3.366 ms vs serial 3.048 ms → 10 % slower**; real-game `p2_bfs` avg **4.822 →
  5.710 ms (+18 %)**, `encirclement` avg 5.479 → 6.352 ms, **P99 unchanged at
  10 ms**, and allocs **21 → 89/call** (1.18 KB → 4.00 MB/call). Reverted.
- **Conclusion:** the flood is **serial-bound** (dependent store→load chain over a
  1 MB `safe` grid) and its parallelism is **not** cleanly partitionable by player
  (regions interleave). rayon's dispatch overhead dominates at every thread count
  we tried. The one remaining avenue to the ≤ 2 ms P99 goal is **③ amortization**
  (spread the flood over N ticks) — a gameplay decision (a ~1-tick staleness per
  player in the encircled flag), so it is **not** shipped without the user's OK.
- **Tree state:** `run` → `flood_serial` only. `flood_per_player`,
  `should_per_player_flood`, `Ptr`, the per-player equivalence test, and `rayon`
  are all **removed**. `build_bench_dominant` / `seed_queue` remain (used by the
  serial-only `encirclement/flood_serial_1000` microbenchmark + the serial
  degenerate-case test). `delta.rs` TODO updated to note rayon was trialed/removed.
**Baseline (run `11detailed`, 2026-08-21):** encirclement **5.913 ms avg / 10 ms P99**
on the ticks it runs (56/1500 ticks ≈ every 27th). Split: `p1_seed` 0.477 ·
`p2_bfs` **5.311 (90 %)** · `p3_flags` 0.124. Tick average is 4.12 ms → the P99 is a
**2.4× frame hitch**.
**Success metric:** `p2_bfs` P99 ≤ **2 ms**; randomized equivalence test green
(serial vs parallel produce identical `safe` grids); existing encirclement tests
green.

## Why it's slow

The combined multi-player BFS walks up to ~1 M cells serially (5.3 ms ≈ 5 ns/cell —
a dependent store→load chain across the 1 MB `safe` grid + queue growth). It is
genuinely serial-bound; no micro-optimization removes the chain.

## Key correctness fact (makes parallelism trivially safe)

The final `safe` grid is **order-independent**: `safe[cell] == p` iff the cell is
dominated by `p` *and* reachable (4-connected) from a same-player safe anchor
(edge or city-radius). Which order cells are visited does not change the result.
Therefore *any* correct multi-source flood fill — serial, parallel, threads claiming
cells in any order — yields a **bit-identical** `safe` grid, and hence identical
`UnitFlags::encircled` flags. This is what the equivalence test asserts.

## Steps (re-stress after each; independent of W1 — different files)

Collect after each step:
```
./run-stresstest.sh 11w2s<N> --bench-detailed encirclement --bench-prev=./reports/stress_11detailed.html
```

### ① Drop per-cell `/` and `%` (cheap, ship with ② or standalone)

Current inner loop computes `cx = idx % w` and `cy = idx / w` per dequeued cell —
two runtime divisions (~20–30 cycles each). Replace the queue with packed
coordinates:

- queue entry: `u32` = `(cy << 16) | cx` (map dims ≤ 65536 — 1000×1000 stress is fine;
  `debug_assert!(w < 65536 && h < 65536)`; fall back to the old path or store
  `u64` if a map could exceed — decide then, the stress map is 1000²);
- on dequeue: `cy = v >> 16`, `cx = v & 0xFFFF`, `idx = (cy as usize) * w + cx`
  (one multiply);
- neighbors: `(cx+1, cy)` → `idx + 1`, `(cx-1, cy)` → `idx − 1`, `(cx, cy±1)` →
  `idx ± w` — pure index math; bounds checks unchanged.

Also hoist: `let pidx = safe[idx]` stays; nothing else.
**Expected:** modest (divs ≈ 50–100 µs of 5.3 ms), but free, and it makes the
parallel step's inner loop tighter.

### ② Parallel flood — wave version (REJECTED, removed)

First attempt: bounded-depth parallel **waves** over a *combined* frontier — each
wave split the frontier into chunks, each chunk ran a local BFS to depth 32
claiming cells, unexpanded boundary merged into the next wave, repeat. **Measured
result: only 5 % faster than serial at 1000², and the *same* 5 % at 4 threads and
32 threads.** The flat line across thread counts is the verdict — it wasn't
parallelizing: the per-wave join barriers + the `safe[n]==NONE` claim (late-wave
cells already claimed by other threads) starved each wave's independent work, and
~30 barrier-separated waves at depth 32 on a 1000-deep flood cost as much as they
saved. Not worth the rayon dependency. **Removed from the tree.**

### ②′ Per-player parallel flood (REJECTED, reverted, run `14`)

The structural idea the wave design missed: **each player's safe region is
*disjoint* from the others'** — a cell is dominated by exactly one player, and a
flood only ever claims cells whose `dominant == p` — so the combined multi-player
BFS was re-expressed as **one independent single-player flood per player**, run
concurrently (one rayon task per player, each to completion).

```text
partition seeds by dominant player  →  locals[p]
locals.par_chunks_mut(1).enumerate().for_each(|(p, one)| {
    run player p's single-player BFS to completion over `one`   // one rayon task/player
});
```

**It was bit-identical to serial** (exclusive-writer — each cell is written by
exactly one thread — so no competing writes; the randomized equivalence test
passed). That part of the premise held. **But the performance premise was wrong:**
the four dominance regions are *disjoint as sets of cells* yet **large, intermixed,
and together cover the whole map** (influence frontiers tangle). Each "disjoint"
region is a big, spatially-scattered, cache-hostile set, not a clean contiguous
block. Splitting them across threads doesn't reduce the ~1M cells visited; it
gives each thread a scattered walk (worse locality) **plus** a rayon task and a
per-player BFS queue (the alloc spike) for **zero speedup**.

**Measured (run `14`, the real-game A/B since `run` used per-player where the
gate fired and `13` was serial):**
| metric | serial (13) | per-player (14) | verdict |
|---|---|---|---|
| `flood_serial_1000` microbench | 3.048 ms | — | baseline |
| `flood_per_player_1000` microbench | — | **3.366 ms** | **10 % slower**, not 4× faster |
| `encirclement/p2_bfs` avg | 4.822 ms | **5.710 ms** | **+18 %** |
| `encirclement` total avg | 5.479 ms | 6.352 ms | +16 % |
| `encirclement` P99 | 10.000 ms | **10.000 ms** | **no change** |
| encirclement allocs/call | 21.00 (1.18 KB) | **89.02 (4.00 MB)** | **+7×** |

**Verdict:** the flood is serial-bound (dependent store→load chain over the 1 MB
`safe` grid) and its work is **not cleanly partitionable by player** (regions
interleave). rayon's dispatch overhead dominates at every thread count tried (4 and
32 both flat). Reverted to serial; `flood_per_player` / `should_per_player_flood`
/ `Ptr` / the equivalence test / the rayon dep are all removed.

### ③ (only if P99 still > ~2 ms) Amortization — needs human sign-off

Spread the flood over N ticks (player rotation: tick k floods player k mod N).
The encircled flag is strategic (drives supply cutoff), so a ~1-tick staleness per
player is likely acceptable, but it's a **gameplay decision** — do not ship without
the user's OK.

## Verification per step

1. `cargo build --release` clean (production, no features); `cargo clippy --profile release --features debug-overlay,microbenchmarks,bench-alloc --all-targets -- -D warnings` — 0 warnings. Confirmed after the revert (rayon removed).
2. `cargo test` — **446 green** (the per-player equivalence test was removed with the code; `flood_serial_degenerate_cases` remains). Note: `alloc_invariants::warm_tick_net_heap_growth_is_bounded` uses the dhat process-global profiler, which picks up *other* test threads' transient heap under parallel `cargo test`; its single-tick check now takes the **median of 7 samples** (a real per-tick leak is a consistent offset → survives the median; a concurrent spike is an outlier → filtered). This fix is kept — it is independent of the encirclement work.
3. `make check-vectorization` — PASS (revert only adds a serial path; no SIMD canary touched).
4. Real-game A/B already run (`14 --bench-prev=13`): per-player lost on every axis (see table above) → reverted. No further re-run needed; `run` is back to run-13 serial behavior.

## Combined `(dominant<<8)|safe` u16 grid — REJECTED (measured, 2026-08-23)

Idea: merge the two 1 MB `u8` arrays (`safe`, `dominant`) into one 2 MB `u16`
array so the BFS neighbor test is a single 2-byte load + one compare pair
instead of two 1-byte loads (usually two cache lines).

**Measured with the `flood_serial_timing_probe` test** (1000² contested
quadrants, min/median over 5, same conditions both runs):

| variant | setup (reset+seed) | flood | verdict |
|---|---|---|---|
| two × `u8` (shipped) | **0.46 ms** | **3.03 ms** | baseline |
| combined `u16` | 0.66 ms | 3.32 ms | **~10 % slower, reverted** |

**Why the merge loses — the original design was already exploiting a detail
the merge throws away:** the shipped neighbor test
`safe[nidx] == NONE && dominant[nidx] == pidx` **short-circuits**. The vast
majority of neighbor checks are against already-claimed interior cells, where
the first byte-load fails and the `dominant` load never happens — the shipped
code reads **1 byte** per rejected neighbor. The merged form must read 2 bytes
to even test the low byte, and the 2 MB array additionally spills out of L2.
A code comment at `flood_serial` now warns against merging the arrays.

**Left in the tree from this attempt (intentional):** the
`flood_serial_timing_probe` test — a local before/after tripwire for the flood
(so future micro-opt attempts get a signal without a full stress run; note the
absolute numbers scale with CPU clock state — compare only same-session A/B).

**W2 is now fully closed.** The only remaining lever is the 30-tick cadence
(gameplay decision: encircled-flag staleness), not an engineering item.

## Before/after log

| step | p2_bfs avg | p2_bfs P99 | allocs/call |
|---|---|---|---|
| baseline (11detailed) | 5.311 ms | 10.000 ms | 9.00 (9/call, 0.30/tick) |
| ① (shipped, run 13) | 4.822 ms | 10.000 ms | 21.00 (1.18 KB/call) |
| ② wave A/B (1000² microbench) | 2.94 ms vs 3.10 ms serial → **5 %, rejected** | — | — |
| ②′ per-player (real game, run 14) | **5.710 ms** (+18 % vs 13) | 10.000 ms (no change) | **89.02 (4.00 MB/call)** |
| ②′ per-player (1000² microbench) | **3.366 ms vs 3.048 ms serial → 10 % slower** | — | — |
| **final: reverted to ① serial** | 4.822 ms (run 13) | 10.000 ms | 21.00 (1.18 KB/call) |
| combined-u16 probe (local, same-session) | 3.32 ms vs 3.03 ms two-×u8 → **+9 %, reverted** | — | — |

## SIMD dilation probe (fmarqvardt scanline-style) — REJECTED, 2026

Explored the "bit-plane + shift-OR dilation to fixpoint" approach (fmarqvardt/scanline-floodfill) as a SIMD replacement for the BFS. Implemented a branchless, SIMD-friendly variant (`(self|L|R|U|D) & mask` per cell, per player, iterated to fixpoint) — verified **byte-identical** to `flood_serial` — then measured it at the 1000² contended 4-player grid:

| approach | 1000² time |
|---|---|
| shipped BFS `flood_serial` | **11.15 ms** |
| dilation (SIMD) probe | **2453 ms** (~220× slower) |

**Why it can never win here (fundamental, not an implementation detail):** dilation converges in O(map-diameter) *full-plane* passes — each pass advances the frontier by one cell and re-reads/re-writes the whole 1 MB plane per player. Our territory floods are map-scale (diameter ≈ 1000 on the stress map), so that is ≈1000 passes × 4 players × 1 MB × (read + write) ≈ tens of GB of memory traffic, regardless of how well the arithmetic vectorizes. The bottleneck is bytes moved, not FLOPs, so SIMD parallelism cannot help. The fmarqvardt trick is aimed at *small* flood regions (click-to-fill a bounded blob) where the region diameter is tiny; that is the opposite of our workload. The single O(cells) BFS sweep touches each visited cell once and is ~220× less total traffic. **Decision: keep the serial BFS; no SIMD formulation of this flood fill is worthwhile.** (Probe code + test + microbenchmark were removed after measurement; finding recorded here.)
