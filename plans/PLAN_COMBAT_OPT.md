# Plan: Combat Adjacency Optimization (Spatialty)

## Context

The combat system currently performs a fresh SpatialHash lookup for every combat candidate each tick. This is wasteful because most units don't move — they either stand still or are already engaged in combat. The adjacency graph is largely stable between ticks.

Additionally, combat, attrition, and spatial hash building are scattered across systems in a way that creates duplication and unclear ownership.

**Goal:** Introduce a `Spatialty` system that maintains a `NearbyEnemies` component on each unit, so combat reads pre-computed adjacency instead of doing per-candidate spatial lookups. This reduces per-tick work for the majority of units that haven't moved.

---

## Approach

> **NOTE:** The following fixes from PLAN.md are **already implemented** and are incorporated into the steps below:
> - ✅ `moved` flag clearing moved from combat to movement (movement clears at start)
> - ✅ Terrain asymmetry: each unit uses its own terrain modifier
> - ✅ Organisation loss scales proportionally with men lost
> - ✅ Equipment attrition uses `fighting_attrition` from unit type (no longer magic `0.05`)
> - ✅ `&GameSettings` parameter passed into `combat::run()`
> - ✅ `unit_types_by_name` HashMap precomputed at tick start
> - ✅ Test helpers include `UnitTypeId`

### 1. New Component: `NearbyEnemies`

```rust
/// Cached list of enemy entities within ±1 spatial-hash cells.
/// Maintained by the Spatialty system. Read by combat.
#[derive(Debug, Clone, Default)]
pub struct NearbyEnemies {
    /// Enemy entities found during last spatialty update.
    pub entities: Vec<hecs::Entity>,
    /// Whether this list was ever populated by the spatialty system.
    /// false at spawn, set to true after first Tier 1 initialization.
    pub initialized: bool,
}
```

Every unit spawned via `WorldExt::spawn_unit()` gets `NearbyEnemies { entities: vec![], initialized: false }`. The spatialty system ensures every living unit has a valid (initialized) `NearbyEnemies` on its first pass, and subsequent ticks maintain it incrementally.

### 2. New System: `spatialty.rs`

Runs between `movement` and `combat` in the tick order. Maintains the `NearbyEnemies` component and the `attacking` flag using a three-tier update strategy:

**Tier 1 — Initializers:** ALL units where `NearbyEnemies.initialized == false` (regardless of `moved` status). This catches newly spawned units. Their `NearbyEnemies` is populated from the SpatialHash, and `initialized` is set to `true`.

**Tier 2 — Movers:** Units where `flags.moved == true`. Their `NearbyEnemies` is rebuilt from scratch using the SpatialHash. **Additionally**, if the rebuilt list is non-empty, `flags.attacking = true` is set (unit moved into/through enemy territory). **If the rebuilt list is empty, `flags.attacking = false` is cleared** (mover moved away from enemies — retreat).

**Tier 3 — Neighbor Propagation:** When unit A moves from cell X to cell Y, all units in the 3×3 cell clusters around both X and Y get their `NearbyEnemies` updated. This fixes the staleness problem: if A moves next to B, B didn't move but now has a new enemy neighbor — B's list must be updated. **For Tier 3 units**, if the updated list is empty, `flags.attacking = false` is cleared (retreat: no enemies nearby). If the list is non-empty, `attacking` is left unchanged (preserves previous attacker state for stationary defenders).

The spatialty system also builds the SpatialHash (eliminating the one currently built in `game.rs`). It takes no `&GameSettings` parameter — it only needs `UnitPosition` and `UnitOwner` data.

**Processing filter:**
```
spatialty processes:
  - all units where NearbyEnemies.initialized == false (spawn/initial) — EVERY such unit
  - all units where moved=true (movers)
  - all units in ±1 cells of any mover's old and new positions (neighbor update)
```

A `HashSet<Entity>` tracks which units were processed (initializers + movers + neighbors) to avoid duplicate work in a single tick.

#### `attacking` vs `fighting` Flag Lifecycle

| Flag | Set by | Cleared by | Purpose |
|------|--------|------------|---------|
| `attacking` | spatialty: mover with enemies in NearbyEnemies | spatialty: any unit (mover or stationary) with empty NearbyEnemies (retreat) | "I moved toward or am actively pursuing an enemy" — used by combat for **attacker** candidate selection |
| `fighting` | combat: both sides of a resolved pair | combat retreat check: candidate with no nearby enemy | "engaged in combat this tick" — used by the combat retreat check to distinguish units that were fighting from units that were never in combat |

**Why both flags?** If we used only `fighting` for both systems, spatialty would set `fighting = true` for every unit with an enemy nearby (including defenders), making both attacker and defender satisfy the combat candidate filter `moved \|\| fighting`. Depending on hecs iteration order, the "defender" could become the attacker — changing who gets to strike and who only counter-attacks. The `attacking` flag solves this: only movers set `attacking`, so only movers initiate combat.

**Candidate filter:** `flags.attacking || flags.fighting`
- `attacking` → unit that moved into/through enemy territory this tick (initiator)
- `fighting` → unit already engaged from a previous tick (persists combat state)

### 3. Modified `combat.rs`

- **Removes** the SpatialHash dependency — no longer passed into `run()`
- ~~**Removes** Phase 5 (clearing `moved` flags)~~ ✅ **Already done** — movement now owns this.
- **Removes** the per-candidate spatial hash lookup loop
- **Reads** `NearbyEnemies` component directly from the ECS query
- **Candidate filter** changes from `flags.moved || flags.fighting` to `flags.attacking || flags.fighting`
- **No longer writes** `flags.attacking` — spatialty owns the `attacking` flag. Combat still writes `flags.fighting = true` for both sides of a resolved pair (needed to persist combat state — the retreat check uses it to determine which candidates should disengage).
- ~~**Handles fighting attrition**~~ ✅ **Already done** — `&GameSettings` param, per-unit terrain, proportional org loss, fighting_attrition lookup all implemented.
- **Retreat check** updated: clears `flags.fighting = false` for candidates that didn't fight AND had no enemy in their `NearbyEnemies` component (same logic, reads from NearbyEnemies instead of scratch set).

### 4. Tick Reorder

Current order in `game.rs`:
```
movement → attrition → combat → city_capture → ... → surrender → reinforce
```

New order:
```
movement → spatialty → combat → attrition → city_capture → ... → surrender → death_cleanup → reinforce
```

**Rationale for attrition after combat:** Attrition runs **after** combat so it sees post-combat `men` values. A unit reduced to `men=0` by combat is then skipped by attrition (attrition already skips `men <= 0` units) and collected by the death_cleanup system.

**Rationale for death_cleanup after surrender:** Surrender runs every 30 ticks and checks `men <= 0` OR `men/max_men < 0.05`. Dead units from combat and attrition are already at `men=0`, so surrender sees them correctly. Surrender returns its own `Vec<hecs::Entity>` of units to remove. **All deaths** (combat + attrition + surrender) are collected into one `all_dead` vector and processed by the death_cleanup system **once**, after all systems have finished. This ensures:
1. Surrender units are despawned in the same pass as combat/attrition casualties
2. `NearbyEnemies` lists are cleaned of **all** dead entities in one pass
3. No system runs after cleanup on entities that are already gone

### 5. New System: `death_cleanup.rs`

A dedicated system that takes the combined `Vec<hecs::Entity>` of dead units from combat, attrition, and surrender, and performs the full cleanup in a single pass. This is a separate system so it can be timed via the `tick_system!` macro like all other systems.

```rust
pub fn run(
    world: &mut World,
    all_dead: Vec<hecs::Entity>,
    entity_reverse: &mut HashMap<hecs::Entity, String>,
) {
    // Skip cleanup entirely if nothing died
    if all_dead.is_empty() {
        return;
    }

    // Build a HashSet for O(1) lookup
    let dead_set: std::collections::HashSet<hecs::Entity> = all_dead.iter().copied().collect();

    // Clean NearbyEnemies references (single pass, O(total entries) amortized)
    for (_, mut nearby) in world.query::<&mut NearbyEnemies>().iter() {
        nearby.entities.retain(|e| !dead_set.contains(e));
    }

    // Remove the NearbyEnemies component from dead units (so they don't block despawn)
    for dead in &all_dead {
        world.delete_component::<NearbyEnemies>(*dead).ok();
    }

    // Despawn + cleanup entity registry
    for dead in &all_dead {
        entity_reverse.remove(dead);
        world.despawn(*dead).ok();
    }
}
```

`combat.rs`, `attrition.rs`, and `surrender.rs` all return `Vec<hecs::Entity>` of dead units. The `game.rs` `tick_game()` method chains them into one vector and passes it to the death_cleanup system:

```rust
let all_dead: Vec<_> = combat_dead
    .into_iter()
    .chain(attrition_dead)
    .chain(surrender_dead)
    .collect();

tick_system!(
    "death_cleanup",
    &mut self.world,
    self.world_entity,
    systems::death_cleanup::run(&mut self.world, all_dead, &mut self.entity_reverse)
);
```

The existing per-system despawn loops in `tick_game()` (combat and surrender) are **replaced** by this single unified call. Because it runs inside `tick_system!`, its timing appears in the debug overlay and benchmark output.

### 6. Modified `movement.rs`

- ~~Adds `flags.moved = false` clear for ALL units at the **start** of its run (before computing movement). This fully owns the `moved` flag lifecycle.~~ ✅ **Already done.**
- **Optional `#[cfg(feature = "perfect-movement")]` check** (see below)

#### Optional: `perfect-movement` Feature

Behind `#[cfg(feature = "perfect-movement")]`, movement.rs performs an interception check: before accepting a new position, it queries for enemies along the movement segment using a **segment-circle distance check** (distance from line segment `old_pos → new_pos` to each enemy's position, compared against `COMBAT_RANGE`). If an enemy is intercepted:
- Movement is capped to the intersection point (unit stops just outside the enemy's combat radius)
- `flags.attacking = true` is set (will be caught by spatialty's `moved=true` check anyway)

This prevents the "long-distance move past enemy without engaging" edge case. It is **opt-in** via feature flag and disabled by default to avoid the SpatialHash dependency in the movement system during normal builds.

### 7. Modified `game.rs`

- Add `Spatialty` system invocation between movement and combat
- Remove the shared SpatialHash build (`self.world.build_spatial_hash()`) — spatialty builds its own. **Note:** the existing comment says "shared by Movement and Combat" but movement never uses the SpatialHash; it is combat-only. This is a documentation fix.
- Reorder: movement → spatialty → combat → attrition → city_capture → encirclement → surrender → death_cleanup → reinforce
- Chain combat_dead + attrition_dead + surrender_dead into one vector, pass to `death_cleanup::run()` via `tick_system!`
- ~~Pass `&self.settings` to `combat::run()`~~ ✅ **Already done.**
- spatialty takes no `&GameSettings` — only needs position/owner data

### 8. Modified `attrition.rs`

Remove **both** `flags.fighting` branches. The fighting rate is already applied by combat per engagement, so attrition should never re-apply it.

**Branch 1 — Equipment attrition rate:**
```rust
// REMOVE this branching:
let attrition_rate = if flags.fighting {
    unit_type.fighting_attrition
} else {
    unit_type.base_attrition
};
// REPLACE with:
let attrition_rate = unit_type.base_attrition;
```

**Branch 2 — Boat supply drain rate:**
```rust
// REMOVE this branching:
let drain_rate: f32 = if flags.fighting { 2.0 } else { 1.0 };
// REPLACE with: always use 2 × base_attrition
let drain_rate = (unit_type.base_attrition * 2.0).max(1.0);
```

This eliminates any risk of a stale `fighting` flag causing incorrect rates in either path. No other functional changes to water effects, starvation, or manpower drain logic.

**Return dead units:** After applying attrition, collect entities where `new_stats.men <= 0.0` and return `Vec<hecs::Entity>`. (Previously returned nothing — dead units relied on surrender to clean up.)

---

## Files to Modify / Create

| File | Action | Change |
|------|--------|--------|
| `src/game/components.rs` | Modify | Add `NearbyEnemies` struct + `attacking` flag to `UnitFlags` |
| `src/game/systems/spatialty.rs` | **New** | New system: builds SpatialHash, maintains NearbyEnemies, sets `attacking` flag |
| `src/game/systems/combat.rs` | Modify | Remove SpatialHash dep, read NearbyEnemies, candidate filter uses `attacking \|\| fighting` |
| `src/game/systems/movement.rs` | Modify | ✅ clear `moved=false` at start (done). Optional: `#[cfg(feature = "perfect-movement")]` interception |
| `src/game/systems/attrition.rs` | Modify | Remove both `flags.fighting` branches (equipment + boat supply), return dead units vector |
| `src/game/systems/surrender.rs` | Modify | No functional changes (already returns `Vec<hecs::Entity>`) |
| `src/game/systems/death_cleanup.rs` | **New** | New system: retains NearbyEnemies, deletes components, despawns, cleans registry |
| `src/game/systems/mod.rs` | Modify | `pub mod spatialty; pub mod death_cleanup;` |
| `src/game/world_ext.rs` | Modify | Spawn `NearbyEnemies { entities: vec![], initialized: false }` in `spawn_unit()`. Deprecate `build_spatial_hash()` |
| `src/game/game.rs` | Modify | Tick reorder, remove shared spatial hash, chain dead units into death_cleanup system |
| `Cargo.toml` | Modify | Add optional `perfect-movement` feature |

## Reuse

| Existing code | Where | How reused |
|---------------|-------|------------|
| `SpatialHash` | `spatial_hash.rs` | Reused by spatialty (no longer shared globally) |
| `attrition.rs` unit-type lookup | `attrition.rs` | Same HashMap pattern for fighting attrition in combat |
| `UnitType.fighting_attrition` | `economy.rs` | Used by combat for equipment attrition rate |
| `UnitTypeId` component | `components.rs` | Fetched per engagement for attrition rate |

---

## Steps

### Step 1: Add `NearbyEnemies` component and `attacking` flag

- [ ] Add `NearbyEnemies { entities: Vec<hecs::Entity>, initialized: bool }` to `components.rs` with `Debug, Clone, Default` derives
- [ ] Add `attacking: bool` field to `UnitFlags` (false by default)
- [ ] Update `spawn_unit()` in `world_ext.rs` to include `NearbyEnemies { entities: vec![], initialized: false }`

### Step 2: Create `spatialty.rs`

- [ ] Implement `SpatialtyScratch` struct with `HashSet<Entity>` for processed tracking, `SpatialHash` for lookups
- [ ] Implement `run()` that:
  - Builds SpatialHash from all unit positions
  - **Tier 1:** identifies units where `NearbyEnemies.initialized == false`, populates their NearbyEnemies, sets `initialized = true`
  - **Tier 2:** identifies movers (`moved=true`), rebuilds NearbyEnemies from SpatialHash. If list is non-empty, sets `flags.attacking = true`. If list is empty, clears `flags.attacking = false` (mover retreated from enemies)
  - **Tier 3:** identifies neighbor units in ±1 cells of each mover's old and new positions, rebuilds their NearbyEnemies. If list is empty, clears `flags.attacking = false`. If list is non-empty, leaves `attacking` unchanged
  - For each processed unit: queries SpatialHash, filters by enemy owner, writes `NearbyEnemies` component
  - Uses a `HashSet<Entity>` to avoid processing the same unit twice

### Step 3: ~~Move `moved` flag clearing to `movement.rs`~~ ✅ Done

- [x] Add `flags.moved = false` loop at start of `movement::run()`
- [x] Remove Phase 5 (`moved` flag clearing) from `combat.rs`

### Step 4: Modify `combat.rs` to use `NearbyEnemies`

- [x] ~~Add `&GameSettings` parameter~~ ✅ Already done in PLAN.md
- [x] ~~Precompute `unit_types_by_name` HashMap~~ ✅ Already done in PLAN.md
- [x] ~~Replace magic `0.05` attrition rate with `fighting_attrition` lookup~~ ✅ Already done in PLAN.md
- [x] ~~Replace flat `-10.0` org loss with proportional org loss~~ ✅ Already done in PLAN.md
- [x] ~~Fix terrain asymmetry~~ ✅ Already done in PLAN.md
- [ ] Remove `SpatialHash` parameter from `run()`
- [ ] Change candidate query to include `&NearbyEnemies`
- [ ] Change candidate filter from `flags.moved || flags.fighting` to `flags.attacking || flags.fighting`
- [ ] Remove per-candidate spatial hash lookup (`spatial.query_nearby_into`)
- [ ] Iterate `nearby.entities` directly instead of `scratch.nearby`
- [ ] Keep squared distance check and pair dedup (NearbyEnemies is a superset, not a precise combat-range list — it filters by owner but combat needs normalized pair tracking)
- [ ] Update retreat check to read from `NearbyEnemies` component instead of `scratch.had_enemy_nearby`
- [ ] Remove `scratch.had_enemy_nearby` field from `CombatScratch` (no longer needed)

### Step 5: Modify `attrition.rs` — remove both `fighting` branches, return dead units

- [ ] **Equipment attrition:** remove `if flags.fighting { unit_type.fighting_attrition } else { unit_type.base_attrition }`; always use `unit_type.base_attrition`
- [ ] **Boat supply drain:** remove `if flags.fighting { 2.0 } else { 1.0 }`; always use `(unit_type.base_attrition * 2.0).max(1.0)`
- [ ] After applying attrition, collect entities where `new_stats.men <= 0.0`
- [ ] Return `Vec<hecs::Entity>` of dead units

### Step 6: Optional — `perfect-movement` feature in `movement.rs`

- [ ] Add `perfect-movement` as optional feature in `Cargo.toml`
- [ ] Behind `#[cfg(feature = "perfect-movement")]`, add segment-circle interception check before accepting new position
- [ ] If intercepted: cap movement to intersection point, set `flags.attacking = true`

### Step 7: Create `death_cleanup.rs`

- [ ] Implement `run(world, all_dead: Vec<Entity>, entity_reverse: &mut HashMap)` that:
  - Returns early if `all_dead.is_empty()`
  - Builds `HashSet<Entity>` for O(1) lookup
  - Retains alive entities from all `NearbyEnemies.entities` vecs
  - Deletes `NearbyEnemies` component from dead units
  - Despawns dead entities and removes from `entity_reverse` map
- [ ] Add `#[cfg(test)]` test: death cleanup removes entities from NearbyEnemies lists
- [ ] Add test: death cleanup handles empty input gracefully (early return)

### Step 8: Reorder tick and wire up `game.rs`

- [x] ~~Pass `&self.settings` to `combat::run()`~~ ✅ Already done in PLAN.md
- [ ] Remove shared SpatialHash build (`self.world.build_spatial_hash()`) — note: this is combat-only, not shared with movement
- [ ] Insert `spatialty::run()` between movement and combat
- [ ] Reorder: movement → spatialty → combat → attrition → city_capture → encirclement → surrender → death_cleanup → reinforce
- [ ] Collect dead units from combat + attrition + surrender into one `all_dead` vector
- [ ] Call `death_cleanup::run(&mut self.world, all_dead, &mut self.entity_reverse)` wrapped in `tick_system!`
- [ ] **Remove** existing per-system despawn loops (combat loop + surrender loop) — replaced by death_cleanup system
- [ ] spatialty takes no `&GameSettings` — only needs position/owner data

### Step 9: Update `mod.rs`

- [ ] Add `pub mod spatialty;`
- [ ] Add `pub mod death_cleanup;`

### Step 10: Update tests

- [x] Add `UnitTypeId { name: "infantry".to_string() }` to combat test helpers ✅ Already done
- [x] Update combat test assertions for new org scaling and fighting attrition ✅ Already done
- [x] Add cross-terrain combat test (attacker on hills, defender on plains) ✅ Already done
- [ ] Update combat tests to use `attacking` flag in candidate setup (tests that spawn units with `fighting: true` should also consider `attacking`)
- [ ] Add spatialty test: mover propagates adjacency to stationary neighbor
- [ ] Add spatialty test: new spawn gets NearbyEnemies initialized (`initialized` flips from false to true)
- [ ] Add spatialty test: `attacking` flag set for mover with enemies, cleared on retreat
- [ ] Add spatialty test: `attacking` flag persists across ticks for stationary unit in combat range
- [ ] Add death_cleanup test: dead entity removed from other units' NearbyEnemies lists
- [ ] Add death_cleanup test: early return when no dead entities
- [ ] Add integration test: unit killed by combat is skipped by attrition and despawned by cleanup
- [ ] Add integration test: unit killed by combat + unit below 5% both cleaned by unified cleanup after surrender
- [ ] Update attrition tests: verify `base_attrition` is always used (no `fighting` branch)
- [ ] Update attrition tests: verify boat supply drain uses `2 * base_attrition` (no `fighting` branch)

### Step 11: Remove unused `build_spatial_hash` from `WorldExt`

- [ ] Remove `build_spatial_hash()` from the `WorldExt` trait (spatialty builds its own; combat tests use a local `build_spatial()` helper). If tests outside combat still reference it, migrate them first.

---

## Verification

1. [x] `cargo test combat` — all 9 combat tests pass (done in PLAN.md)
2. [ ] `cargo test spatialty` — new spatialty tests pass
3. [ ] `cargo test` — full test suite passes (no regressions)
4. [ ] `cargo build --release` — compiles cleanly with no warnings
5. [ ] `cargo test --features perfect-movement` — perfect-movement feature compiles and tests pass (if implemented)
6. [ ] Tick order verified: movement → spatialty → combat → attrition → ... → death_cleanup in `game.rs`
7. [ ] Debug overlay: `death_cleanup` system timing visible in system_timings breakdown
8. [ ] Manual: units that move adjacent to stationary enemies should engage (neighbor propagation works)
9. [ ] Manual: stationary defender should not become attacker (attacking flag only set by movers)
10. [ ] Manual: unit that moves away from enemies should stop being a combat candidate (attacking flag cleared by spatialty Tier 2)
11. [ ] Manual: dead units should not appear in other units' NearbyEnemies on the next tick
12. [ ] Manual: terrain asymmetry — hills attacker vs plains defender should produce different damage values ~~(already verified in new test)~~
