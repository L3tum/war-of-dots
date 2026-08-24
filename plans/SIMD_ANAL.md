# SIMD Auto-Vectorization Analysis

> **⚠️ SUPERSEDED — do not use as the source of truth.**
> This analysis predates the COMBAT_TARGETING redesign landing. It has been
> re-analyzed against the current tree in
> **`plans/deep-plan-20260821-231145.md`** (reviewed spec, evidence-labeled to
> HEAD), which: updates every `file:line` reference, supersedes §1/§4.4 (the
> deleted `nearby_enemies` / `NearbyEnemies` surface), re-ranks the
> recommendations (R1–R7), and rewrites `scripts/check_vectorization.sh`. Keep
> this file for the original `[CARRY]` asm evidence only.

Analysis of whether the SIMD-capable code paths are actually auto-vectorized by the
compiler, plus a survey of additional spots where SIMD could be applied easily.

- **Binary audited:** `target/release/war-of-dots` (release, up-to-date with `src`)
- **Codegen target:** `-C target-cpu=x86-64-v3` (from `.cargo/config.toml`, `rustflags`)
- **Toolchain:** `rustc 1.96.0`
- **Method:** `objdump -d` disassembly + per-symbol instruction audit (scratch under `/tmp`)
- **Key evidence ids:** `e684df2` (for_each_within scalar / GridIndex::build AVX2), `e4a5be3` (script false-PASS, per-loop scalar status)
- **Vectorization library (decision):** `glam` — stable, `#[repr(simd)]`, game-math oriented. See §4.3.
- **Companion plan:** `plans/COMBAT_TARGETING.md` — the combat targeting redesign. **Decisions resolved** (§8 there): explicit `attacking: Option<Entity>` target, directed engagements with **no counter-attack**, `soft_attack`/`soft_defense` split, two phases (targeting → fold + delete `NearbyEnemies`). It changes what combat iterates over and reshapes the combat distance loop, so re-measure §1 / §4.4 there *after* it lands.

---

## 1. The circle-intersection tests are NOT vectorized

`spatial::for_each_within` (the circle-intersection distance filter used by the
movement occupancy gate and `nearby_enemies`) has **4 monomorphizations** in the
binary. **All 4 have zero `ymm` (AVX2) instructions.** The per-entry inner loop is:

```asm
vmovsd  -0x8(%r15,%rbp,1),%xmm0   ; load (entry.x, entry.y) as 128-bit pair
vsubps  %xmm0,%xmm3,%xmm0         ; pos - entry
vmulps  %xmm0,%xmm0,%xmm0         ; dx^2, dy^2
vmovshdup %xmm0,%xmm1
vaddss  %xmm1,%xmm0,%xmm0         ; dist^2
vucomiss %xmm0,%xmm2              ; dist^2 < radius_sq ?
jbe     <next entry>              ; stride 0x14 (one CellEntry)
```

That's one scalar (x,y) pair per entry — **no `vcmpltps` mask, no `vpmovmskb`,
no cross-entry SIMD**. This is *expected*: the `f(entity, pos, owner_idx)` callback
sits **inside** the filter loop, which blocks LLVM's mask-and-gather idiom. In the
`nearby_enemies` monomorphization every hit additionally takes a direct per-element
`Vec::push` call (resolves to `RawVec::grow_one`); in the `movement` monomorphization
the callback is fully inlined (entity self-check via `vpcmpeqd`/`vtestpd`) with zero
per-element calls.

Note: the `spatial.rs` doc comment calls the inner test "FMA/SIMD-friendly,
branch-free inner test" — that is **aspirational**. `vfmadd` count is **0 in the
entire binary**.

### What IS genuinely vectorized
`GridIndex::build` (both copies) has **real AVX2 math** — 52 ymm lines including
4-wide `vdivps`/`vroundps` + `vpminsd`/`vpmaxsd` for the `cell_for` coordinate math.
So `x86-64-v3` *is* reaching codegen; the distance filter simply isn't the
beneficiary. AVX2 (`ymm`) appears in ~1,948 symbols binary-wide, but dominated by
`tokio`/`serde`/`hecs` struct copies and `GridIndex::build` — not the hot
circle-intersection test.

---

## 2. The existing `check-vectorization` script gives a FALSE PASS

`make check-vectorization` (i.e. `scripts/check_vectorization.sh`) currently reports:

```
✓ vector ops found: influence.*run (15 SIMD instructions)
✓ vector ops found: nearby_enemies.*run (6 SIMD instructions)
RESULT: PASS
```

That's wrong for three reasons:

1. Those 15/6 matches are **ymm struct copies** (`vmovdqu (%rbx,%r9,8),%ymm0` moving
   32-byte struct chunks) and loose-regex hits on xmm scalar ops — **no vector math**.
   There is **no u16-lane SIMD in any game function** (the only 16-bit SIMD ops in
   the binary live inside statically-linked sqlite3).
2. `FUNCS` does **not** include `movement.*run`, `for_each_within`, or
   `GridIndex::build` — the symbols that actually matter.
3. `AVX_RE` matches the substring `ymm` anywhere, so memory-copy code satisfies it.

---

## 3. Other hot loops — verified codegen status

| Loop | Location | Status |
|---|---|---|
| `fold_influence_into` (u16 max + u8 stamp, full grid × player, every dirty tick) | `src/game/systems/influence.rs:261` | **Scalar** — `movzwl`/`cmp`/`jbe` per cell, even with in-loop bounds checks. Best hand-SIMD payoff in the game code |
| `stamp_circle` inner row (u16 saturating add, ≤21 iters) | `src/game/systems/influence.rs:163` | **Scalar** — `add`+`cmovb` per cell; low payoff (short rows) |
| `union_vision_grids` flat byte-OR (`if grid[i] { result_flat[i] = true }`) | `src/game/game.rs:1552` | **Scalar per-byte** — the `if` guard blocks vectorization |
| `supervisor_vision_grid` row OR (`*cell = *cell \|\| s`) | `src/game/game.rs:1477` | **Already 256-bit vectorized** (`vpor`/`vorps`, 128 cells/iter) |
| `for_each_within` distance filter | `src/game/spatial.rs:209` | **Scalar** (see §1) |

The `supervisor_vision_grid` vs `union_vision_grids` asymmetry is a useful proof
point: the zipped row-OR loop vectorizes to 256-bit, while the flat-indexed loop with
the redundant `if` stays scalar — same underlying byte-OR.

`fold_influence_into` scalar loop (inlined into `influence::run`):

```asm
47 0f b7 0c 42   movzwl (%r10,%r8,2),%r9d   ; grid[i]  (u16)
66 46 3b 0c 42   cmp    (%rdx,%r8,2),%r9w   ; grid[i] > max_inf[i]
76 da            jbe    <loop>
66 46 89 0c 42   mov    %r9w,(%rdx,%r8,2)   ; max_inf[i] = grid[i]
46 88 0c 01      mov    %r9b,(%rcx,%r8,1)   ; dominant[i] = idx
eb c7            jmp    <loop>
```

`union_vision_grids` scalar byte-OR:

```asm
80 3c 38 01      cmpb   $0x1,(%rax,%rdi,1)   ; grid[i]
75 dd            jne    <loop>
c6 44 3d 00 01   movb   $0x1,(%rbp,%rdi,1)   ; result_flat[i] = true
eb cd            jmp    <loop>
```

`supervisor_vision_grid` (already vectorized, 128 cells/iter):

```asm
c4 a1 7e 6f 04 19   vmovdqu (%rcx,%r11,1),%ymm0
c4 a1 7c 10 4c 19 20 vmovups 0x20(%rcx,%r11,1),%ymm1
...
c4 a1 7d eb 04 1f   vpor   (%rdi,%r11,1),%ymm0,%ymm0
c4 a1 74 56 4c 1f 20 vorps 0x20(%rdi,%r11,1),%ymm1,%ymm1
...
49 83 eb 80         sub    $0x80,%r11        ; +128
75 a6               jne    <loop>
```

### Explicitly NOT worth SIMD
- **`supply`** — order-dependent FCFS funding (`remaining_funds[oi] -= cost`);
  vectorizing would change *which* unit gets funded → breaks documented
  determinism.
- **`combat`** — sequential break-on-first-target semantics + per-pair
  `world.get` / `HashSet::contains`; irreducibly sequential + allocation-free by design.
- **`death_cleanup`** — bit-identical-semantics contract (order-preserved `retain`).
- **`city_capture`** — needs AoS→SoA restructure for marginal f32 FMA gain.
- **`encirclement`** — the BFS dominates; seed-scan vectorization is low payoff.
- **`water` / `surrender` / `delta`** — no parallel numeric loop surface.

---

## 4. Recommendations, ranked by effort/risk

1. **Free win, stable, zero risk — `union_vision_grids`** (`src/game/game.rs:1552`):
   replace `if grid[i] { result_flat[i] = true }` with branchless
   `result_flat[i] |= grid[i]`. Same shape as `supervisor_vision_grid`, which LLVM
   already auto-vectorizes to 256-bit.
2. **Fix `scripts/check_vectorization.sh`**:
   - Add `for_each_within`, `movement.*run`, `spatial.*GridIndex.*build` to `FUNCS`.
   - Tighten `AVX_RE` to require an arithmetic mnemonic on a ymm register
     (e.g. `(vaddps|vmulps|vcmpltps|vporps|vpadd|vpmax).*%ymm`) so struct copies
     stop producing false passes.
3. **Vectorization library — decision: `glam`.** Use **`glam`** (stable,
   `#[repr(simd)]` vector types, game-math oriented) for any hand-rolled SIMD. It
   auto-vectorizes very well on the existing `x86-64-v3` target, needs **no nightly
   feature gate**, and is the realistic stable option. (Rejected alternatives:
   `std::simd`/`portable_simd` — nightly + feature-gate; `wide` — weaker maintenance;
   `cgmath` — scalar math types, no vector codegen guarantee; wrong tool for lanes.)
   First target: `fold_influence_into` (u16 compare + select over 16 lanes, u8
   dominant select, ~15 lines). A branchless-select rewrite might *also* let LLVM
   auto-vectorize it on stable — recheck codegen either way.
4. **`for_each_within` two-phase (SIMD mask → gather hits → call `f`)** is the honest
   fix for §1, **but** `CellEntry` is interleaved `(x, y, x, y, …)`, so 4-wide `f32`
   lanes need an **SoA split** (separate x/y arrays) or per-load `vpermilps` shuffles —
   this is a buffer-layout change, not just adding intrinsics. Also set expectations:
   cell occupancy is O(1) by the `MIN_UNIT_DISTANCE` rule, so the distance filter runs
   over small slices and the SIMD setup cost may not pay back at typical densities.
   Measure with the microbenchmark harness first; the per-hit `Vec::push` in the
   `nearby_enemies` shape may be worth more than the vector math. **Note:** the
   `nearby_enemies` shape is slated for removal in `COMBAT_TARGETING.md` — re-evaluate
   this item *after* the combat redesign changes the loop's caller.
5. **Ordering:** land `COMBAT_TARGETING.md` **before** any hand-SIMD of the combat /
   `nearby_enemies` distance paths — the redesign changes the iteration shape, and
   re-measuring a loop you're about to restructure is wasted work. Items 1 and 2
   (branchless `|=`, script fix) are combat-independent and safe to do anytime.

---

## 5. Suggested next steps (proposed)

- Implement §4.1 (branchless `|=`) and §4.2 (script fix) — small, safe, stable-only.
- Re-run `make check-vectorization` after the script fix to confirm it correctly
  reports the `for_each_within` / `movement` loops as non-vectorized.
- Vectorization library is decided: **`glam`** (stable). No nightly/`portable_simd`
  decision needed.
- **Sequence the combat work first** (`COMBAT_TARGETING.md`), then re-measure the
  distance-path SIMD (§4.4) against the post-redesign loop.
