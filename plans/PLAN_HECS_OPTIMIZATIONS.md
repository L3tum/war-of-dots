# ECS Optimization Plan — Research Edition

## Research Findings

### 1. hecs Best Practices (from hecs README, GitHub #258, community)

The hecs author (Ralith) explicitly positions hecs as a **library, not a framework**:
- No built-in system scheduler — you dispatch systems manually (we're doing this correctly)
- No parallel query support in the core crate — issue #258 discusses it but it's open/closed
  without a merged PR. Community pattern: **read data → compute in parallel → merge back**
- **Spatial index recommendation from README**: *"Most games will store significant amounts
  of state in other structures. For example, many games maintain a spatial index structure
  (e.g. a tile map or bounding volume hierarchy) used to find entities and obstacles near a
  certain position for efficient collision detection without searching the entire world."*
  → We're already doing this with `SpatialHash`, matching the author's guidance.

**Key community pattern for parallelism** (from hecs #258, Bevy ECS):
1. Main thread: collect data from world into plain Rust structures (`Vec`, arrays)
2. Parallel threads: process data (no world access needed)
3. Main thread: write results back into world

This is because `hecs::World` is **NOT `Send`** — it can't be sent across threads.
Bevy solves this with `SystemSet` scheduling + `QuerySet`, but hecs leaves it to you.

**Benchmark insight from hecs #258**: One contributor reported ~300% speedup (4x) for
20,000 entities by parallelizing just the 2 bottleneck systems. For us (480 units),
parallelism is less impactful but still relevant for the influence system (12 independent players).

### 2. SIMD in Rust (2026) — Current State

**CRITICAL FINDING: `std::simd` is STILL nightly-only as of Rust 1.97 (Oct 2026)**

| Approach | Status | Notes |
|----------|--------|-------|
| `std::simd` (portable_simd) | **Nightly only** (`#![feature(portable_simd)]`) | Best API, not usable on stable |
| `wide` crate | **Stable**, near drop-in for `std::simd` | Supports NEON, WASM, all x86. No multiversioning |
| `pulp` crate | **Stable**, native-width SIMD | Powers `faer` (linear algebra). Limited arch support. Native width only |
| `macerator` | **Stable**, `pulp` fork | Better generics, expanded arch support. Unproven |
| `std::arch` intrinsics | **Stable**, safe since Rust 1.87 (May 2025) | Per-arch implementation. Most painful |
| Auto-vectorization | **Free** if structured right | Compiler handles it. Surprisingly effective for simple loops |

**Recommendation for our codebase (stable Rust)**:

1. **First, try auto-vectorization** — restructure loops to be compiler-friendly
   (sequential access, no branches in inner loop, contiguous buffers). The `rustc` optimizer
   can vectorize simple accumulation loops automatically with `-C target-cpu=native`.
2. **If auto-vectorization isn't enough**: `wide` crate for explicit SIMD on stable.
   It's the closest to `std::simd` and works on stable Rust.
3. **Avoid `std::simd`** — requires nightly, which is a significant maintenance burden.
4. **Consider `pulp`** — only if we need multiversioning (runtime CPU feature detection).
   Our server can assume modern x86, so this isn't needed.

**For our influence system specifically**: the inner loop is a simple accumulation
(`grid[x][y] += 1`) with a radius check. This is EXACTLY the pattern compilers
auto-vectorize well. Before adding a SIMD dependency, we should benchmark with:
```
RUSTFLAGS="-C target-cpu=native" cargo build --release
```
and check if `rustc` already vectorizes it (verify with `cargo-show-asm`).

### 3. Single `world_data_clone()` and Arc Removal

**YES — we can remove Arcs if we clone once per tick.**

Current pattern:
- Each system independently calls `world_data_clone()`, which clones `WorldBundle`
- `WorldBundle` contains `TerrainGrid { grid: Arc<Vec<Vec<Terrain>>> }`, `VisionGrids { grids: Arc<HashMap<...>> }`, `BorderGrids { grids: Arc<HashMap<...>> }`
- `Arc::clone()` is a cheap pointer bump (atomic refcount increment)
- This means each system gets its OWN `Arc` reference, so they don't conflict

With single clone:
- `tick_game()` clones `WorldBundle` ONCE → `Arc` pointer bumped once
- Systems receive `&WorldBundle` reference → **no Arc clone at all**
- `Arc` becomes unnecessary because there's only ONE owner

**However**, there's a subtlety: systems that write to the world (e.g., influence writes
new grids) still need to mutate `WorldBundle` via `world_data_mut()`. The pattern would be:

```rust
// tick_game():
let wb = self.world.world_data_clone(self.world_entity).unwrap(); // single clone, Arc bump
// ... pass &wb to read-only systems ...

// For systems that write back (influence, vision, border):
// They already use world_data_mut() which goes through query_one_mut,
// independent of the cloned wb. So Arc doesn't help them anyway.
```

**Verdict**: Arcs are needed for `world_data_clone()` to be cheap when called multiple
times. If we call it once, Arcs add unnecessary indirection (pointer chase) and atomic
refcount overhead. **Removing Arcs after single-clone refactor is a valid cleanup.**

Cost of Arc removal: need to `clone()` grids explicitly when systems write new values,
instead of `Arc::new()`. This is the SAME cost — `Arc::new(x)` allocates, `x.clone()`
also allocates. The Arc only saved us when `world_data_clone()` was called 12 times.

**Decision**: Arc removal is a Phase 1 followup. Low value (just removes indirection),
but worth doing for cleanliness once we have single clone.

---

### 4. Auto-Vectorization Benchmark Results (VERIFIED)

Built with `RUSTFLAGS="-C opt-level=3"` vs `RUSTFLAGS="-C target-cpu=native -C opt-level=3"`,
100 ticks, 20 units/city, seed 42:

| System | Without target-cpu=native | With target-cpu=native | Improvement |
|--------|--------------------------|----------------------|-------------|
| **influence** | 17.8ms | **12.6ms** | **-29%** |
| **combat** | 14.3ms | **11.9ms** | **-17%** |
| **attrition** | 7.0ms | **6.6ms** | **-6%** |
| **encirclement** | 6.6ms | **5.5ms** | **-17%** |
| **movement** | 5.2ms | **4.5ms** | **-13%** |

**Assembly analysis** (objdump of tick_game with target-cpu=native):
- 110 SIMD instructions found in tick_game function
- `vpaddd ymm` (256-bit AVX2 vector add) — 8× i32 accumulation
- `vmovdqu` (unaligned vector store) — grid writes
- Classic "vector reduce add" pattern: load → vpaddd → horizontal sum → scalar extract
- Influence grid accumulation loop IS being auto-vectorized with AVX2

**Root cause**: Default Rust target is x86-64 baseline (SSE2 only, 128-bit vectors).
`-C target-cpu=native` enables AVX2 (256-bit vectors, 8× i32 vs 4× i32), doubling
the vector width. The compiler then auto-vectorizes the accumulation loops.

**For deployment**: Use `-C target-cpu=x86-64-v3` for a portable binary that still
gets AVX2 on 95%+ of modern servers. See multiversioning research below for dynamic
dispatch analysis.

---

## Current Situation

hecs is providing archetypal storage (contiguous component iteration), but we're not
leveraging its full potential. We're doing manual work that the ECS could handle more
efficiently, and we're missing obvious parallelism.

### What hecs IS giving us
- **Archetypal storage**: `query::<(&A, &B)>()` iterates components in contiguous memory
- **Efficient spawn/despawn**: archetype swap, no HashMap lookups
- **Borrow-checked component access**: compile-time safety

### What we're doing manually (and paying for)

| Task | Current | Problem |
|------|---------|---------|
| Player filtering | Each system queries ALL units, filters by player | 13 systems × 480 unit iterations = 6,240 redundant filter ops |
| World data clone | Each system calls `world_data_clone()` independently | 12 clones per tick (Arc-cheap but unnecessary) |
| Unit queries | Each system does its own `world.query::<()>()` | Redundant archetype resolution per system |
| Parallelism | None — single-threaded | 0% CPU core utilization on multi-core machines |
| SIMD | None — scalar loops | Cache bandwidth underutilized in hot loops |
| Data grouping | Systems re-enumerate players, re-filter cities | Wasted work in influence, encirclement, combat |

---

## Proposal 1: Pre-group Units by Player (LOW EFFORT)

### Problem
Every system that needs per-player data does:
```rust
let players = world.query::<(&UnitOwner,)>().iter()
    .map(|(_, (o,))| o.player_id.clone())
    .collect::<HashSet<_>>().into_iter().collect();

for player_id in &players {
    let units = world.query::<(&UnitOwner, &UnitPosition)>()
        .iter()
        .filter(|(_, (owner, _))| &owner.player_id == player_id)
        .collect();
    // ... process ...
}
```
This iterates ALL units N times (once per system). With 13 systems and 480 units: **6,240 iterations**.

### Solution
In `tick_game()`, group units by player once:
```rust
pub struct TickContext {
    /// Entities grouped by player ID
    pub units_by_player: HashMap<String, Vec<hecs::Entity>>,
    /// Cached world data (cloned once)
    pub world_data: WorldBundle,
}

impl Game {
    fn build_tick_context(&self) -> TickContext {
        let mut units_by_player = HashMap::new();
        for (e, (owner,)) in self.world.query::<(&UnitOwner,)>().iter() {
            units_by_player.entry(owner.player_id.clone()).or_default().push(e);
        }
        let world_data = self.world.world_data_clone(self.world_entity).unwrap();
        TickContext { units_by_player, world_data }
    }
}
```

Systems receive pre-filtered entities:
```rust
// Before:
pub fn run(world: &mut World, world_entity: Entity) { ... }

// After:
pub fn run(world: &mut World, wb: &WorldBundle, entities: &[Entity]) { ... }
```

### Expected Savings
- Eliminates 12 redundant full-scan player enumeration queries
- Eliminates 12 redundant per-player filter iterations
- Reduces system signature from `(World, Entity)` to `(World, &WorldBundle, &[Entity])`
- **Est. 1-2ms/tick** saved from eliminated iterations + queries

### Risk
- Low. Pure refactor, no gameplay changes.
- Need to update 13 system signatures (mechanical change).
- Systems that mutate components still need `&mut World` access.

---

## Proposal 2: Single world_data_clone() per Tick (LOW EFFORT)

### Problem
Each system calls `world_data_clone()` independently. With Arc-wrapped grids this is
cheap (pointer bump), but still unnecessary work. More importantly, it creates
a pattern where systems are inconsistent — some clone, some don't.

### Solution
Clone once in `tick_game()`, pass reference to all systems:
```rust
fn tick_game(&mut self) {
    let wb = self.world.world_data_clone(self.world_entity).unwrap();
    let ctx = TickContext { /* from Proposal 1 */ };

    systems::combat::run(&mut self.world, &wb, &ctx.entities_for_player);
    systems::influence::run(&mut self.world, &wb, &ctx);
    // ...
}
```

### Expected Savings
- Eliminates 11 redundant clone calls per tick
- Consistent API: all systems receive `&WorldBundle`
- **Est. 0.5-1ms/tick** (small but free with Proposal 1)

### Risk
- Low. Combined with Proposal 1, this is a single architectural change.

---

## Proposal 3: Parallel Influence Computation with Rayon (MEDIUM EFFORT)

### Problem
Influence system iterates players sequentially. Each player's influence grid is
independent — perfect for parallelism. On a 4-core machine, we're using 25% of
available compute for this system.

### Current
```
Player 1: ████████████████████  (5ms)
Player 2: ████████████████████  (5ms)
Player 3: ████████████████████  (5ms)
... 9 more players ...
Total: ~60ms sequential
```

### Solution
```rust
// In tick_game():
use rayon::prelude::*;

let entities_by_player: Vec<_> = ctx.units_by_player.into_iter().collect();

let results: Vec<_> = entities_by_player.par_iter()
    .map(|(player_id, entities)| {
        // Each thread gets its own player data
        let cities = wb.cities.cities.iter()
            .filter(|c| c.owner.as_deref() == Some(player_id))
            .collect::<Vec<_>>();
        let (vision, influence) = compute_influence_for_player(
            &wb.terrain, &entities, &cities, /* ... */
        );
        (player_id.clone(), vision, influence)
    })
    .collect();

// Merge results back into world
for (player_id, vision, influence) in results {
    updated_vision.insert(player_id.clone(), vision);
    updated_border.insert(player_id, influence);
}
```

### Expected Savings
- 12 players on 4 cores: **~75% reduction** (60ms → ~15ms)
- 12 players on 8 cores: **~85% reduction** (60ms → ~9ms)
- Scales with core count

### Risk
- Medium. Need to add `rayon` dependency.
- `hecs::World` is not `Send` — can't mutate world from parallel threads.
  Must collect results first, then merge back in single thread.
- Grid allocation per player still happens (could pool to reduce alloc pressure).
- Rayon overhead for small workloads is negligible here (12 players × 5ms each).

---

## Proposal 4: Auto-Vectorization (LOW EFFORT) → Explicit SIMD if needed (MEDIUM EFFORT)

### Problem
Influence system's inner loop processes one cell at a time:
```rust
for dx in -max_radius..=max_radius {
    for dy in -max_radius..=max_radius {
        // 2-4 branches per cell (bounds, radius checks)
        if dx.abs() <= v_radius && dy.abs() <= v_radius {
            vision_grid[xusize][yusize] = true;
        }
        if dx.abs() <= i_radius && dy.abs() <= i_radius {
            influence_grid[xusize][yusize] += 1;
        }
    }
}
```
This is scalar — one cell per iteration. Modern CPUs can do 8-16 cells in parallel.

### Step 1: Compiler Auto-Vectorization (LOW EFFORT, TRY FIRST)
Restructure loops to be compiler-friendly:
- Flattened 1D access pattern instead of nested 2D loops
- No branches in inner loop (pre-compute radius bounds)
- Sequential memory access

Build with `RUSTFLAGS="-C target-cpu=native"` and verify with `cargo-show-asm` that
`rustc` emits SIMD instructions (look for `vadd`, `vbroadcast`, `vmul` with vector
registers like `ymm0`, `xmm0`).

**If auto-vectorization works**: zero code changes, zero dependencies, free speedup.
The Rust compiler is surprisingly good at vectorizing simple accumulation loops.

### Step 2: Explicit SIMD with `wide` crate (MEDIUM EFFORT, STABLE RUST)
If auto-vectorization isn't enough, use the `wide` crate (stable Rust, near drop-in
for `std::simd`):
```rust
use wide::*;

type SimdU32 = M256U32; // 256-bit = 8 x u32
type SimdI32 = M256I32;

// Process 8 cells at once
for source in &units {
    for y in 0..h {
        for x in (0..w).step_by(8) {
            let count = (w - x).min(8);
            let sx = SimdI32::splat(source.x as i32);
            let sy = SimdI32::splat(source.y as i32);
            // Vectorized radius check and accumulation
        }
    }
}
```

**Why `wide` instead of `std::simd`**:
- `std::simd` is STILL nightly-only as of Rust 1.97 (Oct 2026), requires `#![feature(portable_simd)]`
- `wide` is stable, supports NEON, WASM, all x86 instruction sets
- Near drop-in replacement for `std::simd` API
- No multiversioning (not needed — server can assume modern x86)

**Alternative: `pulp` crate** — if we need native-width SIMD that adapts to CPU at
runtime. Powers `faer` (proven performance). More complex API, limited arch support
(NEON, AVX2, AVX-512 only). Not worth the complexity for our use case.

### SIMD Decision Flow
```
Benchmark current → influence at 31ms (with throttle)
  ↓
Try: RUSTFLAGS="-C target-cpu=native"
  ↓
cargo-show-asm: does compiler emit SIMD?
  ↓ YES → measure speedup. If ≥2x, done.
  ↓ NO  → consider wide crate for explicit SIMD
  ↓
If parallelism (rayon) already reduces to ~15ms, SIMD may not be worth it
  → focus on reducing tick frequency (throttle to every 3 ticks) instead
```

---

## Research: Dynamic SIMD Dispatch (Multiversioning)

### The Question
Should we use runtime CPU feature detection to dispatch between AVX2, SSE2, etc.,
instead of (or in addition to) `-C target-cpu=native`?

### Options Compared

| Approach | Description | Pros | Cons |
|----------|-------------|------|------|
| `-C target-cpu=native` | Compile for the build machine's CPU | Simple, zero runtime overhead | Binary only runs on that CPU class. Doesn't work for distributed binaries. |
| `-C target-cpu=x86-64-v3` | Compile for AVX2 baseline (2013 Haswell) | Binary runs on 95%+ of modern servers. AVX2 gives 256-bit vectors. | Older servers (pre-2013) can't run it. No AVX-512. |
| `multiversion` crate | Runtime dispatch: compile multiple versions, select at startup | One binary, best perf on each CPU. Dispatch overhead ≈ 0 after first call. | More complex build. Indirect call overhead per invocation (~1-2ns). |
| `std::is_x86_feature_detected!` | Manual runtime check + `#[target_feature]` | No dependency, full control | Manual, error-prone, unsafe blocks |

### x86-64 Microarchitecture Levels

| Level | SIMD Features | CPU Era | Server Coverage |
|-------|--------------|---------|-----------------|
| v1 (baseline) | SSE, SSE2 | 2003 (Athlon 64) | 100% |
| v2 | SSE3, SSE4.1, SSE4.2 | 2007 (Penryn) | ~99% |
| **v3** | **AVX, AVX2, FMA** | **2013 (Haswell/Zen)** | **~95%+** |
| v4 | AVX-512F, AVX-512VL | 2017 (Skylake-X) | ~20% (server CPUs) |

**Key insight**: x86-64-v3 (AVX2) is the sweet spot. It's been 13 years old, supported
by all modern servers, and gives us 256-bit vectors (8× i32). AVX-512 (v4) is only on
high-end server CPUs and has thermal throttling issues.

### The `multiversion` Crate (Runtime Dispatch)
- **264 stars**, actively maintained, stable Rust, zero unsafe
- API: `#[multiversion(targets("x86_64+avx2", "x86_64+sse4.1"))] fn my_func() { ... }`
- **Dispatch model**: First call does CPU feature detection (static atomic). Subsequent
  calls are either direct jump (elided if compile-time known) or indirect call (~1-2ns).
- **Overhead**: Indirect dispatch adds ~1-2ns per call. For a function that takes
  0.1ms (100,000ns), this is 0.002% overhead — completely negligible.
- **Limitations**: No `self`/`Self`, no `impl Trait` returns. Works with plain functions.

### Recommendation for Our Game Server

**For this project: `-C target-cpu=x86-64-v3` is the best approach.**

Reasoning:
1. Our server runs on known hardware (our own servers, user's machines)
2. x86-64-v3 (AVX2) covers 95%+ of modern servers and gives us 256-bit vectors
3. Multiversion adds complexity for negligible benefit — our hot functions run
   for 0.1-1ms per call; the 1-2ns indirect call overhead is irrelevant
4. `-C target-cpu=native` is great for dev/benchmark but produces non-portable binaries
5. If we ever distribute a public binary, `-C target-cpu=x86-64-v3` is the safe bet

**Multiversion would only be worth it if**:
- We're distributing to unknown/legacy hardware (VMs, old servers)
- We want a single binary that runs optimally on everything
- The explicit SIMD code is complex enough to justify the dispatch machinery

For now, auto-vectorization with AVX2 gives us 29% improvement on influence for free.
The bigger wins are architectural (pre-grouping, parallelism), not SIMD width.

### Expected Savings
- Auto-vectorization: **2-4x** on the inner loop (if compiler picks it up)
- Explicit SIMD: **8x lane width** → influence 60ms → ~10ms
- Diminishing returns if combined with Proposal 3 (parallel × SIMD)
- Combined: 60ms → ~3ms (parallel 4x + SIMD 8x = theoretical 32x)

### Risk
- **Step 1 (auto)**: Zero risk. If it works, we save the effort of Step 2.
- **Step 2 (wide)**: Medium. Need to handle edge cases (grid boundaries, partial lanes).
  May not be worth the complexity if parallelism (Proposal 3) already reduces to ~15ms.
- Benchmarking required to validate actual speedup. SIMD doesn't always help with branching.

---

## Proposal 5: Batch Component Writes (HIGH EFFORT)

### Problem
Systems mutate components one at a time:
```rust
for entity in &entities {
    if let Ok(mut flags) = world.query_one_mut::<(&mut UnitFlags,)>(entity) {
        flags.get_mut().fighting = true;
    }
}
```
Each `query_one_mut` involves a borrow check and archetype lookup. With hundreds of
units, this adds up.

### Solution
Batch writes using `bulk_insert` or direct archetype access:
```rust
// Collect all mutations
let mut flag_changes: Vec<(Entity, bool)> = Vec::new();
// ... during system logic ...

// Apply all at once
for (entity, fighting) in flag_changes {
    world.insert_one(entity, UnitFlags { fighting, .. })
        .ok();
}
```

### Expected Savings
- Eliminates per-entity query overhead in hot systems (combat, movement)
- **Est. 1-2ms/tick** (small, mostly in combat with frequent flag toggles)

### Risk
- High. Requires restructuring system logic to defer writes.
- May conflict with systems that need to read recently-written values.
- hecs doesn't have a true bulk API — still per-entity `insert_one` calls.

---

## Implementation Order

| Priority | Proposal | Effort | Est. Savings | Risk |
|----------|----------|--------|--------------|------|
| 1 | Pre-group units by player | 2 hours | 1-2ms/tick | Low |
| 2 | Single world_data_clone + Arc removal | 1 hour | 0.5-1ms/tick | Low |
| 3 | Parallel influence (rayon) | 2-3 hours | 45ms/tick → 15ms | Medium |
| 4 | Auto-vectorization (`-C target-cpu=native`) | 1-2 hours | 0-5ms/tick | None |
| 5 | Explicit SIMD (`wide` crate, if Step 4 isn't enough) | 3-4 hours | 5-12ms/tick | Medium |
| 6 | Batch component writes | 4+ hours | 1-2ms/tick | High |

### Phase 1: Foundation (Proposals 1-2) + Scoped Arc Extraction (✅ COMPLETE)
- Create `TickContext` struct with pre-grouped data
- **Scoped Arc extraction**: systems extract ONLY the Arc-wrapped field they need (24 bytes),
  not the full WorldBundle (130KB). `hecs::Ref` dropped immediately via scoped match.
- **Slim TickContext**: only holds cities (~20 entries) + funds (~12 entries) + dimensions.
  Grids (terrain/vision/borders) accessed via scoped Arc extraction in systems.
- **Removed useless clones**: water.rs and reinforce.rs had `world_data_clone()` that
  cloned 130KB for completely unused data. Eliminated entirely.
- Arc on ALL grids KEPT (terrain 10KB, vision 60KB, borders 60KB).
  Essential for cheap `Arc::clone()` in scoped extraction.
- **Result: -16% total wall time (798ms → 670ms per 1500 ticks)**
- **Effort: 1 hour**

### Phase 1.5: Cleanup & TickContext Propagation (✅ DONE)
- **Minor fixes:**
  - `combat.rs` clear_moved: removed unused `UnitOwner` from query — smaller archetype match
  - `surrender.rs`: removed unused `UnitOwner` from query + unused `world_data_clone()` call
- **TickContext propagation to healing/supply:**
  - Restructured to per-player iteration using `ctx.units_by_player`
  - Each system only checks player's units against player's cities (no owner filter in inner loop)
  - Eliminated ~480 per-entity `world.get()` calls per system
  - City distance checks: 2,880 → 240 per system per tick (92% reduction)
  - TickContext now built every tick (not just 57%) — trivial 0.010ms overhead
- **Debug branch consolidation:**
  - Merged duplicate debug/non-debug branches in `tick_game()` into single code path
  - Uses `#[cfg]` around timing calls instead of duplicating entire system dispatch
  - Eliminated ~120 lines of duplicate code
- **Total est. savings: ~1-2ms/tick**
- **Effort: ~30 min**

### Phase 2: Parallelism (Proposal 3) — ❌ REJECTED
**Rayon experiment (Dec 2025): Tested 3 parallelization strategies, ALL showed regressions.**

| Strategy | Result | Cause |
|----------|--------|-------|
| Per-player (`par_iter` over 12 players) | 87ms → 241ms (+175%) | 12 tasks × 0.1ms each — below Rayon dispatch breakeven (~5μs/task) |
| Per-player with chunks of 4 | 87ms → 266ms (+205%) | Chunking overhead > parallelism benefit |
| Per-row (`par_iter` over 100 grid rows) | 87ms → 2970ms (+3343%) | 100 rows × 775 calls = 77,500 tasks, massive dispatch overhead |

**Key findings:**
- Influence per-player = ~0.1ms (113μs) — too small for Rayon on 32-core system
- Rayon's work-stealing overhead scales with core count (32 cores = high contention)
- Tokio worker threads are already parallel; adding Rayon creates thread pool conflict
- **Conclusion: Algorithmic optimization > parallelization for this workload size**

**Alternative paths if influence needs more speed:**
1. Reduce influence frequency (every 3-4 ticks instead of 2)
2. Reduce grid resolution (downsample from 100×100 to 50×50, interpolate)
3. Use SIMD via `wide` crate for the circle projection inner loop
4. Switch to multi-source BFS (rejected earlier, was 4-5x slower)

### Phase 3: Auto-Vectorization (Proposal 4 Step 1)
- Build with `-C target-cpu=native`, verify with `cargo-show-asm`
- If compiler already vectorizes → done, free win
- If not → restructure loops for friendlier access patterns
- **Total est. savings: 0-5ms/tick** (depends on compiler)
- **Effort: 1-2 hours**

### Phase 4: Explicit SIMD (Proposal 4 Step 2, IF NEEDED)
- Add `wide` crate dependency (stable Rust)
- Vectorize influence inner loop
- Benchmark to validate actual speedup
- **Total est. savings: additional 5-12ms/tick**
- **Effort: 3-4 hours**
- **Only if Phase 3 + Phase 2 still leaves influence as bottleneck**

---

## Key Architectural Notes

### hecs World is NOT `Send`
Can't pass `&mut World` to rayon threads. Must:
1. Collect all read data in main thread
2. Process in parallel (pure computation, no world access)
3. Merge results back in main thread

This matches the pattern described in hecs issue #258 and used by Bevy ECS.

### Grid Allocation Strategy
Influence system allocates `vec![vec![T; h]; w]` per player per tick. With 12 players
on a 100×100 map: **24 grid allocations/tick**. Could pool these grids to reduce alloc
pressure, but allocator is fast enough that this is premature optimization.

### Borrow Checker Reality
hecs borrow checker is strict. Can't hold `&WorldBundle` (immutable) while doing
`world.insert_one()` (mutable). Pattern:
1. Read all data needed → `world_data_clone()` or `TickContext`
2. Do computation (no world access)
3. Write results back → `world.insert_one()` or `world_data_mut()`

### Current Overhead Measurement
From benchmark: **0.038ms avg overhead** (spawn/query/ECS). This is tiny. The
bottlenecks are algorithmic (influence circle projection at 31ms, combat at 33ms),
not ECS overhead.

### Arc Removal Rationale
Currently `TerrainGrid::grid`, `VisionGrids::grids`, and `BorderGrids::grids` are
wrapped in `Arc` so that `world_data_clone()` is cheap (pointer bump). Once we clone
once per tick and pass `&WorldBundle` to systems, there's no shared ownership — Arc
is unnecessary. Removing it eliminates:
- One level of pointer indirection (better cache locality)
- Atomic refcount overhead on clone (no longer needed)
- Mental complexity of wondering when Arc::make_mut is needed

Cost: systems that write new grids must `Vec::new()` instead of `Arc::new()`.
Same allocation cost, no Arc wrapper.

### SIMD Strategy
**`std::simd` is nightly-only** (confirmed as of Rust 1.97, Oct 2026).
For stable Rust, the hierarchy is:
1. Auto-vectorization (`-C target-cpu=native`) — free, try first
2. `wide` crate — stable, near drop-in for `std::simd`
3. `pulp` crate — stable, native-width, powers `faer`
4. `std::arch` intrinsics — stable since 1.87, most painful

For our server workload (assumed modern x86), auto-vectorization or `wide` is sufficient.
No need for multiversioning (runtime CPU detection).
