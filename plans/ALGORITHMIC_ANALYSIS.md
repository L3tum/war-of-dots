# Algorithmic System Analysis

## Methodology
Analyzed each system's hot path for algorithmic improvements. Focus on reducing per-entity world lookups, eliminating redundant computations, and leveraging hecs query patterns correctly.

## Critical Anti-Pattern: world_data_clone() Full Bundle Clone (✅ FIXED)

**Found in: combat, movement, city_capture, water, reinforce, encirclement, influence**

Every system cloned the entire WorldBundle (130KB+) via `world_data_clone()`,
even when only 1-2 small fields were needed.

| System | What it needed | What it cloned | Waste |
|--------|---------------|----------------|-------|
| combat | terrain grid only | 130KB WorldBundle | vision + borders + cities + funds |
| movement | terrain grid only | 130KB WorldBundle | same |
| water | nothing useful | 130KB WorldBundle | **100% wasted** |
| reinforce | nothing at all | 130KB WorldBundle | **100% wasted** |

### Fix: Scoped Arc Extraction

```rust
// Before: clone 130KB WorldBundle
let wb = world.world_data_clone(world_entity);
// ... use wb.terrain.grid ...

// After: extract only what's needed (24 bytes Arc pointer bump)
let terrain = match world.world_data(world_entity) {
    Some(wb) => Arc::clone(&wb.terrain.grid),  // 24 bytes, not 130KB
    None => return,
};
// wb (hecs::Ref) dropped immediately — borrow released
```

### Fix: Slim TickContext

```rust
// Before: clone entire WorldBundle (130KB)
pub struct TickContext {
    pub world_data: WorldBundle,
    pub units_by_player: HashMap<...>,
}

// After: only small data (~3KB total)
pub struct TickContext {
    pub cities: Vec<City>,      // ~20 entries, ~2KB
    pub funds: HashMap<...>,    // ~12 entries, ~500B
    pub width: u32,
    pub height: u32,
    pub units_by_player: HashMap<...>,
}
```

### Result: **-16% total wall time (-128ms per 1500 ticks)**

| Metric | Before | After | Delta |
|--------|--------|-------|-------|
| Total wall time | 798ms | 670ms | -16% |
| Avg tick | 0.532ms | 0.446ms | -16% |
| combat | 113ms | 93ms | -18% |
| movement | 83ms | 55ms | -34% |
| water | 46ms | 29ms | -37% |
| reinforce | 41ms | 25ms | -39% |
| tickcontext | 24ms | 16ms | -34% |
| city_capture | 33ms | 20ms | -17% |

---

## Critical Anti-Pattern: Entity-ID-Then-Refetch (✅ FIXED — -14% total)

**Found in: attrition, movement, water, reinforce, surrender, combat (partial)**

**Fix: Two-phase pattern — query + collect changes → apply mutations**
```rust
// Phase 1: query + collect changes (data already in cache)
let changes = world.query::<(&A, &B, &C)>().iter().filter_map(|(e, (a, b, c))| {
    compute_change(a, b, c).map(|ch| (e, ch))
}).collect();

// Phase 2: apply mutations
for (entity, change) in changes {
    world.query_one_mut::<(&mut B,)>(entity).map(|(mb,)| *mb = change.new_b);
}
```

**Result: 661ms → 567ms (-94ms, -14%)**

| System | Before | After | Delta | Approach |
|--------|--------|-------|-------|----------|
| attrition | 58ms | 15ms | -74% | two-phase |
| movement | 53ms | 18ms | -66% | two-phase |
| water | 29ms | 16ms | -45% | two-phase |
| reinforce | 25ms | 9ms | -64% | two-phase |
| surrender | 0.5ms | 0ms | -100% | direct filter_map |
| combat | 96ms | 92ms | -4% | stats/flags pre-fetched, equipment deferred |

**Remaining `world.get` calls:** Only in combat (8 calls) — all unavoidable entity2 lookups from spatial hash neighbors.

**Entity2 Caching Experiment (❌ REJECTED):** Attempted to cache entity2 component data in HashMap to deduplicate lookups when same entity2 is neighbor of multiple candidates. Result: 92ms → 161ms (+75%). The clone of `Entity2Data` (with `Vec<Equipment>`) on every cache hit was more expensive than 5× `world.get` calls. Also tried batch-query approach (collect all entity2 IDs → query archetype → HashMap): 92ms → 190ms (+107%). Iterating all 240 entities to find ~100 entity2s was slower than targeted lookups. **Conclusion: With hecs' current API, spatial hash neighbor lookups require individual `world.get()` calls. The only path to improvement is storing component data in the spatial hash itself (not just entity IDs).**
    // comp_a and comp_b are already fetched
}
```

**Caveat:** Cannot do mutable writes while iterating. Two-phase approach needed:
```rust
// Phase 1: collect changes
let changes: Vec<_> = world.query::<(&CompA, &CompB)>().iter().filter_map(|(e, (a, b))| {
    if needs_update(a, b) { Some((e, compute_new_b(b))) } else { None }
}).collect();

// Phase 2: apply changes
for (entity, new_b) in changes {
    world.insert_one(entity, new_b).ok();
}
```

---

## Per-System Analysis

### 1. Combat (105ms, 22%, 1550 calls)

**Current flow:**
1. `world_data_clone()` — clones 130KB WorldBundle (only terrain used)
2. Query candidates: `moved || fighting` filter
3. For each candidate: spatial hash query → 6× `world.get()` per neighbor pair
4. Retreat check: another spatial query + 3× `world.get()` per neighbor
5. Clear moved: full iteration of all units

**Improvements:**
- **A. Skip world_data_clone()** — pass terrain grid reference directly via TickContext
- **B. Merge retreat check into main loop** — track `found_enemy` per candidate, set fighting=false at end
- **C. Use query data directly** — collect candidates with pre-fetched components
- **D. Clear moved in movement system** — movement already sets `moved=true`, could clear after processing

**Estimated savings: 15-25%**

---

### 2. Influence (95ms, 20%, 775 calls, 50%)

**Current flow:**
1. Per-player circle projection on 100×100 grid
2. 20 units × 121 cells + 1 city × 289 cells per player
3. Grid allocation: 2 × 100×100 per player per call

**Improvements:**
- **A. Frequency throttle** — every 3-4 ticks instead of 2 (visual impact minimal)
- **B. Grid pooling** — pre-allocate grids once, reuse (eliminates 240KB alloc/call)
- **C. Downsample** — compute at 50×50, interpolate for client (4× fewer cells)

**Estimated savings: 25-50% (A+C combined)**

---

### 3. Encirclement (67ms, 14%, 103 calls, 7%)

**Current flow:**
1. Build friendly grid per player (100×100)
2. Flood-fill to find connected components
3. Check each component for cities + edge touch
4. Update flags for units in encircled cells

**Improvements:**
- Already well-optimized (tension map, flood-fill, array queue)
- **A. Increase frequency** — every 30 ticks instead of 15 (encirclement is strategic)
- **B. Early-exit** — skip players with no units or all units at map edge

**Estimated savings: 30-50% (A)**

---

### 4. Movement (62ms, 13%, 1550 calls)

**Current flow:**
1. `world_data_clone()` — clones WorldBundle for terrain access
2. Query entities with non-empty paths → collect IDs → refetch components
3. Per-entity: 6× `world.get()` calls + terrain lookup
4. Write back: position update + flag set

**Improvements:**
- **A. Use TickContext terrain** — no world_data_clone needed
- **B. Use query data directly** — no entity-collect-then-refetch
- **C. Clear moved flag here** — movement sets `moved=true`, clear after processing (saves full iteration in combat)

**Estimated savings: 20-30% (A+B)**

---

### 5. Attrition (52ms, 11%, 1550 calls)

**Current flow:**
1. Query 5 components → collect entity IDs
2. For each entity: 5× `world.get()` calls to refetch same components
3. Complex logic: water bonus, fighting attrition, overload penalty, starvation

**Improvements:**
- **A. Use query data directly** — eliminate 5× refetch per entity
- **B. Two-phase pattern** — collect changes → apply in batch
- **C. Early-exit** — skip units with fighting=false, on_water=false, not overloaded (common case)

**Estimated savings: 30-40% (A)**

---

### 6. Water (42ms, 9%, 1550 calls)

**Current flow:**
1. `world_data_clone()` — clones WorldBundle (cities unused, terrain dummy)
2. Query entities → collect IDs → refetch components
3. Per-entity: timer decrement or terrain check

**Improvements:**
- **A. Remove world_data_clone()** — cities are unused, terrain accessed via `terrain_at`
- **B. Use query data directly** — no entity-collect-then-refetch
- **C. Filter to only transitioning units** — most units are NOT embarking/disembarking

**Estimated savings: 40-50% (A+B+C)**

---

### 7. Reinforce (37ms, 8%, 1550 calls)

**Current flow:**
1. Query entities → collect IDs → refetch components
2. Per-entity: org reinforcement with supply malus

**Improvements:**
- **A. Use query data directly** — no entity-collect-then-refetch
- **B. Early-exit** — skip units at max org or with org <= 0

**Estimated savings: 25-35% (A)**

---

### 8. City Capture (24ms, 5%, 1500 calls)

**Current flow:**
1. Query all units → collect (owner, position) pairs
2. For each unit: check against all cities for capture
3. Write changes to city owners

**Improvements:**
- **A. Spatial filter** — only check units near cities (pre-filter by distance)
- **B. Use query data directly** — already collects (owner, position), good pattern

**Estimated savings: 10-20% (A)**

---

### 9. Spatial Hash (24ms, 5%, 1550 calls)

**Current flow:**
1. Query all unit positions → insert into HashMap<(i32,i32), Vec<Entity>>

**Improvements:**
- Already optimal O(n). No obvious algorithmic wins.

**Estimated savings: 0%**

---

### 10. TickContext (20ms, 4%, 1550 calls)

**Current flow:**
1. `world_data_clone()` — clones WorldBundle (130KB)
2. Single-pass query: group entities by player

**Improvements:**
- **A. Lazy world_data_clone** — only clone when systems need it (healing/supply use cities, not grids)
- **B. Selective clone** — clone cities/funds separately, borrow grids via reference

**Estimated savings: 10-20% (B)**

---

### 11. Healing (6ms, 1%, 1550 calls)

**Current flow:**
1. Per-player iteration via TickContext
2. Check units near friendly cities

**Improvements:**
- Already well-optimized (per-player, squared distance, no world.get)

**Estimated savings: 0%**

---

### 12. Supply (4.4ms, 1%, 1550 calls)

**Current flow:** Same as healing, already optimized.

**Estimated savings: 0%**

---

### 13. Surrender (0.5ms, 3%)

**Current flow:** Query stats+flags → check surrender conditions.

**Improvements:** Already minimal. No wins.

---

### 14. Economy (0.1ms, 10%)

**Current flow:** Generate funds per city. Already minimal.

**Estimated savings: 0%**

---

## Summary: Top Optimization Targets

| Priority | System | Current | Est. Savings | Effort | Risk |
|----------|--------|---------|--------------|--------|------|
| ~~1~~ | ~~Attrition (A)~~ | ~~52ms~~ | ~~30-40%~~ | ~~30 min~~ | ~~Low~~ |
| ~~2~~ | ~~Water (A+B+C)~~ | ~~42ms~~ | ~~40-50%~~ | ~~30 min~~ | ~~Low~~ |
| 1 | Scoped Arc extraction (ALL systems) | 130KB/world_data_clone | **-16% total (-128ms)** | 1 hour | Low | ✅ DONE |
| 2 | Two-phase pattern (ALL systems) | 205ms → 111ms | **-14% total (-94ms)** | 1.5 hours | Low | ✅ DONE |
| 3 | Influence frequency (2→3) | 110ms | 33% (-37ms) | 5 min | Low |
| 4 | Encirclement frequency (15→30) | 67ms | 50% (-33ms) | 5 min | Low |
| 5 | Combat retreat merge | 92ms | 10-15% (-9-14ms) | 1 hour | Medium |

**Total potential (remaining): -79-84ms per 1500-tick benchmark (-14-15% of current 567ms)**

## Recommended Implementation Order

1. **Quick wins (10 min):** Influence frequency (2→3), Encirclement frequency (15→30)
   - Expected: -70ms (-12%)
2. **Combat refactor (1 hour):** Merge retreat check into main loop
   - Expected: -9-14ms (-2%)
