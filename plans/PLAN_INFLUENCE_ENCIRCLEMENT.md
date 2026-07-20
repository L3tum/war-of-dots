# Influence & Encirclement Optimization Plan

## Context

Current benchmark (report_13, 1500 ticks, 240 units, 200×200 map):

| System | Time | % of Tick | Calls |
|--------|------|-----------|-------|
| **influence** | **75ms** | **15%** | 516 (every 3 ticks) |
| **encirclement** | **34ms** | **7%** | 51 (every 30 ticks) |
| combat | 94ms | 19% | 1550 |
| everything else | 304ms | 59% | — |

All other algorithmic optimizations are exhausted (two-phase pattern, Arc extraction, frequency throttles, entity2 caching experiments rejected). These two systems are the remaining hot paths.

---

## Influence System (75ms → target: 30-40ms, -47-60%)

### Current Algorithm

Circle projection from each unit/city source on `Vec<Vec<T>>` grids.

**Per call (516 total):**
- 240 units × 225 iterations (15×15 square bounding box for radius-7 circle) = **54,000 iterations**
- 12 cities × 256 iterations = **3,072 iterations**
- Total: ~57,000 cell touches per call, **29.4M total per benchmark**

### Problem 1: `Vec<Vec<T>>` Layout — Cache Miss Nightmare

Every row is a separate heap allocation. Walking `grid[x][y]` means:
- Row transition = new pointer chase → **L1 cache miss** (~4ns penalty)
- Circle projection touches 15 rows per unit → up to **15 cache misses per unit**
- 240 units × 15 misses = **3,600 cache misses per call**

**Fix: Flat 1D Array**

```rust
// Current: scattered row pointers
let vision_grid = vec![vec![false; h_usize]; w_usize];  // 200 separate allocations
// ...
vision_grid[xusize][yusize] = true;  // double indirection, cache miss on row change

// Fixed: contiguous memory, zero cache misses
let vision_grid = vec![false; w_usize * h_usize];  // 1 allocation, 200×200 = 40K bools
// ...
vision_grid[yusize * w_usize + xusize] = true;  // single indirection, sequential access
```

**Impact:**
- Vision grid: 40K bools × 1 byte = 40KB (fits in L2, not L1)
- Influence grid: 40K u32 × 4 bytes = 160KB (L2 territory)
- Row transitions become sequential memory access → **~3,600 fewer cache misses per call**
- Compiler can auto-vectorize inner loop (currently blocked by double indirection)
- **Estimated: -15-20% (-11-15ms)**

### Problem 2: Square Iteration Wastes 45% of Work

```rust
// Current: iterate entire square, check corners
for dx in -max_radius..=max_radius {     // 15 iterations
    for dy in -max_radius..=max_radius { // 15 iterations
        // 225 total, but only ~126 are inside circle
        if dx.abs() <= v_radius && dy.abs() <= v_radius {  // 45% wasted!
            vision_grid[xusize][yusize] = true;
        }
    }
}
```

**Fix: Precomputed Circle Spans**

The set of (dx, dy) offsets inside a circle of radius R is the same regardless of center position. Precompute once at startup as "scanlines" — for each row offset (dy), what are the valid column bounds (dx_start, dx_end).

```rust
// Precomputed once at startup — ~5KB total for radii 0-20
// circle_spans[r] = [(dy, dx_start, dx_end), ...]
static CIRCLE_SPANS: OnceLock<Vec<Vec<(i32, i32, i32)>>> = OnceLock::new();

fn init_circle_spans() {
    let mut spans = Vec::with_capacity(21);
    for r in 0..=20 {
        let mut rows = Vec::new();
        let rr = r as i32;
        for dy in -rr..=rr {
            // pythagorean: dx² ≤ r² - dy²
            let dx_max = ((r * r - dy * dy) as f64).sqrt() as i32;
            rows.push((dy, -dx_max, dx_max));
        }
        spans.push(rows);
    }
    CIRCLE_SPANS.set(spans).unwrap();
}
```

Example for radius 7:
```
  dy = -7:  dx = [  0]          →  1 cell
  dy = -6:  dx = [-1..=1]       →  3 cells
  dy = -5:  dx = [-2..=2]       →  5 cells
  dy = -4:  dx = [-3..=3]       →  7 cells
  dy = -3:  dx = [-4..=4]       →  9 cells
  dy = -2:  dx = [-5..=5]       → 11 cells
  dy = -1:  dx = [-6..=6]       → 13 cells
  dy =  0:  dx = [-7..=7]       → 15 cells
  dy = +1:  dx = [-6..=6]       → 13 cells
  dy = +2:  dx = [-5..=5]       → 11 cells
  dy = +3:  dx = [-4..=4]       →  9 cells
  dy = +4:  dx = [-3..=3]       →  7 cells
  dy = +5:  dx = [-2..=2]       →  5 cells
  dy = +6:  dx = [-1..=1]       →  3 cells
  dy = +7:  dx = [  0]          →  1 cell
                     ─────
                   Total: 126 cells  (-44% vs 225)
```

Runtime application (stamp stencil at any center):
```rust
// Old: square iteration + distance check
for dx in -r..=r {
    for dy in -r..=r {
        if dx*dx + dy*dy <= r*r {
            grid[center_x+dx][center_y+dy] += 1;
        }
    }
}

// New: precomputed spans, no checks, flat array
for &(dy, dx_start, dx_end) in CIRCLE_SPANS[r] {
    let y = (center_y + dy) as usize;
    let row_base = y * width;
    for dx in dx_start..=dx_end {
        let x = (center_x + dx) as usize;
        grid[row_base + x] += 1;
    }
}
```

**Impact:**
- Radius 7: 225 → 126 iterations (**-44%**)
- Radius 4: 81 → 49 iterations (**-40%**)
- Zero `dx.abs()`, `dy.abs()`, or multiplication in inner loop
- Sequential writes on flat array → compiler auto-vectorizes with AVX2
- **Estimated: -10-15% (-8-11ms)**

### Combined Impact

| Optimization | Est. Savings | Risk |
|--------------|-------------|------|
| Flat 1D array | -15-20% | Low (layout change, logic unchanged) |
| Precomputed circle spans | -10-15% | Low (stencil math, no behavior change) |
| **Combined** | **-25-35% (-19-26ms)** | |

**Target: 75ms → 45-50ms** (may be limited by combat's 94ms dominance)

---

## Encirclement System (34ms → target: 12-18ms, -47-65%)

### Current Algorithm

Per-player flood-fill on influence grid to find connected "friendly-dominated" components, then check if each component has a city or touches the map edge.

**Per call (51 total, every 30 ticks):**
- `max_influence` grid: 12 players × 40,000 cells = **480,000 comparisons**
- 12 flood-fills: 12 × 40,000 cells = **480,000 iterations**
- Component analysis: ~50 components × 100 cells × 2 cities = **~10,000 checks**
- Total: ~1M operations per call, **51M total per benchmark**

### Problem 1: Forward Flood-Fill is Inefficient

Current approach: flood-fill ALL friendly regions, then check each one for city/edge status. Most regions are "safe" (touch edge or have city) — we still flood-fill them fully just to confirm.

**Fix: Reverse Flood-Fill from Safe Anchors**

Instead of finding all components and checking each, start from known-safe anchors and flood-fill outward. Any friendly cell NOT reached = encircled.

```rust
// Phase 1: Build friendly-dominated grid (same as current)
let friendly = build_friendly_grid(&border_grids, player_id, w, h, &max_influence);

// Phase 2: Mark safe anchors
let mut safe = vec![false; w * h];
// Edge cells are safe (units can escape)
for x in 0..w { safe[x] = true; safe[(h-1)*w + x] = true; }
for y in 0..h { safe[y*w] = true; safe[y*w + w-1] = true; }
// Cells near friendly cities are safe
for &(cx, cy) in &friendly_cities {
    for dy in -2..=2 {
        for dx in -2..=2 {
            let nx = cx as i32 + dx; let ny = cy as i32 + dy;
            if nx >= 0 && ny >= 0 && nx < w && ny < h {
                safe[(ny*w + nx) as usize] = true;
            }
        }
    }
}

// Phase 3: BFS from safe anchors through friendly cells
let mut queue = [(0i32, 0i32); 40000];
let mut qhead = 0; let mut qtail = 0;
// Seed queue with all safe anchor cells
for i in 0..(w*h) as usize {
    if safe[i] && friendly[i/w][i%w] {
        queue[qtail] = (i as i32 % w, i as i32 / w);
        qtail += 1;
    }
}
while qhead < qtail {
    let (cx, cy) = queue[qhead]; qhead += 1;
    for (nx, ny) in [(cx+1,cy),(cx-1,cy),(cx,cy+1),(cx,cy-1)] {
        if nx >= 0 && ny >= 0 && nx < w && ny < h {
            let idx = (ny*w + nx) as usize;
            if !safe[idx] && friendly[nx as usize][ny as usize] {
                safe[idx] = true;
                queue[qtail] = (nx, ny);
                qtail += 1;
            }
        }
    }
}

// Phase 4: Any friendly cell NOT safe = encircled
for (entity, (x, y)) in entities {
    let idx = (*y as i32 * w + *x as i32) as usize;
    let encircled = friendly[*x as usize][*y as usize] && !safe[idx];
    // ... update flags
}
```

**Why it's faster:**
- Single BFS instead of 1+ N flood-fills (where N = number of components)
- BFS starts from edge + city anchors, spreads only through friendly cells
- Encircled units are typically in small clusters → BFS reaches most cells quickly
- No component collection, no `component_has_city` O(n²) check
- **Estimated: -40-50% (-14-17ms)**

### Problem 2: max_influence Grid Recomputed Every Call

Current approach computes `max_influence` by scanning all 12 player grids (480K cell reads). But influence grids change slowly between calls (every 30 ticks).

**Fix: Incremental Dominant Player Grid**

Instead of `max_influence` (u32) + per-player comparison, build a single `dominant_player` grid in one pass:

```rust
// One pass: for each cell, track which player dominates
let mut dominant: Vec<(u32, u32)> = vec![(0, 0); (w * h) as usize];  // (influence, player_id_hash)
for (player, grid) in &border_grids {
    let pid = player_hash(player);  // u32 hash of player ID
    for (i, &cell) in grid.flatten().enumerate() {
        if cell > dominant[i].0 {
            dominant[i] = (cell, pid);
        }
    }
}
// Now each player's "friendly" check is just: dominant[idx].1 == player_hash
```

**Why it's faster:**
- 1 pass over all grids instead of 1 + 12 passes
- Eliminates `build_friendly_grid_simple()` per player (12 calls × 40K cells)
- Replaces u32 comparison with u32 equality check in flood-fill
- **Estimated: -15-25% (-5-9ms)**

### Combined Impact

| Optimization | Est. Savings | Risk |
|--------------|-------------|------|
| Reverse flood-fill from safe anchors | -40-50% | Medium (logic change, needs testing) |
| Single dominant player grid | -15-25% | Low (restructuring, no behavior change) |
| **Combined** | **-50-65% (-17-22ms)** | |

**Target: 34ms → 12-17ms**

---

## Implementation Order

| Phase | Change | Est. Savings | Effort | Risk |
|-------|--------|-------------|--------|------|
| **1** | Influence: flat 1D array for vision + influence grids | -11-15ms | 1 hour | Low |
| **2** | Influence: precomputed circle spans | -8-11ms | 30 min | Low |
| **3** | Encirclement: reverse flood-fill from safe anchors | -14-17ms | 1 hour | Medium |
| **4** | Encirclement: single dominant player grid | -5-9ms | 30 min | Low |
| **Total** | | **-38-52ms (-7-10% of 507ms)** | **~3 hours** | |

### Phase 1 Details: Flat 1D Array

**Files to modify:**
1. `src/game/components.rs` — `VisionGrids::grids`, `BorderGrids::grids` layout
2. `src/game/systems/influence.rs` — grid indexing
3. `src/game/systems/encirclement.rs` — grid indexing
4. `src/game/systems/vision.rs`, `border.rs` — if still referenced
5. `src/server/delta.rs` — grid access for state building

**Key change:**
```rust
// Old
pub grids: Arc<HashMap<String, Vec<Vec<T>>>>;

// New
pub grids: Arc<HashMap<String, Vec<T>>>;  // flat, indexed as [y * width + x]
```

Must pass `width` through all grid accessors for indexing. This is a structural change touching multiple files but no behavioral change.

### Phase 2 Details: Precomputed Circle Spans

**Files to modify:**
1. `src/game/systems/influence.rs` — add `CIRCLE_SPANS` static, replace inner loop
2. `src/game/game.rs` or `main.rs` — call `init_circle_spans()` at startup

**Key change:**
```rust
// Add at module level
static CIRCLE_SPANS: OnceLock<Vec<Vec<(i32, i32, i32)>>> = OnceLock::new();

pub fn init() {
    CIRCLE_SPANS.get_or_init(|| {
        (0..=20).map(|r| {
            let rr = r as i32;
            (-rr..=rr).map(|dy| {
                let dx_max = ((r * r - dy * dy) as f64).sqrt() as i32;
                (dy, -dx_max, dx_max)
            }).collect()
        }).collect()
    });
}
```

Replace circle projection loops with span-based iteration. No behavioral change (same cells touched, just fewer iterations).

### Phase 3 Details: Reverse Flood-Fill

**Files to modify:**
1. `src/game/systems/encirclement.rs` — replace flood-fill + component analysis with reverse BFS

**Key risks:**
- Must preserve exact same encirclement semantics (edge escape, city radius 2)
- BFS queue needs to be properly sized (40K max for 200×200 map)
- Need to verify: what if a friendly region has NO edge cells and NO cities? (should be encircled)

### Phase 4 Details: Dominant Player Grid

**Files to modify:**
1. `src/game/systems/encirclement.rs` — replace `max_influence` + per-player `build_friendly_grid`

**Key change:**
- Compute `dominant_player: Vec<u32>` once (u32 = hashed player ID)
- Each player's flood-fill checks `dominant[idx] == player_hash` instead of `self_grid[y][x] >= max_influence[y][x]`
- Eliminates 12 × 40K comparisons per call

---

## Benchmark Targets

| Metric | Current (report_13) | After All Phases | Delta |
|--------|---------------------|------------------|-------|
| Total wall time | 508ms | **450-470ms** | **-11-12%** |
| Avg tick | 0.338ms | **0.300-0.313ms** | **-8-11%** |
| influence | 75ms | **45-50ms** | **-33-40%** |
| encirclement | 34ms | **12-17ms** | **-50-65%** |
| combat | 94ms | 94ms | — (unchanged) |

---

## Rejected Alternatives

| Approach | Why Rejected |
|----------|-------------|
| Multi-source BFS for influence | 4-5x slower (347ms vs 60ms) — queue overhead > circle projection trivial inner loop |
| Rayon parallelization | Tasks too small (0.1ms/player) for work-stealing overhead (~5μs/task) |
| Grid downsampling (50×50) | Changes gameplay mechanics (vision range, border precision) |
| Grid pooling (pre-allocated) | Arc already eliminates reallocation; Vec allocation is fast for 40K elements |
| Component data in spatial hash | 48KB alloc/tick > world.get savings (see ALGORITHMIC_ANALYSIS.md) |
| SIMD circle projection | `std::simd` is nightly-only; auto-vectorization already active with AVX2 |
