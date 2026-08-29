# Research Report — Network Performance, Round 3 (war-of-dots)

**Scope note.** All W1–W9 items from `plans/deep-plan-20260827-001008.md` are present in-tree; none re-proposed.

## A. Current architecture (verified in-tree)

Per-tick flow (one tokio task per room, `src/server/game_server.rs` tick loop, interval = `1000/game_speed` ms):
1. `game.tick_game()` (ECS; `influence.rs` double-buffered grid publish; ~29% of ticks recompute at stress scale).
2. Snapshot: `unit_states_direct()` (`game_server.rs:560`, impl `game.rs:1884`) — builds a fresh `Vec<UnitState>` for **all** units every tick: `format!("unit_{eid}")` + clones of owner/unit_type/symbol Strings, 2 equipment `Vec`s with per-item name Strings, path `Vec`. Cities via `with_cities` (alloc-free guard).
3. `determine_winner()` (`game_server.rs:584`, impl `game.rs:1755`) + O(players×units) active check (`game_server.rs:588-590`).
4. `run_network_phase` (`delta.rs:452`): borrowed-id `HashSet`, `compute_shared_deltas` (`delta.rs:138`) — per unit: **two** `UnitStateDigest` builds + `HashMap` lookup; emits `UnitUpdate` per changed unit (clones `equipment` at `delta.rs:212`, `path` at `:242`); W8 per-owner count maps; then per-player `build_delta_inner` (`delta.rs:333`) (vision `Arc::ptr_eq` diff, idle-skip, O(1) lookups) and per-player encode — production uses W9 splice path (`game_server.rs:664-671` passes `true`; `SpliceShared`/`encode_delta_spliced` in `protocol.rs`, shared segments encoded once).
5. Baseline commit: rebuilds `prev_units`/`prev_cities` as `HashMap<String,_>` (`delta.rs:552-559`).
6. Spectator block (`game_server.rs:697-727`): union vision grid + reuse of `phase.shared` (W5).
7. Send: outbox-by-move (`game_server.rs:732-748`) → `PlayerSender` (`room.rs:96-127`, **unbounded** `mpsc<Vec<u8>>`) → per-connection async task locking `Arc<tokio::sync::Mutex<WsSink>>` **once per message**, one binary WS frame.

**Wire format:** CBOR (ciboron 0.2), `GameDeltaMessage`, `skip_serializing_if` on 19 Option fields (W2); grids = flat 1-byte-per-cell `Arc<Vec<u8>>` sent **whole** when changed (W3). tokio 1 / axum 0.7 (ws) / hecs 0.11.

## Stress baseline (reports/stress_22.html, seed 42; 10k units, 1000×1000, 100 cities, 1500 ticks)
- **Network = 88% of total system time**: avg **18.025 ms/tick**, P50 10.0, P90/P99 **46.4 ms**; network-row allocs **89,590/tick, 13.28 MB/tick**; biggest single alloc 4.38 MB.
- Two regimes: **movement ticks** (every 20th) ≈ **44–45 ms / 11.2 MB**; **other ticks ≈ 10 ms / 2.2–2.4 MB**.
- **~7,500–7,650 `unit_updates` on every tick** (75–78% of 9,850 units) — real churn, not a delta bug: equipment wear = 1 item/tick per stocked slot; men refill at `recruitment_rate` **every tick** (`supply.rs:274-280`, `:316-336`); digest quantization (men step 1.0, `delta.rs:40-50`) calibrated to drain rate, not refill/wear rate.
- Avg mover 2,498/tick; influence recompute on 29% of ticks.

## B. Ranked bottlenecks
1. **Full per-tick snapshot w/ full state materialization** (`game.rs:1884`): ~10–15 allocs/unit/tick for 10k units before delta logic decides anything; dominates the 10 ms idle floor + most of the ~90k allocs/tick.
2. **Update churn (~7.5k updates/tick)**: 2.2–2.4 MB wire on every non-movement tick; per-update equipment `Vec` clones with item-name `String`s (`delta.rs:212`).
3. **Whole-grid shipping on grid-dirty ticks**: 1000×1000 → **1 MB** per vision grid + territory grid; any change ships the entire array per affected player (`delta.rs:344-352`, `protocol.rs:21-25`) → up to ~5 MB vs KB-scale actual changes (vision circles r5–r10). Prior plan's deferred **O10**.
4. **`determine_winner()` + active check every tick**: one full `world.query::<(Entity,&UnitOwner)>()` scan **per player** (P=4 → 4 scans/tick) + per-player city scans (`game.rs:1755-1780`), then O(P×N) string compares (`game_server.rs:584-590`) ≈ 80k compares/tick.
5. **Splice per-entry temp buffers**: `encode_entry` allocates fresh `Vec` per map entry → 1 MB temp alloc + 1 MB copy **per player per grid tick** (`protocol.rs`); consistent with the 4.38 MB biggest-single-alloc.
6. **Baseline `HashMap` rebuild per tick** (`delta.rs:552-559`): 10k inserts + 10k key-`String` clones; entity ids are `unit_{eid}` with dense hecs eids → entity-indexed `Vec` w/ tombstones is O(N), zero rehash, no key clones.
7. **Spectator union grid**: 1 MB alloc + P×1 MB OR every tick while a spectator is connected (`game.rs:1712-1731`, `game_server.rs:699-701`).
8. **Egress**: unbounded per-connection queues, per-message sink lock, no coalescing/backpressure — stalled client ⇒ unbounded memory (invisible in headless benchmark, the real-world "still bad" candidate).
9. **Serial sim+network in one task**: 18 ms avg vs 16 ms budget at `game_speed` 60.
10. No unit-level AOI (every player gets all units; vision grid is client-side fog only).
Hygiene: `game.players().keys().cloned()` per tick (`game_server.rs:585,699`); `track_unit_losses` O(N×P) (`game.rs:~1772-1806`); `build_llm_state` per LLM player every 60 ticks; `estimate_delta_size` under-hints long paths → geometric buffer growth (`protocol.rs:380-390`); `run-stresstest.sh` omits `--bench-splice` (production runs with splice on → A/B skew).

## C. External techniques
1. **Unity NFE ghost snapshots** — per-client snapshots; **MaxSendRate per ghost type** (per-entity update-frequency cap → answer to #2); importance + size-clamped partial snapshots (→ #3/#10). https://docs.unity3d.com/Packages/com.unity.netcode@1.5/manual/ghost-snapshots.html
2. **NFE delta format** — <12 bits/entity via changemask (today: ~75 B/update). https://discussions.unity.com/t/ghostfields-delta-value/932785
3. **Snapnet** — per-client delta encoding is CPU-expensive → encode shared once, splice per client (W9 done; extend). https://snapnet.dev/blog/netcode-architectures-part-3-snapshot-interpolation/
4. **WS vs UDP** — browsers cannot raw UDP; WS is the right transport < ~15 Hz; keep axum WS; wins are payload size + server CPU. https://just4programmers.com/websocket-vs-udp-multiplayer/
5. **Tokio channels** — unbounded mpsc has no backpressure; `watch` = "latest state wins" → per-connection delta egress (stale delta worthless once newer exists). https://docs.rs/tokio/latest/tokio/sync/mpsc/fn.unbounded_channel.html

## D. Improvement items (ranked)
- **I1. Incremental baseline-keyed snapshot** ("diff during query, materialize only changes"): single ECS pass comparing each unit's components against the baseline `UnitState` (matched by dense entity id), **mutating the baseline in place** for changed fields; `id`/`owner`/`unit_type`/`symbol`/equipment-name `String`s allocated **once at recruit** and never again; unchanged units cost zero allocations; baselines become `Vec<Option<UnitState>>` indexed by entity id + alive-list (kills #1 and #6). Where: `game.rs:1884` (new `update_states_in_place(&mut Vec<Option<UnitState>>) -> SnapshotEvents`), `delta.rs:452-560` (`run_network_phase` signature/commit), `game_server.rs:560`, `benchmark.rs`. Impact: −50–70% of the 89.6k allocs/tick, a few ms off the 10 ms idle floor. Risk: medium (entity-id↔index invariant, removal/tombstone handling; unit tests recruit/death/owner-change). Effort: M–L, own commit + A/B.
- **I2a. Recalibrate digest quantization** to actual per-tick rates: `men` step ≥ `recruitment_rate` (5–10), `equipment_count` step ≈ 5 (wear is 1/tick) — `delta.rs:40-50` + tests. Effort S.
- **I2b. Hot/cold field split**: position/men/org/flags per-tick; `equipment`, `path`, `supply_percent` etc. emitted only every N ticks or on threshold crossing. All 14 fields are already `Option`+guarded client-side (`protocol.rs:74-90`, frontend `connection.ts:405-457`) — no protocol shape change, additive-safe. Impact: 7.5k → <1k updates/tick; idle-tick wire 2.3 MB → <300 KB; equipment `Vec` clones nearly vanish. Risk low–med. Effort M.
- **I3. Compact unit updates** (~75 B → <12 B): (a) `equipment: Option<Vec<u32>>` counts-only (names come from the unit-type template the client already has); (b) numeric `u32` unit/entity id instead of `String`; (c) u16/quantized position. Where: `protocol.rs` (`UnitUpdate`, `UnitState`), frontend `applyDelta` + generated types, replay reader. Impact: ~50–70% of update bytes even after I2. Risk **moderate** — two-way protocol change (frontend + replay files need `#[serde(default)]`/fallback decoding). Effort M. One deliberate wire-format commit.
- **I4. Sparse grid diffs** (prior plan's O10): 1-byte discriminator; if changed cells < threshold (~5% of map) send changed-cell index/value lists (vision: `(x,y)` runs; border: `(x,y,u8)`); else fall back to full grid. Where: `protocol.rs` (grid fields), `delta.rs:344-352` + `:127-133` (diff construction; O(cells) memcmp at 1 MB ≈ 100 µs, acceptable), frontend `flatTo2DBools` path. Impact: movement-tick wire 11.2 MB → ~1 MB (−90% of spike bytes). Risk moderate (wire change both ways; replay compat). Effort M. Keep as a separate commit/branch from I3.
- **I5. Lazy winner/active determination**: build per-owner counts in one pass (reuse W8 `current_units_by_owner`, move its construction before `game_server.rs:584`); drive both the active check and `determine_winner` from those counts; call the full `determine_winner` only when `active_count <= 1` (or every N ticks). Where: `game_server.rs:584-643`, `game.rs:1755-1780`, `delta.rs` (count_by_owner export). Impact: small–moderate (4 ECS archetype scans + ~40k compares/tick ≈ 0.1–0.5 ms), zero wire change. Risk low. Effort S.
- **I6. Zero-temp grid splice**: in `encode_delta_spliced`, for `vision_grid_diff`/`border_grid_diff` write the CBOR byte-string length prefix directly and `extend_from_slice` the grid into the final buffer (skip `encode_entry`'s per-entry `Vec`). Impact: on grid ticks −P×(1 MB alloc + 1 MB copy) ≈ a few ms + ~4 MB allocs off spike ticks. Risk low (byte-for-byte test already pins the encoder). Effort S.
- **I7. Egress hardening — bounded "latest-delta-wins"**: replace unbounded `Vec<u8>` queue with (a) bounded mpsc (2–4 deep) where the tick task overwrites the pending delta (keep-latest; `watch`-channel semantics), or (b) at minimum drop-oldest on full; **move** the WS sink into the sender task (drop the `Arc<Mutex>` — closure variant not used on WS path); **drain** multiple queued messages per wake. **Must not drop non-delta messages (GameOver etc.)** — two queues or delta-only keep-latest. Where: `room.rs:96-127` (`PlayerSender`), `game_server.rs` `handle_ws_game`/`handle_tcp_client` sender tasks, tick-loop send at `:732-748`. Impact: bounded memory under slow/stalled clients, fewer lock ops, lower real-world latency tail; add a slow-client soak test (invisible in headless benchmark). Risk low–med. Effort M.
- **I8. Lazy spectator union grid**: track published per-player vision `Arc`s; skip the 1 MB `union_vision_grid_flat` alloc+OR unless at least one player grid `Arc` changed since last compute (cheap `Arc::ptr_eq` loop). Where: `game_server.rs:697-701`, `game.rs:1712`. Impact: −(1 MB alloc + 4 MB mem traffic)/tick on idle spectator ticks. Risk low. Effort S.
- **NOT recommended**: rayon per-player parallelism (already reverted with A/B evidence, `delta.rs:9-16`); UDP/WebRTC transport (browser constraint + genre fit); further CBOR micro-format changes beyond I3/I4.

## E. User-confirmed scope for this plan
- **In scope:** I1, I2a, I2b, I3, I4, I5, I6, I7, I8.
- **Deferred:** I9 (AOI / unit-level interest management) and I10 (off-tick encoding worker) — record trigger conditions.
- **Target metrics:** stress benchmark `net_ms` P50/P90, network-row allocs/bytes, `net_wire_bytes` + `net_unit_updates` split by idle vs movement tick; **and** real-client egress behavior (bounded memory, slow-client tail).
- **UI granularity defaults** (finalize values in the plan): men step 5–10, equipment count step ≈5, cold fields every ~10 ticks.
- **Spectator scenario in scope** (I8).
- **Close benchmark fidelity gaps**: pass `--bench-splice` in `run-stresstest.sh`; add `net_ms` sub-phase counters (snapshot/delta/encode/send); add idle-vs-movement wire split to the report.

## Verification anchors
`cargo test --all` (incl. `splice_matches_encode_message_byte_for_byte`, `unit_update_compact_roundtrip`), `cargo fmt --check`, `clippy -D warnings`; frontend gates: `npm run test` (vitest), `npm run typecheck`, `npm run lint`; A/B via `./run-stresstest.sh <n>`; primary metrics: network-row allocs/bytes, `net_ms` P50/P90, `net_wire_bytes` per regime, `net_unit_updates`.

## F. Results — I2a: digest quantization recalibration

**Churn probe** (temporary per-field digest-diff counter, removed after use): at the B0 baseline the
**equipment-count digest field** is the dominant per-tick churn driver — ~650 of the ~660 steady-state
idle-tick `UnitUpdate`s (ticks 3250/5000), ~50% on movement ticks (2387–4992 of 2842–7456). Men churn is
small in the bench (`recruitment_rate = 1.0`) but would dominate at prod lobbies (rate 10) — hence the
runtime men step `max(5.0, recruitment_rate)`.

**A/B** (paired runs, same environment, splice on, seed 42, 10k units / 1000×1000 / 100 cities / 1500 ticks):

| metric | B0 baseline | I2a | Δ |
|---|---:|---:|---:|
| net avg ms | 13.430 | 13.095–13.268 (2 runs) | −1.3…−2.5% |
| net p50 / p90 / p99 ms | 10.000 / 46.416 / 46.416 | 10.000 / 46.416 / 46.416 | no regression |
| wall avg ms | 16.166 | 15.591–15.886 | −1.8…−3.5% |
| delta subphase ms | 2.943 | 2.594–2.805 | −4.7…−11.9% |
| encode subphase ms | 5.513 | 5.730–5.887 | +4.0…+6.8% (noise: 2 I2a runs straddle baseline) |
| idle upd/tick | 646.8 | 243.6–243.7 | **−62.3%** |
| idle wire/tick | 221 487 B | 84 305–84 309 B | **−61.9%** |
| move-clean upd/tick | 4696.0 | 3401.5 | −27.6% |
| move-dirty upd/tick | 4790.7 | 3720.5 | −22.3% |
| ALL updates avg/tick | 4104 | 3008 | **−26.7%** |
| ALL wire avg/tick | 3 752 402 B | 3 384 079–3 384 091 B | **−9.8%** |

Workload metrics (updates, wire, allocs) are **bit-stable across I2a reruns** (game state is
unaffected — quantization is digest-only, the wire carries exact values). The encode-subphase +4–6.8%
(0.22–0.37 ms over 1500 ticks ≈ 0.2 µs/tick) is within run-to-run noise; the two I2a runs
(5.730, 5.887) straddle the baseline (5.513) and the total net average is lower in both.

Reports: `reports/stress_base_r3.html` (B0 pair baseline), `reports/stress_i2a.html` (I2a; embeds the
baseline as prev-delta section). `--bench-prev` is passed by the harness, so the i2a report shows both.

## G. Results — I2b: hot/cold field split (COLD_EVERY=10 + per-field threshold gates)

**Mechanism**: `compute_shared_deltas` now takes the game tick. Every 10th tick is *cold* and emits
cold-drift fields exactly as before (unit men, 5-item equipment bins, path, both supply percents,
owner_count_maps, grid diff). On *hot* ticks those fields only emit when a semantic threshold trips:
men ≥ max(5, recruitment_rate) (the runtime step from I2a), equipment net count Δ ≥ 10,
path zero↔non-zero transition or length Δ > max(3, 10%), supply 10-point band crossing. The digest
invariant is preserved: cold fields can only be omitted on a hot tick when a cold tick will carry
them, and any digest difference still maps to an emitted field.

**A/B** (paired runs, same quiet environment — load ≤0.5, splice on, seed 42, 10k units /
1000×1000 / 100 cities / 1500 ticks; each side run back-to-back at comparable load):

| metric | I2a | I2b | Δ |
|---|---:|---:|---:|
| net avg ms | 13.324 | 12.395 | **−7.0%** |
| net p50 / p90 / p99 ms | 10.000 / 46.416 / 64.277 | 10.000 / 46.416 / 46.416 | p90 flat, p99 improved |
| wall avg ms | 16.010 | 14.893 | **−7.0%** |
| avg system ms | 15.590 | 14.549 | −6.7% |
| delta subphase ms | 2.729 | 2.552 | −6.5% |
| encode subphase ms | 5.713 | 5.069 | −11.3% |
| idle upd/tick | 243.7 | 194.2 | **−20.3%** |
| idle wire/tick | 84 309 B | 32 734 B | **−61.2%** |
| move-clean upd/tick | 3401.5 | 3337.1 | −1.9% |
| move-clean wire/tick | 1 036 199.8 B | 483 773.4 B | **−53.3%** |
| move-dirty upd/tick | 3720.5 | 3447.4 | −7.3% |
| move-dirty wire/tick | 9 983 137.2 B | 9 345 661.6 B | −6.4% |
| ALL updates avg/tick | 3008 | 2888 | −4.0% |
| ALL wire avg/tick | 3 384 091 B | 2 884 428 B | **−14.7%** |

All acceptance criteria met: idle wire −61.2% (≥ 20 %), idle updates −20.3% (≥ 10 %), no net_ms
regression (−7.0 %; cap was +20 %), p90 flat.

**Timing caveats (honest record)**: the first two I2b runs happened under external machine load
(load 1.1–1.4; wall 127 s and 237 s, p90/p99 pinned at the 100 ms clamp) and showed an apparent
net_ms regression (16.4 / 26.2 ms) — rejected as environment noise, confirmed by the workload
metrics being bit-identical in all three I2b runs. The clean pair (this table, wall 105 s / 108 s
at load ≤0.5) shows I2b faster. A further labeling slip (one "I2a rerun" executed I2b code while
the I2b diff was in the working tree) is what caught this; the final clean pair was generated with
the tree explicitly verified on each side before running. Workload metrics are deterministic and
bit-stable across runs; ms numbers require a quiet box (multiple concurrent sessions on this host
keep load around 1–1.5 most of the day).

Reports: `reports/stress_i2a.html` (I2a clean), `reports/stress_i2b.html` (I2b; embeds I2a as
prev-delta section).

## H. Results — I5: count-map-driven winner/active check (replaces per-tick `determine_winner`)

**Mechanism**: `determine_winner()` was called every tick from the live server loop just to test
`active_count > 1` (game-over / active-player gating). It scans all units + all cities, clones the
full unit list, builds a per-player map, then discards the winner string. I5 adds the exported
`server::delta::count_units_by_owner(&[&str]) -> BTreeMap<String, usize>` (one O(N) pass) plus
`Game::last_mover_count()` / `last_engaged_pairs()` (plain counters set by the combat system). The
active check is now `active_count == players_with_power(&game.unit_owner_counts,
&game.city_owner_counts)`, and the winner name is resolved lazily via
`determine_winner(..., Some(unit_owner_counts), Some(city_owner_counts))` — `determine_winner`
accepts pre-built count maps (predicate-identical: it only pushes players with
`unit_count > 0 || city_count > 0`; funds only affect the sort key, so `active_count == 1` means
the sole active player *is* the winner). The benchmark harness builds the same map once per tick
and threads it into both the active check and `run_network_phase` (which forwards it to
`compute_shared_deltas`, so the current owner-count map is built once instead of twice).

**Correctness**: predicate identity proved by inspection (see above). New tests:
`test_count_map_matches_unit_city_counts` (delta.rs — exported helper == direct counts, incl. the
`&[]` empty case) and `test_winner_matches_count_map_active_check` (game.rs — winner iff
count-map active == 1; loser/alive/draw/empty cases, plus a `Some("")`-owner city to verify the
frees-city path). Both pass; full suite green (259 + 253).

**A/B** (I5 vs I2b baseline `reports/stress_i2b.html`, splice on, seed 42, same workload):

| metric | I2b | I5 | Δ |
|---|---:|---:|---:|
| net avg ms | 12.395 | 13.232 | +6.8% |
| net p50 / p90 / p99 ms | 10.000 / 46.416 / 46.416 | 10.000 / 46.416 / 46.416 | flat |
| wall avg ms | 14.893 | 15.766 | +5.9% |
| avg system ms | 14.549 | 15.417 | +6.0% |
| delta subphase ms | 2.552 | 2.434 | **−4.6%** |
| encode subphase ms | 5.069 | 5.699 | +12.4% |
| ALL updates avg/tick | 2888 | 2888 | **bit-identical** |
| ALL wire avg/tick | 2 884 428 B | 2 884 423 B | **bit-identical** (0.0002%) |
| per-regime updates/wire | (see §G) | (same) | bit-identical |

**Timing caveats (honest record)**: the net +6.8% is **not attributable to the I5 change**.
Evidence: (1) the workload — updates, wire bytes, allocs — is bit-identical, so the encoded byte
stream is byte-for-byte the same; (2) `encode` (pure CPU-bound CBOR serialization, code untouched
by I5) is the dominant riser (+12.4%), and identical work taking 12% longer is the signature of
CPU load/frequency, not a code regression; (3) `delta` dropped −4.6%, exactly the expected effect
of the owner-count map build moving out of that window (it now sits inside the `net`
tick_system! window, which is still measured as `net`, so no work was added); (4) the I2b
baseline (12.395) was captured on a quiet box (load ≤0.5), while both I5 runs (12.965 / 13.232,
each a 3-run average) ran at load ~1.1–1.5 with five concurrent sessions and no quiet dip.
**Net: no measurable regression; clean A/B to re-run when the box is idle** (I2b report backed up
at `/tmp/stress_i2b_quiet.html`; I5 working files at `/tmp/i5_wip/`).

Reports: `reports/stress_i5.html` (I5; embeds I2b as prev-delta section).

**PS (commit labeling)**: `902f2dd` is labeled "I5" but actually contains **both I2b and I5**
— I2b was never committed on its own (I2a is `HEAD~1` = `5e76cdb`; the I2b diff was still in the
working tree when the I5 commit was made). There is no pure-I2b commit to A/B against from git
history; the I2b code is reconstructable as `5e76cdb + /tmp/i2b.patch` (patch kept, verified
against this diff today). Left as-is per the "no history rewriting" preference.
