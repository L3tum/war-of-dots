# Deep Plan: HECS Optimization Analysis for War of Dots

## Problem Statement

Four HECS optimization techniques (QueryBatch/BatchedIter, PreparedQuery/QueryBuilder, Archetype Iteration, hecs_ptr) were proposed for the War of Dots game server. This analysis determines whether these techniques apply to this codebase, provide measurable savings, or represent optimization theater.

## Context

This analysis was prompted by the desire to squeeze more performance from the game server tick loop. The benchmark configuration (200×200 map, 12 cities, 4 players, default ~20 units/city = ~240 units, scalable via CLI) produces measurable tick times, with combat resolution as the dominant system. The question is whether low-level HECS tuning techniques can materially improve tick performance, or if the actual bottlenecks lie elsewhere.

The codebase uses **hecs 0.10.5** (see `Cargo.toml`). The tick loop in `src/game/game.rs::tick_game()` dispatches systems in this order: movement → spatialty → combat → supply → city_capture → influence → encirclement → surrender → death_cleanup. All systems already use pre-allocated scratch buffers with capacity reuse (`CombatScratch`, `MovementScratch`).

Notably, the codebase already benefits from several optimization wins documented in `plans/PLAN_HECS_OPTIMIZATIONS.md`:
- **Auto-vectorization** (`-C target-cpu=native`): 29% speedup on influence, 17% on combat, 13% on movement
- **Scoped Arc extraction**: systems extract only the Arc-wrapped field they need, not the full 130KB WorldBundle
- **TickContext**: pre-grouped `units_by_player` HashMap built once per tick, eliminating redundant player enumeration
- **Rayon was explicitly tested and rejected** for the influence system: 87ms → 241ms (+175%) due to dispatch overhead

These existing optimizations make the four proposed ECS techniques even less relevant.

## Technique Analysis

### 1. QueryBatch / BatchedIter

**What it is:** HECS 0.10 exposes `BatchedIter` (via `iter_batched()`), which yields `Batch` structs containing groups of entities grouped by archetype. The HECS documentation describes batched iteration as useful for distributing work over a threadpool.

**Does it exist in hecs 0.10?** Yes — `BatchedIter` is available in the hecs 0.10.5 API.

**Does it apply to this codebase?** **Not directly, but there's an important adjacent optimization.**

**The real win here is not `collect`ing.** Every system currently materializes its entire query result into a Vec before processing:
   ```rust
   // combat.rs
   let candidates: Vec<(...)> = world
       .query::<...>()
       .iter()
       .filter(|...| ...)
       .map(|...| ...)
       .collect();
   ```
   This allocates a Vec and clones/copies all components upfront. For systems that could **stream-process entities** (iterate without collecting), the query iterator itself already provides zero-copy, cache-friendly sequential access over archetype columns. **Not calling `.collect()` IS an optimization** — it avoids the Vec allocation and the clone overhead, processing entities as they stream from the archetype.

**However, not all systems can stream:**
- **Combat** fundamentally needs random access — it resolves pairwise engagements where entity A fights entity B, requiring lookups back into other entities' components. Even with batched iteration, the inner loop still does `world.get::<&T>(entity2)` for every pair. Batching doesn't help random-access patterns.
- **Movement and Supply** already use a two-phase pattern (query → compute → apply mutations) that naturally collects changes. They could theoretically stream the read phase, but the mutation phase requires collecting changes first to avoid borrow conflicts.

**Multithreading across systems is theoretically possible** (e.g., running independent systems in parallel), but:
- The influence system explicitly tested Rayon and got regressions (87ms → 241ms) due to dispatch overhead on small tasks
- The tick loop has data dependencies between systems (spatialty must run before combat, combat must run before supply), limiting parallelism
- HECS `World` is not `Send`, so any parallelism must collect data first, process in parallel, then merge back

**Why TickContext doesn't help here:** `TickContext` pre-groups units by player ID, which eliminates redundant owner filtering across systems. But combat's bottleneck is random entity-to-entity component lookups for pair resolution — not player filtering. Even with pre-grouped entities, each pair still needs `world.get::<&T>(entity2)` to fetch the target's components.

**Verdict:** THE `BatchedIter` API itself doesn't help (combat needs random access, other systems need to collect changes), BUT **streaming without `.collect()` IS a valid optimization** for systems that can process entities sequentially. The combat system cannot do this (random access), but movement/supply could potentially stream their read phase.
**Estimated savings from not-collecting (where possible):** 2-5% per system (eliminates Vec allocation + clone overhead)

---

### 2. PreparedQuery / QueryBuilder

**What it is:** `PreparedQuery` lets you construct a query's component signature ahead of time and "prepare" it, theoretically amortizing the cost of matching the query against archetypes across multiple invocations.

**Does it exist in hecs 0.10?** Yes — `hecs::PreparedQuery` and `hecs::QueryBuilder` are available.

**Does it apply to this codebase?** **No.** Three independent reasons:

1. **Query setup is O(number of archetypes) = O(3).** Every time you call `world.query::<...>().iter()`, HECS iterates over the archetype storage to find matching archetypes. With ~3 archetypes in this codebase (see §3), this is a sub-microsecond operation — a handful of `TypeId` comparisons and pointer lookups.

2. **The benchmark overhead data confirms this.** The benchmark (`src/benchmark.rs`) computes an "overhead" metric: `tick_ms - sum(system_ms)`, representing archetype rebuild, query setup, entity spawning, and debug guard overhead. Historical benchmarks show this overhead averages **~0.038ms** across the full benchmark run. PreparedQuery would only save the archetype-matching portion within that budget (nanoseconds, not microseconds).

3. **Code complexity for negligible gain.** Every system in `src/game/systems/` uses the simple `world.query::<...>().iter()` pattern. Switching to `PreparedQuery` would require:
   - Storing the `PreparedQuery` on the `Game` struct or in a scratch buffer
   - Changing every `run()` signature to accept the prepared query
   - Re-preparing if component sets change (archetype churn)
   ...all for an estimated 0-5 μs savings per tick spread across all systems.

**Verdict:** DOESN'T MAKE SENSE (THEATER)
**Estimated savings:** 0-5 μs per tick total

---

### 3. Archetype Iteration

**What it is:** The idea that manually iterating over a specific archetype's column arrays (bypassing the query iterator) would be faster than using `world.query::<...>().iter()`.

**Does it exist in hecs 0.10?** Partially. HECS exposes `ArchetypeColumn` and `ArchetypeColumnMut` internally, but they are **not public API** in hecs 0.10. Direct archetype access would require `unsafe` code.

**Does it apply to this codebase?** **No, and even if it did, you're already optimal.**

1. **All units share ONE archetype in steady state.** The component signature is fixed at spawn time (`src/game/world_ext.rs::spawn_unit()`):
   ```
   UnitOwner, UnitTypeId, UnitDisplay, UnitPosition, UnitPath,
   UnitStats, UnitEquipment, UnitFlags, UnitWaterTransition
   ```
   After spatialty adds `NearbyEnemies`, the steady-state signature is 10 components. In steady state (all units initialized), every unit has the exact same component set. This means the query iterator already iterates over a single archetype's contiguous arrays — the theoretical minimum.

2. **The query iterator IS archetype iteration.** `world.query::<...>().iter()` in HECS internally iterates matching archetypes, then for each archetype, iterates its contiguous component arrays. With one matching archetype, this IS archetype-level iteration. There is no performance gap between the query API and manual archetype access.

3. **Transient archetypes are harmless.** Fresh spawns (without `NearbyEnemies`) create a second archetype for exactly one tick. The comment in `spawn_unit` is explicit: "NOTE: no NearbyEnemies at spawn — its ABSENCE marks the unit as 'uninitialized'". Systems that query units with `NearbyEnemies` (combat) simply skip these entities via the filter. Systems that query all units (movement, supply) match both archetypes — but the second archetype contains at most a handful of entities per tick.

**Archetype inventory:**

| Archetype | Components | Entity Count |
|-----------|-----------|-------------|
| WorldBundle | 1 | 1 |
| Units (uninitialized, 1 tick) | 9 | 0-N (transient) |
| Units (initialized, steady state) | 10 | N-0 |

Total: 3 archetypes.

**Verdict:** ALREADY OPTIMAL
**Estimated savings:** 0 ns

---

### 4. hecs_ptr

**What it is:** A hypothetical crate or technique for raw pointer access to HECS component data, bypassing the borrow-checked API.

**Does it exist in hecs 0.10?** **No. This crate does not exist.** A search of crates.io shows no `hecs_ptr` crate. This appears to be vaporware or confusion with HECS's internal `ArchetypeColumn` / `ArchetypeColumnMut` types.

**Why it's dangerous even if it existed:**

1. **HECS already provides zero-copy access.** The `world.query::<...>().iter()` API returns references (`&T`) to component data in-place. There is no copying involved — the `hecs::Ref` and `&mut` returned by `world.get()` are zero-cost wrappers around archetype column pointers with borrow-checking enforcement.

2. **Raw pointers would be unsound.** HECS's archetype storage is not stable:
   - Entities can migrate between archetypes when components are added/removed (e.g., `NearbyEnemies` being added by spatialty)
   - Archetype columns can be reallocated during entity spawning
   - The borrow checker in HECS's API prevents use-after-free and data races

   Bypassing the borrow-checked API with raw pointers would break these invariants. A pointer obtained before `spatialty::run()` adds `NearbyEnemies` to some entities would be invalidated when those entities migrate archetypes.

3. **The combat system already does `world.get()` for random access** — which is the correct HECS API pattern returning a `hecs::Ref<'_, T>` with proper lifetime tracking.

**Verdict:** DOESN'T EXIST (VAPORWARE)
**Estimated savings:** N/A — attempting this would introduce memory safety bugs, not performance gains

---

## Summary Table

| Technique | Exists in hecs 0.10? | Applies to this codebase? | Estimated Savings | Verdict |
|---|---|---|---|---|
| QueryBatch / BatchedIter | ✅ Yes | ⚠️ Nuanced — `BatchedIter` API doesn't help (combat needs random access), but streaming without `.collect()` IS a valid optimization | 2-5% (streaming) | ⚠️ Partial |
| PreparedQuery / QueryBuilder | ✅ Yes | ❌ No — O(3) archetype matching, sub-μs overhead, benchmark confirms | 0-5 μs/tick | 🎭 Theater |
| Archetype Iteration | ⚠️ Internal only | ❌ Already optimal — single steady-state archetype, query iter = archetype iter | 0 ns | ✅ Already done |
| hecs_ptr | ❌ Doesn't exist | ❌ N/A — vaporware; unsafe even if it existed | N/A | 🚫 Vaporware |

---

## Real Bottlenecks

The actual performance costs in this codebase are **not** at the ECS storage/iteration layer. They are in the game logic patterns. The percentages below are estimates based on code analysis — confirm with `--bench-detailed=combat,movement,spatialty,supply` before investing time.

### 1. Combat: Per-pair `world.get()` random access — **est. 20-50% of combat time**

**File:** `src/game/systems/combat.rs` (inner loop, approximate lines 210-310)

For each (entity1, entity2) candidate pair, the combat system performs ~12 `world.get::<&T>(entity)` calls:

```rust
// Per pair, approximately:
world.get::<&UnitOwner>(entity2)         // owner check
world.get::<&UnitPosition>(entity2)      // position check
world.get::<&UnitStats>(*entity1)        // re-fetch current stats (entity1)
world.get::<&UnitFlags>(*entity1)        // re-fetch current flags (entity1)
world.get::<&UnitStats>(entity2)         // stats (entity2)
world.get::<&UnitFlags>(entity2)         // flags (entity2)
world.get::<&UnitEquipment>(*entity1)    // equipment (entity1, multiple calls)
world.get::<&UnitEquipment>(entity2)     // equipment (entity2, multiple calls)
world.get::<&UnitTypeId>(*entity1)       // unit type (entity1)
world.get::<&UnitTypeId>(entity2)        // unit type (entity2)
```

Each `world.get()` call is an indirect lookup: entity ID → entity table entry → generation check → archetype lookup → column offset → component reference. With ~240 units and potentially dozens of candidate pairs per unit, this is **O(N×K) lookups** where K is the average `NearbyEnemies` list length.

**Why this is expensive:** These are not cache-friendly sequential accesses. Each lookup traverses multiple levels of indirection, and the `entity2` targets are scattered across the combat candidate set. The candidate collection phase already pre-fetches `entity1`'s components, but the inner loop still needs `world.get()` for `entity2`.

### 2. Combat: HashSet allocations for deduplication — **est. 5-15%**

**File:** `src/game/systems/combat.rs` (lines 15-40)

The `CombatScratch` struct holds three `HashSet` instances:

```rust
pub struct CombatScratch {
    resolved: HashSet<(hecs::Entity, hecs::Entity)>,  // dedup pairs
    fought: HashSet<hecs::Entity>,                     // which units fought
    dead_set: HashSet<hecs::Entity>,                   // quick dead lookup
    dead: Vec<hecs::Entity>,                           // dead units list
    // ...
}
```

While `.clear()` preserves capacity (good), the per-tick insertion and lookup operations on these HashSets still involve hash computation per call. For ~240 units with dense combat fronts, these sets can reach hundreds of entries.

### 3. Movement + Supply: `query_one_mut` per entity in the apply phase — **est. 2-5%**

**Files:** `src/game/systems/movement.rs` (approx. lines 115-151), `src/game/systems/supply.rs` (approx. lines 157-175)

Both use a two-phase pattern: query all → compute changes → apply per-entity via `query_one_mut`:

```rust
// movement.rs
if let Ok((mpos,)) = world.query_one_mut::<(&mut UnitPosition,)>(entity) {
// supply.rs
if let Ok((ms,)) = world.query_one_mut::<(&mut UnitStats,)>(entity) {
```

Each `query_one_mut` is an O(1) but non-trivial lookup. Movement calls `query_one_mut` up to 4x per changed entity (Position, Path, Stats, Flags); supply calls it up to 3x (Stats, Equipment, WaterTransition).

### 4. Clone overhead in combat candidate collection — **est. 2-5%**

**File:** `src/game/systems/combat.rs` (approx. lines 150-170)

The combat system collects candidates with clones:

```rust
.map(|(e, (pos, owner, flags, stats, nearby))| {
    (
        e,
        *pos,                          // Copy — fine
        owner.player_id.clone(),       // String clone — allocates
        *stats,                        // Copy — fine
        *flags,                        // Copy — fine
        nearby.clone(),                // NearbyEnemies clone (Vec<Entity>) — allocates
    )
})
.collect();
```

With ~240 candidates, that's ~240 String clones and ~240 Vec clones per tick. Similar cloning occurs in spatialty (`nearby.entities.clone()` per mover) and in the retreat check.

### 5. Supply: Full `UnitEquipment` clone per unit — **est. 1-3%**

**File:** `src/game/systems/supply.rs`

```rust
let mut eq_copy = (*equipment).clone();
```

Every unit's equipment is cloned at the start of the supply pass for mutation. With multiple equipment slots per unit, this is a `Vec<Equipment>` clone per unit.

---

## Recommendation

**Do not pursue any of the four proposed ECS techniques.** The ECS layer is already at the theoretical minimum for this workload. The four proposed optimizations target a problem that doesn't exist — query iteration overhead is ~0.038ms/tick, while combat alone takes orders of magnitude more.

Instead, if tick time reduction is still needed, focus on these actionable optimizations — **but only after confirming they matter via the detailed benchmark**.

### What to measure first

**Option A: Existing detailed benchmark.** Run the benchmark with `--bench-detailed=combat` to get sub-part timing data. The detailed timer infrastructure already exists (`src/game/debug_stats.rs`, `DetailedTimer`). This will tell you which of the bottlenecks above actually dominate within the game tick loop.

**Option B: Criterion microbenchmarks (recommended addition).** The existing benchmark measures whole-tick timing. To isolate per-system or per-function costs (e.g., "how much does `world.get::<&UnitStats>(entity)` cost in the inner loop?"), add Criterion benchmarks:

```rust
// Criterion bench for combat inner-loop patterns
#[cfg(test)]
mod benches {
    use criterion::{black_box, criterion_group, criterion_main, Criterion};

    fn bench_world_get(c: &mut Criterion) {
        c.bench_function("world.get<UnitStats>", |b| {
            b.iter(|| world.get::<&UnitStats>(entity).unwrap())
        });
    }
    fn bench_hashmap_get(c: &mut Criterion) {
        c.bench_function("HashMap get<UnitStats>", |b| {
            b.iter(|| hashmap.get(&entity).unwrap())
        });
    }
}
```

Criterion gives statistically rigorous comparisons (median, IQR, confidence intervals) that the existing wall-clock benchmark doesn't provide. This would definitively answer: "is a HashMap lookup faster than a `world.get()` in the hot path?"

**Add Criterion as a dev dependency:**
```toml
[dev-dependencies]
criterion = "0.5"

[[bench]]
name = "ecs_benches"
harness = false
```

This complements the existing benchmark rather than replacing it — use Criterion for micro-benchmarks (per-function, per-lookup) and the existing benchmark for macro-benchmarks (whole-tick, game simulation).

### Priority 1: Pre-group combat component data

**Problem:** ~12 `world.get()` calls per pair in the O(N×K) inner loop.

**Approach:** During candidate collection, also fetch `entity2`'s components and build a `HashMap<hecs::Entity, CombatData>` lookup. This trades O(N) HashMap construction for O(1) HashMap lookups instead of O(archetype-traversal) ECS lookups in the inner loop.

**Why it works:** HashMap lookups are cache-friendly (single hash table probe) vs. ECS lookups (entity table → archetype → column → component). For the inner loop, which runs O(N×K) times, even a modest per-lookup saving compounds significantly.

**Risk:** Adds allocation for the HashMap. Mitigate with a pre-allocated scratch buffer that reuses capacity across ticks (same pattern as `CombatScratch`).

### Priority 2: Replace HashSet with sorted Vec for combat dedup

**Problem:** Hash computation and potential collision resolution on `HashSet` per pair.

**Approach:** Use a sorted `Vec<(u64, u64)>` (entity IDs sorted within each pair) with binary search for `resolved` dedup. For `fought` and `dead_set`, consider a `Vec<bool>` indexed by entity generation or a bitmap.

**Why it works:** Binary search on a sorted Vec is deterministic O(log N) with no hash computation, no allocator pressure, and better cache locality.

### Priority 3: Eliminate String clones in combat

**Problem:** `owner.player_id.clone()` per candidate allocates a String.

**Approach:** Use a player ID integer (0..N_players) stored as `UnitOwnerId: u8` instead of a String. The `TickContext` already groups units by player — this mapping exists.

**Why it works:** `u8` is Copy, eliminating the allocation entirely. The string-based player ID is only needed for serialization/API. Internal game logic should use integers.

### Priority 4: Acknowledge `-C target-cpu=native` as the highest-leverage optimization already in place

The `plans/PLAN_HECS_OPTIMIZATIONS.md` document records that `-C target-cpu=native` already delivers:
- **29% speedup** on influence (17.8ms → 12.6ms)
- **17% speedup** on combat (14.3ms → 11.9ms)
- **13% speedup** on movement (5.2ms → 4.5ms)

This single compiler flag outperforms all four proposed ECS techniques combined. Ensure it's used in release builds (or `-C target-cpu=x86-64-v3` for portability).

### Priority 5: Add Criterion microbenchmarks for isolated performance testing

**Problem:** The existing benchmark measures whole-tick timing. It can't answer questions like "is a HashMap lookup faster than `world.get()` in the inner loop?" or "does eliminating `.collect()` actually save measurable time?"

**Approach:** Add Criterion as a dev dependency with micro-benches for the hot-path operations:
- `world.get::<&T>(entity)` vs. `HashMap<Entity, T>` lookup
- `.collect()` vs. streaming iteration (for systems that can stream)
- HashSet insert/contains vs. sorted Vec binary search (for dedup)

**Why it works:** Criterion provides statistically rigorous comparisons (median, IQR, confidence intervals) across thousands of iterations, eliminating noise from whole-tick measurements. This is essential for validating whether the Priority 1-3 optimizations actually help before investing implementation effort.

**Add to `Cargo.toml`:**
```toml
[dev-dependencies]
criterion = "0.5"

[[bench]]
name = "ecs_benches"
harness = false
```

### Priority 6: Stream query iteration where possible (avoid `.collect()`)

**Problem:** Every system materializes its query into a Vec via `.collect()`, then processes the Vec. This allocates memory and clones/copies components upfront, even for systems that could process entities sequentially.

**Approach:** For systems that can process entities one-by-one without needing to look ahead or random-access other entities, replace the `.collect()` pattern with direct iteration:
```rust
// Before (allocates Vec, clones all):
let candidates: Vec<(...)> = world.query::<...>().iter().filter(...).map(...).collect();
for candidate in candidates { ... }

// After (streams, no Vec allocation):
for (entity, (pos, owner, stats, flags, nearby)) in world.query::<...>().iter() {
    if !filter_condition(entity, ...) { continue; }
    // process directly, no owned clone needed
}
```

**Why it works:** Eliminates Vec allocation + clone overhead per system per tick. For systems processing 240+ entities, this is a free 2-5% saving with minimal code changes.

**Risk:** Low — this is a mechanical refactor. Systems that need random access (combat) or need to collect changes for mutation (movement, supply) cannot do this and should be excluded.

### Priority 7: Reusable `unit_types_index` pattern (APPLIED to supply + combat; backlog for other systems)

**What it is:** Several systems did a per-tick `HashMap` build keyed on unit-type name
(`HashMap<&str, &UnitType>` / `HashMap<&str, f32>`) just to turn a unit's `UnitTypeId.name`
into config fields. That rebuild is pointless — `Game.settings.unit_types` is fixed for the
lifetime of a `Game`. The fix is a **dense index built once** and threaded into each system:

```rust
// Game::new (built once, before `settings` is moved into Self):
let unit_types_index: HashMap<String, usize> = settings
    .unit_types
    .iter()
    .enumerate()
    .map(|(i, ut)| (ut.name.clone(), i))
    .collect();
```

Each system's `run()` then takes `unit_types_index: &HashMap<String, usize>` and indexes
straight into `settings.unit_types[i]` per unit — no per-tick map build:

```rust
let ut = unit_types_index.get(&utype.name).map(|&i| &settings.unit_types[i]);
let rate = ut.map(|u| u.fighting_attrition).unwrap_or(1.0); // or .speed / .base_attrition / ...
```

**Status:**
- ✅ **APPLIED — `supply.rs`** (`run()` takes the index; per-unit `base_attrition` /
  `reinforcement_rate` / max-org lookups). Replaced the per-tick `HashMap<&str,&UnitType>`.
- ✅ **APPLIED — `combat.rs`** (`run()` takes the index; per-engagement `fighting_attrition`
  lookup, ~O(N×K) in the hot loop). Replaced the per-tick `HashMap<&str,&UnitType>`.
- ⬜ **NEXT CANDIDATE — `movement.rs`** builds `unit_type_speeds: HashMap<&str, f32>` every
  tick (line ~65). Thread the same index through and use `settings.unit_types[i].speed`.

**Perf note:** In isolation this is **perf-flat** — a 3-entry `HashMap` build per tick is
already tiny and the microbenchmark showed no measurable delta (see
`implementation-report-20260814-090000.md`). It is worth doing **opportunistically** as part
of touching a system (it's a strict simplification, removes per-tick allocation, and shrinks
the per-unit lookup), **not** as a standalone optimization. Keep it in mind when revisiting
any system that currently builds a per-tick name-keyed map.

**Safety invariant (why the cached index can't go stale):** `Game.settings` is a **private**
field, written only in `Game::new`, and only ever read afterward (`fn settings(&self)` returns
`&GameSettings`; there is no `&mut self.settings` and no reassignment). So
`settings.unit_types` is immutable for a `Game`'s lifetime and the index stays valid. Every
`run()` call site pairs the index with the same `settings` it was built from. (If `Game`
ever gains a `set_unit_types`-style mutator, the index must be rebuilt in it.)

**Microbenchmark plumbing note:** `microbenchmarks.rs::bench_systems()` builds the index once
in un-timed setup and passes it to both the supply and combat closures, mirroring `Game`.

### What NOT to do

- **Don't add `PreparedQuery`** — adds code complexity for nanosecond savings
- **Don't chase archetype layout** — you're at the theoretical minimum
- **Don't install non-existent crates** — `hecs_ptr` doesn't exist
- **Do consider streaming (not-collecting)** — for systems that can process entities sequentially without collecting, this is a free optimization over the current `.collect()` pattern

## Files Examined

| File | Purpose | Key Findings |
|---|---|---|
| `Cargo.toml` | Dependencies | hecs = "0.10" |
| `src/game/components.rs` | ECS component definitions | 10 components per unit in steady state; `NearbyEnemies` has `Vec<Entity>` |
| `src/game/game.rs` | Game facade, tick loop | `tick_game()` dispatch order; `TickContext` pattern; `CombatScratch`/`MovementScratch` |
| `src/game/systems/combat.rs` | Combat system (hot loop) | ~12 `world.get()` per pair; HashSet dedup; String/Vec clones |
| `src/game/systems/movement.rs` | Movement system | Two-phase: query → compute → `query_one_mut` per entity (up to 4x) |
| `src/game/systems/spatialty.rs` | Spatialty system | Incremental `NearbyEnemies` maintenance; clones `entities` per mover |
| `src/game/systems/supply.rs` | Supply + attrition | Single-pass → collect → `query_one_mut` apply; full `UnitEquipment` clone per unit |
| `src/game/world_ext.rs` | World extension trait | `spawn_unit()` with fixed 9-component signature (no `NearbyEnemies` at spawn) |
| `src/benchmark.rs` | Benchmark harness | Overhead metric (~0.038ms avg); combat diagnostics tracking; `--bench-detailed` support |
| `src/game/debug_stats.rs` | Debug timing | `DetailedTimer` for sub-part timing within systems |
| `src/game/systems/influence.rs` | Influence system | Rayon explicitly tested and rejected (87ms → 241ms regression) |
| `plans/PLAN_HECS_OPTIMIZATIONS.md` | Prior optimization plan | Auto-vectorization results, Rayon rejection, Arc removal, TickContext propagation |
