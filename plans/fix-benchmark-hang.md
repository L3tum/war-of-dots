# Plan: Benchmark Fixes

## Context

### 1. Missing `--benchmark` flag in `run-benchmark.sh`

The benchmark hangs indefinitely when `--benchmark` is missing from `run-benchmark.sh`. Without it, the CLI parser sets `cli.benchmark = false`, so the code takes the server path instead of the benchmark path. The server starts listening on a port and hangs forever waiting for client connections. The `--bench-*` args are silently accepted by clap but ignored since the benchmark branch is never entered.

The `run-benchmark.sh` script has already been fixed to include `--benchmark`. But we should prevent this from happening again.

### 2. No validation when bench args are provided without `--benchmark`

Currently, if bench parameters (`--bench-ticks`, `--bench-seed`, etc.) are given but `--benchmark` is omitted, the program silently starts the server and hangs. There's no warning or error to tell the user something is wrong.

## Approach

Add a check in `main.rs` that warns the user if any bench parameters were provided (non-default values) but `--benchmark` is missing. This prevents the silent-hang confusion.

## Files to modify

- `src/main.rs` — add validation after `Cli::parse()` that checks if bench args have non-default values without `--benchmark`
- `src/game/systems/supply.rs` — add organisation recovery inside the main unit loop
- `src/game/systems/reinforce.rs` — delete this file
- `src/game/systems/mod.rs` — remove `pub mod reinforce`
- `src/game/game.rs` — remove `tick_system!("reinforce", ...)` from `tick_game()`

## Steps

- [x] Add a validation check in `main.rs` that detects bench args with non-default values when `--benchmark` is false
- [x] Print a helpful error message and exit with code 1
- [x] Verify the error triggers correctly when `--benchmark` is omitted but bench args are provided

## Verification

```bash
# Should fail with a clear error message:
cargo run --profile release --features debug-overlay -- --bench-seed 42 --bench-ticks 10

# Should work normally:
cargo run --profile release --features debug-overlay -- --benchmark --bench-seed 42 --bench-ticks 2

# Should work normally (server mode, no bench args):
cargo run --profile release --features debug-overlay --
```

---

## Merge Reinforce into Supply

Reinforce iterates all units and modifies `UnitStats` — just like supply does. Merging eliminates one full ECS iteration per tick.

### What reinforce.rs does
```rust
// For each unit, compute supply malus and restore org
let supply_pct = supply_percent(&equipment, &max_equipment);
let malus = if supply_pct > 50.0 { 1.0 } else { supply_pct / 50.0 };
let reinforce = stats.reinforcement_rate * malus;
let new_org = (stats.organisation + reinforce).min(stats.max_organisation);
```

### What supply.rs already does
- Iterates all units with `(UnitTypeId, UnitOwner, UnitStats, UnitEquipment, UnitFlags, UnitWaterTransition)`
- Already mutates `UnitStats` (men, and water org drain)
- Already computes `equipment_ratio()` — essentially the same as reinforce's `supply_percent()`

### Merge plan
Inside supply.rs, after the men attrition/supply block, add organisation recovery:
```rust
// Organisation recovery (moved from reinforce.rs)
// Units in combat (fighting or attacking) do NOT recover org.
if !flags.fighting && !flags.attacking {
    let supply_pct = equipment_ratio(equipment) * 100.0;
    let malus = if supply_pct > 50.0 { 1.0 } else { supply_pct / 50.0 };
    let unit_reinforce_rate = unit_types_by_name
        .get(utype.name.as_str())
        .map(|u| u.reinforcement_rate)
        .unwrap_or(5.0);
    let reinforce = unit_reinforce_rate * malus;
    let new_org = (new_stats.organisation + reinforce).min(new_stats.max_organisation);
    if (new_org - new_stats.organisation).abs() > f32::EPSILON {
        new_stats.organisation = new_org;
        changed = true;
    }
}
```

Note: the current reinforce.rs does NOT check combat flags (it only queries `UnitStats` + `UnitEquipment`), but the intended design is that fighting/attacking units don't recover org. The merged version adds this correct behavior.

### Files to modify

- `src/game/systems/supply.rs` — add organisation recovery inside the main unit loop
- `src/game/systems/reinforce.rs` — delete this file
- `src/game/systems/mod.rs` — remove `pub mod reinforce`
- `src/game/game.rs` — remove `tick_system!("reinforce", ...)` from `tick_game()`

### Steps

- [x] Add organisation recovery logic inside supply.rs unit loop (after men attrition block)
- [x] Update supply.rs tests to verify org recovery behavior
- [x] Delete `src/game/systems/reinforce.rs`
- [x] Remove `pub mod reinforce` from `src/game/systems/mod.rs`
- [x] Remove reinforce tick from `src/game/game.rs`
- [x] Run benchmarks to confirm no regression and slight speedup
