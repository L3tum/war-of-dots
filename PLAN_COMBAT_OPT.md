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
/// Cached list of enemy entities within COMBAT_RANGE.
/// Maintained by the Spatialty system. Read by combat.
#[derive(Debug, Clone, Default)]
pub struct NearbyEnemies(pub Vec<hecs::Entity>);
```

Each unit carries this component. It's lazily initialized on first encounter (spawn or first movement) and incrementally updated.

### 2. New System: `spatialty.rs`

Runs between `movement` and `combat` in the tick order. Maintains the `NearbyEnemies` component using a three-tier update strategy:

**Tier 1 — Initializers:** Units where `NearbyEnemies` is missing (newly spawned) or uninitialized. These get their first adjacency scan.

**Tier 2 — Movers:** Units where `flags.moved == true`. Their `NearbyEnemies` is rebuilt from scratch using the SpatialHash.

**Tier 3 — Neighbor Propagation:** When unit A moves from cell X to cell Y, all units in the 3×3 cell clusters around both X and Y get their `NearbyEnemies` updated. This fixes the staleness problem: if A moves next to B, B didn't move but now has a new enemy neighbor — B's list must be updated.

The spatialty system also builds the SpatialHash (eliminating the shared one currently built in `game.rs`).

**Processing filter:**
```
spatialty processes:
  - all units where NearbyEnemies is missing (spawn/initial)
  - all units where moved=true (movers)
  - all units in ±1 cells of any mover's old and new positions (neighbor update)
```

A `HashSet<Entity>` tracks which units were processed (movers + neighbors) to avoid duplicate work in a single tick.

### 3. Modified `combat.rs`

- **Removes** the SpatialHash dependency — no longer passed into `run()`
- ~~**Removes** Phase 5 (clearing `moved` flags)~~ ✅ **Already done** — movement now owns this.
- **Removes** the per-candidate spatial hash lookup loop
- **Reads** `NearbyEnemies` component directly from the ECS query
- ~~**Handles fighting attrition**~~ ✅ **Already done** — `&GameSettings` param, per-unit terrain, proportional org loss, fighting_attrition lookup all implemented.

### 4. Tick Reorder

Current order in `game.rs`:
```
movement → attrition → combat → city_capture → ...
```

New order:
```
movement → spatialty → combat → attrition → city_capture → ...
```

**Rationale:** Attrition runs **after** combat so it sees post-combat `men` values. A unit reduced to `men=0` by combat is then properly cleaned up by attrition (or the surrender system). Base attrition, water effects, and starvation apply to the post-combat state.

### 5. Death Cleanup

Both `combat.rs` and `attrition.rs` return `Vec<hecs::Entity>` of dead units. The `game.rs` `tick_game()` method collects dead units from all sources and:

1. Removes each dead entity from all other units' `NearbyEnemies` lists (one ECS query pass, using `retain()`)
2. Despawns the dead entity from the ECS
3. Cleans up the entity registry

This is done **once** per tick after all systems have reported deaths, rather than per-death or per-system. The O(N) NearbyEnemies cleanup is bounded by the number of dead units per tick (typically 0-5).

```rust
// In game.rs tick_game(), after combat and attrition:
let all_dead = combat_dead.into_iter().chain(attrition_dead).collect::<Vec<_>>();

// Clean NearbyEnemies references
for (entity, mut nearby) in world.query::<(&hecs::Entity, &mut NearbyEnemies)>().iter() {
    if nearby.0.iter().any(|e| all_dead.iter().any(|d| d.id() == e.id())) {
        nearby.0.retain(|e| !all_dead.iter().any(|d| d.id() == e.id()));
    }
}

// Remove the NearbyEnemies component from dead units (so they don't block despawn)
for dead in &all_dead {
    world.delete_component::<NearbyEnemies>(*dead).ok();
}

// Despawn
for dead in &all_dead {
    self.entity_reverse.remove(dead);
    self.world.despawn(*dead).ok();
}
```

### 6. Modified `movement.rs`

- ~~Adds `flags.moved = false` clear for ALL units at the **start** of its run (before computing movement). This fully owns the `moved` flag lifecycle.~~ ✅ **Already done.**
- No other changes.

### 7. Modified `game.rs`

- Add `Spatialty` system invocation between movement and combat
- Remove the shared SpatialHash build (spatialty builds its own)
- Reorder: movement → spatialty → combat → attrition
- Collect dead units from both combat and attrition
- Add post-tick NearbyEnemies cleanup and despawn
- ~~Pass `&self.settings` to `combat::run()`~~ ✅ **Already done.**

### 8. Modified `attrition.rs`

- No functional changes to base attrition, water effects, or starvation logic
- Returns `Vec<hecs::Entity>` of units that died (men reached 0)

---

## Files to Modify / Create

| File | Action | Change |
|------|--------|--------|
| `src/game/components.rs` | Modify | Add `NearbyEnemies` component |
| `src/game/systems/spatialty.rs` | **New** | New system: builds SpatialHash, maintains NearbyEnemies |
| `src/game/systems/combat.rs` | Modify | Remove SpatialHash dep, read NearbyEnemies, **already**: added GameSettings, per-unit terrain, proportional org, fighting_attrition lookup |
| `src/game/systems/movement.rs` | Modify | **Already done**: clear `moved=false` at start of run |
| `src/game/systems/attrition.rs` | Modify | Return dead units vector |
| `src/game/systems/mod.rs` | Modify | `pub mod spatialty;` |
| `src/game/game.rs` | Modify | Tick reorder, remove shared spatial hash, add cleanup logic, pass settings |
| `src/game/systems/combat.rs` (tests) | Modify | Add `UnitTypeId`, update assertions, add cross-terrain test |

## Reuse

| Existing code | Where | How reused |
|---------------|-------|------------|
| `SpatialHash` | `spatial_hash.rs` | Reused by spatialty (no longer shared globally) |
| `attrition.rs` unit-type lookup | `attrition.rs` | Same HashMap pattern for fighting attrition in combat |
| `UnitType.fighting_attrition` | `economy.rs` | Used by combat for equipment attrition rate |
| `TerrainGrid.get()` | `components.rs` | Terrain lookup in spatialty for filtering |
| `UnitTypeId` component | `components.rs` | Fetched per engagement for attrition rate |

---

## Steps

### Step 1: Add `NearbyEnemies` component

- [ ] Add `NearbyEnemies(Vec<hecs::Entity>)` to `components.rs` with `Debug, Clone, Default` derives

### Step 2: Create `spatialty.rs`

- [ ] Implement `SpatialtyScratch` struct with `HashSet<Entity>` for processed tracking, `SpatialHash` for lookups
- [ ] Implement `run()` that:
  - Builds SpatialHash from all unit positions
  - Identifies initializers (missing NearbyEnemies), movers (moved=true), and their neighbors
  - For each processed unit: queries SpatialHash, filters by enemy owner + distance, writes `NearbyEnemies` component
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
- [ ] Remove per-candidate spatial hash lookup (`spatial.query_nearby_into`)
- [ ] Iterate `nearby.0` directly instead of `scratch.nearby`
- [ ] Keep squared distance check and pair dedup (NearbyEnemies is a superset, not a precise combat-range list — it filters by owner but combat needs normalized pair tracking)

### Step 5: Modify `attrition.rs` to return dead units

- [ ] After applying attrition, collect entities where `new_stats.men <= 0.0`
- [ ] Return `Vec<hecs::Entity>` of dead units

### Step 6: Reorder tick and add cleanup in `game.rs`

- [x] ~~Pass `&self.settings` to `combat::run()`~~ ✅ Already done in PLAN.md
- [ ] Remove shared SpatialHash build (`self.world.build_spatial_hash()`)
- [ ] Insert `spatialty::run()` between movement and combat
- [ ] Reorder: movement → spatialty → combat → attrition → city_capture → ...
- [ ] Collect dead units from both combat and attrition
- [ ] Add NearbyEnemies cleanup loop (retain alive entities from all NearbyEnemies vecs)
- [ ] Delete NearbyEnemies component from dead units before despawning
- [ ] Pass `&self.settings` to `combat::run()` ~~(done)~~ and `spatialty::run()`

### Step 7: Update `mod.rs`

- [ ] Add `pub mod spatialty;`

### Step 8: Update tests

- [x] Add `UnitTypeId { name: "infantry".to_string() }` to combat test helpers ✅ Already done
- [x] Update combat test assertions for new org scaling and fighting attrition ✅ Already done
- [x] Add cross-terrain combat test (attacker on hills, defender on plains) ✅ Already done
- [ ] Add spatialty test: mover propagates adjacency to stationary neighbor
- [ ] Add spatialty test: new spawn gets NearbyEnemies initialized

### Step 9: Remove unused SpatialHash global build

- [ ] If `build_spatial_hash()` in `world_ext.rs` is no longer called from `game.rs`, either keep it (used by tests/spatialty) or mark appropriately

---

## Verification

1. [x] `cargo test combat` — all 9 combat tests pass (done in PLAN.md)
2. [ ] `cargo test spatialty` — new spatialty tests pass
3. [ ] `cargo test` — full test suite passes (no regressions)
4. [ ] `cargo build --release` — compiles cleanly with no warnings
5. [ ] Tick order verified: movement → spatialty → combat → attrition in `game.rs`
6. [ ] Manual: units that move adjacent to stationary enemies should engage (neighbor propagation works)
7. [ ] Manual: dead units should not appear in other units' NearbyEnemies on the next tick
8. [ ] Manual: terrain asymmetry — hills attacker vs plains defender should produce different damage values ~~(already verified in new test)~~
