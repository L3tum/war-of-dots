# Plan: Apply `unit_types_index` to `movement.rs`

## Context

`plans/PERF_ANAL.md` analyzed four HECS ECS optimization techniques plus a set of
"real bottleneck" follow-ups. On review, most of the document is **already
implemented** (supply + combat `unit_types_index`, `target-cpu=x86-64-v3` in
`.cargo/config.toml`, the `src/microbenchmarks.rs` harness, the supply single-pass
`&mut` rewrite, and the combat u8 owner lookups). The four ECS techniques are
correctly rejected (single steady-state archetype — no room to gain).

The one remaining, safe, mechanical item is the document's **Priority 7 "next
candidate"**: `movement.rs` still builds a per-tick
`unit_type_speeds: HashMap<&str, f32>` to turn a unit's `UnitTypeId.name` into its
speed. `Game.settings.unit_types` is immutable for the `Game`'s lifetime, so this
per-tick map build is pointless. The fix reuses the already-cached
`Game.unit_types_index: HashMap<String, usize>` (name → index into
`settings.unit_types`) the same way `supply.rs` and `combat.rs` already do.

This is a **strict simplification** (removes a per-tick heap allocation + map
build, shrinks the per-unit lookup, and makes movement consistent with the other
two systems). It is *not* expected to be a measurable perf win on its own (a
3-entry HashMap build per tick is tiny) — the value is consistency + reduced
allocation. Scope chosen by the user: **just this safe win** (no combat inner-loop
rework, no dedup/streaming changes).

## Reuse (existing code — no new utilities)

- `Game.unit_types_index: HashMap<String, usize>` — built once in
  `Game::new` (src/game/game.rs), already threaded into `supply::run` and
  `combat::run`. Reuse it verbatim for movement.
- `GameSettings.unit_types[i].speed: f32` — the value looked up today; reached via
  the index instead of a separate name→speed map.
- The exact index-construction idiom already used in `supply.rs`/`combat.rs` tests
  and `Game::new`: `settings.unit_types.iter().enumerate().map(|(i,ut)|(ut.name.clone(), i)).collect()`.

## Files to modify

1. **`src/game/systems/movement.rs`**
   - `run(...)`: add param `unit_types_index: &HashMap<String, usize>`; **delete**
     the per-tick `let unit_type_speeds: HashMap<&str, f32> = ...collect();` block
     (movement.rs:65–68). Pass `unit_types_index` into `compute_movement` (the
     `&unit_type_speeds` arg at :95).
   - `compute_movement(...)`: change the `unit_type_speeds: &HashMap<&str, f32>`
     param (movement.rs:162) to `unit_types_index: &HashMap<String, usize>`.
     Replace the lookup (movement.rs:171–174):
     ```rust
     let unit_type_speed = unit_types_index
         .get(&utype.name)
         .map(|&i| settings.unit_types[i].speed)
         .unwrap_or(1.0);
     ```
     (`settings: &GameSettings` is already a param, so `settings.unit_types[i]` is
     in scope. `utype: &UnitTypeId`, `utype.name: String` → `get(&utype.name)` works.)
   - Tests: replace the `speeds_map(settings) -> HashMap<&str, f32>` helper
     (movement.rs:247–252) with an equivalent
     `ut_index(settings) -> HashMap<String, usize>` helper, and swap the 5
     `&speeds_map(&settings)` call sites (movement.rs:381, 420, 465, 511, 553) to
     `&ut_index(&settings)`. Keep `#[allow(clippy::too_many_arguments)]` on
     `compute_movement` (arg count unchanged).

2. **`src/game/game.rs`**
   - `tick_system!("movement", ...)` call (game.rs:530–539): pass
     `&self.unit_types_index` as the new trailing arg to `systems::movement::run`.

3. **`src/microbenchmarks.rs`**
   - The three movement closures (microbenchmarks.rs:902, 923, 944) call
     `crate::game::systems::movement::run(&mut f.world, f.world_entity, &settings,
     &mut scratch)`. In `bench_systems()` a `ut_index` (`HashMap<String, usize>`)
     is already built in un-timed setup and passed to supply/combat — pass `&ut_index`
     into each movement closure.

## Steps

- [ ] `movement.rs::run`: add `unit_types_index` param, remove the per-tick
      `unit_type_speeds` HashMap build, thread the index into `compute_movement`.
- [ ] `movement.rs::compute_movement`: swap param + lookup to index into
      `settings.unit_types[i].speed`.
- [ ] `movement.rs` tests: swap `speeds_map` helper for a `ut_index` helper and
      update the 5 call sites.
- [ ] `game.rs`: pass `&self.unit_types_index` at the movement `tick_system!` site.
- [ ] `microbenchmarks.rs`: pass `&ut_index` in the 3 movement bench closures.
- [ ] `grep` to confirm no remaining `unit_type_speeds` / `speeds_map` references.

## Verification

- `cargo build --release` — compiles clean.
- `cargo test movement` — all `movement::tests` pass with **unchanged assertions**
  (speed on plains/hills/forest/mountain/water, waypoint reach, org drain, etc. —
  the speed value is identical whether read from the old map or
  `settings.unit_types[i].speed`).
- `cargo test` (full) — green (unit + integration).
- `cargo clippy --all-targets --features microbenchmarks` — clean, no new warnings
  in movement.rs / game.rs / microbenchmarks.rs.
- `cargo check --all-targets --features microbenchmarks` — clean.
- **Benchmark (expected ~flat, small p1_setup drop):**
  `./run-benchmark.sh 60 --bench-prev=<latest report>` → confirm identical game
  behavior (units remaining ~baseline) and `systems/movement_*` flat or marginally
  faster, no regression. This confirms the change is a safe simplification, not a
  perf risk.

## Out of scope (deferred, per user choice)

- **Priority 1** — combat inner-loop `world.get()` pre-grouping (bigger effort; only
  pursue after the microbenchmark harness shows combat lookups truly dominate; the
  per-unit `break` after one engagement makes the doc's 20-50% estimate suspect).
- **Priority 2** — combat `HashSet` → sorted Vec/bitmap (low value; sets are small).
- **Priority 6** — streaming / skipping `.collect()` (limited: combat needs the
  candidates Vec for its retreat phase; movement needs the changes Vec for apply).
