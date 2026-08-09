# Combat System Optimization Plan

## Context

The combat system (`src/game/systems/combat.rs`) is the slowest system in the game tick loop. Current profiling and code analysis reveal these hotspots:

- **Per-pair ECS reads:** 8+ `world.get()` calls per combat pair (owner, position, stats×2, flags×2, equipment×2). In dense melees, this means hundreds of ECS lookups per tick.
- **Per-pair ECS writes:** 2 `world.query_one_mut()` calls per resolved pair (one per entity). A unit attacked by 3 enemies gets written 3× independently.
- **Per-tick allocations:** 4 `HashSet::new()` calls (resolved, fought, dead_set, had_enemy_nearby) plus per-candidate `String` clones for owner IDs.
- **No TickContext usage:** Unlike healing/supply/influence/encirclement, combat doesn't benefit from the pre-grouped `TickContext` data built once per tick.
- **Unnecessary full queries:** "Clear moved flags" iterates ALL units (not just candidates), and the retreat pass is a second full iteration over candidates.

The movement system already solved these problems with a two-phase "collect → apply" pattern. This plan adapts that pattern plus TickContext integration to combat.

**Expected outcome:** 30–50% reduction in combat tick time, with zero behavioral changes.

---

## Approach

Three complementary optimizations, ordered by impact:

1. **Two-phase combat resolution** — Collect all combat results into `HashMap<Entity, CombatChange>` during the read phase, then apply mutations in a single batch pass. Matches `movement.rs` pattern.
2. **TickContext integration** — Accept `&TickContext` to get O(1) entity-to-owner lookup and pre-filtered candidates.
3. **Pre-allocated scratch buffers** — `CombatScratch` struct with `.clear()` reuse (same pattern as the existing `nearby: Vec<Entity>` buffer).

---

## Files to Modify

| File | Changes |
|------|---------|
| `src/game/systems/combat.rs` | Main refactor: two-phase pattern, scratch buffers, TickContext usage, retreat merge |
| `src/game/game.rs` | Update combat call site to pass `&TickContext` (lines ~310-320) |

---

## Reuse

| Pattern | Location | Combat Usage |
|---------|----------|-------------|
| Two-phase mutation | `movement.rs` (lines 34–77) | `CombatChange` + HashMap collection + single-pass apply |
| Scratch buffer reuse | `combat.rs` line 66 (`nearby`) | Extend to all 4 HashSets via `.clear()` |
| Scoped Arc extraction | `combat.rs` lines 36–44, `movement.rs` lines 21–24 | Keep terrain Arc extraction as-is |
| TickContext iteration | `healing.rs`, `supply.rs` | Entity-to-owner map, candidate filtering |

---

## Data Structures

### `CombatChange` — Accumulated changes per entity

```rust
/// Accumulated combat changes for a single entity across all engagements this tick.
struct CombatChange {
    /// Final men count after all engagements this tick.
    men: f32,
    /// Organisation delta to apply (-10.0 per engagement).
    org_delta: f32,
    /// New equipment if attrition changed anything (None = no change).
    new_equipment: Option<Vec<crate::game::unit::Equipment>>,
    /// Supply delta to subtract.
    supply_delta: f32,
    /// True if this entity engaged in combat this tick.
    fighting: bool,
    /// True if this entity should retreat (no enemy nearby, was fighting).
    retreated: bool,
    /// True if this entity died this tick.
    is_dead: bool,
}
```

### `CombatScratch` — Pre-allocated, reused scratch buffers

```rust
/// Reusable scratch buffers for combat — .clear()'d each tick, capacity preserved.
struct CombatScratch {
    resolved: std::collections::HashSet<(hecs::Entity, hecs::Entity)>,
    fought: std::collections::HashSet<hecs::Entity>,
    dead_set: std::collections::HashSet<hecs::Entity>,
    had_enemy_nearby: std::collections::HashSet<hecs::Entity>,
    nearby: Vec<hecs::Entity>,
    /// Collected changes — Entity -> accumulated CombatChange.
    changes: std::collections::HashMap<hecs::Entity, CombatChange>,
}

impl CombatScratch {
    fn new() -> Self {
        Self {
            resolved: std::collections::HashSet::new(),
            fought: std::collections::HashSet::new(),
            dead_set: std::collections::HashSet::new(),
            had_enemy_nearby: std::collections::HashSet::new(),
            nearby: Vec::new(),
            changes: std::collections::HashMap::new(),
        }
    }

    fn clear(&mut self) {
        self.resolved.clear();
        self.fought.clear();
        self.dead_set.clear();
        self.had_enemy_nearby.clear();
        self.nearby.clear();
        self.changes.clear();
    }
}
```

---

## Steps

### Step 1: Add `CombatChange` and `CombatScratch` structs to `combat.rs`

- [ ] Add both structs as module-level items (before `attrition_from`)
- [ ] Implement `CombatScratch::new()` and `CombatScratch::clear()`
- [ ] No public API changes — these are internal implementation details

### Step 2: Update function signature to accept `&TickContext`

- [ ] Change signature from:
  ```rust
  pub fn run(world: &mut World, world_entity: hecs::Entity, spatial: SpatialHash) -> Vec<hecs::Entity>
  ```
  to:
  ```rust
  pub fn run(
      world: &mut World,
      world_entity: hecs::Entity,
      spatial: SpatialHash,
      ctx: &TickContext,
  ) -> Vec<hecs::Entity>
  ```
- [ ] Add `use crate::game::game::TickContext;` import

### Step 3: Build entity-to-owner map from TickContext

- [ ] At start of `run()`, build `HashMap<hecs::Entity, String>` from `ctx.units_by_player`:
  ```rust
  let mut entity_owner: std::collections::HashMap<hecs::Entity, String> = 
      std::collections::HashMap::with_capacity(
          ctx.units_by_player.values().map(|v| v.len()).sum()
      );
  for (player_id, handles) in &ctx.units_by_player {
      for (entity, _) in handles {
          entity_owner.insert(*entity, player_id.clone());
      }
  }
  ```
- [ ] Use this map for same-owner checks instead of `world.get::<&UnitOwner>(entity2)`

### Step 4: Rewrite candidate collection using TickContext + CombatScratch

- [ ] Replace the current candidate ECS query (lines 48–55) with TickContext-based filtering:
  ```rust
  let mut scratch = CombatScratch::new(); // or reuse from Game if stored there
  // ...
  
  let candidates: Vec<(hecs::Entity, UnitPosition, String, UnitStats, UnitFlags)> = {
      let mut cands = Vec::new();
      for (player_id, handles) in &ctx.units_by_player {
          for (entity, &pos_tuple) in handles {
              let (stats, flags) = match (
                  world.get::<&UnitStats>(*entity),
                  world.get::<&UnitFlags>(*entity),
              ) {
                  (Ok(s), Ok(f)) => (*s, *f),
                  _ => continue,
              };
              if (flags.moved || flags.fighting) && stats.men > 0.0 {
                  cands.push((*entity, UnitPosition { x: pos_tuple.0, y: pos_tuple.1 }, 
                               player_id.clone(), stats, flags));
              }
          }
      }
      cands
  };
  ```
- [ ] This still does one `world.get()` per unit for stats+flags, but avoids the full query scan + String clone per candidate. The String clone is from the HashMap value (shared across all units of the same player — but still cloned per candidate).

### Step 5: Phase 1 — Main combat loop (collect, don't write)

- [ ] Rewrite the main loop (lines 70–200) to accumulate into `scratch.changes` instead of writing:
- [ ] For each resolved combat pair:
  - Compute damage as before
  - Instead of `world.query_one_mut()`, update `scratch.changes`:
    ```rust
    fn upsert_change(
        changes: &mut HashMap<Entity, CombatChange>,
        entity: Entity,
        men: f32,
        org_delta: f32,
        supply_delta: f32,
        new_equipment: Option<Vec<Equipment>>,
    ) {
        let entry = changes.entry(entity).or_insert(CombatChange {
            men: f32::MAX, // sentinel: will be set on first insert
            org_delta: 0.0,
            new_equipment: None,
            supply_delta: 0.0,
            fighting: false,
            retreated: false,
            is_dead: false,
        });
        
        // First time seeing this entity — read current values
        if entry.men == f32::MAX {
            // We need current men to start from. Store the final men after this engagement.
            // Actually: for the first engagement, men is the result of this engagement.
            // For subsequent engagements, men accumulates further damage.
        }
        
        entry.men = men; // final men after this engagement
        entry.org_delta += org_delta;
        entry.supply_delta += supply_delta;
        entry.fighting = true;
        
        // Equipment: only keep the latest non-None result
        if let Some(eq) = new_equipment {
            entry.new_equipment = Some(eq);
        }
    }
    ```

- [ ] **Critical correctness note for equipment:** The current code re-reads entity1's CURRENT stats before each attack (lines 132–143) because a unit may have already taken damage from an earlier engagement. In the batch model, we must track the working equipment state per entity as engagements accumulate. Solution: store the current equipment in `CombatChange` and pass it through attrition for each subsequent engagement:
  ```rust
  struct CombatChange {
      // ... existing fields ...
      /// Working equipment state, updated after each engagement's attrition.
      /// None means "same as ECS" (not yet touched).
      working_equipment: Option<UnitEquipment>,
  }
  ```

- [ ] The `break` after first attack (line 200) still applies — each unit attacks at most once per tick.

- [ ] Replace `dead.push()` / `dead_set.insert()` with `scratch.changes[entity].is_dead = true` + `scratch.dead_set.insert(entity)`.

- [ ] Replace `resolved.insert(pair)` and `fought.insert(...)` with `scratch.resolved.insert(...)` etc.

### Step 6: Merge retreat logic into collection phase

- [ ] After the main combat loop, check retreat conditions for each candidate:
  ```rust
  for (entity, _, _, _, flags) in &candidates {
      if scratch.fought.contains(entity) {
          continue;
      }
      if !flags.fighting {
          continue;
      }
      if scratch.had_enemy_nearby.contains(entity) {
          continue;
      }
      // Retreat
      let entry = scratch.changes.entry(*entity).or_insert(CombatChange {
          men: f32::MAX, org_delta: 0.0, new_equipment: None,
          supply_delta: 0.0, fighting: false, retreated: false,
          is_dead: false, working_equipment: None,
      });
      entry.retreated = true;
      entry.fighting = false;
  }
  ```

### Step 7: Phase 2 — Batch mutation pass

- [ ] Iterate `scratch.changes` and apply via a single `query_one_mut` per entity:
  ```rust
  for (entity, change) in &scratch.changes {
      if let Ok((ms, me, mf)) = world.query_one_mut::<(
          &mut UnitStats, &mut UnitEquipment, &mut UnitFlags
      )>(*entity) {
          // Apply men (only if this entry actually computed men, not the sentinel)
          if change.men != f32::MAX {
              ms.men = change.men;
          }
          // Apply org delta
          ms.organisation = (ms.organisation - (-change.org_delta)).max(0.0);
          // Apply equipment
          if let Some(ref eq) = change.new_equipment {
              me.equipment = eq.clone();
          }
          // Apply supply delta
          me.supply = (me.supply - change.supply_delta).max(0.0);
          // Apply flags
          if change.fighting {
              mf.fighting = true;
          }
          if change.retreated {
              mf.fighting = false;
          }
          // Clear moved flag
          mf.moved = false;
      }
  }
  ```

- [ ] Collect dead entities for return:
  ```rust
  let dead: Vec<hecs::Entity> = scratch.changes
      .iter()
      .filter(|(_, c)| c.is_dead)
      .map(|(e, _)| *e)
      .collect();
  ```

### Step 8: Limit moved-flag clearing to candidates only

- [ ] The moved-flag clearing is now handled inside the Phase 2 batch mutation (Step 7).
- [ ] For candidates that didn't engage in combat and don't have a `CombatChange` entry, we still need to clear `moved`. Add a separate pass ONLY for these:
  ```rust
  for (entity, _, _, _, _) in &candidates {
      if !scratch.changes.contains_key(entity) {
          if let Ok((mf,)) = world.query_one_mut::<(&mut UnitFlags,)>(*entity) {
              mf.moved = false;
          }
      }
  }
  ```
- [ ] Remove the old "clear moved flags" full-world query (lines 221–223).

### Step 9: Update `game.rs` call site

- [ ] In `src/game/game.rs`, around line 310-320, update the combat call:
  ```rust
  // Before:
  let dead_entities = tick_system!(
      "combat", &mut self.world, self.world_entity,
      systems::combat::run(&mut self.world, self.world_entity, spatial)
  );
  
  // After:
  let dead_entities = if let Some(ref ctx) = ctx {
      tick_system!(
          "combat", &mut self.world, self.world_entity,
          systems::combat::run(&mut self.world, self.world_entity, spatial, ctx)
      )
  } else {
      Vec::new()
  };
  ```

### Step 10: Update tests

- [ ] Update the test helper `run()` calls in the `#[cfg(test)]` module to pass a dummy `TickContext` or refactor tests to build one:
  ```rust
  fn make_context(world: &World, world_entity: hecs::Entity) -> TickContext {
      TickContext::build(world, world_entity).unwrap()
  }
  // Then: run(&mut world, world_entity, spatial, &ctx)
  ```
- [ ] Verify `combat_skips_dead_attacker` and `combat_all_adjacent_units_engage_with_one_attack_each` still pass.

### Step 11: Minor cleanups

- [ ] Change `.cloned()` to `.copied()` for Terrain access (line 153) — same behavior, better semantics.
- [ ] Remove unused debug timer scopes if they overlap incorrectly (lookup/resolve/stats timers started before the loop but the loop structure changed).
- [ ] Ensure `CombatScratch` could be moved into `Game` for full capacity reuse (future optimization — not required for this PR).

---

## Verification

### Unit Tests
- [x] `cargo test combat` — both existing tests pass:
  - `combat_skips_dead_attacker`
  - `combat_all_adjacent_units_engage_with_one_attack_each`
- [x] Full test suite: 143/143 tests pass

### Benchmark Results
- 30 units: original 0.504ms vs optimized 0.504ms (neutral)
- 50 units: original 0.634ms vs optimized 0.664ms (slightly higher due to HashMap overhead)
- **Note:** The batch mutation pattern shows its value at larger scales with dense melees.
  At current benchmark sizes, HashMap allocation overhead offsets the ECS query savings.
  Future optimization: move `CombatScratch` into `Game` struct for capacity reuse across ticks.

### Correctness Invariants
- [x] Each unit makes at most **one attack per tick** (the `break` after first engagement)
- [x] A unit can be **attacked by multiple enemies** in the same tick (damage accumulates)
- [x] Dead units (as defenders) **cannot attack** other enemies (dead_set guard)
- [x] A unit already dead from a prior engagement **stops attacking** (effective_men check)
- [x] Retreat logic: units with `fighting=true` but no nearby enemy get `fighting=false`
- [x] `moved` flag is cleared for all units each tick

---

## Risks

| Risk | Mitigation |
|------|-----------|
| **Equipment attrition correctness** — tracking working equipment across multiple engagements when a unit is attacked by 3+ enemies | Store `working_equipment: Option<UnitEquipment>` in `CombatChange`, update after each engagement's attrition. First engagement reads from ECS, subsequent ones use the working copy. |
| **Dead entity handling** — a unit dies as defender, then should not attack as attacker | `dead_set` guard in the main loop preserved (check `scratch.dead_set.contains(entity1)` at loop start). |
| **Borrow checker** — collecting changes requires mutable HashMap during read phase, then iterating for writes | Two-phase separation is clean: Phase 1 only reads from ECS (using `world.get<&T>`), Phase 2 only writes (using `query_one_mut`). No overlap. |
| **String clones for owner** — still cloning String per candidate in Step 4 | Future optimization: change `UnitOwner.player_id` from `String` to `u32` (separate PR). For now, the clone count is reduced because candidates are fewer (pre-filtered by TickContext vs full query). |
| **f32::MAX sentinel** — edge case if a unit legitimately has f32::MAX men | Use `Option<f32>` for `men` in `CombatChange` instead of sentinel. Safer. |

### Refinement: Use `Option<f32>` instead of sentinel

```rust
struct CombatChange {
    men: Option<f32>,  // None = not yet computed (entity hasn't been in a combat pair yet)
    // ...
}
```

This is cleaner and avoids the f32::MAX sentinel issue.
