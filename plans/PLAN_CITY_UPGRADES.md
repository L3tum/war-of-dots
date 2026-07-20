# City Upgrades & Supply System — Plan

## Context

Add an optional feature (off by default in the standard ruleset) for upgrading cities. Upgraded cities provide more income and more resupply capacity. The supply system models equipment logistics: units carry equipment above their "needed" baseline as a buffer, which gets consumed each tick. Cities resupply this equipment. When below the needed threshold, equipment effectiveness drops sharply. When fully depleted, units starve (rapid manpower attrition).

## Approach

### City Upgrade Tiers

Cities start at level 1. They can be upgraded (costing funds) to higher levels. Each level increases:
- **Income multiplier** — funds generated per tick
- **Supply output** — how much equipment resupply the city can distribute per tick
- **Supply range** — how far units can be and still receive supply

### Supply Model: Equipment as Supply

Supply is modeled as **carried equipment above the "needed" baseline**:

- Each unit type has equipment with a `needed` count (for 100% effectiveness) — implicitly the max_equipment minus a small buffer
- Units carry a buffer above needed (e.g., 105 rifles when 100 is needed)
- Supply-increasing equipment raises the buffer further (e.g., 120 rifles)
- `max_supply` = total equipment buffer above needed (carrying capacity)
- `supply` = current buffer remaining (decreases as equipment is consumed)

### Effectiveness Curve: Plateau Then Drop

- While `carried >= needed`: **100% effectiveness** (the buffer absorbs consumption)
- Below needed: **effectiveness = carried / needed** (linear drop from 100% to 0%)
- This means units retain full combat strength through the buffer zone, then degrade rapidly

### Supply Consumption (per tick, per unit)

- Equipment attrition naturally decreases equipment count each tick
- Base attrition: small decrease (march/operations)
- Fighting attrition: larger decrease (combat)
- This consumption draws down the buffer (`supply` decreases)

### Resupply from Cities (per tick)

- Each city has a **supply output budget** per tick (e.g., level 1 = 30, level 2 = 60, level 3 = 120)
- Budget represents total equipment items that can be distributed
- Budget is **shared among all units the city supplies** within range
- **Closest city** provides full contribution; **secondary cities** contribute at reduced rate proportional to distance
- A unit's total resupply = sum of contributions from all cities, capped by how much supply it can absorb
- **No resupply** during combat (unit.attacking) or when encircled
- Resupply increases equipment count (up to max_equipment), restoring the buffer
- **Resupply costs funds** — each equipment item resupplied costs its individual unit cost (defined per equipment type in unit config). This cost is the same as the per-item cost used during recruitment. Total resupply cost = sum of (items_resupplied × item_cost) across all equipment types.
- **Toggle: disable resupply** — in addition to "turn off healing" (stops manpower replenishment), units can also have equipment resupply disabled separately to save funds. A unit with both healing and resupply disabled will slowly degrade until destruction.

### Manpower as Carrying Capacity

- `men` / `max_men` determines how much equipment weight a unit can carry
- Unit type design: `max_men` must be sufficient to carry all `max_equipment` weight
- Prevents broken unit designs (e.g., 10 men carrying 1000 rifles)
- When supply is completely depleted → **starvation**: manpower attrition spikes (e.g., 3x normal rate)

### Supply Penalties

- **In buffer zone (supply > 0):** No penalties. Full effectiveness. Resupply costs funds per item.
- **Below needed (supply < 0):** Equipment effectiveness drops linearly. Org reinforcement rate reduced by 50%.
- **Fully depleted (supply at minimum):** Starvation — manpower attrition 3x normal. Equipment effectiveness at floor.
- **Resupply disabled:** Unit continues to consume supply (equipment attrition) but receives no resupply. Strategic choice to save funds when supply lines are unreliable or the unit is being sacrificed.

## Files to Modify

### Backend (Rust)

| File | Change |
|------|--------|
| `src/config.rs` | Add `CityUpgradeSettings` to `StandardRuleset` config. Add `supply_buffer_percent` per unit type. Update equipment parsing to support `name:count:cost_per_item` format. |
| `src/game/economy.rs` | Add `level`, `supply_output`, `supply_range` to `City`. Add `upgrade_city()` method. Modify `generate_funds()` for level scaling. |
| `src/game/game.rs` | Add `apply_supply()` tick phase. Add `upgrade_city()` game method. Add supply consumption per tick. |
| `src/game/unit.rs` | Wire `supply`/`max_supply` into combat effectiveness. Add equipment effectiveness modifier. Add starvation attrition. Add `resupply_enabled: bool` toggle. Add overload check. Add `cost_per_item: f32` to `Equipment` struct. |
| `src/server/protocol.rs` | Add `level` to `CityState`/`CityUpdate`. Add `UpgradeCity` and `ToggleResupply` client messages. Add `resupply_enabled` to `UnitState`/`UnitUpdate`. |
| `src/server/game_server.rs` | Handle `UpgradeCity` and `ToggleResupply` commands. Update city delta building for level changes. Deduct funds for resupply. |
| `src/llm/tools.rs` | Add `upgrade_city` and `toggle_resupply` tools. Handle in `tool_call_to_command`. |
| `src/llm/agent.rs` | Add `level` to `CityState` for LLM visibility. Add `supply`, `supply_percent`, and `resupply_enabled` to `UnitState`. |

### Frontend (TypeScript)

| File | Change |
|------|--------|
| `frontend/src/generated/protocol.ts` | Regenerate: `level` on CityState, `UpgradeCity`/`ToggleResupply` messages, `resupply_enabled`/`supply_percent` on UnitState |
| `frontend/src/main.ts` | Handle `UpgradeCity` and `ToggleResupply` client messages. Update delta for city level and unit resupply_enabled. |
| `frontend/src/game/renderer.ts` | Render city markers with level indicators (size/glow). Existing equipment bar already shows supply — add color transition (green to red) based on supply level vs needed threshold. |
| `frontend/src/ui/controls.ts` | Add upgrade button for selected owned city. Add toggle for equipment resupply (separate from healing). Show supply status. |
| `frontend/src/game/game_state.ts` | Track city level, unit supply percentage, and resupply_enabled state. |

## Proposed Default Values (standard ruleset, feature OFF)

```toml
[standard_ruleset.city_upgrades]
enabled = false
max_level = 3
upgrade_costs = [0, 200, 500]         # cost in funds to reach each level (index = level)
income_multipliers = [1.0, 1.5, 2.5]  # funds per tick multiplier per level
supply_output = [30, 60, 120]         # total supply budget per tick per level
supply_range = [5.0, 8.0, 12.0]       # max distance for supply in grid cells
secondary_city_decay = 0.5            # multiplier for non-closest city contribution
resupply_rate_per_unit = 2.0          # max supply restored per unit per tick from one city

[unit_type supply settings — added to each unit type definition]
supply_buffer_percent = 10            # extra equipment carried above "needed" (10% = 105 rifles for 100 needed)

# Equipment definitions now include per-item cost (used for recruitment AND resupply):
# Old format: equipment = ["rifles:100", "radios:10"]
# New format: equipment = ["rifles:100:0.5", "radios:10:2.0"]  (name:count:cost_per_item)
# recruitment_cost is derived: sum(count * cost_per_item) + base manpower cost
```

### Equipment Effectiveness Calculation

```
needed_amount = max_equipment * (1.0 - supply_buffer_percent / 100.0)
               e.g., for 100 rifles with 10% buffer: needed = 90
               
current_effectiveness = 
  if carried >= needed: 1.0 (full)
  else: carried / needed (linear drop)
```

### Supply Tick Flow

```
for each unit:
  1. Consume supply: equipment count -= attrition_rate (per equipment type)
  2. If not in combat, not encircled, and resupply_enabled:
     - Find all friendly cities in range
     - For each city, compute contribution (budget / units_served * distance_factor)
     - Resupply: equipment count += total_resupply (capped at max_equipment, proportional across types)
     - Deduct funds: player_funds -= sum(items_resupplied[type] * cost_per_item[type])
  3. Compute effectiveness: for each equipment type, effectiveness = plateau_then_drop(carried, needed)
  4. If supply fully depleted (any equipment at 0): starvation — manpower -= 3x attrition
  5. Org reinforcement: if supply below needed, apply 50% malus to reinforcement_rate
```

## Steps

### Phase 1: Data Model & Config ✅ DONE
- [x] Add `CityUpgradeSettings` struct to `config.rs` with all supply/upgrade parameters
- [x] Add `supply_buffer_percent` field to unit type config (default 10%)
- [x] **Add per-equipment cost** to equipment definition format: change `"rifles:100"` to `"rifles:100:0.5"` (name:count:cost_per_item). Update parsing in `Equipment::from_str()` and config loading.
- [x] Derive `recruitment_cost` from sum of (count × cost_per_item) + base manpower cost (or keep explicit as override)
- [x] Add `level: u32`, `supply_output: f32`, `supply_range: f32` fields to `City` struct
- [x] Add `upgrade_city(city_id, owner)` method to `Economy` — checks cost, deducts funds, increments level
- [x] Wire city level into `generate_funds()` — apply `income_multipliers[level]`
- [x] Add `resupply_enabled: bool` field to `Unit` struct (default true, separate from `healing`)
- [x] Store `cost_per_item` in `Equipment` struct for use during resupply

### Phase 2: Supply Consumption & Effectiveness ✅ DONE
- [x] In `Unit`, compute `needed_amount` per equipment type from `max_equipment` and buffer %
- [x] Add `equipment_effectiveness(equipment_name)` method — plateau-then-drop curve
- [x] Modify `soft_attack()`, `soft_defense()`, `hard_attack()`, `hard_defense()` to multiply by equipment effectiveness
- [x] Add supply consumption in game tick: equipment attrition draws down carried amounts
- [x] Add starvation check: if any critical equipment at 0, apply 3x manpower attrition
- [x] Add overload check: if total equipment weight exceeds manpower capacity, apply speed penalty and +50% base attrition

### Phase 3: City Resupply System ✅ DONE
- [x] Add `apply_supply()` to game tick — runs after movement, before combat
- [x] For each non-combat, non-encircled unit with `resupply_enabled`: find all friendly cities within supply_range
- [x] Sort cities by distance, compute each city's per-unit contribution:
  - `per_unit_budget = city.supply_output / units_being_supplied_by_this_city`
  - `contribution = per_unit_budget * distance_factor * resupply_rate_per_unit`
- [x] Apply resupply: increase equipment counts (proportional across equipment types, up to max)
- [x] **Deduct funds** for resupply using per-equipment costs: `cost = sum(items_resupplied[type] * equipment.cost_per_item[type])`
- [x] Track per-city supply load (how many units each city is serving) for budget division

### Phase 4: Protocol & Server Commands ✅ DONE
- [x] Add `UpgradeCity { city_id: String }` and `ToggleResupply { unit_id: String, enabled: bool }` to `ClientMessage` enum in `protocol.rs`
- [x] Add `level: u32` field to `CityState` and `CityUpdate`
- [x] Add `resupply_enabled: bool` and `supply_percent: f32` to `UnitState` and `UnitUpdate`
- [x] Add `Command::UpgradeCity(String)` and `Command::ToggleResupply(String, bool)` variants in `game_server.rs`
- [x] Handle in game_server.rs command loop: call `game.upgrade_city()` and `game.toggle_resupply()`
- [x] Update city delta diff to detect level changes
- [x] Update unit delta diff to detect `resupply_enabled` changes
- [x] Regenerate protocol types (schema + TypeScript)

### Phase 5: LLM Agent Tools ✅ DONE
- [x] Add `upgrade_city` and `toggle_resupply` tool definitions in `tools.rs`
- [x] Handle both in `tool_call_to_command`
- [x] Add `level` to LLM `CityState` struct in `agent.rs`
- [x] Add `supply`, `supply_percent`, and `resupply_enabled` to LLM `UnitState` struct

### Phase 6: Frontend ✅ DONE
- [x] Update frontend protocol types (auto-generated)
- [x] Update Unit class to include new fields (supplyPercent, resupplyEnabled)
- [x] Update connection delta handler for new fields
- [x] The existing equipment bar on units already shows supply level (via supplyPercent)

### Phase 7: Remaining TODO
- [ ] Add UI for upgrade button for selected owned city
- [ ] Add UI toggle for equipment resupply (separate from healing toggle)
- [ ] Add starvation check (3x attrition at 0 supply)
- [ ] Add overload check (speed penalty +50% attrition when equipment > capacity)
- [ ] Render city markers with level indicators (size scaling)
- [ ] Tune default values for balance
- [ ] Test & verify supply math, plateau-then-drop, upgrade flow, resupply costs
  - Closest city: `distance_factor = 1.0`; others: `distance_factor = secondary_city_decay * (closest_dist / this_dist)`
- [ ] Apply resupply: increase equipment counts (proportional across equipment types, up to max)
- [ ] **Deduct funds** for resupply using per-equipment costs: `cost = sum(items_resupplied[type] * equipment.cost_per_item[type])`
- [ ] Track per-city supply load (how many units each city is serving) for budget division

### Phase 4: Protocol & Commands
- [ ] Add `UpgradeCity { city_id: String }` and `ToggleResupply { unit_id: String, enabled: bool }` to `ClientMessage` enum in `protocol.rs`
- [ ] Add `level: u32` field to `CityState` and `CityUpdate`
- [ ] Add `resupply_enabled: bool` and `supply_percent: f32` to `UnitState` and `UnitUpdate`
- [ ] Add `Command::UpgradeCity(String)` and `Command::ToggleResupply(String, bool)` variants in `game_server.rs`
- [ ] Handle in game_server.rs command loop: call `game.upgrade_city()` and `game.toggle_resupply()`
- [ ] Update city delta diff to detect level changes
- [ ] Update unit delta diff to detect `resupply_enabled` changes

### Phase 5: LLM Agent Integration
- [ ] Add `upgrade_city` and `toggle_resupply` tool definitions in `tools.rs`
- [ ] Handle both in `tool_call_to_command`
- [ ] Add `level` to LLM `CityState` struct in `agent.rs`
- [ ] Add `supply`, `supply_percent`, and `resupply_enabled` to LLM `UnitState` struct

### Phase 6: Frontend ✅ DONE
- [x] Regenerate protocol types
- [x] Handle `UpgradeCity` and `ToggleResupply` messages from client in connection handlers
- [ ] Render city markers with level indicators (size scaling)
- [x] The existing equipment bar on units already shows supply level via supplyPercent
- [x] Add upgrade button for selected owned city (disabled if can't afford or already max level)
- [x] Add toggle for equipment resupply (separate from healing toggle)
- [x] Update game state to track city level, unit supply percentage, and resupply_enabled

### Phase 7: Remaining
- [ ] Render city markers with level indicators on the map
- [ ] Tune default values for balance
- [ ] Integration testing

## Verification

- **Manual testing:** Run game with feature enabled, upgrade a city, verify increased income and supply radius
- **Supply edge cases:** Test unit equidistant between two cities, test encircled unit, test unit beyond range, test combat unit not getting resupply
- **Equipment effectiveness:** Verify plateau zone (full effectiveness with buffer) and sharp drop below needed
- **Resupply costs:** Verify funds deducted match per-equipment costs (e.g., resupplying 5 rifles at 0.5 each = 2.5 funds)
- **Toggle resupply:** Disabling stops resupply and fund drain, re-enabling restores it
- **LLM agent:** Verify LLM can call upgrade_city and toggle_resupply tools and they execute correctly
- **Frontend:** Verify city level is displayed, upgrade button works, equipment bar color reflects supply level, resupply toggle works

## Design Decisions

- **Upgrade is instant** — no construction timer. Simpler and faster-paced.
- **Cannot downgrade** — one-way progression.
- **Supply display** — the existing equipment bar on units IS the supply indicator (equipment count = supply level). No additional bar needed. Color shifts from green (full) to red (depleted).
- **City rendering** — size scales with level, label shows level number
- **Equipment capture** — captured equipment counts toward the carrying unit's supply buffer
- **Manpower-carrying capacity** — soft guidance with penalties. Overloaded units (equipment weight exceeds manpower capacity) can exist but suffer reduced speed and higher base attrition. This allows flexible custom unit designs while discouraging broken combinations.
