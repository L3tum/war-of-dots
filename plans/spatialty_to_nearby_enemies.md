# Replace SpatialHash with Brute-Force NearbyEnemies

## Context

The current `SpatialHash`-based spatialty system has several inefficiencies for the game's scale:

1. **9-cell query (3×3) returns massive false positives** — cell_size equals combat_range (2.0), so max false-positive distance is ~5.66 vs actual range of 2.0 (8× area)
2. **HashMap overhead** — hashing `(i32, i32)` keys, dynamic Vec growth, pointer chasing
3. **Incremental maintenance complexity** — movers tracking, owner string comparisons, "announce to neighbors" dance, death_cleanup SpatialHash.remove path
4. **`uninitialized_units` bookkeeping** — tracking which units haven't been seen by spatialty yet

At the game's typical unit counts (tens to low hundreds), the O(n²) brute-force approach is likely **faster** than the current spatial-hash + incremental-update pipeline because:
- Each distance check is ~4 FP ops (dx*dx + dy*dy < 4.0) — SIMD-friendly, branch-predictable
- No HashMap lookups, no Vec allocations, no pointer chasing
- Combat already does a narrow-phase distance check on every candidate anyway

Research consensus (Game Programming Patterns, Bevy Physics Book): *"For < 50 objects: brute force is fine. For 50-500: spatial grid. For 500+: quadtree."*

## Approach

Replace the incremental spatial-hash system with a **full brute-force rebuild** of `NearbyEnemies` every tick. This runs after movement (so positions are current) and before combat (so combat reads correct neighbor lists).

The new `nearby_enemies` module will:
1. Collect all living unit positions, owners, and `flags.moved` status in one ECS query (needed for the O(n²) distance check — every unit's `NearbyEnemies` must be accurate so combat can detect when a mover enters a stationary unit's range)
2. O(n²) nested loop: for each unit, check distance to every other unit, filter by owner
3. Write `NearbyEnemies` components for all units (create if missing, update if exists)
4. Set `attacking` flag **only for movers** (units with `flags.moved`): `attacking` is true when a unit moved and its current position is within `COMBAT_RANGE` of an enemy unit. This means the unit either moved into the enemy's combat radius or is now adjacent to an enemy after repositioning. Stationary units never get `attacking` set here (they keep whatever combat left them).

**No more incremental state.** Every tick is a clean rebuild. No more `uninitialized_units`, no more spatial hash, no more death cleanup for the hash.

## Files to Modify

### New
- **`src/game/systems/nearby_enemies.rs`** — new brute-force module (replaces spatialty)

### Deleted
- **`src/game/systems/spatial_hash.rs`** — entire module removed
- **`src/game/systems/spatialty.rs`** — entire module removed

### Modified
- **`src/game/systems/mod.rs`** — remove `spatial_hash` and `spatialty` modules; add `nearby_enemies`
- **`src/game/systems/death_cleanup.rs`** — remove `SpatialHash` parameter and all hash operations; keep NearbyEnemies cleanup + despawn
- **`src/game/game.rs`** — remove `SpatialHash` import, `spatial_hash` field, `uninitialized_units` field; replace spatialty call with nearby_enemies call; update CombatHealth docs
- **`src/game/components.rs`** — update NearbyEnemies doc comment (no longer "maintained by Spatialty")
- **`src/microbenchmarks.rs`** — remove SpatialHash benchmarks; rewrite spatialty benchmarks to use new module; update SpatialtyFixture; remove SpatialHash import

## Steps

### Step 1: Create `src/game/systems/nearby_enemies.rs`

New module with a single `run()` function:

```rust
/// Rebuild NearbyEnemies for all living units using brute-force O(n²) distance checks.
/// Sets `attacking` flag only for movers (units with flags.moved set by movement system).
pub fn run(world: &mut hecs::World) {
    // 1. Collect all living unit data in one query — need all positions for the
    //    O(n²) neighbor check, and flags.moved to gate attacking-flag updates.
    let units: Vec<(Entity, UnitPosition, String, bool)> = world
        .query::<(Entity, &UnitPosition, &UnitOwner, &UnitFlags)>()
        .iter()
        .map(|(e, pos, owner, flags)| (e, *pos, owner.player_id.clone(), flags.moved))
        .collect();

    // 2. For each unit, find enemies within COMBAT_RANGE and write NearbyEnemies
    for i in 0..units.len() {
        let enemies: Vec<Entity> = (0..units.len())
            .filter(|&j| {
                i != j && units[i].2 != units[j].2  // different owner
                    && dist_sq(units[i].1, units[j].1) < crate::game::constants::COMBAT_RANGE_SQ
            })
            .map(|j| units[j].0)
            .collect();

        // 3. Write NearbyEnemies (create or update)
        // ... (insert or update the NearbyEnemies component)

        // 4. Set `attacking` ONLY for movers: a unit that moved and is now within
        //    COMBAT_RANGE of an enemy. This covers both "moved toward enemy" and
        //    "moved into enemy combat radius". Stationary units keep whatever
        //    state combat left them in (not modified here).
        if units[i].3 && !enemies.is_empty() {
            // set attacking = true
        } else if units[i].3 {
            // set attacking = false (mover with no nearby enemies)
        }
    }
}
```

Key design decisions:
- **All positions collected** — every unit's `NearbyEnemies` must be accurate because combat reads it for stationary units too (e.g., a mover entering a stationary defender's range)
- **`attacking` only updated for movers** — matches current spatialty behavior; stationary units' `attacking` flag is untouched (combat may set `fighting` instead)
- **No scratch buffer** — the O(n²) vec is small enough to allocate per-tick at game scale (tens to low hundreds of units)
- **`initialized` always `true`** — no more lazy initialization; every unit gets its `NearbyEnemies` component every tick
- **No `world_entity` parameter** — no debug timing recording needed for this simple system

### Step 2: Delete `src/game/systems/spatial_hash.rs`

Remove the entire file. No replacement needed — the hash is gone.

### Step 3: Delete `src/game/systems/spatialty.rs`

Remove the entire file. Its functionality is now in `nearby_enemies.rs`.

### Step 4: Update `src/game/systems/mod.rs`

```rust
// Remove: pub mod spatial_hash;
// Remove: pub mod spatialty;
// Add:    pub mod nearby_enemies;
```

### Step 5: Update `src/game/systems/death_cleanup.rs`

- Remove `use crate::game::systems::spatial_hash::SpatialHash;`
- Remove `spatial: &mut SpatialHash` parameter from `run()`
- Remove the `dead_positions` collection loop
- Remove the `spatial.remove()` loop
- Keep the NearbyEnemies cleanup (retain dead refs) and despawn logic
- Update doc comment: "Removes dead units from NearbyEnemies lists and despawns them"
- Update tests: remove `spatial` parameter and all SpatialHash references

### Step 6: Update `src/game/game.rs`

Changes to struct fields:
- Remove `use crate::game::systems::spatial_hash::SpatialHash;`
- Remove `spatial_hash: SpatialHash` field
- Remove `uninitialized_units: u32` field

Changes to constructor:
- Remove `spatial_hash: SpatialHash::new(),`
- Remove `uninitialized_units: 0,`

Changes to tick loop:
- Replace the conditional spatialty block with an unconditional nearby_enemies call:
  ```rust
  // Old: conditional spatialty call with spatial_hash, movers, etc.
  // New:
  tick_system!(
      "nearby_enemies",
      &mut self.world,
      self.world_entity,
      systems::nearby_enemies::run(&mut self.world)
  );
  ```
- Remove `uninitialized_units` decrement

Changes to unit recruitment:
- Remove `self.uninitialized_units += 1;` in `recruit_unit()`

Changes to death_cleanup call:
- Remove `&mut self.spatial_hash` parameter

Update `CombatHealth` doc comment:
- Change "how spatialty left the world" to "how nearby_enemies left the world"

### Step 7: Update `src/game/components.rs`

Update `NearbyEnemies` doc comment from "Maintained by the Spatialty system" to "Maintained by the nearby_enemies system".

### Step 8: Update `src/microbenchmarks.rs`

- Remove `use crate::game::systems::spatial_hash::SpatialHash;`
- Remove all `spatial_hash/*` benchmarks (insert, query_sparse, query_dense, query_buffer_reuse, remove_reinsert)
- Rewrite `systems/spatialty_*` benchmarks to use `systems::nearby_enemies::run()` instead of the old API:
  ```rust
  // Old: SpatialHash::new() + insert loop + spatialty::run(..., &mut spatial, &movers, ...)
  // New: nearby_enemies::run(&mut f.world)
  ```
- Rename `SpatialtyFixture` to `NearbyEnemiesFixture` (or keep the name)
- Update benchmark IDs: `systems/nearby_enemies_100`, etc.
- Remove `movement_scratch` references from the old spatialty benchmarks (no longer needed)

## Reuse

- `COMBAT_RANGE_SQ` from `src/game/constants.rs` — distance threshold
- `NearbyEnemies` component from `src/game/components.rs` — unchanged struct
- `UnitPosition`, `UnitOwner`, `UnitFlags` components — same as today
- `populate_nearby_enemies` pattern from `combat.rs` tests — the brute-force loop is basically this function, generalized
- Combat system already reads `NearbyEnemies` and does narrow-phase distance checks — no changes needed to combat

## Verification

1. **`cargo test`** — all existing tests should pass:
   - `spatialty.rs` tests → replaced by new `nearby_enemies.rs` tests
   - `death_cleanup.rs` tests → updated to not use SpatialHash
   - `combat.rs` tests → no changes (they use `populate_nearby_enemies` which is similar logic)

2. **`cargo test --release -- --nocapture`** — verify no regressions

3. **Microbenchmarks** — run `--bench` and compare old spatialty benchmarks vs new nearby_enemies benchmarks

4. **Game smoke test** — start a game, recruit units, move them, verify combat works

5. **New tests for `nearby_enemies.rs`** (port from spatialty tests):
   - Two nearby enemy units see each other
   - Same-owner units don't see each other
   - Mover with enemy nearby gets `attacking=true`
   - Mover without enemies gets `attacking=false` (moved but no enemy in range)
   - Stationary defender with attacker moving toward it: stationary unit's `attacking` is NOT set (only movers get `attacking` updated), but its `NearbyEnemies` is populated so combat can detect it
   - Three players all see each other
   - Dense cluster filters self and allies
   - Empty world doesn't panic
