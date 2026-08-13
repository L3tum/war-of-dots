# Upgrade hecs 0.10.5 → 0.11.0

## Context

The project currently uses `hecs = "0.10"` (resolving to 0.10.5). Version 0.11.0 was released on Jan 10, 2023 and contains 3 breaking changes, several non-breaking API improvements, and a notable performance improvement (query caching).

## hecs 0.11.0 Release Summary

### Breaking Changes
1. **Query iterators now yield `Q::Item` instead of `(Entity, Q::Item)`** — Entity now implements `Query`, so it must be requested explicitly. This is the biggest change.
2. **`World::satisfies` now returns `bool` rather than `Result`** — Not used in this codebase.
3. **`World::query_one` no longer returns a `Result`** — Errors are yielded by `QueryOne::get` instead. **Not used in this codebase** (we only use `query_one_mut` which is unaffected).

### Non-Breaking Changes / New Features
- `TypeIdMap` and `TypeInfo` are now public (facilitates cloning World)
- `ColumnBatchBuilder::writer` now takes `&self` (not used)
- `BatchWriter` now implements `Drop` (not used directly)
- Renamed `get_many_mut` → `get_disjoint_mut` (not used)
- **Query caching**: "Set-up work is now automatically cached for most uses of queries, dramatically improving the performance of small queries in worlds with many archetypes." — **Potential perf benefit**

## Impact Analysis

### Breaking Change #1: Query Iterators (MAJOR IMPACT)

**Every** `.query::<...>().iter()` call in the codebase is affected. The iterator previously yielded `(Entity, Q::Item)` tuples. Now it yields just `Q::Item`. To get the entity, you must explicitly include `Entity` in the query:

```rust
// BEFORE (0.10):
for (entity, (pos, owner)) in world.query::<(&UnitPosition, &UnitOwner)>().iter() { ... }

// AFTER (0.11):
for (entity, pos, owner) in world.query::<(Entity, &UnitPosition, &UnitOwner)>().iter() { ... }
```

This is a pervasive change — there are **~17 query iteration sites** across the codebase, plus several `.filter()`/`.map()` chains.

### Breaking Changes #2 and #3: NO IMPACT
- `World::satisfies` is not used anywhere.
- `World::query_one` is not used (only `query_one_mut` is used, which is unaffected).

## Files Affected

### High Impact (multiple query sites):
1. **`src/game/game.rs`** — 3 query iterations (lines 201, 330, and `query_all_units()`)
2. **`src/game/systems/combat.rs`** — 2 query iterations (lines 187, 588 in tests)
3. **`src/game/systems/movement.rs`** — 3 query iterations (lines 54, 74, 118)

### Medium Impact (single query site each):
4. **`src/game/systems/supply.rs`** — 1 query iteration (line 114)
5. **`src/game/systems/spatialty.rs`** — 1 query iteration (line 77)
6. **`src/game/systems/city_capture.rs`** — 1 query iteration (line 24)
7. **`src/game/systems/surrender.rs`** — 1 query iteration (line 21)
8. **`src/game/systems/water.rs`** — 1 query iteration (line 26)
9. **`src/game/systems/death_cleanup.rs`** — 1 query iteration (line 43)
10. **`src/microbenchmarks.rs`** — 2 query iterations (lines 911, 931)

### Low Impact (no query iteration):
- **`src/game/world_ext.rs`** — Uses `query_one_mut` only (not affected)
- **`src/game/components.rs`** — Component definitions only
- **`Cargo.toml`** — Version bump

## Approach

For each query iteration site, the migration follows the same pattern:

1. **If the entity is used**: Add `Entity` as the first element of the query tuple. The iterator now yields `(Entity, component1, component2, ...)` directly — no nested tuples.
2. **If the entity is NOT used**: Add `Entity` and ignore it in the pattern.

### Pattern Examples

**Before (entity used):**
```rust
for (entity, (pos, owner)) in world.query::<(&UnitPosition, &UnitOwner)>().iter() {
```
**After:**
```rust
for (entity, pos, owner) in world.query::<(Entity, &UnitPosition, &UnitOwner)>().iter() {
```

**Before (entity ignored):**
```rust
for (_, flags) in world.query::<&mut UnitFlags>().iter() {
```
**After:**
```rust
for (_, flags) in world.query::<(Entity, &mut UnitFlags)>().iter() {
```

**Before (chain with entity):**
```rust
.map(|(e, (pos, owner))| (e, *pos, owner.player_id.clone()))
```
**After:**
```rust
.map(|(e, pos, owner)| (e, *pos, owner.player_id.clone()))
```

## Steps

- [x] 1. Update `Cargo.toml`: change `hecs = "0.10"` to `hecs = "0.11"`
- [x] 2. Update `src/game/game.rs` — 6 query sites (3 planned + 3 discovered during compile):
  - `TickContext::build` (line 201): add `Entity` to query, flatten tuple
  - `combat_health()` (line 330): add `Entity` to query (entity not used)
  - `query_all_units()`: add `Entity` to query, flatten tuple
  - `is_player_active()`: add `Entity` to query (entity not used)
  - `determine_winner()`: add `Entity` to query (entity not used)
  - `track_unit_losses()`: add `Entity` to query (entity not used)
- [x] 3. Update `src/game/systems/combat.rs` — 2 query sites:
  - Main candidate collection (line 187): add `Entity`, flatten all tuple patterns
  - Test `populate_nearby_enemies` (line 588): add `Entity`, flatten tuple
- [x] 4. Update `src/game/systems/movement.rs` — 3 query sites:
  - Clear flags (line 54): add `Entity` to query
  - Collect changes (line 74): add `Entity`, flatten tuple
  - Perfect-movement positions (line 118): add `Entity`, flatten tuple
- [x] 5. Update `src/game/systems/supply.rs`:
  - Single pass (line 114): add `Entity`, flatten tuple
- [x] 6. Update `src/game/systems/spatialty.rs`:
  - Find uninitialized (line 77): add `Entity`, flatten tuple
- [x] 7. Update `src/game/systems/city_capture.rs`:
  - Collect units (line 24): add `Entity` (not used)
- [x] 8. Update `src/game/systems/surrender.rs`:
  - Find surrendering (line 21): add `Entity`
- [x] 9. Update `src/game/systems/water.rs`:
  - Collect water transitions (line 26): add `Entity`
- [x] 10. Update `src/game/systems/death_cleanup.rs`:
  - Clean NearbyEnemies (line 43): add `Entity` (not used)
- [x] 11. Update `src/microbenchmarks.rs`:
  - Benchmark queries (lines 911, 931): add `Entity` to query
- [x] 12. Update comment in `src/game/components.rs` line 6: change "In hecs 0.10" to "In hecs 0.11"

## Verification

- [x] `cargo check` — compile clean
- [x] `cargo test` — all 349 tests pass (164 unit + 164 bin + 21 integration)
- [x] `cargo build --release` — compiles cleanly (warnings are pre-existing, not from this change)
- [x] `cargo check --features debug-overlay` — clean
- [x] `cargo check --features perfect-movement` — clean
- [x] `cargo check --features microbenchmarks` — clean

## Summary

**Total files modified: 11** (Cargo.toml + 10 source files)
**Total query sites updated: 22** (17 planned + 5 discovered during compile)
**All 349 tests pass. All features compile clean.**

## Risks & Concerns

1. **Scope of change**: ~17 query sites need modification. Mechanical but tedious — easy to miss a site or introduce a typo.
2. **No performance regression expected**: The query caching in 0.11 should actually help performance for small queries.
3. **`Entity::from_bits` is already fallible**: We already use `.unwrap()` on line 243 of microbenchmarks.rs, which has been correct since 0.7.0.
4. **`query_one_mut` is NOT affected**: This codebase heavily uses `query_one_mut` (~30 call sites), which still returns `Result`. Only the iterator pattern changed.
5. **MSRV**: hecs 0.10 raised MSRV to 1.65; 0.11 doesn't appear to raise it further.
