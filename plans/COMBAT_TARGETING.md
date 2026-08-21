# Combat Targeting Redesign

Replace the boolean `UnitFlags.attacking` + `NearbyEnemies` list with an **explicit
per-unit target** (`attacking: Option<Entity>`), so combat resolves a concrete
attacker→target edge, and (optionally) fold the spatial scan into combat to delete
the `nearby_enemies` system + `NearbyEnemies` component.

Companion to `plans/SIMD_ANAL.md` (cross-linked). This redesign changes what combat
iterates over and reshapes the combat distance loop, so any SIMD work on those paths
(`SIMD_ANAL.md` §1 / §4.4) must be **re-measured after this lands**.

- **Companion plan:** `plans/SIMD_ANAL.md`
- **Status:** decisions D1–D5 **RESOLVED** — final design is **§8**, revised by **§9**
  (sticky acquisition = approved; defensive-counter damage model = **APPROVED**,
  incl. §9.3 infantry def bump). **Phase 1 IMPLEMENTED & GREEN** (see §10).
  §3–§7 kept for history.
- **Resolved one-liners:** D1 = introduce the `soft_attack`/`soft_defense` split ·
  D2 = closest + `Entity::id()` tiebreak (sticky — see §9.1) · D3 = role-based:
  one-way = defender counters with **defensive** stats (revised §9.2); mutual
  = both attack · D4 = **persist** target, clear only on target death or retreat ·
  D5 = **two phases** (targeting now, structural fold after).
- **Scope files:** `components.rs`, `systems/nearby_enemies.rs`, `systems/combat.rs`,
  `systems/supply.rs`, `systems/death_cleanup.rs`, `game.rs` (run order +
  `combat_health` + snapshot), `world_ext.rs`, `server/protocol.rs` / `delta.rs`,
  `benchmark.rs`.

---

## 0. Findings that reshape the proposal (verified, with file:line)

These are the facts the design must account for. Each is verified in the current tree.

**F1 — No attack/defense stats in the damage formula.**
`config.rs:181–184` defines `soft_attack`/`soft_defense`/`hard_attack`/`hard_defense`
on `UnitType`, and they are set in `unit.rs:284–287` / `game.rs:1801–1804`, but combat
**never reads them**. `combat.rs` computes a single `soft` for both sides:
`a_soft = 10·(men/max_men)·(org/100)·water`, `d_soft` the same, then
`a_eff = a_soft/(d_soft+0.1)`, `d_eff = d_soft/(a_soft+0.1)`. The only
attacker/defender asymmetry is terrain (`a_terrain` vs `d_terrain`) and equipment
effectiveness (`a_eff_malus` vs `d_eff_malus`) — each side uses its **own** values, so
for identical units on identical terrain the two sides deal equal damage.
⇒ The "both use attacking stats vs defensive stats" rule has **zero mechanical effect
today**. Making it real is a **new mechanic** (decision **D1**), not a bug fix.

**F2 — The client "attacking" field is `UnitFlags.fighting`, not `UnitFlags.attacking`.**
`game.rs:1697` maps the protocol snapshot `attacking: u.fighting`. So retyping
`UnitFlags.attacking: bool → Option<Entity>` does **not** change the wire protocol.
The `attacking` flag is **internal-only**. Its full read surface:
- write: `nearby_enemies.rs:89` (`if moved { attacking = has_enemies }`)
- combat candidate filter: `combat.rs:193` (`attacking || fighting || !nearby.is_empty()`)
- combat `attackers` counter: `combat.rs:211`
- supply org-recovery gate: `supply.rs:299` (`!fighting && !attacking`)
- benchmark: `game.rs:651` (`combat_health` → `units_with_attacking`)

**F3 — `attacking` is never cleared ⇒ real org-recovery bug.**
The only write is `nearby_enemies.rs:89`, gated on `moved`. A unit that moves while an
enemy is in range gets `attacking=true`; if it then **stops moving** (path consumed)
and the enemy leaves, `fighting` is cleared by the retreat check but `attacking` stays
`true` **forever**. `supply.rs:299` gates org recovery on `!fighting && !attacking`, so
the unit **never recovers organisation** until it moves again. Regression-test this
first (Phase 0).

**F4 — `death_cleanup` depends on `NearbyEnemies` freshness/symmetry.**
`death_cleanup.rs` purges dead units from other units' lists via a reverse lookup
through the dead units' **own** lists, which is exact only because `nearby_enemies`
rebuilds every list from one symmetric grid snapshot each tick (documented in
`death_cleanup.rs` (a)–(d)). Removing the list removes this whole dependency (a win)
but `death_cleanup` still must **despawn dead entities + clean `entity_reverse`** —
it is not deletable, only simpler.

**F5 — Multi-enemy melees break the "single target + mutual" model.**
`attacking: Option<Entity>` holds **one** target. A unit within `COMBAT_DISTANCE` of
2+ enemies (common in a melee, since `MIN_UNIT_DISTANCE=8 < COMBAT_DISTANCE=10`) has no
well-defined target without a tiebreak. "Both movers point at each other" is also
over-constrained: a mover with two enemy neighbors can't set two targets.

**F6 — Folding `nearby_enemies` into combat does NOT remove the O(n×c) scan.**
`nearby_enemies` runs a 3×3 grid + `dist²` scan per unit (`spatial::for_each_within`,
`COMBAT_DISTANCE_SQ=100`), c=O(1) by the occupancy rule. Folding it into combat moves
the same scan into combat's candidate loop. You save: the `NearbyEnemies` component
(+ its archetype churn), the `death_cleanup` purge pass, and one system call. You do
**not** save the spatial scan.

**F7 — Run order is fixed.** `game.rs::tick_game`:
`economy → water → movement → [tickcontext] → nearby_enemies → combat → supply →
city_capture → [influence] → [encirclement] → surrender → death_cleanup`.
`movement` builds the **post-move** `GridIndexScratch`; that grid is what
`nearby_enemies`/combat consume, and it is already reachable from combat (passed as
`grid_scratch`), so a fold needs no new grid plumbing.

---

## 1. Current state (verified)

**Components / flags** (`components.rs`):
- `UnitFlags { healing, fighting, attacking, moved, encircled, resupply_enabled, on_water }` — all `bool`, `#[derive(Copy)]`.
- `NearbyEnemies { entities: SmallVec<Entity,8>, initialized: bool }`.

**`nearby_enemies::run`** (consumes movement's post-move grid):
- For **every** unit: scan within `COMBAT_DISTANCE`, collect enemies (skip self + same-owner via u8 `owner_idx`), write `NearbyEnemies.entities` + `initialized=true`, and set `attacking = has_enemies` **for movers only** (`moved` set by movement marks the applied movers).

**`combat::run`**:
- Candidates = units with `men>0 AND (attacking OR fighting OR !nearby.is_empty())`.
- Per candidate: scan its `nearby.entities`, pick the **first** in-range enemy (order =
  grid/hecs iteration order ≈ spawn order, *not* distance), resolve **one** engagement
  bidirectionally (attacker deals via `a_eff`, defender counters via `d_eff`), set
  `fighting=true` on both, dedupe via normalized `resolved` pair set.
- Retreat check: candidates that didn't fight and whose `nearby` is now empty →
  `fighting=false`.
- Returns `combat_dead`.

**`supply::run`**: org recovery gated on `!fighting && !attacking` (`supply.rs:299`).

**`death_cleanup::run`**: reverse-lookup purge of dead from `NearbyEnemies` lists,
then despawn + `entity_reverse.remove`.

**Snapshot** (`game.rs:1672+`): protocol `attacking: u.fighting` (F2).

**Constants** (`constants.rs`): `COMBAT_DISTANCE=10` (SQ=100),
`MIN_UNIT_DISTANCE=8` (SQ=64), `SPATIAL_CELL_SIZE=10`.

---

## 2. Proposed design (your model, restated)

**State:** `attacking: Option<Entity>` (the explicit target) replaces `attacking: bool`.

**Acquisition** (scan, per moved unit A):
- A moves into range of enemy B:
  - B **not** moving → `A.attacking = Some(B)`.
  - B **moving** → `A.attacking = Some(B)` **and** `B.attacking = Some(A)` (mutual; both "attack").

**Resolution** (combat):
- For each unit `U` with `attacking = Some(T)`:
  - if `T.attacking == Some(U)` → **mutual**: both are "attacking" (both use attack stats).
  - else → `U` attacks a **defending** `T`.
  - if `T` is dead (stale from a prior tick) → clear `U.attacking`.

**Cleanup:** `death_cleanup` handles what it can.

**Two structural options:**
- **A (your primary):** fold the scan into combat; delete `nearby_enemies` + `NearbyEnemies`.
- **B (your alternative):** add a `Combat` component; combat iterates only units with it.

---

## 3. Open decisions — **RESOLVED; see §8** (kept for history)

**D1 — attack vs defense stats (NEW mechanic).** The formula has no
`soft_attack`/`soft_defense` (F1). Options:
- **(a) Keep single `soft`** — targeting-only change, **no** damage change. Cheapest.
- **(b) Introduce the split** — mutual ⇒ both use attack-derived strength; one-way ⇒
  attacker uses `soft_attack`, defender uses `soft_defense`. Changes **every**
  engagement's damage, shifts all benchmark/test golden numbers, needs tuning.
**Leaning: (a) for the first cut**, add (b) as a separate phase if you want the mechanic.

**D2 — multi-enemy target selection.** `Option<Entity>` needs a deterministic pick
when k>1 enemies are in range. Options: **closest** (min `dist²`, tie-break by
`Entity.id()`), first-in-scan, or a gameplay policy (weakest/lowest-org).
**Leaning: closest + id tiebreak** — O(c), c=O(1); also fixes the current
"first-in-list = spawn order" quirk.

**D3 — does a one-way defender counter-attack?** Currently **every** resolved pair is
bidirectional (defender counters). If "A attacks defending B" means B only takes
damage (no counter), that's a further damage-model change.
**Leaning: keep bidirectional** (defender still counters) to minimize behavior change;
the "attack vs defense" distinction is expressed via stats (D1), not via whether a
counter exists.

**D4 — does the target persist across ticks?** Your "next tick combat is resolved"
implies persistence (intent continuity: A keeps firing at B until B dies/leaves).
- **(a) Persist + revalidate** — keep target if alive & in range; else clear (optionally
  re-acquire closest). This is your "cleaned up here" step.
- **(b) Recompute fresh each tick** — no staleness, but intent can flip between
  equidistant targets.
**Leaning: (a) persist + revalidate.**

**D5 — fold (drop `NearbyEnemies`) vs keep / Combat component.**
- **A (fold + delete):** removes a system + component + the `death_cleanup` purge, but
  the scan moves into combat and you must rework `combat_health` benchmark metrics
  (`units_with_enemies`, `total_nearby_entries`), the `world_ext` "initialized/absence"
  marker, and the retreat check (needs "has in-range enemy" from a scan/target).
- **B (Combat component):** smaller, lower-risk; a hecs query visits **only** engaged
  units (arguably *more* scalable than A), but you still need the in-range scan for
  target acquisition + retreat, so `NearbyEnemies` (or an equivalent) is not fully gone.
**Leaning: do the targeting fix first in the current structure; treat A/B as a later,
separate phase chosen after the benchmark shows the extra pass is worth removing.**

---

## 4. Recommended architecture (incremental; each phase ships green)

The targeting fix and the system-removal are **independent** — separating them de-risks
a full combat rewrite. Recommended order:

### Phase 0 — real bug fix, no behavior change (do first, standalone)
Fix the sticky-`attacking` org-recovery bug (F3). Two options:
- **0a:** in the same pass that sets `attacking`, also clear it when the unit has no
  in-range enemy (for movers); and/or
- **0b:** gate org recovery on **actual engagement** (`fighting` or has-in-range-enemy)
  instead of the sticky `attacking`.
Add a regression test: mover with enemy in range → clear path → enemy gone → run N
ticks → assert organisation recovers. **Ships independently; zero determinism change.**

### Phase 1 — explicit target (answers "who am I attacking?")
- Change `UnitFlags.attacking: bool → Option<Entity>` (still `Copy`; `hecs::Entity` is
  `Copy`). *(Alternative: a dedicated `UnitTarget { target: Option<Entity> }` component
  if you prefer separation from `UnitFlags`.)*
- Set it during acquisition (Phase D5=keep: in `nearby_enemies`; or folded). Pick per
  **D2** (closest + id tiebreak).
- Combat resolves against the explicit target: validate `T` alive + still in range (else
  clear per **D4**), then the mutual check `T.attacking == Some(U)` for the
  attack/defense treatment (per **D1**/**D3**).
- Candidate set stays equivalent: `attacking.is_some() OR fighting OR has-in-range`.
- Client field stays `fighting` (F2) — no wire change.
- **Re-baseline** the `combat.rs` men-range tests (`985.0..1000.0` etc.) — they shift
  slightly once target = closest (vs first) and if D1/D3 change damage.

### Phase 2 — structural (only after Phase 1 is green)
- **Option A (fold + delete):** move the scan into combat; delete `nearby_enemies::run`
  + `NearbyEnemies`; rework `combat_health` (replace `units_with_enemies` /
  `total_nearby_entries` with target-based or drop), the `world_ext` "initialized/absence"
  marker, and the retreat check; simplify `death_cleanup` (drop the purge pass, keep
  despawn + `entity_reverse`).
- **Option B (Combat component):** add `Combat { target, fighting }`; combat queries
  `(Combat, …)`.
Choose **A or B** per D5 after benchmarking.

### Phase 3 — optional: wire in `soft_attack`/`soft_defense` (only if D1 = (b))
Introduce the attack/defense split; re-tune and re-baseline goldens.

---

## 5. Removal/impact checklist (for Phase 2, Option A)

| Item | Location | Action |
|---|---|---|
| `nearby_enemies::run` | `systems/nearby_enemies.rs` | delete (scan moves to combat) |
| `NearbyEnemies` component | `components.rs:69–78` | delete |
| `NearbyEnemies` spawn note (absence=uninitialized) | `world_ext.rs:96–97` | delete / rework the "initialized" marker if anything keys off it |
| `combat_health` reads `nearby` | `game.rs:647–663` | replace `units_with_enemies`/`total_nearby_entries` (target-based) or drop; `units_with_attacking` → `attacking.is_some()` |
| `combat` candidate filter / target scan / retreat check | `combat.rs:193, 265+, 490+` | replace list reads with per-candidate scan + explicit target |
| `death_cleanup` purge pass | `death_cleanup.rs` | delete the reverse-lookup purge; keep despawn + `entity_reverse` |
| `benchmark.rs` health print | `benchmark.rs:898–900, 1136` | update or drop the with-enemies/total-nearby metrics deliberately |
| `supply` org gate | `supply.rs:299` | re-express on `attacking.is_some()` / engagement (align with Phase 0) |
| `combat.rs` tests that spawn `NearbyEnemies` | `combat.rs:581, 627`, `populate_nearby_enemies` | migrate fixtures to the new target model |

---

## 6. Test plan

- **Phase 0:** sticky-`attacking` org-recovery regression (see above).
- **Phase 1 semantics:**
  - A↔B both movers, in range → `A.attacking=Some(B)`, `B.attacking=Some(A)` (mutual).
  - A mover → B static in range → `A.attacking=Some(B)`, `B.attacking=None` (A attacks a defending B).
  - A↔B↔C 3-mover melee, all in range → each unit attacks **exactly one** (closest, id tiebreak); each takes counter-damage from the others; one attack per tick preserved.
  - Target dies mid-tick (killed by an earlier engagement) → stale target not used; `dead_set` short-circuit preserved.
  - Target moves out of range → target cleared/re-acquired per D4.
  - Determinism: same seed/positions → identical target choices (id tiebreak).
- **Golden re-baseline** (if D1=(b) or D3 changes damage): regenerate `combat.rs` ranges + any benchmark golden.
- **Bench:** tick time not regressed; `combat_health` metrics still emit (or intentionally removed).
- **Phase 2:** the `stale_one_sided_reference_survives_is_safe` / reverse-lookup tests
  are retired or reworked **intentionally** (their invariant no longer exists); confirm
  `death_cleanup` still despawns + cleans `entity_reverse`.

---

## 7. Risks

- **Behavior/determinism change** (combat numbers shift) — mitigate: incremental phases,
  re-baseline goldens at the boundary of each.
- **Melee target flip-flop** if D4=recompute — mitigate: persist (D4=a) + id tiebreak (D2).
- **`death_cleanup` invariant coupling** — the reverse-lookup purity argument (a)–(d)
  breaks when the list goes away; verify the stale-window test is retired, not silently
  failing.
- **Benchmark `combat_health` consumers** (`benchmark.rs` prints these) — update or drop
  deliberately; don't leave a metric pointing at a deleted component.
- **Client "attacking" semantics** — stays `fighting` (F2); confirm that is the intended
  client signal, or wire it to `attacking.is_some()` (a separate, small protocol choice).
- **Scope creep** — F1 (attack/defense stats) is the biggest hidden cost; keep it out of
  the targeting fix (Phase 1) unless D1=(b) is explicitly chosen.

---

## 8. Final design — decisions D1–D5 resolved (supersedes §3–§7)

### 8.1 State
- `UnitFlags.attacking: Option<hecs::Entity>` — the explicit target; `None` = no
  target. `hecs::Entity` is `Copy`. **Check** it also impls `Default` (needed for
  the `#[derive(Default)]` on `UnitFlags`, used at `world_ext.rs:94`); if not,
  hand-write `impl Default for UnitFlags`.
- `UnitFlags.fighting: bool` — **kept**. Set by combat on **both** sides of a
  resolved engagement (attacker *or* defender). Feeds the wire field (F2) and the
  supply org gate. Cleared by the retreat check.
- `NearbyEnemies` — kept in Phase 1 (write-only for the game loop; kept so the R2
  option below stays open and Phase 2 has a clean before/after), **deleted in
  Phase 2**.

### 8.2 Acquisition (mover retarget rule) — replaces `nearby_enemies`' flag logic
Runs where `nearby_enemies` runs today (Phase 1), inside combat (Phase 2):
- For **each moved unit** (`moved` flag = applied movers): scan the post-move
  grid (3×3 + `dist² < COMBAT_DISTANCE_SQ`, skip self + same `owner_idx`) and set
  `attacking = closest in-range enemy by dist², tiebreak `Entity::id()``, or
  `None` if none in range`.
- **Movers always overwrite** (re-target every tick they move — how B switches
  A→C in the D3 scenario). **Non-movers persist** (D4).
- Fixes the F3 sticky-`attacking` org-recovery bug **structurally** (a mover with
  no in-range enemy gets `None`).
- Documented behavior changes:
  - **C1 — parked units are defense-only.** A stationary, target-less unit never
    attacks, even with an enemy in range (old: auto counter-attack via the
    `!nearby.is_empty()` candidate term). The player must order it to move to
    fight back.
  - **C2 — moving through an enemy's range attacks it.** A retreating B is a
    mover; while A is still B's closest in-range enemy, B attacks A even while
    retreating. B switches to C only once C is in range and closer.

### 8.3 Resolution — directed engagements, no counter (D3)
Candidates = `men > 0 && attacking.is_some()` — **`NearbyEnemies` leaves the
query**. For each candidate U with target T, in hecs iteration order:
1. **Validate T**: T ∉ `dead_set` (died earlier this tick → skip + clear
   U.target); T alive in ECS; different owner; `dist² < COMBAT_DISTANCE_SQ`.
   T alive but **out of range** → **clear U.target** (the D4 "retreat" clear)
   and skip.
2. **Resolve U→T (directed)**: U deals damage to T (§8.5). T deals **nothing**
   back. T takes men/org/attrition/supply bookkeeping (today's defender side).
   U takes **no damage** from this engagement; `U.fighting = true`.
   T dies → `dead` + `dead_set` **at most once** (fixes the old double-
   `dead.push` wart for defenders killed earlier in the same tick).
   `fought.insert(U); fought.insert(T)`.
3. **`resolved` (normalized-pair) set is DELETED** — each unit has exactly one
   target → at most one attack per tick; directed edges never need dedup. A
   mutual A↔B = **two directed resolutions** (A→B and B→A), each when its
   attacker is iterated; first-iterated deals damage first (inherited,
   deterministic). A defender can be hit by **multiple** attackers in one tick
   (no change) while itself attacking its single target (the D3 scenario: B
   defends vs A, attacks C).

### 8.4 Target lifecycle — the only writes
| # | Site | Rule |
|---|---|---|
| 1 | Mover scan (`nearby_enemies` Ph1 / combat Ph2) | moved unit: `target = closest in-range, or None` (overwrite) |
| 2 | Combat candidate validation | target dead-this-tick / gone / out of range → `None` |
| 3 | `death_cleanup` (after supply/surrender deaths) | every unit with `target ∈ tick dead set` → `None` |

`fighting`: set on both sides of resolved engagements; retreat check clears
`fighting && !fought_this_tick && attacking.is_none()` → a unit out of range
gets target cleared (site 2) *and* `fighting` cleared in the same combat run →
**org recovery resumes while out of range**; re-acquires on re-entry if moving.

### 8.5 Damage formula (D1) — **superseded by §9.2** (kept for history)
Per directed engagement U(attacker) → T(defender):

```
a_soft = 10 · (U.men/U.max_men) · (U.organisation/100) · (U.on_water ? 0.75 : 1.0)
           · unit_type(U).soft_attack          // fixture 5.0; prod Light 10.0 / Heavy 15.0
d_soft = 10 · (T.men/T.max_men) · (T.organisation/100) · (T.on_water ? 0.75 : 1.0)
           · unit_type(T).soft_defense         // fixture 5.5; prod Light 11.0 / Heavy 16.5 (§9.3)
a_eff  = a_soft / (d_soft + 0.1)
dmg    = a_eff · terrain_modifier(U's terrain) · 5.0 · equipment_effectiveness(U)
```

- **Deleted:** `d_eff`, `dmg_to_attacker`, defender `equipment_effectiveness`
  (it only fed the counter), all attacker-side damage bookkeeping.
- **Kept:** per-side terrain (attacker's), water 0.75, `+0.1` floor, org loss
  `10·(men_lost/men)`, `attrition_in_place(dmg, fighting_attrition)`, supply
  loss `(dmg·rate).max(1.0)`.
- Effect (pre-§9.3): infantry `soft_attack/soft_defense` = 5/4 → superseded; with
  the §9.3 bump, `soft_defense > soft_attack` (11/10, 16.5/15) → **defending is
  slightly favorable** (≈1.21× exchange, §9.2). `hard_attack`/`hard_defense`
  remain unused (dead config today — document, don't delete).
- **Every combat number shifts** → re-baseline goldens (Phase 1 gate).

### 8.6 Consumers
- `supply.rs:299` org gate → `!flags.fighting && flags.attacking.is_none()`.
- Wire `attacking: u.fighting` — **no protocol change** (F2).
- `combat_health` (game.rs:647+): `units_with_attacking` → `attacking.is_some()`;
  `units_with_enemies`/`total_nearby_entries` stay in Phase 1, reworked in Phase 2.
- `CombatScratch`/`combat_diag` counters: `pairs_*` re-scoped to target
  validation (`targets_out_of_range`, `targets_dead`, …); update `benchmark.rs`
  prints accordingly.

### 8.7 Phase 1 — explicit targeting, current structure (lands first)
Per-file changes:
- **`components.rs`** — `UnitFlags.attacking: bool → Option<hecs::Entity>` + doc;
  `Default` check (8.1); `NearbyEnemies` doc updated (write-only from Phase 1 on
  for the game loop).
- **`systems/nearby_enemies.rs`** — for movers: write `attacking = closest
  in-range enemy (dist², id tiebreak) or None` (replaces `attacking =
  has_enemies`); `NearbyEnemies` lists + `initialized` still written (kept).
- **`systems/combat.rs`** — candidate query drops `&NearbyEnemies`, filter
  `men>0 && attacking.is_some()`; target taken from `flags.attacking` (the whole
  nearby-list scan loop is gone); validation + clear per 8.3; directed resolution
  per 8.5 (delete `d_eff`/`dmg_to_attacker`/attacker bookkeeping); **delete
  `resolved`** (struct field, `clear()`, insert, and the capacity test at
  `combat.rs:1011-1021`); counters re-scoped (8.6); retreat check = 8.4 rule.
- **`systems/supply.rs`** — org gate expression (8.6).
- **`systems/death_cleanup.rs`** — add **site 3**: one pass over
  `(Entity, &mut UnitFlags)`, `attacking.is_some() && dead_set.contains(target)`
  → `None` (alongside the existing purge + despawn; purge itself stays in Ph1).
- **`game.rs`** — `combat_health` + `combat_diag` per 8.6.
- **`server/protocol.rs` / `delta.rs`** — **no change**.
- **Tests** — migrate combat fixtures (`populate_nearby_enemies` → set targets
  directly); re-baseline men-range goldens for the new formula; add the 8.10
  semantics tests.
- **Exit criteria:** full test suite green incl. re-baselined combat numbers;
  benchmark tick-time delta recorded (no regression); F3 regression test passes
  (mover → enemy leaves → org recovers without moving again).

### 8.8 Phase 2 — fold the scan into combat, delete `NearbyEnemies`
- **Delete** `systems/nearby_enemies.rs` + its `tick_system!` call (game.rs).
- **`combat::run`** gains `&MovementScratch` (the `movers` list) +
  `&GridIndexScratch` (post-move grid) — both already exist and are passed by
  `game.rs`; **no new plumbing** (F7). Mover scan (site 1) runs at the top of
  `combat::run`, before the resolution loop.
- **Delete** `NearbyEnemies` (components.rs), the `world_ext.rs:96-97`
  absence-means-uninitialized note + `initialized` marker, and the `death_cleanup`
  reverse-lookup **purge pass** (keep despawn + `entity_reverse` + site 3).
- **`combat_health`** — drop `units_with_enemies`/`total_nearby_entries` (or
  replace with target counts + `engaged_pairs`); update `benchmark.rs` prints
  deliberately.
- Grid: `movement` still builds both grids (pre-move = occupancy gate,
  post-move = combat). If the fold ever proves not worth it (benchmark), the
  fallback is the original **Option B** (`Combat` component) — the targeting
  work in Phase 1 is unaffected by that choice.

### 8.9 Behavior changes (review before implementing)
1. **C1** parked, target-less units are defense-only (no auto counter-attack).
2. **C2** moving through an enemy's range attacks it (retreat ≠ ignoring).
3. **D3** attacker takes no damage in a one-way engagement → **charging a parked
   unit deals free damage** (today the parked unit counters). Biggest gameplay
   swing of the redesign.
4. Combat numbers shift: one-way fights — defender counters with its defensive
   stat at ≈1.21× exchange (infantry, §9.3); mutual fights — both use attacking
   stats (same-type pairs symmetric at atk 10 vs def 11).
5. Two adjacent *stationary* mutual attackers keep fighting every tick via
   **persisted targets** (the old mechanism, nearby-list, is gone in Ph2).
6. A defender killed earlier in the same tick is hit at most once (no more
   double `dead.push` / double `dead_count`).
7. Multiple attackers per tick on one defender — **no change**.

### 8.10 New semantics tests (Phase 1)
1. Mover A → parked B in range: `A.attacking=Some(B)`, `B.attacking=None`;
   A→B only; B loses men, A loses none; both `fighting=true`.
2. Both movers, in range: mutual — both lose men; each uses its own
   `soft_attack` vs the other's `soft_defense` (assert the atk-vs-def asymmetry,
   e.g. fixture 5.0 vs 5.5).
3. Parked A (no target) + mover B into range: B attacks A, A deals 0 (pins C1).
4. 3+ melee, all moving: each unit attacks **exactly one** (closest, id
   tiebreak); a defender takes damage from all attackers; ≤1 attack/unit/tick.
5. The D3 scenario: A↔B mutual → B retargets to C (moves, C becomes closest) →
   next tick A attacks defending B while B attacks C.
6. Pursuit/retreat: B leaves range → A's target cleared (site 2), A `fighting`
   cleared, org recovers (supply gate) while out of range; re-acquires on
   re-entry if A still moves.
7. Target killed earlier in the same tick (by attacker C): A skips (no wasted
   engagement, no double `dead.push`); `dead_count` exact.
8. Target dies to **supply** (post-combat): site 3 clears A's target; A does not
   attack a ghost next tick.
9. F3 regression: mover with enemy in range → stops → enemy leaves → N ticks →
   organisation recovers.
10. Determinism: two equidistant enemies → id tiebreak stable across ticks/runs.
11. Org-recovery gate: engaged (attacker *or* defender) does not recover org that
    tick; disengaged does.
12. D1 unit test: formula values for a known men/org/terrain/equipment case
    (pin the 5.0/4.0 multipliers).
13. **Sticky target** (§9.1): A moves toward parked B, targets B; C (parked) is
    also in range and closer after A's move → A **keeps** B (own movement does
    not trigger retarget). Then B moves → A rescans (may switch). A retargets
    only per triggers T1–T4.
14. **Mutual pair resolved once** (§9.2): A↔B mutual → exactly one engagement,
    both take attacker-type damage (atk vs def each way); `dead_count` exact;
    independent of iteration order (id rule).
15. **One-way defender counter** (§9.2): parked B hit by A → B deals damage back
    computed from B.soft_defense (assert the ≈1.21× exchange vs an attacker of
    equal stats — B deals *more* than it takes, §9.3), not B.soft_attack.

### 8.11 Risks (updated)
- **Gameplay swings C1/C2/D3** — the plan implements the spec literally; each is a
  one-line spec change if you want them back (e.g. "defender without its own
  target still counters" would restore old bidirectional behavior as a special
  case, contradicting a strict D3).
- **Determinism/ordering** — mutual pairs: first-iterated attacker strikes first
  (inherited); pinned by test 10.
- **Golden churn** — D1+D3 change every combat number; re-baseline at the Phase 1
  boundary, keep the old values in git history for diffing.
- **`hecs::Entity: Default`** — if unimplemented, `#[derive(Default)]` on
  `UnitFlags` breaks (check at first compile; hand-written Default is a
  one-liner).
- **Diagnostics churn** — `combat_diag`/`combat_health`/`benchmark.rs` consumers
  must be updated in the same commit as the counter re-scope.

---

## 9. Revision — sticky acquisition (approved) + defensive-counter damage (approved)

### 9.1 Sticky target acquisition (APPROVED — supersedes §8.2 "movers always overwrite")

A mover **does not retarget unless its target moved or it is retreating**. Its
own movement never triggers a rescan (it changes distances to *all* in-range
enemies equally-ish and would churn targets needlessly); a *different* enemy
entering range or moving likewise does not pull A away from its committed
`B`.

**Rescan triggers** (A scans for closest in-range enemy per §8.2 tiebreak only
when):
- **T1** A's `attacking == None` and A moved — first acquisition.
- **T2** A's target died / was despawned — site 3 (death_cleanup) cleared it;
  A's next mover scan reacquires.
- **T3** A's target is out of range (A is retreating, or B left) — rescan for a
  new target, possibly `None`.
- **T4** A's **target moved this tick** — rescan (closest among in-range
  enemies; A may switch B→C only if C is now closer *and B moved*).

Not triggers: A's own movement while B stays in range; C (a non-target enemy)
moving or entering range.

Implementation (cheap, verified): "target moved" = `world.get::<&UnitFlags>(
target).moved` — the `moved` flag is fully lifecycle'd per tick by `movement`
(cleared for all at tick start, `movement.rs:64-67`; set for actual movers at
end). One extra component read per candidate. Phase 1: `nearby_enemies` already
holds `&mut UnitFlags` per mover (it checks `.moved` at `:88`), so the target's
`moved` state is one `world.get`. Phase 2: same, inside combat.

Consequences vs §8.2: a pursuing A keeps B even while walking around a closer C
(C1-ish for attackers); B→C switching (your D3 scenario) now requires B's target
A to **move** or A to leave B's range — if A stands still and B retires toward
C, B keeps "attacking" A (out-of-range → T3 clears it on the tick A leaves
range, then B's next mover scan picks C). Behavior change to accept: **B cannot
voluntarily drop a stationary target** — the only drops are target-moved, dead,
or out-of-range. (If you want voluntary retarget, the client must re-issue move
orders that carry a new target — out of scope now.)

### 9.2 Role-based damage: defensive counter (APPROVED — supersedes §8.3 step 2 "T deals nothing" and §8.5's "deleted" list)

A purely one-way engagement makes parked units free damage — bad gameplay.
Fix: **keep bidirectional damage, but make the stats role-based**, and treat
mutual pairs specially (your original D2 "both attack" rule survives for the
one case where it's coherent):

**One-way engagement** (only U targets T — T is defending; parked or retreating):
```
T takes attacker damage : dmg_to_T = a_eff · terrain(U) · 5.0 · equip(U)
U takes defensive damage: dmg_to_U = d_eff · terrain(T) · 5.0 · equip(T)
  where a_eff = a_soft / (d_soft + 0.1)
        d_eff = d_soft / (a_soft + 0.1)   ← identical shape to today's counter
  a_soft = 10·(U.men/U.max)·(U.org/100)·water(U) · type(U).soft_attack
  d_soft = 10·(T.men/T.max)·(T.org/100)·water(T) · type(T).soft_defense  ← T's DEFENSE
```
The defender counters with its **defensive** stat: an infantry defender (def 11)
vs equal attacker (atk 10) now deals ≈ 1.21× the damage it takes (see §9.3) —
being on the foot is slightly **favorable**. T's `soft_attack` is **not used
anywhere** in a one-way fight; T's `soft_defense` governs both how hard it is to
hurt T *and* how hard T hits back.

**Mutual engagement** (U targets T and T targets U — both chose to attack):
resolved **once**, by the unit with the **lower `Entity::id()`** (deterministic;
replaces the `resolved` HashSet — the dispatch rule is self-evident:
> U processes its engagement with T iff `T.attacking ≠ Some(U)`, or (mutual ∧
> U.id < T.id). The other unit's iteration skips it.

- Both sides use **attacking** stats: T takes `dmg(U.atk vs T.def)`, U takes
  `dmg(T.atk vs U.def)` — exactly the old two-way formula shape with the D1
  split. No defensive-counter term (both are attacking).
- Same-type mutual pairs are damage-symmetric (both exchange atk 10 vs def 11);
  iteration
  order affects only simultaneous-death bookkeeping.

**Resolution flow (per candidate U, target T):**
1. Validate (as §8.3): T ∉ `dead_set`, alive, other owner, in range — else
   clear/skip per §8.4.
2. Classify: `mutual = T.attacking == Some(U)`.
   - mutual ∧ U.id > T.id → **skip** (T's iteration resolves it).
3. Compute: always `dmg_to_T = f(U.atk, T.def)` (U is attacking). Additionally,
   if **one-way** (¬mutual): `dmg_to_U = f(T.def, U.atk)` (defensive counter).
4. Apply T-side bookkeeping (men/org/attrition/supply) with `dmg_to_T`; U-side
   with `dmg_to_U`. Both `fighting = true`; deaths → `dead`/`dead_set` once;
   `fought` both.

**U always takes damage.** One-way: U takes the defensive counter (T.def vs
U.atk). Mutual: U takes the atk-based damage (T.atk vs U.def). There is no
engagement in which an engaged unit takes nothing — **every engaged unit both
takes and deals damage**.

**Effect on the §8.9 swing list:**
- Swing 3 ("charging a parked unit deals free damage") → **mitigated**: parked
  B counters at ≈ 1.21× exchange (infantry, §9.3), on its own terrain bonus
  (hills: defending on hills still 1.5×).
- Swing 1 (C1 parked defense-only) → softened: parked units still deal damage
  (defensive) but never initiate; they cannot pursue/retarget either.
- New stat usage: `soft_attack` of a *defending* unit is unused in that
  engagement; `soft_defense` is now load-bearing. `hard_*` still dead config.
- **All combat numbers shift** (same golden re-baseline as §8.5).

**Tests added** (§8.10 items 13–15); existing tests 1–2 updated: test 1 (A→parked
B) now expects B to take damage back (defensive counter, test 15);
test 2 (mutual) expects one engagement, both atk-based (test 14).

**Terrain rule (resolved, approved):** the defensive counter uses the
**defender's own terrain** — consistent with "defender's stats characterize the
defense" (defending on hills is harder to chew through, and its counter comes
with the 1.5× hill bonus).

### 9.3 Config: infantry defensive bump (APPROVED, APPLIED)
- **Production** (`config.toml`, the only runtime source): Light Infantry
  `soft_defense` 10.0 → **11.0**; Heavy Infantry 15.0 → **16.5** (+10% relative —
  defending slightly favorable). Tank unchanged (5/10 — not infantry, already
  defensive). No schema/protocol impact (values only; fields already shipped).
- **Test fixtures** mirroring the ratio: `unit.rs::test_unit_type` and
  `game.rs::basic_unit_type` `soft_defense` 4.0 → **5.5** (attack stays 5.0).
- **Exchange math** (fresh unit, plains, full men/org, equal everything else):
  damage-taken ∝ `a_soft/(d_soft+0.1)`, counter ∝ `d_soft/(a_soft+0.1)`. With
  `d_soft/a_soft = 1.1` (e.g. 110 vs 100): counter/taken ≈ **1.21**. The stat
  ratio roughly *squares* through the exchange (def appears in both the
  damage-taken denominator and the counter numerator), so +10% stat ≈ +21%
  exchange. Dial: ±0.5 def ≈ ∓10% exchange if tuning later.
- **Applied pre-Phase 1** — zero behavioral impact until combat reads the field
  (verified: no read sites in current code); `cargo check` + 17 combat tests
  green at time of change.

---

## 10. Phase 1 implementation log (COMPLETE)

**Landed as specified in §9.1/§9.2/§9.3 + §8.7/§8.10.** All 427 tests green
(205 lib + 200 bin + 22 integration, incl. new e2e), zero warnings, all
targets build.

**Changes by file:**
- `components.rs` — `UnitFlags.attacking: bool` → `Option<hecs::Entity>`;
  hand-written `Default` (`hecs::Entity` has no `Default`); `fighting` is now
  the per-tick engaged flag (set by combat as attacker *or* defender, cleared
  by the retreat check); `NearbyEnemies` doc marked write-only-for-combat
  (diagnostics + death_cleanup still use it; deleted in Phase 2).
- `nearby_enemies.rs` — sticky acquisition (T1–T4): movers keep their target
  unless it vanished (T2) / retreated out of range (T3) / moved this tick
  (T4); rescan picks closest in-range enemy (`dist²`, `Entity::id` tiebreak),
  `None` when none — fixes the old sticky-`attacking` org-recovery bug. The
  3×3 scan result is reused as the rescan candidate set; **acquisition runs
  before the `NearbyEnemies` write** (that write drains the candidate vec).
- `combat.rs` — candidate = `men > 0 && attacking.is_some()` (NearbyEnemies no
  longer drives combat); per-candidate **target validation** (dead-this-tick /
  same-owner / gone / out-of-range → skip + clear = lifecycle site 2);
  **mutual dispatch**: one-way pairs resolve at the attacker, mutual pairs
  exactly once at the lower `Entity.id` (replaces the `resolved` normalized-
  pair set); **role-based damage** (§9.2): attacker `atk vs def`, defender
  counters `def vs atk` (one-way) or `atk vs def` (mutual), per-side terrain,
  equipment malus, supply/org/attrition as before. Retreat check now scans ALL
  units (a parked attacked defender is not a candidate) and clears `fighting`
  when `attacking.is_none() && !fought`. Scratch: `resolved` set dropped;
  counters reworked (`targets_dead/gone/same_owner/out_of_range/skipped_`
  `mutual`, `engagements`); `engaged_pairs()` = `engagements`.
- `supply.rs` — org-recovery gate: `!fighting && attacking.is_none()`.
- `death_cleanup.rs` — lifecycle site 3: after the list purge, units whose
  target died post-combat this tick get `attacking = None` (collect-then-
  mutate around the query borrow).
- `game.rs` — `CombatDiag` reworked to the new counters; `combat_health`
  counts `attacking.is_some()`; wire `attacking` still = `fighting` (unchanged
  protocol).
- `benchmark.rs` — diag accumulators/JSON/print updated to the new counters.
- Tests: 6 sticky-acquisition tests in `nearby_enemies` (T1/T2/T3/T4, sticky
  keep, defense-only parked); combat tests rewritten for explicit targets
  (`populate_targets` = closest-in-range stand-in for the mover scan), incl.
  new `parked_defender_engaged_by_targeting_mover` (asserts the defense-
  favorable ratio), `stationary_untargeted_enemies_do_not_engage` (C1),
  `combat_skips_dead_attacker` (victim untouched); `tests/integration_game.rs`
  fixture def bumped to 5.5 + new **`e2e_mover_engages_parked_defender`**
  (full `tick_game` pipeline: parked-parked no-fight → mover engages → combat-
  scale damage both sides → `fighting` flags → defender loses fewer men).

**Design notes that emerged during implementation (binding for Phase 2):**
- Combat candidates are **movers-with-targets only**; a parked defender is
  engaged via the attacker's edge and never becomes a candidate itself.
- Separation nudges (unit spacing) set `moved` on the nudged unit → that unit
  rescans next tick (T4/T1). Sticky hold keeps churn at zero in the common
  case; jitter cost = one rescan picking the same closest target.
- The 8–10 tile combat band (`MIN_UNIT_DISTANCE=8`, `COMBAT_DISTANCE=10`) is
  load-bearing: parked pairs sit in it and a mover can hold target without
  leaving range.

**Open / deferred:**
- **Tuning** (§9.3 dial) — explicitly deferred; defense currently ≈1.21×
  exchange.
- **SIMD** (`SIMD_ANAL.md`): the combat distance loop is now a 1-target
  validation (no pair loop) — the §1/§4.4 SIMD candidates are **moot or
  reshaped**; re-measure before any SIMD work.

## Phase 2 — DONE (2026-07-13)

Folded the mover scan into `combat::run`, deleted the `nearby_enemies` system
+ `NearbyEnemies` component per §8.8. All 8 steps landed:

1. `combat::run` takes `&mut GridIndexScratch`; mover scan (site 1) at the top,
   before the resolution loop — acquires closest in-range enemy for movers,
   sticky target revalidation + range-death-clear for all candidates.
2. `NearbyEnemies` deleted from `components.rs`; `world_ext.rs` absence note
   removed; `systems/nearby_enemies.rs` deleted; `mod.rs` decl + `game.rs`
   `tick_system!` call removed.
3. `death_cleanup` purge pass + `seen` field deleted (despawn +
   `entity_reverse` + site 3 kept); test module rewritten for the leaner
   behavior.
4. `combat_health` dropped `units_with_enemies`/`total_nearby_entries`;
   `benchmark.rs` prints updated (target counts + `engaged_pairs`).
5. Feature-gated `microbenchmarks.rs` updated: `setup_nearby_enemies` helper,
   `NearbyEnemiesFixture` + its 3 bench entries, the obsolete `mod dc_measure`
   purge-pass measurement all removed; `CombatFixture` now marks units as
   movers + pre-builds the post-move grid; `ecs/insert_component` bench
   switched to `UnitStats`.
6. Stale `nearby_enemies` doc references fixed in `movement.rs`,
   `city_capture.rs`, `integration_game.rs` (the two intentional
   "folded-from-deleted-system" history notes in `combat.rs`/`game.rs` stay).

**Gate — benchmark (§8.8 "no regression"):**

After-state, `run_benchmark` (480 units → 153 remaining, 300 ticks, 150×150
map, release, throwaway harness since `cargo run` is whitelisted off):

```
mean_tick_ms=0.2003  p50=0.139  p90=0.303  p99=0.599  max=0.909
mean_combats=9 (max 17)  mean_movers=86
combat_diag: avg_candidates=15.29  avg_engagements=9.49
             avg_targets_skipped_mutual=5.69  avg_targets_dead=0.037
```

- The mutual-pair-resolves-once rule is visibly firing (15.29 candidates →
  9.49 engagements; 5.69 mutual skips).
- Targeting is consistent: `health_avg_units_with_attacking` (14.96) ≈
  `avg_candidates` (15.29).
- **Strict before/after tick-time delta: MEASURED** — stress test 9c
  (pre-fold) vs 10 (post-fold), 10k units, 1000×1000 map, 1500 ticks, seed
  42, release: `avg_tick_wall_ms` 5.140 → 4.937 (~4% lower, no regression);
  `avg_combats_per_tick` 96.9 → 96.2 (identical engagement volume, so the
  tick delta is not less combat being done); units remaining 9814 → 9776
  (the expected small combat-behaviour shift). Gate **closed**.

**Tests:** 421/0 across all targets (202 lib + 197 bin + 22 integration);
`--features microbenchmarks --all-targets` compiles clean; release-mode
warnings are 8 pre-existing debug-overlay items (untouched). One caveat:
`alloc_invariants::warm_tick_net_heap_growth_is_bounded` is **flaky under
parallel load** (failed 2× at ~150 blocks/103KB ambient dhat noise vs 64-block
bound; passes isolated and with `--test-threads=1` — its own docstring
records ~0.3–0.4MB ambient noise on shared machines). Not a regression:
the fold removed per-tick allocations (SmallVec writes + `seen` HashSet).

