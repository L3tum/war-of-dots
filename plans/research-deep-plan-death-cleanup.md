# Research: death_cleanup deep-plan (from RESEARCH subagent)

## 1. Cost breakdown

**Which sub-step dominates the 6.4 ms avg / ~30 ms death-tick: the `retain` pass. Despawn is negligible.** (retain share: *estimated* ≥90%; all inputs verified-by-reading)

- Structure (`src/game/systems/death_cleanup.rs:29-54`, verified): early-return if all three dead lists are empty (no-death ticks cost ~0); then (a) `dead_set` build O(deaths), (b) `retain(|e| !dead_set.contains(e))` over **every** `NearbyEnemies` list, (c) `entity_reverse.remove` + `world.despawn` per dead entity.
- Workload (reports/stress_5.html JSON, verified): `health_avg_units = 9764`, `health_avg_total_nearby_entries = 588 146`, `health_avg_units_with_enemies = 577` → ~9 190 empty SmallVecs (cheap) + 588k real entries, averaging **~1019 entries** among the 577 units that have any (matches combat.rs:33/234 comment "~10KB+ lists in dense melees" — 1019 × 8 B ≈ 8 KB, verified).
- Cost model (*estimated*): 588k × (`HashSet<Entity>::contains` on a ~40-element set + 8 B move-on-retain in `SmallVec<[Entity;8]>` heap buffers). ~20–50 ns/entry → ~12–29 ms. Measured death-tick cost from the report: total 10 905 ms / 1700 calls, P50 29.729 ms (P50 over the *last 200* durations — see §4) → death ticks are ≈30 ms, implying deaths on ~21% of ticks (10 905/29.7/1700, *estimated*), consistent with 8.6 deaths/tick avg (task-stated).
- Despawn cost, measured: `systems/death_cleanup_50` microbench = 5.81 µs for 200 units / 50 deaths (stress_5.html microbench table, verified). **Caveat (verified-by-reading, `src/microbenchmarks.rs:775-800` + `:422-436` + `src/game/world_ext.rs:59-97`)**: `DeathCleanupFixture` spawns via `world.spawn_unit`, which *omits* the `NearbyEnemies` component, and `setup_nearby_enemies` is never called — so the microbench's `retain` query iterates **zero** entities. Its 5.81 µs measures only set-build + 50 despawns + `entity_reverse` removes ≈ **0.1 µs per death**. ~40 deaths/death-tick ⇒ ~4–50 µs. Two orders of magnitude below 30 ms.
- Allocs (stress_5.html, verified): death_cleanup = 16 allocs / 8 900 bytes total over the run (scratch `clear()` preserves capacity) → allocator is not a factor.
- **Conclusion:** dropping the `retain` pass removes ≈ the entire 10 905 ms (subject to verification in §5). The despawn loop and `dead_set` must stay (they're ~µs anyway).

## 2. Safety of dropping the retain pass

**Every reader of `NearbyEnemies.entities` (full grep of src + tests, verified):**
1. `src/game/systems/combat.rs:193` — candidate filter `!nearby.entities.is_empty()` (during combat, same tick as rebuild)
2. `src/game/systems/combat.rs:243` — target scan loop (combat)
3. `src/game/systems/combat.rs:492` — retreat/`fighting` check (combat, same tick)
4. `src/game/game.rs:649-652` — `combat_health()`, benchmark/status-only hook; sole call site is `src/benchmark.rs:471`, once per measured tick **after** `tick_game`
5. `src/game/systems/death_cleanup.rs:44` — the retain itself

Writer: `nearby_enemies.rs:279-281` (+ insert at :257-263). Test helpers: `combat.rs:656`, tests in `nearby_enemies.rs`/`death_cleanup.rs`. **No** reader in supply, city_capture, influence, encirclement, surrender, movement, water, economy, `track_unit_losses` (game.rs:1553), `guard.finish`, replay, or `src/server/` (grep "nearby" in server/: no matches). `NearbyEnemies` is **not** serde-serialized (`components.rs:59-69` derives only Debug/Clone/Default) — no save/replay/serialization path touches it. `tests/integration_game.rs` (around :425-445) only asserts despawn/no-leak, never list contents.

**No reader runs between combat kills and the next rebuild (verified):**
- Tick order (`src/game/game.rs:688-920`): [economy, water,] movement → (tickcontext, 33% of ticks) → **nearby_enemies → combat** → supply → city_capture → influence → encirclement → surrender → death_cleanup → `track_unit_losses` → `guard.finish`.
- `nearby_enemies.rs:215-295` (verified): **full rebuild every tick, unconditionally** — every unit with `(UnitPosition, UnitFlags)` gets `nb.entities.clear(); push…` (or insert if missing). It is not incremental, not dirty-gated, and runs *before* combat on every tick. So any entry left stale by skipping the retain is overwritten **before** the only in-tick reader (combat) ever reads it.
- Between death_cleanup (end of tick N) and combat (tick N), the only code is: benchmark loop reads (`combat_health` at benchmark.rs:471 — see drift below), next tick's economy/water/movement/tickcontext — none read `NearbyEnemies`.

**What combat does with a dead/stale entry** (`combat.rs`, verified):
- `ctx.owner_id_by_entity.get(&entity2)` — the map is built in `build_initial` (game.rs:317-341) and incrementally inserted at recruit (game.rs:990); **never invalidated on death** (game.rs:277-281 doc: "stale entries on death are harmless since dead entities don't appear in ECS queries"; `rebuild_both` at :384 is `#[cfg(test/microbenchmarks)]` only). So a despawned entity that somehow stays in a list still hits `Some(owner)` in the same-owner check → the `debug_assert!(false)` at combat.rs:266-272 **cannot fire for a dead entity** (it only fires for entities genuinely missing from the map, e.g. a spawn path that forgets the recruit insert — not the case here). Release behavior of the assert arm is silent `continue` regardless.
- Next check, `world.get::<&UnitPosition>(entity2)` → `Err` for a despawned entity → `continue` (combat.rs:274-277). So a stale dead entry costs one hash lookup + one failed `world.get` and produces **no fight, no damage, no state change**.
- **hecs generation safety (standard hecs 0.11 semantics; verified in Cargo.toml:34 `hecs = "0.11"`; local registry source not found, so marked standard-semantic not read-verified):** `Entity` = (index, generation); `World::despawn` bumps the generation, so when an index is reused the new `Entity` has a *different* generation. A stale `Entity` value in a list will never compare equal to the successor and `world.get`/`query_one_mut` on it returns `Err(NotAnEntity)`. **No aliasing/collision hazard.**

**Units without the component (verified):** `spawn_unit` (world_ext.rs:59-97) omits `NearbyEnemies` ("ABSENCE marks the unit as uninitialized"). Consequences, unchanged by this change:
- Combat's candidate query *requires* `&NearbyEnemies` (combat.rs:185-191) → freshly recruited units can't fight until the next tick's rebuild inserts the component.
- `combat_health` (game.rs:636-653) queries `(Entity, &UnitFlags, &NearbyEnemies)` → units lacking the component are skipped; `health_avg_units` (9764) can be ≤ `units_remaining` + spawned count. One-tick undercount, pre-existing.

**Stats drift in `combat_health()` if stale entries persist (estimated, bounded):** called once per benchmark tick after `tick_game` (benchmark.rs:471). Without the retain, lists contain this tick's ~dead-units entries for exactly one tick (rebuild overwrites next tick). Drift: `total_nearby_entries` inflated by ≈ deaths × ~67 avg-entries-per-unit-of-them (in melee, up to ~deaths × ~1000 ≈ at most a few thousand vs the 588k baseline → <1%); `units_with_enemies` may retain a unit whose only enemy died (≤ deaths, vs 577 baseline). No gameplay effect: combat already ran; next tick's rebuild fixes everything before combat reads. The drift is visible only in the benchmark's `health_*` aggregates (averaged over 1500 ticks → negligible) — and combat outcomes are **bit-identical**, so `units_remaining`, `deaths_per_tick`, and all other rows should match stress_5 within noise under the same seed 42.

**Existing test that must change (verified):** `death_cleanup.rs:66-115` `cleanup_removes_dead_from_nearby_enemies` asserts `nearby.entities.is_empty()` after `run()` — it encodes the retain invariant and **will fail** if the retain is dropped; rewrite it to assert only despawn + `entity_reverse` cleanup. Other death_cleanup tests (`:117-130` empty-input early return, `:132-155` entity_reverse) are unaffected.

## 3. Other systems' interactions

- **nearby_enemies per-unit cost (verified):** now a spatial-grid rebuild, not brute force — `CELL_SIZE = 2.0` = `COMBAT_RANGE`, 3×3 cell query, O(n × c) (nearby_enemies.rs:1-20, :240-300). stress_5: 7 351 ms total / 4.324 ms avg per tick (1700 calls) ≈ 440 ns/unit incl. fixed costs (*estimated* per-unit split). 22.9 allocs/tick, 36.7 KB/tick (SmallVec heap growth for >8-enemy lists). The stale `game.rs:758` comment "brute-force rebuild" describes pre-grid architecture. **This 4.3 ms/tick is the new floor**: even with retain dropped, nearby_enemies remains #2 system cost and will become #1.
- **combat `resolved` HashSet (verified, combat.rs:25, :48-84):** `HashSet<(Entity,Entity)>` + `fought` + `dead_set` + `dead` Vec, all `clear()`ed per tick with capacity preserved; cost is O(pairs touched) and combat totals only 643 ms over the run (0.378 ms avg) — not affected by this change.
- **surrender per-tick `Vec::new()` (verified, game.rs:883-893):** on non-surrender ticks the else-branch returns a zero-capacity `Vec::new()` — **no allocation**, no cost. Surrender itself runs every 30 ticks, totals 0.6 ms over 56 calls; its `collect()` allocates only when non-empty (bench-alloc: 32 allocs/150 calls).
- **All `.despawn(` call sites in src (verified grep):** only `death_cleanup.rs:51` (production) and `microbenchmarks.rs:876` (the `ecs/despawn` microbench). Nothing else despawns units; dead units cannot accumulate if death_cleanup is skipped entirely.
- **`entity_reverse` maintenance (verified):** insert at recruit (game.rs:979), remove in death_cleanup (death_cleanup.rs:50). No other users. Note (verified, pre-existing, out of scope): `entity_registry` (unit_id→Entity, game.rs:978) is **not** cleaned on death — it keeps stale Entity values for dead units (harmless: stale generation never matches the successor via hecs; ordering a dead unit just fails `world.get` lookups at :1011/:1024/:1037/:1050).
- **`owner_id_by_entity`** (verified): grows monotonically until… it is *never* rebuilt in the tick loop — unbounded stale growth (≈ one entry per unit ever recruited, u8 values; slow memory creep, pre-existing, unrelated to this change but worth noting in risks).
- **`unit_count()`/`fill_unit_id_owner`** (game.rs:560-615): 9/10-component queries with no `NearbyEnemies` — unaffected.

## 4. P99 spikes

- **Percentiles are computed over the last 200 calls only** (verified): `debug_stats.rs` `record_system_timing` keeps a ring buffer `durations_ms` capped at 200 (`if len > 200 { remove(0) }`), and `get_debug_stats` sorts that buffer for P50/P90/P99. `total_ms`/avg cover all 1700 calls. So P50 29.729 / P99 34.498 describe the *final ~200-tick window*, where combat density (and thus deaths/tick) was high — in that window a **majority of ticks had deaths**.
- **The "spike" is simply the presence of deaths**: no-death ticks hit the early return (~0 ms); *any* death tick pays the full ~30 ms retain over 588k entries. 10 905 ms ≈ 363 death-ticks × ~30 ms (*estimated*), i.e. ~21% of ticks. The P50→P99 spread (29.7→34.5 ms) is small; the spread is driven by tick-to-tick variation in total list entries / entity count and by death-spike ticks, not by a separate GC/alloc phenomenon (death_cleanup allocs ≈ 0).
- **Max dead/tick sources (verified):**
  - **surrender** (surrender.rs:17-31): every 30 ticks, returns **all** units with `men <= 0` *or* `men/max_men < 0.05` — unbounded, can sweep hundreds in one tick during a collapse (the deterministic 5% rule, D-3). This is the largest single-tick death-spike source.
  - **combat** (combat.rs:455-470): one `dead.push` per engagement kill; bounded by engaged pairs that tick (avg `last_engaged_pairs` low in this 1000×1000 stress map — *estimated* from `combat` total 643 ms / 1700 ≈ 0.38 ms).
  - **supply/attrition** (supply.rs:212, :293-295): men drain is (1/600)×(1..10) per tick → reaching 0 this tick is rare; effectively ≤ a few/tick.
  - Even a huge death tick doesn't scale the 30 ms much: despawn is ~0.1 µs each (measured, §1) and `dead_set` insert is O(1); a 500-death tick would add ~500 µs to the 30 ms. So after the retain pass is dropped, **death spikes become nearly free** and P99 should collapse toward ~0.1–1 ms (the remaining despawn+map cost + timer overhead — *estimated*).

## 5. Verification recipe

- **Command to produce reports/stress_6.html** (verified, run-stresstest.sh; do NOT let it run unattended — it's one `cargo run --profile release` invocation):
  ```
  ./run-stresstest.sh 6 --bench-prev ./reports/stress_5.html
  ```
  which expands to (script lines 25-31):
  ```
  cargo run --profile release --features debug-overlay,microbenchmarks,bench-alloc -- \
    --benchmark --bench-seed 42 --bench-ticks 1500 \
    --bench-html ./reports/stress_6.html \
    --bench-initial-units 10000 --bench-map-size 1000 --bench-prev ./reports/stress_5.html
  ```
  `--bench-prev` is parsed by `benchmark.rs:1179-1187` (`parse_previous_report`) and drives the green/red deltas in every table; without it the report renders but no deltas.
- **Release vs debug:** `--profile release` ⇒ **no `debug_assertions`** ⇒ the `debug_assert!(false)` owner-map arms are compiled out; timing instrumentation comes from the `debug-overlay` feature (the `tick_system!` macro's `#[cfg(any(debug_assertions, feature = "debug-overlay"))]` branch, game.rs:37-59). stress_5 was produced the same way, so the comparison is apples-to-apples. (Note: the microbenchmarks run inside the same binary, gated by the `microbenchmarks` feature.)
- **Rows to check in stress_6.html:**
  1. *System Timing Breakdown → `death_cleanup`*: Total should drop from 10 905 ms by ≈ the retain share (expect ≤ ~1–2% of original, i.e. a few hundred ms or less — *estimate*); Avg/Call 6.415 → ≪1; **P99 34.498 → should fall to sub-millisecond range**; P50 same.
  2. *`nearby_enemies`*: must be **unchanged** (4.324 ms avg, 7 351 ms total, ~12 KB allocs/tick) — it is the control row proving the rebuild still runs.
  3. *Summary*: `avg tick wall` (16.009 ms) should improve by ~6.4 ms on average; `units_remaining` (8775), deaths/tick, movers, combats should match stress_5 within noise (same seed 42, bit-identical combat per §2); `health_avg_total_nearby_entries` (588 146) may show a tiny (+<1%) one-tick-stale drift.
  4. *Microbench table → `systems/death_cleanup_50`*: should shrink from 5.81 µs toward the pure-despawn cost (a few µs → ~1-3 µs, *estimated*); `nearby_enemies_*` rows unchanged.
  5. *Alloc table → `death_cleanup`*: still ~0 allocs/tick.
- **Tests that must pass** (Makefile `test` target, verified — though the Makefile header is stale/from another project, the cargo lines apply):
  ```
  cargo test --all-targets -- --skip alloc_invariants::warm_tick_net_heap_growth_is_bounded
  cargo test --lib alloc_invariants::warm_tick_net_heap_growth_is_bounded -- --exact
  ```
  Must pass specifically: `game/systems/death_cleanup.rs` tests (with `cleanup_removes_dead_from_nearby_enemies` **rewritten** — it asserts the retain invariant, see §2), `combat.rs` tests, `nearby_enemies.rs` tests, `tests/integration_game.rs` (the dead-unit cleanup test at ~:425-445 only asserts despawn/no-leak, so it survives). Grep of `tests/` found **no** test asserting NearbyEnemies list contents.
- Additionally: `cargo build --release` compiles, and (optional) a debug build `cargo test` exercises the `debug_assert!` paths in combat — with the change, no dead entity ever reaches the missing-owner arm, so no new debug panics are expected (verified-by-reasoning from §2; *not* runtime-verified since I couldn't run cargo).

## 6. Alternatives

- **(a) Reverse-index from nearby_enemies:** have the rebuild maintain `dead→(units listing it)` — i.e. invert the problem: instead of scanning all 9764 lists at death time, the rebuild (which already visits all 588k pairs) could store, per *target* entity, who lists it, so death_cleanup removes each dead entity from its ~67-1019 listing units in O(entries touching the dead) ≈ O(deaths × fan-in) rather than O(all entries). This is the asymptotically right fix if death ticks become common, but it adds a per-tick HashMap/Vec-index build into nearby_enemies (~588k inserts/tick ≈ potentially *more* than the retain it replaces, plus allocations) and one more structure to keep in sync — for 21% death-tick frequency the flat retain (or its removal) is cheaper. Complexity: medium; risk: sync bugs between index and lists.
- **(b) Generation-check at read time:** keep the retain (or drop it) and make readers robust: combat already is — `world.get::<&UnitPosition>(entity2)` returns Err for stale-generation entities and `continue`s; the only addition would be skipping the `debug_assert!(false)` false-positive risk, which §2 shows cannot fire since `owner_id_by_entity` is never invalidated on death. So (b) is essentially free *and already implemented in combat*; it is what makes dropping the retain safe rather than a separate optimization. Cost: +1 failed `world.get` per stale entry at combat time (bounded by deaths × fan-in per tick, ~hundreds → negligible). No hecs API needed — generations are automatic.
- **(c) Tombstoned units instead of despawn:** mark units dead (`men = 0` or a `Dead` flag) and keep the entity alive until a lazy sweep: lists never go stale, no retain, no generation juggling. But: nearby_enemies would then need to filter dead units out of *every* list each tick (the O(all entries) cost moves into nearby_enemies — the #2 system, running 100% of ticks, vs death_cleanup's 21%), `combat_health`/`unit_count`/ECS queries would over-count the dead, owner/registry bookkeeping changes, and hecs archetype layouts change (a flag component splits archetypes). Strictly worse for this workload.

## 7. Risks & open questions

- **R1 — determinism/regression (low, verified-by-reading):** removing retain cannot change combat outcomes, because the only gameplay reader (combat) always sees rebuilt lists; same seed ⇒ same `units_remaining`. *Open:* confirm empirically via stress_6 summary rows (§5).
- **R2 — benchmark-stat drift (low, estimated):** `combat_health`'s `total_nearby_entries`/`units_with_enemies` include stale entries for one tick after deaths; aggregates drift <1%. If a future consumer makes decisions from `combat_health`, that's a landmine — currently benchmark-only (benchmark.rs:471).
- **R3 — `debug_assert!` semantics (low, verified-by-reading):** the missing-owner `debug_assert!(false)` arm (combat.rs:266-272) cannot fire for dead entities because `owner_id_by_entity` is never removed on death (game.rs:277-281). If someone later adds death-time invalidation of that map *and* keeps stale list entries, debug builds would panic — add a code comment where the retain was removed.
- **R4 — hecs generation semantics (standard, not locally source-verified):** relies on hecs 0.11 bumping the generation on despawn so stale `Entity` values never alias successors. This is core hecs/bevy-style behavior; verify with a 10-line test (spawn/despawn/spawn, `world.get` with the old Entity → Err) if anyone wants belt-and-braces.
- **R5 — test breakage (certain, verified):** `cleanup_removes_dead_from_nearby_enemies` (death_cleanup.rs:66-115) fails as-is; must be rewritten to assert despawn + registry cleanup only.
- **R6 — microbench blind spot (verified):** `systems/death_cleanup_50` never exercised the retain pass (no `NearbyEnemies` in fixture, §1) — it will silently move; consider adding a fixture variant with populated lists (`setup_nearby_enemies` exists at microbenchmarks.rs:471 but is unused by this fixture) so the microbench tracks real workload.
- **R7 — unbounded growth, pre-existing (verified):** `owner_id_by_entity` (and `entity_registry` with stale entries) grow for the whole game life; out of scope but adjacent cleanup candidates.
- **R8 — percentiles over 200-tick window (verified, debug_stats.rs):** P50/P90/P99 in the reports are last-200-calls only, not run-wide; interpret stress_6 percentiles with that caveat (or fix `record_system_timing` to store a histogram — separate change).
- **Open question:** is 4.3 ms/tick `nearby_enemies` (the new #1) in scope for a follow-up? The grid rebuild re-writes *all* 588k entries every tick even when nothing moved; a moved-only dirty rebuild would be the natural next optimization (see prior plans/deep-plan-20260818-004912.md which already discusses this).
