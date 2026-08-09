# Plan: Fix Terrain Asymmetry, Attrition Modeling, and `moved` Flag Ownership

## Context

Three issues were found in `combat.rs` during code review:

1. **Terrain asymmetry**: The terrain modifier is looked up only at the attacker's position (`pos1`), so both units get the same modifier even if they stand on different terrain types. In a hills-vs-plains engagement, both sides would get the hills modifier — which doesn't make sense. Each unit should use the terrain it stands on.

2. **Unmotivated attrition values**: Organisation loss is a flat `10.0` per engagement regardless of men lost. Equipment attrition uses a magic `0.05` multiplier. Meanwhile, `UnitType.fighting_attrition` already exists in the config (e.g., `1.0` for infantry) but is **never used in combat** — only by `attrition.rs`. Org loss should scale with men lost, and equipment attrition should use `fighting_attrition` from the unit type.

3. **`moved` flag ownership**: `combat.rs` currently clears `flags.moved = false` for all units at the end of combat. This flag is set by `movement.rs` and read by `combat.rs`, so movement should own its full lifecycle (set **and** clear). Currently, combat has a stray cleanup responsibility that doesn't belong to it.

---

## Approach

### 1. Pass `&GameSettings` into combat

The combat system currently takes `(world, world_entity, spatial, scratch)`. Add `&GameSettings` so it can look up each unit's type-specific `fighting_attrition`, mirroring the pattern already used by `attrition.rs`.

Precompute a `HashMap<&str, &UnitType>` lookup once per tick (same pattern as `attrition.rs`) so per-engagement lookups are O(1).

### 2. Per-unit terrain modifier

Instead of one `terrain_mod` computed at `pos1`, look up terrain at both `pos1` and `pos2`, compute two modifiers, and apply each to its owner's damage:

```rust
// Before: one shared modifier
let terrain_mod = terrain_mod_for(pos1);
let dmg_to_defender = a_eff * terrain_mod * 5.0;
let dmg_to_attacker = d_eff * terrain_mod * 5.0;

// After: per-unit modifiers
let a_terrain_mod = terrain_mod_for(pos1);  // attacker's terrain
let d_terrain_mod = terrain_mod_for(pos2);  // defender's terrain
let dmg_to_defender = a_eff * a_terrain_mod * 5.0;   // attacker hits harder from cover
let dmg_to_attacker = d_eff * d_terrain_mod * 5.0;   // defender counters from their ground
```

This models: a unit fighting **from** hills (defensive terrain) attacks with more strength because the terrain benefits them.

### 3. Organisation loss scales with men lost

Replace flat `-10.0` org with a proportional loss:

```rust
// How severe was the engagement for this unit?
let men_lost = s.men - new_men;
let men_ratio = men_lost / s.men.max(0.1);  // fraction of strength lost
let org_loss = 10.0 * men_ratio;
```

Small scratch → tiny org loss. Losing half your men → ~50 org loss.

### 4. Equipment attrition uses `fighting_attrition`

Replace the magic `0.05` rate with a lookup of the unit type's `fighting_attrition`. The `attrition_from` helper currently takes `(equipment, dmg, attr_rate)`. The `attr_rate` should come from the unit type config, not a hardcoded constant.

Since the combat system doesn't have access to `UnitTypeId` in the main loop (it's not in the candidate snapshot), we need to fetch it per engagement — which is cheap (one ECS read per unit per engagement).

```rust
// Fetch unit type for attrition lookup
let attr_rate = match (world.get::<&UnitTypeId>(*entity1), unit_types_by_name) {
    (Ok(ut), map) => map.get(ut.name.as_str())
        .map(|ut| ut.fighting_attrition)
        .unwrap_or(1.0),
    _ => 1.0,
};
```

### 5. Move `moved` flag clearing from combat to movement

Currently:
1. `movement.rs` sets `moved = true` for units that moved (Phase 2)
2. `combat.rs` reads `moved` as a candidate filter
3. `combat.rs` clears `moved = false` for ALL units at the end of combat (Phase 5)

Proposed:
1. `movement.rs` **clears** `moved = false` for ALL units at the **start** of its run
2. `movement.rs` sets `moved = true` only for units that moved (unchanged)
3. `combat.rs` reads `moved` as a candidate filter, **does NOT clear it**

This works because `movement.rs` runs before `combat.rs` in `tick_game()` order. Clearing at the start of movement ensures no stale flags carry over from the previous tick. Movement now fully owns the `moved` flag lifecycle.

### 6. Update tests

All existing tests pass empty equipment and use hardcoded assertions on men/org values. These need updating:

- Tests that use `make_unit()` don't spawn `UnitTypeId` → need to add `UnitTypeId` to test units
- Numerical assertions (e.g., `985.0..1000.0` range) may shift slightly
- Add new tests for cross-terrain engagement and attrition scaling

---

## Files to Modify

| File | Change |
|------|--------|
| `src/game/systems/combat.rs` | Main changes: new param, per-unit terrain, attrition model, remove `moved` flag clearing, test updates |
| `src/game/systems/movement.rs` | Add `moved = false` clear at start of run |
| `src/game/game.rs` | Pass `&self.settings` to `combat::run()` |
| `src/game/systems/combat.rs` (tests) | Add `UnitTypeId` to test units, update assertions, add new tests |

## Reuse

| Existing code | Where | How reused |
|---------------|-------|------------|
| `attrition.rs` unit-type lookup pattern | `attrition.rs` | Copy the `HashMap<&str, &UnitType>` precompute + lookup pattern |
| `UnitType.fighting_attrition` | `game/economy.rs` | Already defined in config, just unused in combat |
| `TerrainGrid.get()` | `components.rs` | Already used for terrain lookup; reuse for pos2 |
| `UnitTypeId` component | `components.rs` | Already on all units; combat just doesn't query it yet |

---

## Steps

### Step 1: Add `GameSettings` parameter to combat

- [ ] Change `combat::run()` signature to accept `settings: &GameSettings`
- [ ] Update call site in `game.rs` to pass `&self.settings`

### Step 2: Precompute unit type lookup map

- [ ] Add `use crate::game::economy::UnitType;` and `use std::collections::HashMap;` (HashMap already imported)
- [ ] Build `unit_types_by_name: HashMap<&str, &UnitType>` at start of `run()`, same pattern as `attrition.rs`

### Step 3: Fix terrain asymmetry

- [ ] After fetching `pos2`, look up terrain at both `pos1` and `pos2`
- [ ] Extract terrain modifier into a helper function (or inline both)
- [ ] Apply `a_terrain_mod` to attacker's damage output and `d_terrain_mod` to defender's counter-attack

### Step 4: Scale organisation loss with men lost

- [ ] In both ECS write blocks (attacker and defender), replace flat `-10.0` with proportional calculation
- [ ] Use original `s1.men`/`s2.men` (pre-damage) as the denominator

### Step 5: Use `fighting_attrition` for equipment attrition

- [ ] Fetch `UnitTypeId` for both entities in the engagement (one extra ECS read each)
- [ ] Look up `fighting_attrition` from the unit type map
- [ ] Pass the correct rate into `attrition_from()` instead of `0.05`

### Step 6: Update existing tests

- [ ] Add `UnitTypeId { name: "infantry".to_string() }` to `make_unit()` and `make_unit_with_equipment()` helper functions
- [ ] Update numerical assertions in `combat_all_adjacent_units_engage_with_one_attack_each` (org will now be lower since it scales)
- [ ] Update `defender_takes_compounding_damage` assertion
- [ ] Update `equipment_attrition_compounds` assertion (now uses `fighting_attrition = 1.0` instead of `0.05`)

### Step 7: Move `moved` flag clearing from combat to movement

- [ ] In `movement.rs`, add a loop at the **start** of `run()` that sets `flags.moved = false` for all entities with `UnitFlags`
- [ ] In `combat.rs`, **remove** Phase 5 (the loop that clears `moved = false` for all units and its debug timer)

### Step 8: Add new tests

- [ ] **Cross-terrain test**: Attacker on hills, defender on plains → attacker's damage should be higher
- [ ] **Attrition scaling test**: Unit losing 50% men should lose ~50 org, unit losing 5% men should lose ~5 org

---

## Verification

1. `cargo test combat` — all existing tests pass with updated assertions
2. `cargo test` — full test suite passes (no regressions in other systems)
3. `cargo build --release` — compiles cleanly with no warnings
4. Manual: run a game tick with units on different terrain types, verify damage values differ correctly
5. Verify tick order is correct: `movement` runs before `combat` in `game.rs::tick_game()` — if `moved` is cleared at the start of movement and read by combat, this order must be preserved
