# SIMD Auto-Vectorization Plan (re-analysis, supersedes `plans/SIMD_ANAL.md`)

Companion to `plans/COMBAT_TARGETING.md` (that redesign is now **landed**). This document is the re-analysis of SIMD auto-vectorization status and hand-SIMD opportunities in the **current** tree, with verdicts re-anchored to HEAD line numbers.

- **Codegen target:** `-C target-cpu=x86-64-v3` (AVX2), all x86_64 targets — `.cargo/config.toml` (unchanged). Tripwire test: `target_feature_guard` in `src/lib.rs` (fails if a release build silently falls back to SSE2, costing ~29% influence throughput).
- **Method:** RESEARCH phase disassembly of `target/release/war-of-dots` (at HEAD) + source verification at HEAD in this phase.
- **Evidence labels used below:**
  - `[DIS]` — verified by disassembly of the release binary in the RESEARCH phase.
  - `[SRC]` — verified directly against source at HEAD in this phase (line numbers exact).
  - `[CARRY]` — carried from `plans/SIMD_ANAL.md`; underlying source is byte-identical, line numbers updated. asm snippets are quoted from the old plan, **not** re-captured.
  - `[OPEN]` — unresolved; needs a fresh build to confirm.

---

## 1. Context

The old SIMD plan (`plans/SIMD_ANAL.md`) was written **before** the COMBAT_TARGETING redesign landed. That redesign (a) deleted `src/game/systems/nearby_enemies.rs` and the `NearbyEnemies` component, and (b) folded the spatial neighbour scan into combat as "Site 1 — sticky target acquisition". It changed *who calls* `spatial::for_each_within`, *what the callback does*, and broke `scripts/check_vectorization.sh` in a new way. The old plan's §4.5 already said: re-measure the distance-path SIMD *after* the redesign lands. **This plan is that re-measurement.**

Intended outcome: an up-to-date, evidence-labeled codegen audit; a ranked, safe-to-execute implementation checklist; and a working `check_vectorization.sh` that stops passing vacuously.

---

## 2. What changed in the last commit (re: SIMD)

Per-file, with the SIMD-relevant consequence of each change:

| File | Change | SIMD consequence |
|---|---|---|
| `src/game/systems/nearby_enemies.rs` | **Deleted** (346 lines) `[SRC]` — no file, no `NearbyEnemies` component (components.rs has only `attacking: Option<Entity>` at line 99) | Old plan §1's `nearby_enemies` monomorphization (per-hit `Vec::push` into the list) is **gone**. Old §4.4's "the per-hit `Vec::push` may be worth more than the vector math" question is **moot**. |
| `src/game/systems/combat.rs` | New **Site 1 — sticky target acquisition** for movers `[SRC]`: loop at combat.rs:218 over `grid_scratch.units`, `for_each_within` call at combat.rs:256. Callback (combat.rs:261–278) does self-skip + owner `u8` equality + **running `best_dist_sq` min with `Entity::id` tiebreak** (combat.rs:272–277) — no `Vec::push`. Resolution phase at combat.rs:319 is per-candidate (`world.get`, mutual-pair check at combat.rs:395). | `for_each_within` now has exactly **2 production call sites** (was 4 monomorphizations per old §1): the distance filter is still SIMD-blocked (per-element callback inside the filter), and the per-hit cost is now a *branchy running-min select* (data-dependent scalar state) — **less** SIMD-favorable than the old `Vec::push` shape. No new vectorizable numeric loop was introduced. `[DIS]` |
| `src/game/systems/death_cleanup.rs` | Reworked `[SRC]`: `run` at death_cleanup.rs:22, grow-only `dead_set: HashSet` (line 19), per-entity loops (lines 39–80), plus new "site 3" that clears targets pointing at post-combat deaths (test at line 246). | Still an order-preserved, bit-identical-semantics contract — **no SIMD surface** (re-confirmed against the redesigned code). `[DIS]` |
| `src/game/spatial.rs` | Doc comments only `[SRC]` (header now names `combat`'s mover scan as the consumer). `for_each_within` (line 198), `GridIndex::build` (line 72), `CellEntry` (line 23) code **unchanged**. | Old §1/§3 verdicts carry over with updated line numbers. |
| `src/game/systems/movement.rs` | `populate_grid_units` helper at movement.rs:249 (extracted for benchmarks); occupancy-gate `for_each_within` call at movement.rs:161 unchanged in shape. | Movement monomorphization of `for_each_within` unchanged (inlined self-check callback). |
| `src/microbenchmarks.rs` | `CombatFixture::new_dense` (microbenchmarks.rs:537–587) now marks every unit `moved` and builds the post-move grid (lines 576–582) so combat's Site 1 runs in `systems/combat_50pairs` / `combat_200pairs`. | Existing harness **already exercises** the new combat scan — reusable for any before/after measurement. |
| `src/game/components.rs` | `UnitFlags.attacking: Option<hecs::Entity>` (line 99); `NearbyEnemies` removed. | — |
| `plans/COMBAT_TARGETING.md` | Status updated: decisions resolved, Phase 1 implemented & green. | Triggers this re-analysis (its companion note). |

**Net effect:** no loop that was scalar became vectorized, and no new hand-SIMD opportunity appeared. The changes are (a) one `for_each_within` caller removed, (b) one caller's callback reshaped (running-min + tiebreak), (c) the check script now broken by symbol stripping *on top of* the old loose-regex defect.

---

## 3. Current codegen audit

All line numbers verified at HEAD (`[SRC]`). "Vectorized?" reflects disassembly (`[DIS]`/`[CARRY]`) as noted per row.

| Loop | Location | Vectorized? | Evidence | Label |
|---|---|---|---|---|
| `fold_influence_into` — u16 running-max + u8 dominant select, **full grid × player, every dirty influence tick** | `influence.rs:262` (fn), inner loop `263–268`; production call inside `run()` (`influence.rs:301`) at `influence.rs:384` | **No — scalar** | Per-cell `movzwl`/`cmp`/`jbe`/`mov` (even with in-loop bounds checks). **Best hand-SIMD payoff in the game code.** | `[CARRY]` + `[DIS]` re-confirmed |
| `stamp_circle` inner row — u16 saturating add, ≤ ~2·radius+1 iters (short rows) | `influence.rs:139` (fn), inner `for x in lo..=hi` at `164–167` | **No — scalar** | `add`+`cmovb` per cell. Low payoff (short rows). | `[CARRY]` + `[DIS]` |
| `union_vision_grids` flat byte-OR | `game.rs:1516` (fn), loop `1529–1531`, the blocking `if grid[i]` at `1530` | **No — scalar per-byte** | `cmpb`/`jne`/`movb` per cell — the `if` guard blocks vectorization. | `[CARRY]` + `[DIS]` |
| `supervisor_vision_grid` row OR | `game.rs:1448` (fn), inner at `1456–1458` (`*cell = *cell \|\| s` at `1458`) | **Yes — 256-bit** | `vpor`/`vorps`, 128 cells/iter. Proof point: same underlying byte-OR as `union_vision_grids`, but the branchless form vectorizes. | `[CARRY]` + `[DIS]` |
| `GridIndex::build` (cell_for coordinate math) | `spatial.rs:72` | **Yes — the only real AVX2 math in game code** | 52 ymm lines incl. 4-wide `vdivps`/`vroundps` + `vpminsd`/`vpmaxsd`. Proves `x86-64-v3` reaches codegen. **Useful positive control for the check script.** | `[CARRY]` + `[DIS]` |
| `for_each_within` distance filter | `spatial.rs:198` (fn), inner test `214–218` (`if ddx*ddx + ddy*ddy < radius_sq` at `217`) | **No — scalar** | One 128-bit `(x,y)` pair per entry: `vmovsd` load, `vsubps`/`vmulps`/`vaddss`/`vucomiss`/`jbe`, stride = one `CellEntry`. No `vcmpltps` mask, no cross-entry SIMD. *Expected*: the `f(entity, pos, owner_idx)` callback sits inside the filter loop, blocking LLVM's mask-and-gather idiom. | `[CARRY]` (filter body byte-identical) + `[DIS]` (callers re-audited) |
| combat Site 1 — sticky target acquisition (new) | `combat.rs:218` (loop), `for_each_within` call `256`, callback `261–278` (running-min + id tiebreak at `272–277`) | **N/A — ECS-sequential by design** | Per-unit `world.get::<&UnitFlags>` (combat.rs:219), rescan branches, per-hit scalar running-min select. No vectorizable numeric loop. | `[DIS]` (new in HEAD) |
| combat resolution (redesigned) | `combat.rs:319` | **N/A — sequential** | Per-candidate `world.get`, `HashSet` membership, mutual-pair check (combat.rs:395). | `[DIS]` |
| `death_cleanup` (reworked) | `death_cleanup.rs:22` | **N/A** | `HashSet`-based, order-preserved despawn loops (39–80). | `[DIS]` |
| movement occupancy gate | `movement.rs:47` (run), `for_each_within` call `161` (self-exclusion callback) | **No** (via `for_each_within`) | Callback fully inlined (entity self-check), zero per-element calls. | `[CARRY]` |

`[CARRY]` asm snippets from the old plan (quoted verbatim there; the source they were captured from is byte-identical at HEAD):

- `fold_influence_into` (inlined into `influence::run`):
  `movzwl (%r10,%r8,2),%r9d` · `cmp (%rdx,%r8,2),%r9w` · `jbe <loop>` ·
  `mov %r9w,(%rdx,%r8,2)` · `mov %r9b,(%rcx,%r8,1)` · `jmp <loop>`
- `union_vision_grids` scalar byte-OR:
  `cmpb $0x1,(%rax,%rdi,1)` · `jne <loop>` · `movb $0x1,(%rbp,%rdi,1)` · `jmp <loop>`
- `supervisor_vision_grid` (vectorized, 128 cells/iter):
  `vmovdqu …%ymm0` · `vmovups 0x20(…)%ymm1` · `vpor …,%ymm0,%ymm0` ·
  `vorps 0x20(…)%ymm1,%ymm1` · `sub $0x80,%r11` · `jne <loop>`
- `for_each_within` (per entry):
  `vmovsd -0x8(…)%xmm0` · `vsubps` · `vmulps` · `vmovshdup` · `vaddss` ·
  `vucomiss` · `jbe <next entry>`

Note: the `spatial.rs` doc comment calling the inner test "FMA/SIMD-friendly, branch-free inner test" remains **aspirational** — `vfmadd` count was 0 in the entire binary (`[CARRY]`; re-confirm on next build with a cheap `grep -c vfmadd` on emitted asm).

**Binary-wide context** `[CARRY]`: AVX2 (`ymm`) appears in ~1,948 symbols, dominated by `tokio`/`serde`/`hecs` struct copies + `GridIndex::build`. There is no u16-lane SIMD in any game function (the only 16-bit SIMD ops live in statically-linked sqlite3).

---

## 4. Recommendations, ranked by effort/risk

### R1 — Free win, zero risk: branchless `|=` in `union_vision_grids`

- **Change:** `src/game/game.rs:1530` — replace
  ```rust
  if grid[i] {
      result_flat[i] = true;
  }
  ```
  with
  ```rust
  result_flat[i] |= grid[i];
  ```
- **Why safe:** `bool` OR is monotone and idempotent; the outer loop OR-accumulates each player's grid, so per-cell final state is bit-identical. Same shape as `supervisor_vision_grid` (game.rs:1458), which already auto-vectorizes to 256-bit.
- **Caveat:** this is a supervisor/debug-view path, not the per-tick hot loop — the absolute win is small; its value is (a) trivial + zero-risk, (b) a second auto-vectorized proof point, (c) an end-to-end test of the R2 asm workflow.
- **Verify:** emitted asm for `Game::union_vision_grids` shows `vpor`/`vorps` on `ymm`; `make test` green (vision/union semantics unchanged).

### R2 — Process fix: rewrite `scripts/check_vectorization.sh`

**Root cause of current breakage** `[DIS]`: the release binary is **stripped** (no symbol table), so the script's `objdump -d` output has **no function labels** → the awk boundary extractor matches nothing → every `FUNCS` entry reports "not found" → **PASS vacuously**. The old plan's §2 defects (loose `ymm` substring regex matching struct copies; `FUNCS` missing the symbols that matter) remain as *second* defects that would bite on any unstripped artifact.

Changes (all in `scripts/check_vectorization.sh`; Makefile target at `Makefile:56` unchanged):

1. **Source of truth = rustc-emitted asm, not the stripped binary.**
   `cargo rustc --release --lib -- --emit asm` (the lib contains every hot game loop; avoids building the three bin targets). The emitted `.s` (under `target/release/deps/`, or a custom path if the toolchain honours `--emit asm=<path>` — implementer to confirm) retains symbol labels, so per-function boundary extraction works.
   - `build.rs` runs `npm run build` unconditionally → the script **requires node/npm** (same pre-existing prerequisite as `build-release`). On any cargo failure the script must **fail loudly, never silently pass**.
   - The current awk expects objdump-style `<name>:` labels; `.s` files use bare `name:` labels (mangled, possibly new `_R…` scheme) — **adjust the label matcher empirically** (this is the main implementation risk of R2).
2. **Fix `FUNCS`:** drop `nearby_enemies.*run` (file deleted `[SRC]`); keep `influence.*run`; **add** `for_each_within`, `movement.*run`, `GridIndex.*build`.
3. **Tighten `AVX_RE`** to require vector *arithmetic* on a wide register on the same line, e.g.:
   ```
   (vaddps|vsubps|vmulps|vdivps|vminps|vmaxps|vcmpltps|vroundps|vpadd|vpmax|vpmin|vpor|vorps).*(%ymm|%zmm)
   ```
   (must cover `GridIndex::build`'s actual op set — `vdivps`/`vroundps`/`vpminsd`/`vpmaxsd` — so the positive control passes; excludes plain `vmovdqu` copies.)
4. **Two-list semantics** so "correctly scalar" loops don't turn the script red:
   - `EXPECT_VECTOR`: `GridIndex.*build` (passes today — positive control) and, after R1 lands, `union_vision_grids`.
   - `REPORT_ONLY`: `for_each_within`, `movement.*run`, `influence.*run` — print scalar/vector status as a diagnostic, don't fail.
   - Default: print per-function verdict table, exit 0. `--strict`: exit 1 if any `EXPECT_VECTOR` function lacks vector math or is missing from the asm.
   - Update the header comment: explain the stripped-binary root cause and the emit-asm workflow; keep the "positive = evidence, negative = meaningful" caveat.
- **Why safe:** no game-code change.
- **Verify:** run `make check-vectorization` → expect `GridIndex::build` ✓ vector math; `influence`/`movement`/`for_each_within` ✗ scalar (reported, not failing); exit 0. After R1: `union_vision_grids` ✓.

### R3 — Top payoff: `fold_influence_into` (gated on measurement)

`fold_influence_into` (`influence.rs:262–268`) runs the **full map grid per player on every dirty influence tick** (`INFLUENCE_TICK_INTERVAL = 3`, `constants.rs:10`) — the largest remaining scalar numeric loop in the game code.

**Step A — branchless-select rewrite (zero new dependencies).** Replace the branch with a select so LLVM *might* auto-vectorize on stable:

```rust
#[inline]
fn fold_influence_into(dominant: &mut [u8], max_inf: &mut [u16], grid: &[u16], idx: u8) {
    let idx_u32 = idx as u32;
    for i in 0..dominant.len() {
        // Branchless select; preserves the strict-`>` tie-break (lowest-sorted
        // player wins) — take == 0 keeps the current accumulators.
        let sel = 0u32.wrapping_sub(u32::from(grid[i] > max_inf[i])); // 0 or !0
        max_inf[i] = ((max_inf[i] as u32) & !sel | (grid[i] as u32) & sel) as u16;
        dominant[i] = ((dominant[i] as u32) & !sel | idx_u32 & sel) as u8;
    }
}
```
Semantics are bit-identical (when `take == 0` both accumulators are kept; when taken, both are overwritten with exactly the old values). Keep `#[inline]` so the vectorized code stays inside the `influence.*run` symbol the script inspects.
**[OPEN]:** whether LLVM actually vectorizes the mixed u16/u8 select on stable rustc 1.96 is unresolved until built. Re-check codegen either way.

**Step B — glam hand-SIMD (only if Step A stays scalar AND the benchmark justifies it).** Library decision carried from old §4.3: **`glam`** (stable, no feature gate; rejected: `std::simd`/`portable_simd` nightly, `wide` weaker maintenance, `cgmath` wrong tool). Add `glam` to `Cargo.toml`. Implementation notes: glam has **no native 16-lane u16 type** — pack two u16 per `UVec4` lane (8 u16/vector → two vectors per 256-bit step) with per-half compare/select via 16-bit masks; the u8 `dominant` select runs 32 lanes per 256 bits through a bitcast. Scalar fallback tail for non-multiple lengths. ~30–50 lines. Add a randomized equivalence test against the scalar loop alongside the existing fold unit tests (`influence.rs:786–794`) and the `build_territory_grid` tests (`influence.rs:276`).

**Why safe (both steps):** pure per-element function; determinism contract (strict `>` tie-break → lowest-sorted player wins; `NONE = 255` sentinel untouched because a cell is only written when `grid[i] > max_inf[i]`, which is never true at 0 vs 0) is preserved exactly.

**Verify:** `systems/influence_100` / `influence_500` / `influence_100_skip` microbenchmarks before/after; emitted asm for `influence.*run` shows vector select ops (`vpcmp`/`vand`/`vpor`/`vp*` on `ymm`); full `make test` (territory/encirclement tie-breaks are determinism-sensitive).

### R4 — Demoted: `for_each_within` two-phase (SIMD mask → gather → call `f`)

Carried from old §4.4, **demoted to "measure first, likely not worth it"**:

- Call sites are now exactly 2: movement occupancy gate (`movement.rs:161`, self-exclusion only) and combat target acquisition (`combat.rs:256`, running-min + id tiebreak). The old 4-monomorphization picture (incl. `nearby_enemies`) is gone.
- Cell occupancy is **O(1)** by the `MIN_UNIT_DISTANCE = 8` / `SPATIAL_CELL_SIZE = 10` rules (`constants.rs:32–42`): a 3×3 scan touches a small slice (≤ 9 entries per cell, typically far fewer), so SIMD setup cost may not pay back at realistic densities.
- The combat callback is *less* SIMD-favorable than the deleted `nearby_enemies` one: per-hit running-min + id tiebreak is a data-dependent scalar chain — even a masked filter only accelerates the *miss* distance tests, not the hits.
- `CellEntry` (`spatial.rs:23`) is interleaved `(x, y, …)`: 4-wide `f32` lanes need an **SoA split** (separate x/y arrays) or per-load `vpermilps` shuffles — a buffer-layout change touching `GridIndexScratch`, the builder (`movement::populate_grid_units`, movement.rs:249), and the benchmark fixtures (microbenchmarks.rs:576). Not "just add intrinsics".
- **Decision:** add a dedicated `spatial/for_each_within_*` microbenchmark id (none exists today; `systems/combat_50pairs` / `combat_200pairs` / `movement_500` / `movement_2000` only exercise it indirectly) and measure first. If it is a meaningful slice of movement/combat time at realistic densities, write a separate follow-up plan for the SoA restructure. Otherwise document and stop.

---

## 5. Explicitly NOT worth SIMD

Carried from the old plan; combat and death_cleanup **re-confirmed** against the redesigned code (`[DIS]`) — no new surface appeared.

- **`supply`** — order-dependent FCFS funding (`remaining_funds[oi] -= cost`); vectorizing would change *which* unit gets funded → breaks documented determinism.
- **`combat`** — sequential semantics + per-pair `world.get` / `HashSet`; irreducibly sequential, allocation-free by design. Re-confirmed on Site 1 (combat.rs:218–281) and resolution (combat.rs:319+).
- **`death_cleanup`** — bit-identical-semantics contract (order-preserved despawns), now incl. the post-combat target-clear "site 3" (death_cleanup.rs:22–80).
- **`city_capture`** — needs AoS→SoA restructure for marginal f32 FMA gain.
- **`encirclement`** — the BFS dominates; seed-scan vectorization is low payoff.
- **`water` / `surrender` / `delta`** — no parallel numeric loop surface.

---

## 6. Files to modify

| File | Recommendation | Nature |
|---|---|---|
| `src/game/game.rs` | R1 — 1-line change at line 1530 | trivial |
| `scripts/check_vectorization.sh` | R2 — rewrite (emit-asm source, FUNCS, AVX_RE, two-list semantics, header) | self-contained |
| `src/game/systems/influence.rs` | R3 — `fold_influence_into` (line 262) rewrite [Step A] or glam hand-SIMD [Step B]; extend fold unit tests (~line 786) | small–medium |
| `Cargo.toml` | R3-Step-B **only** — add `glam` | trivial, gated |
| `src/game/spatial.rs` | R4 **only if pursued** — `CellEntry` SoA split | medium, separate plan |
| `src/game/systems/movement.rs` | R4 **only if pursued** — `populate_grid_units` (line 249) layout update | medium |
| `src/microbenchmarks.rs` | R4 **only if pursued** — fixtures (line 576) + new `spatial/for_each_within_*` ids | small |
| `plans/SIMD_ANAL.md`, `plans/COMBAT_TARGETING.md` | Supersede notice + cross-link update | docs |

---

## 7. Reuse (existing code/utilities — do not reinvent)

- `spatial::for_each_within` (`spatial.rs:198`), `GridIndex` / `GridIndexScratch` (`spatial.rs:53` / `spatial.rs:35`), `cell_for` (`spatial.rs:187`).
- Microbenchmark harness: `run_bench_with_setup`, `MovementFixture` / `CombatFixture` / `InfluenceFixture` (microbenchmarks.rs:504 / 537 / 619). Existing ids: `systems/influence_100`, `systems/influence_500`, `systems/influence_100_skip`, `systems/combat_50pairs`, `systems/combat_200pairs`, `systems/movement_500`, `systems/movement_2000`. Run: `cargo run --release --features microbenchmarks -- --benchmark`.
- `movement::populate_grid_units` (movement.rs:249) — the grid-build helper combat's benchmark fixture already uses; reuse for any new spatial benchmark.
- `target_feature_guard` test (`src/lib.rs`) — build-config tripwire proving AVX2 is on; keep green.
- Existing fold/territory unit tests: `build_territory_grid` tests (`influence.rs:276`), `fold_influence_into` direct tests (`influence.rs:786–794`) — extend, don't replace.
- `scripts/check_vectorization.sh` + `make check-vectorization` (`Makefile:56`) — rewrite in place, keep the Makefile target name.
- `glam` library decision (old plan §4.3) and the rejected-alternatives rationale.
- Constants: `MIN_UNIT_DISTANCE` / `COMBAT_DISTANCE` / `SPATIAL_CELL_SIZE` / `INFLUENCE_TICK_INTERVAL` (`constants.rs:10, 28–42`).

---

## 8. Steps (implementation checklist)

Safe/stable-only items first; hand-SIMD gated on measurement.

- [ ] **S0 — Baseline.** `cargo build --release`; confirm the binary is fresh vs HEAD (research disassembled the binary present at analysis time — re-confirm). Capture microbenchmark baselines: `influence_100/500/100_skip`, `combat_50pairs/200pairs`, `movement_500/2000`.
- [ ] **S1 (R2) — Rewrite `check_vectorization.sh`** per §4-R2: emit-asm source, corrected `FUNCS`, strict `AVX_RE`, `EXPECT_VECTOR`/`REPORT_ONLY` lists, loud failure on build error, updated header.
- [ ] **S1.1 — Validate the script:** `make check-vectorization` → expect `GridIndex::build` ✓ (positive control), scalar loops reported ✗, exit 0. If the `.s` label format breaks the awk matcher, fix the matcher before proceeding (the fixed script is the measurement backbone for S2–S4).
- [ ] **S2 (R1) — Branchless `|=`** at `game.rs:1530`.
- [ ] **S2.1 — Verify R1:** re-run the script (`union_vision_grids` ✓ 256-bit), inspect emitted asm, `make test`.
- [ ] **S3 (R3-Step A) — Branchless fold rewrite** (`influence.rs:262`). Re-check codegen via emitted asm for `influence.*run`.
  - If vectorized: benchmark before/after, `make test`, done — **no glam needed**.
  - If still scalar: proceed to S3.1 **only if** the S0 baseline shows influence spend justifying it (compare against S0 numbers; the ~29% AVX2 baseline noted in `lib.rs` is the reference for "is this loop's cost material").
- [ ] **S3.1 (R3-Step B, gated) — glam hand-SIMD** for the fold: add `glam` to `Cargo.toml`, implement u16-packed select + u8 select with scalar tail, add randomized equivalence test, re-verify asm + benchmarks + `make test`.
- [ ] **S4 (R4) — Measure-only.** Add `spatial/for_each_within_*` microbenchmark ids (via `populate_grid_units` + `GridIndex::build` at realistic cell densities). If the loop is a meaningful slice of movement/combat time → write a separate follow-up plan (SoA split). Otherwise document the decision and stop.
- [ ] **S5 — Docs.** Mark `plans/SIMD_ANAL.md` as superseded (header note → this plan); update the cross-link in `plans/COMBAT_TARGETING.md`; fix `spatial.rs` doc comments that over-promise ("FMA/SIMD-friendly" → accurate).

---

## 9. Verification (end-to-end)

1. **Build/config:** `make check` (`cargo check --all-targets --all-features`); `make clippy`. `target_feature_guard` test confirms AVX2 codegen target.
2. **Behavior/determinism:** `make test` — full suite incl. influence fold unit tests, `build_territory_grid` tie-break tests, and `tests/integration_game.rs` (combat targeting + territory determinism are the sensitivity points for R3).
3. **Vectorization:** `make check-vectorization` with the fixed script — expected final state: `GridIndex::build` ✓, `union_vision_grids` ✓ (post-R1), `influence.*run` ✓ (post-R3 if it vectorizes) or ✗ reported (if Step B skipped), `for_each_within` / `movement.*run` ✗ reported. Direct spot-checks via `cargo rustc --release --lib -- --emit asm`:
   - `Game::union_vision_grids` region → `vpor`/`vorps` on `ymm`
   - `influence::run` region → vector select/compare ops if R3 landed
   - `GridIndex::build` region → `vdivps`/`vpminsd` (positive control)
   - `grep -c vfmadd` over the asm → expect 0 (confirms no accidental FMA reliance)
4. **Performance:** before/after microbenchmarks from S0 — `influence_*` for R3, `combat_*` / `movement_*` for any R4 measurement. No regression on unrelated ids. For release-scale sanity, `make bench-alloc` (note: needs node/npm, as does any release build via `build.rs`).

---

## 10. Open questions / needs re-verify after build

1. **[OPEN] Branchless fold auto-vectorization (S3):** does the Step-A rewrite actually vectorize on stable rustc 1.96 with mixed u16/u8 selects? Unresolved until built. Fallback is Step B (glam).
2. **[OPEN] `for_each_within` monomorphization set in a fresh asm:** research found the filter scalar at both current call sites; the exact label list/count in the `.s` (vs the old binary's 4 monomorphizations) should be captured during S1.1 and is what the script's `FUNCS` matching must handle.
3. **[OPEN] Emit-asm mechanics:** confirm the toolchain's `--emit asm` output location and `.s` label format (new `_R…` mangling scheme?) so the script's label matcher is correct. Confirm whether `--emit asm=<custom path>` is honoured through `cargo rustc` (else glob the newest `target/release/deps/*.s`).
4. **[OPEN] Binary freshness:** all `[DIS]` claims rest on the release binary present at RESEARCH time. If `target/release/war-of-dots` is stale vs HEAD, re-disassemble before trusting any per-loop claim (S0 covers this).
5. **npm prerequisite:** every release-build path (incl. `cargo rustc --emit asm`) runs `build.rs` → `npm run build`. If an environment lacks node/npm, the script must error clearly (S1 requirement), and R4's measurement step needs the environment noted in its follow-up plan.
6. **R4 scope:** whether `for_each_within` is worth the SoA restructure is explicitly deferred to a benchmark (S4) — do not start the `CellEntry` layout change before the measurement exists.
