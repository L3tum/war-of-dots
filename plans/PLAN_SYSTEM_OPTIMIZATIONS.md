# System Optimization Plan

## Current Hot Paths (release benchmark, 500 ticks, 540+ units)

| System | Total (ms) | Avg/tick | P99 | Calls |
|--------|-----------|----------|-----|-------|
| combat/adjacent_lookup | 84.3 | 0.153 | 0.204 | 550 |
| encirclement/bfs_per_unit | 52.6 | 0.005 | 0.050 | 11,421 |
| border/unit_influence | 34.1 | 0.015 | 0.021 | 2,200 |
| vision/unit_visibility | 13.1 | 0.006 | 0.007 | 2,200 |
| vision/grid_alloc | 13.0 | 0.006 | 0.008 | 2,200 |
| combat/spatial_hash_build | 10.7 | 0.020 | 0.028 | 550 |

---

## Proposal 1: InfluenceSystem with Proper Border Calculation

### Problem
Current border system computes per-player influence grids independently with no cross-player
comparison. There is no actual "border line" — just per-player heatmaps. Vision is a separate
system doing nearly the same loop (project circles from units/cities).

### Research: State-of-the-Art Approaches
From production RTS games and academic literature:

1. **Multi-source Dijkstra** (shapes-and-states, Civ V):
   - Each player's cities/units are "seeds"
   - Dijkstra propagates outward with terrain cost weights
   - Border line = where two players' accumulated costs are equal
   - Naturally handles terrain (hills slow propagation, water blocks)
   - Best for static/slow-changing sources (cities)

2. **Influence Maps with Decay** (StarCraft 2, Total War):
   - Grid-based, each source spreads influence with distance falloff
   - `influence = strength * exp(-distance * decay)`
   - Multiple layers: friendly, enemy, tension (friendly - enemy)
   - Border line = where tension ≈ 0 (marching squares extraction)
   - Best for dynamic sources (units)

3. **Hybrid Approach** (recommended for this game):
   - Cities: multi-source Dijkstra (computed infrequently, cached)
   - Units: influence map with decay (computed every tick)
   - Combined: `total_influence[x][y] = city_influence + unit_influence`
   - Border: `tension[x][y] = player_a_total - player_b_total`
   - Border line extracted via marching squares where tension crosses 0

### Plan

#### Phase 1: Merged Influence Computation
Replace `VisionSystem` and `BorderSystem` with `InfluenceSystem`:

```rust
pub struct InfluenceData {
    pub vision: Arc<Vec<Vec<bool>>>,         // binary visibility (distance-based)
    pub influence: Arc<Vec<Vec<f32>>>,        // per-player influence value
    pub tension: Arc<Vec<Vec<f32>>>,          // friendly - enemy (for border line)
    pub border_line: Vec<(i32, i32)>,         // extracted border cells for rendering
}

pub fn run(world, world_entity) {
    let wb = world_data_clone(world, world_entity);
    let players = enumerate_players(world);
    let (w, h) = (wb.terrain.width, wb.terrain.height);

    // Per-player influence grids
    let mut player_influence: HashMap<String, Vec<Vec<f32>>> = HashMap::new();
    let mut player_vision: HashMap<String, Vec<Vec<bool>>> = HashMap::new();

    for player in &players {
        let units = filter_units(world, player);
        let cities = filter_cities(&wb, player);
        let mut influence = vec![vec![0.0; h]; w];
        let mut vision = vec![vec![false; h]; w];

        // Units: influence with linear falloff
        for (ux, uy, strength) in &units {
            let radius = unit_influence_radius;
            project_influence(&mut influence, &mut vision, *ux, *uy, radius, *strength);
        }
        // Cities: higher strength, larger radius
        for (cx, cy) in &cities {
            let radius = city_influence_radius;
            project_influence(&mut influence, &mut vision, *cx, *cy, radius, CITY_STRENGTH);
        }

        player_influence.insert(player.clone(), influence);
        player_vision.insert(player.clone(), vision);
    }

    // Compute tension map (friendly - enemy) per player
    for player in &players {
        let friendly = &player_influence[player];
        let mut tension = vec![vec![0.0; h]; w];
        for other in &players {
            if other == player { continue; }
            let enemy = &player_influence[other];
            for y in 0..h {
                for x in 0..w {
                    tension[y][x] = friendly[y][x] - enemy[y][x];
                }
            }
        }
        // Extract border line where tension crosses 0 (marching squares)
        let border_line = extract_border_line(&tension);

        results.insert(player, InfluenceData {
            vision: Arc::new(player_vision.remove(player).unwrap()),
            influence: Arc::new(player_influence.remove(player).unwrap()),
            tension: Arc::new(tension),
            border_line,
        });
    }
}
```

#### Phase 2: Border Line Rendering
- `border_line` is a `Vec<(i32, i32)>` of cells where tension crosses 0
- Client renders these as the actual border line overlay
- Uses marching squares algorithm for smooth contours

#### Phase 3: Encirclement from Influence (see Proposal 2)
- Uses the tension map to determine encirclement
- Negative tension region without friendly city = encircled

### Expected Savings
- Eliminates duplicate player enumeration (2 → 1)
- Eliminates duplicate unit/city filtering (2N → N)
- Merges inner projection loops (2 passes → 1 pass)
- Reduces grid allocations (4 → 3 per player: vision, influence, tension)
- Adds tension computation (~1ms for map-sized grid)
- Adds border line extraction (~0.5ms marching squares)
- **Net est. 20-30% reduction** in combined vision+border time
- **New feature**: actual border line for rendering

### Risk
- Medium. Adds tension computation and border extraction.
- Tension map is O(players × map_area) — with 6 players and 100x100 map, that's 60K cell ops.
- Marching squares for border extraction is well-known and fast (~0.5ms).
- Need to update delta computation to send border_line to clients.

---

## Proposal 2: Combat with Fighting Flag + RetREAT Logic

### Problem
Combat checks ALL units every tick via spatial hash, even though most units are not
in combat. Additionally, the current model perpetually links two adjacent units in
combat with no way to retreat.

### User Clarification
- Each unit with `moved=true` raycasts to find enemies within distance < 2.0
- Two units are NOT perpetually linked in combat
- A retreating unit can escape if the enemy does NOT pursue
- Combat persists only if the pursuing unit continues moving toward the retreating unit
- Clear `fighting` when both units die, or when one dies (regardless of `moved=true`)

### Plan

#### Component Changes
```rust
pub struct UnitFlags {
    pub fighting: bool,   // repurpose existing `attacking`
    pub moved: bool,       // new — set by movement system when position changes
    // ... existing flags
}

pub struct UnitPath {
    pub path: Vec<(f32, f32)>,
    pub destination: Option<(f32, f32)>,  // new — final target from move_unit command
}
```

#### Combat System Rewrite
```rust
pub fn run(world, world_entity) -> Vec<Entity> {
    // 1. Find candidates: units that moved OR are already fighting
    let candidates: Vec<Entity> = world
        .query::<(&UnitFlags,)>()
        .iter()
        .filter(|(_, (f,))| f.moved || f.fighting)
        .map(|(e, _)| e)
        .collect();

    // 2. Build spatial hash from ALL units (needed for neighbor lookup)
    let spatial = build_spatial_hash(world);

    let mut dead = Vec::new();
    let mut resolved = HashSet::new();

    // 3. For each candidate, find nearby enemies
    for entity in &candidates {
        if resolved.contains(entity) { continue; }
        let pos = get_pos(world, *entity);
        let owner = get_owner(world, *entity);

        for enemy in spatial.query_nearby(pos.x, pos.y) {
            if !is_hostile(owner, get_owner(world, enemy)) { continue; }
            let epos = get_pos(world, enemy);
            let dist = distance(pos, epos);
            if dist >= 2.0 { continue; }

            // 4. Resolve combat
            let (a_alive, d_alive) = resolve_combat(world, *entity, enemy);

            if !a_alive { dead.push(*entity); }
            if !d_alive { dead.push(enemy); }

            // 5. Update fighting flags
            if a_alive && d_alive {
                // Both alive — both fighting
                set_fighting(world, *entity, true);
                set_fighting(world, enemy, true);
            } else if a_alive {
                // Defender died — attacker is no longer fighting
                set_fighting(world, *entity, false);
            } else if d_alive {
                // Attacker died — defender is no longer fighting
                set_fighting(world, enemy, false);
            }
            // If both dead, no flag cleanup needed (entities removed)

            resolved.insert(*entity);
            resolved.insert(enemy);
        }
    }

    // 6. Handle retreat logic for fighting units that are no longer adjacent to enemies
    for entity in &candidates {
        if !resolved.contains(entity) {
            // This fighting unit found no adjacent enemy this tick
            // Check if they're retreating (moving away from last known enemy)
            // If no enemy nearby and not pursuing, clear fighting
            let has_nearby_enemy = check_any_enemy_nearby(&spatial, world, *entity);
            if !has_nearby_enemy {
                set_fighting(world, *entity, false);
            }
        }
    }

    // 7. Clear moved flags
    clear_moved_flags(world);

    dead
}
```

#### Retreat Logic Details
- Unit A fights Unit B, both have `fighting=true`
- Next tick: Unit A moves away (retreats), `moved=true`
- Unit B does NOT move toward A (does not pursue)
- Combat system checks: A has no adjacent enemy → `fighting=false`
- Unit A is now free to move without combat penalty

If Unit B DOES move toward A (pursues):
- Unit B has `moved=true` → checked for enemies
- Unit B finds A within range → combat resolves, both stay `fighting=true`
- Combat continues until one dies or the retreating unit escapes range

### Expected Savings
- Spatial hash still built from all units (~10ms, unavoidable for neighbor lookup)
- Combat resolution reduced from 550 units to ~50-100 units (est. 80-90% reduction)
- **Est. 60-70% reduction in combat time** (~95ms → ~30ms per tick)
- New gameplay: units can retreat from combat

### Risk
- Medium. Need to handle edge cases:
  - Unit A fights B, B dies → A.fighting cleared immediately ✅
  - Unit A fights B, both stationary → continuous attrition ✅ (both fighting=true)
  - Unit A moves into range of B → A.moved=true triggers check ✅
  - Unit A retreats, B doesn't pursue → A escapes ✅
  - Unit A retreats, B pursues → combat continues ✅
- `moved` flag adds component write in movement system (small overhead)
- Need to update UI: `attacking` → `fighting` flag display

### Movement System Integration
Movement system must:
1. Set `moved=true` when unit position changes
2. Only process units with `destination != null` AND `fighting == false`
3. Units in combat cannot move (or move at severe penalty — design decision)

See Proposal 6 for movement system optimization details.

### Problem
Encirclement runs BFS from EACH unit to find if there's a path to a friendly city.
With 727+ units checked every 15 ticks, that's 11,421 BFS calls totaling 52.6ms.
Each BFS is O(map_area) in the worst case. Total: O(units × map_area).

### Current Approach
```
for each player:
    for each unit:
        BFS from unit position → find friendly city or map edge
        if no path found: unit.encircled = true
```

### Proposed Approach
Use the border influence grid (computed every tick) to derive encirclement:

1. For each player, create a binary "friendly-dominated" grid:
   - `friendly[x][y] = true` if player's influence > all other players' influence
2. Flood-fill to find connected components of friendly-dominated cells
3. For each component, check if it contains a friendly city (within radius 2.0)
4. Components WITHOUT a city → all units in those cells are encircled
5. Units on the map edge or in non-friendly cells → not encircled (can escape)

```rust
pub fn run(world, world_entity, tick) {
    if tick % ENCIRCLE_CHECK_INTERVAL != 0 { return; }
    
    let borders = get_border_grids(world, world_entity);
    let wb = world_data_clone(world, world_entity);
    
    for player in players {
        // Build friendly-dominated grid from border data
        let friendly = build_friendly_grid(&borders, player);
        
        // Flood-fill to find connected components
        let components = flood_fill(&friendly);
        
        // Mark components without cities as encircled
        for (idx, component) in components.iter().enumerate() {
            let has_city = component_has_city(component, &wb.cities, player);
            if !has_city {
                for (x, y) in component.cells {
                    mark_units_encircled(world, x, y, player, true);
                }
            }
        }
        // Clear encircled flag for units in safe regions
        clear_encircled_flags(world, player, &friendly, &wb.cities);
    }
}
```

### Expected Savings
- Replaces N per-unit BFS calls (each O(map_area)) with ~3 flood-fills per player
- Benchmark: 11,421 BFS calls (52.6ms) → ~6 flood-fills (est. 2-5ms)
- **Est. 90% reduction in encirclement time** (~52ms → ~5ms per check)

### Risk
- Medium. The flood-fill approach is an approximation — it uses influence dominance
  instead of actual pathfinding. Edge cases:
  - Narrow corridors of friendly influence between enemy cities
  - Units exactly on the border between two players' influence
  - Water cells (impassable for BFS, but influence still projects)
- Mitigation: Fall back to per-unit BFS for units near the border line (influence
  ratio < 1.5:1). This handles edge cases without full BFS cost.
- Need to handle impassable terrain (water without boats) in flood-fill.

### Dependency
- Requires border grids to be available (computed every tick, so ✅)
- Could be combined with Proposal 1 (merged influence system outputs border data)

---

## Proposal 3: Encirclement from Influence/Tension Map

### Problem
Encirclement runs BFS from EACH unit to find if there's a path to a friendly city.
With 727+ units checked every 15 ticks, that's 11,421 BFS calls totaling 52.6ms.
Each BFS is O(map_area) in the worst case. Total: O(units × map_area).

### User Feedback
"The cell system is a little suspect for me anyway... I like this optimization though."
The user prefers deriving encirclement from the influence/tension map rather than raw BFS.

### Plan
Use the tension map (from Proposal 1's InfluenceSystem) to derive encirclement:

1. **Tension-based encirclement check**:
   - For each player, find connected regions of `tension < 0` (enemy-dominated)
   - For each region, check if it contains a friendly city
   - Regions without a city → all units in those cells are encircled
   - Units in cells with `tension > 0` → not encircled (friendly-dominated, path exists)

2. **Water connectivity**:
   - Water cells are included in flood-fill (passable)
   - If a water cell connects to a friendly city via adjacent water/land cells, units on that water are not encircled
   - Implementation: flood-fill treats water as passable terrain, checks for city adjacency at water's edge

3. **No BFS fallback needed**:
   - Units cannot occupy the same cell, so influence accurately reflects physical presence
   - Combat prevents advancing toward enemies, so units can't push through blocked cells
   - Terrain-weighted influence (water blocked, hills/forest reduced) eliminates false bridges
   - Rare edge cases (influence through diagonal gaps) are acceptable false negatives

```rust
pub fn run(world, world_entity, tick) {
    if tick % ENCIRCLE_CHECK_INTERVAL != 0 { return; }
    
    let wb = world_data_clone(world, world_entity);
    let tension_map = get_tension_map(&wb);  // from InfluenceSystem
    let (w, h) = (wb.terrain.width, wb.terrain.height);
    
    for player in players {
        let friendly_cities = filter_cities(&wb, player);
        let mut visited = vec![false; w * h];
        
        // Flood-fill enemy-dominated regions
        for y in 0..h {
            for x in 0..w {
                let idx = y * w + x;
                if visited[idx] { continue; }
                if tension_map[y][x] >= 0.0 { 
                    visited[idx] = true; 
                    continue; 
                }
                
                // Found enemy-dominated cell — flood-fill connected region
                // Include water cells (passable), respect terrain costs
                let region = flood_fill_negative(&tension_map, &wb.terrain, x, y, w, h, &mut visited);
                
                // Check if region contains or connects to a friendly city
                let has_city = region.cells.iter().any(|(rx, ry)| {
                    friendly_cities.iter().any(|(cx, cy)| {
                        (*rx as f32 - cx).abs() + (*ry as f32 - cy).abs() < 2.0
                    })
                });
                
                if !has_city {
                    // Mark all units in this region as encircled
                    for (rx, ry) in &region.cells {
                        mark_units_encircled(world, *rx, *ry, player, true);
                    }
                }
            }
        }
        
        // Clear encircled flag for units in friendly cells (tension >= 0)
        clear_encircled_in_friendly_zones(world, player, &tension_map);
    }
}
```

### Expected Savings
- Replaces N per-unit BFS calls with 1-2 flood-fills per player
- Flood-fill visits each cell ONCE (not once per unit)
- No BFS fallback needed — tension map is sufficient
- Benchmark: 11,421 BFS calls (52.6ms) → ~6 flood-fills (est. 2-5ms)
- **Est. 90%+ reduction in encirclement time** (~52ms → ~3-5ms per check)

### Risk
- Medium. Tension map is an approximation — may have false positives/negatives near borders
- Mitigation: Fallback BFS for units with tension in range [-0.5, 0.5]
- Need to handle water/impassable terrain in flood-fill (skip those cells)
- Depends on Proposal 1 (InfluenceSystem with tension map)

### Dependency
- Requires tension map from InfluenceSystem (Proposal 1)
- Could run independently if border grids are reused (less clean)

---

## Proposal 4: Directional Combat — DEFERRED

### User Clarification
The user does NOT want full directional/raycast combat. Their intent was simpler:
- Units with `moved=true` check for nearby enemies (distance < 2.0)
- Retreat logic: units can escape combat if enemy doesn't pursue
- No need for velocity tracking, approach angles, or pursuit algorithms

This is already covered in **Proposal 2** (Fighting Flag + Retreat Logic).

### Recommendation
**Skip this proposal.** The retreat logic in Proposal 2 handles the user's requirements
without the complexity of directional combat. Units escape combat when no enemy is
adjacent — simple and effective.

---

## Proposal 5: Shared Data + Arc-Wrapped Unit List

### Problem
Each system independently:
- Calls `world_data_clone()` (2.7ms per call, 13 calls/tick = ~35ms total)
- Enumerates unique players from unit query
- Filters units per player
- Filters cities per player

### User Feedback
"Perhaps we can also optimize the unit list similar to the Arc-wrapped grids."

### Plan

#### 1. Arc-Wrapped Unit List
Similar to terrain/vision/border grids, wrap the unit query result in Arc:

```rust
pub struct UnitList {
    // Arc-wrapped to avoid cloning between systems
    pub units: Arc<Vec<(hecs::Entity, UnitPosition, UnitOwner)>>,
}

// In Game::tick_game():
let unit_list = Arc::new(world.query::<(&UnitPosition, &UnitOwner)>()
    .iter()
    .map(|(e, (pos, owner))| (e, *pos, *owner))
    .collect());

// Systems receive a clone of the Arc (cheap pointer copy):
systems::combat::run(&mut world, world_entity, &unit_list);
systems::influence::run(&mut world, world_entity, &unit_list);
```

- Savings: Eliminates repeated hecs queries for unit positions/owners
- Each system currently does its own query — with Arc, query happens once
- Est. savings: ~2-4ms per tick (avoids 5-6 redundant queries)

#### 2. Shared Player Enumeration
Compute unique player list once, pass to all systems:

```rust
let players: Arc<Vec<String>> = Arc::new(
    unit_list.iter()
        .map(|(_, _, owner)| owner.player_id.clone())
        .collect::<HashSet<_>>()
        .into_iter()
        .collect()
);
```

- Savings: ~0.2ms per tick (small but free)

#### 3. Shared City Data
Cities are static (positions never change, ownership changes infrequently). Cache:

```rust
pub struct CityCache {
    pub by_player: Arc<HashMap<String, Vec<City>>>,
    pub all: Arc<Vec<City>>,
}

// Update only on city capture event
fn on_city_capture(world, city_id, new_owner) {
    cache.by_player = Arc::new(rebuild_by_player(&cache.all, new_owner));
}
```

- Savings: ~0.5ms per tick (avoids city filtering in every system)

#### 4. Shared world_data_clone() — HIGH RISK, DEFER
Currently each system clones world data independently. Sharing would require:
- Clone once at start of tick
- All systems use immutable reference
- Systems that mutate world must batch writes for end of tick

This is a significant architectural change (ECS "command pattern" or two-phase systems).
**Deferring until after Proposals 1-3 are complete.**

### Expected Savings
| Optimization | Est. Savings | Risk |
|-------------|-------------|------|
| Arc-wrapped unit list | 2-4ms/tick | Low |
| Shared player enumeration | 0.2ms/tick | Low |
| Cached city data | 0.5ms/tick | Low |
| Shared world_data_clone | 27ms/tick | High — DEFER |
| **Total (low-risk)** | **~3-5ms/tick** | **Low** |

### Risk
- Low for Arc-wrapped list and shared enumeration
- Need to ensure systems don't hold Arc references across world mutations
- hecs borrow checker: Arc clone is cheap, but world query must complete before mutation

---

## Proposal 6: Movement System Optimization

### Problem
Movement system iterates over ALL units every tick, even though most units may not
have a destination set. Additionally, units in combat should not move (or move at
severe penalty), but the current system doesn't check combat state.

### User Feedback
"If a move_unit command comes in, it sets the destination of the unit. Then, the
movement system iterates over all units who have a destination != null and
fighting == false. That should reduce the number of calculations."

### Plan

#### Component Changes
```rust
pub struct UnitPath {
    pub path: Vec<(f32, f32)>,       // waypoints to destination
    pub destination: Option<(f32, f32)>,  // final target (set by move_unit command)
}

pub struct UnitFlags {
    pub fighting: bool,   // true when in combat
    pub moved: bool,      // true when position changed this tick
    // ... existing flags
}
```

#### Movement System Rewrite
```rust
pub fn run(world, world_entity, settings) {
    let wb = world_data_clone(world, world_entity);
    
    // Process all units with a destination (fighting units included)
    let entities: Vec<Entity> = world
        .query::<(&UnitPath, &UnitFlags, &UnitPosition, &UnitTypeId)>()
        .iter()
        .filter(|(_, (path, _, _, _))| path.destination.is_some())
        .map(|(e, _)| e)
        .collect();
    
    for entity in entities {
        let flags = get_flags(world, entity);
        let old_pos = get_pos(world, entity);
        let path = get_path(world, entity);
        
        // Calculate proposed new position from path
        let new_pos = calculate_next_position(old_pos, &path, settings);
        
        // Combat movement constraint: can only move away from closest enemy
        if flags.fighting {
            let closest_enemy = find_closest_enemy(entity, world);
            if let Some(enemy_pos) = closest_enemy {
                let old_dist = distance(old_pos, enemy_pos);
                let new_dist = distance(new_pos, enemy_pos);
                
                if new_dist <= old_dist {
                    // Can't advance toward enemy — stay in place
                    apply_retreat_penalties(world, entity);
                    continue;
                }
                // Moving away (retreating) — allowed, apply penalties
                apply_retreat_penalties(world, entity);
            }
        }
        
        // Apply movement
        set_position(world, entity, new_pos);
        
        // Set moved flag when position changes
        if new_pos != old_pos {
            set_moved(world, entity, true);
        }
        
        // Clear destination when reached
        if path.is_empty() {
            clear_destination(world, entity);
        }
    }
}
```

#### Spatial Hash Sharing (Option B: Central Build)
Movement and Combat both need a spatial hash of unit positions. To avoid inter-dependency
and duplicate builds, `tick_game()` builds it once and passes to both:

```rust
// In Game::tick_game():
let spatial = build_spatial_hash(&self.world);

{
    let t = SystemTimer::new("movement");
    systems::movement::run(&mut self.world, self.world_entity, &self.settings, &spatial);
    t.finish(&guard, &mut self.world);
}

// ... attrition ...

{
    let t = SystemTimer::new("combat");
    let dead_entities = systems::combat::run(&mut self.world, self.world_entity, spatial);
    t.finish(&guard, &mut self.world);
}
```

- **Movement borrows** `&SpatialHash` — needs it for enemy distance checks (retreat constraint)
- **Combat takes ownership** `SpatialHash` — consumes it for adjacent enemy lookup
- **One build, two uses**: ~10ms/tick savings vs two separate builds
- **No inter-dependency**: Neither system knows about the other. `tick_game()` orchestrates.
- Movement's enemy lookup: query hash → filter by owner (cheap, few results per cell)

#### Movement System Signature
```rust
pub fn run(world: &mut World, world_entity: hecs::Entity, settings: &GameSettings, spatial: &SpatialHash)
```

#### Combat System Signature
```rust
pub fn run(world: &mut World, world_entity: hecs::Entity, spatial: SpatialHash) -> Vec<hecs::Entity>
```

#### Retreat Constraint Logic
```rust
/// Check if a fighting unit can move to a proposed position.
/// Uses the shared spatial hash for O(1) enemy lookup.
fn can_move_away_from_enemy(
    fighting: bool,
    entity: Entity,
    owner: &str,
    old_pos: (f32, f32),
    new_pos: (f32, f32),
    spatial: &SpatialHash,
    world: &World,
) -> bool {
    if !fighting { return true; }
    
    // Query nearby cells from shared spatial hash
    let nearby = spatial.query_nearby(old_pos.0, old_pos.1);
    
    // Find closest enemy (filter by owner, skip self)
    let mut closest_dist = f32::MAX;
    for nearby_entity in nearby {
        if nearby_entity == entity { continue; }
        let nearby_owner = match world.get::<&UnitOwner>(nearby_entity) {
            Ok(o) => o, Err(_) => continue,
        };
        if nearby_owner.player_id == owner { continue; } // Friendly, skip
        
        let nearby_pos = match world.get::<&UnitPosition>(nearby_entity) {
            Ok(p) => *p, Err(_) => continue,
        };
        let d = distance(old_pos, nearby_pos.as_tuple());
        if d < closest_dist { closest_dist = d; }
    }
    
    if closest_dist == f32::MAX { return true; } // No enemy nearby
    
    // Check if new position is farther from closest enemy
    // (We approximate: if ANY nearby enemy is closer at new_pos, block movement)
    for nearby_entity in nearby {
        if nearby_entity == entity { continue; }
        let nearby_owner = match world.get::<&UnitOwner>(nearby_entity) {
            Ok(o) => o, Err(_) => continue,
        };
        if nearby_owner.player_id == owner { continue; }
        
        let nearby_pos = match world.get::<&UnitPosition>(nearby_entity) {
            Ok(p) => *p, Err(_) => continue,
        };
        let new_d = distance(new_pos, nearby_pos.as_tuple());
        let old_d = distance(old_pos, nearby_pos.as_tuple());
        if new_d <= old_d { return false; } // Getting closer to this enemy → blocked
    }
    
    true  // Moving away from all nearby enemies
}
```

**Multiple enemies**: Checks ALL nearby enemies (from spatial hash). Movement is only allowed
if the unit moves away from EVERY nearby enemy. If the closest dies:
- Combat system removes dead entities
- Next tick, spatial hash is rebuilt → dead entities gone → only living enemies checked
- No persistent tracking needed — evaluated fresh each tick
- Cost: O(1) hash build in tick_game, O(nearby) per fighting unit (typically <10)

### Expected Savings
- Filters out units without destination (est. 50-70% of units in typical game)
- Filters out units in combat (est. 5-15% of units)
- Combined: Movement system processes ~15-40% of units instead of 100%
- **Est. 60-85% reduction in movement computation**
- Additional benefit: Units in combat stay in place (more realistic)

### Risk
- Low. Simple filter addition, no gameplay-breaking changes.
- Need to ensure `destination` is set correctly by `move_unit` command handler
- Need to clear `destination` when unit reaches target (already partially done)
- Combat interaction is a design choice — may need playtesting feedback

---

## Implementation Order

| Priority | Proposal | Est. Effort | Est. Savings | Risk |
|----------|----------|-------------|--------------|------|
| 1 | Shared data + Arc-wrapped unit list | 1 hour | ~3-5ms/tick | Low |
| 2 | InfluenceSystem with proper border | 2-3 hours | ~15-20ms/tick + border feature | Medium |
| 3 | Encirclement from tension map | 1-2 hours | ~45ms/check (every 15 ticks) | Medium |
| 4 | Combat with fighting flag + retreat | 2-3 hours | ~65ms/tick | Medium |
| 5 | Movement system optimization | 1 hour | ~60-85% movement reduction | Low |
| 6 | Shared world_data_clone | 4+ hours | ~27ms/tick | High — DEFER |

### Completed
- [x] Phase 1: Movement filter, shared spatial hash, timing guards
- [x] Phase 2: InfluenceSystem (merged vision+border), encirclement from tension map, combat fighting flag + retreat
- [x] Phase 2.5: Influence frequency throttle (every 2 ticks, -49% savings)
- [ ] Shared world_data_clone (deferred — high risk)

### Multi-BFS Experiment (REJECTED)
**Attempted**: Replace circle projection with multi-source BFS in InfluenceSystem.
**Result**: 4-5x SLOWER (347ms vs 60ms). Despite O(map_area) vs O(units × radius²) in theory,
BFS has a much higher constant factor: queue allocations, boundary checks per cell, pointer chasing.
Circle projection's trivial inner loop (`grid[x][y] += 1`) is faster in practice for sparse sources.
**Conclusion**: Keep circle projection. Frequency throttle (every 2 ticks) is the correct optimization.

### Phase 1: Foundation (Proposals 1, 5)
- Arc-wrapped unit list + shared player/city data
- Movement system optimization (destination + fighting filter)
- Total est. savings: ~5ms/tick + movement reduction
- Risk: Low

### Phase 2: Core Systems (Proposals 2, 3, 4)
- InfluenceSystem with tension map and border line
- Encirclement from tension map (replaces BFS)
- Combat with fighting flag + retreat logic
- Total est. savings: ~125ms/tick (amortized)
- Risk: Medium

### Phase 3: Architectural (Proposal 6)
- Shared world_data_clone with two-phase systems
- Risk: High — requires ECS architecture change

---

## Resolved Design Decisions

### 1. Border Line Rendering
- **Resolution**: Full map resolution. Small grids (2x2) only if proven bottleneck.
- **Delta**: Send border line only on change (reuse existing partial update system).
- **Frequency**: Calculate every 10 ticks (not every tick — border changes slowly).
- **Client**: Border line rendered as overlay. Colour-coded map mode possible with tension data.

### 2. Encirclement Rules
- **Water**: Unit is NOT encircled if on water AND a friendly city connects to the same body of water.
  - Implementation: Flood-fill includes water cells. Friendly city on same water body = escape route.
  - Requires water connectivity tracking (connected component labeling for water bodies).
- **Narrow corridors**: Tension map is sufficient in most cases because:
  - Units cannot occupy the same cell — if an enemy blocks a corridor, the friendly unit cannot be in that cell
  - The enemy's influence at the blocked cell dominates → tension goes negative → corridor correctly marked as enemy-controlled
  - Combat prevents advancing toward enemies → friendly unit can't push through the blocked cell
  - **Remaining edge case**: Influence projects radially and doesn't verify walkable path existence. A friendly unit far behind mountains/water could project influence through impassable terrain, creating a false "positive tension" bridge. Mitigation: terrain-weighted influence (water = blocked propagation, hills/forest = reduced) eliminates most cases. Rare remaining cases (influence through diagonal gaps) are acceptable false negatives (unit not marked encircled when it arguably should be — safer to under-report than over-report).
- **Frequency**: 15 ticks is fine. Encirclement is strategic, not tactical.

### 3. Combat Retreat Semantics
- **Retreating unit penalties**:
  - Takes increased damage (e.g., 1.5x) while retreating
  - Loses small org per tick (e.g., -0.5 org/tick) while retreating
  - Penalties clear once unit exits combat radius (distance > 2.0 from all enemies)
- **Movement during combat**: NOT hard-blocked. Units can move, with constraints:
  - Unit can move to any destination
  - If `distance_to_enemy(new_pos) <= distance_to_enemy(old_pos)`: movement blocked (can't advance into enemy)
  - If `distance_to_enemy(new_pos) > distance_to_enemy(old_pos)`: movement allowed (retreating)
  - This means units can retreat but not charge through enemies
- **Pursuit logic**:
  - Attacking unit keeps `destination` post-combat
  - If attacker moves toward retreating defender (distance decreasing), combat continues
  - If attacker stops pursuing (distance stable or increasing), defender escapes
  - `fighting` flag cleared for both when distance > 2.0

### 4. Movement System
- **Destination handling**: Keep destination after combat ends. Unit resumes path.
- **Unreachable destination**: Remove if pathfinding fails. Should be rare.
- **Combat interaction**: Movement system checks distance-to-enemy constraint (see #3).
  - Not a simple `!fighting` filter — units in combat CAN move, just with restrictions.

### 5. Influence Map Parameters
- **Decay**: Linear falloff. `influence = strength * (1 - distance/max_distance)`
- **Terrain affects propagation**:
  - Hills: slower propagation (e.g., 0.7x effective radius)
  - Forest: moderate penalty (e.g., 0.8x)
  - Water: blocked for influence (no propagation across water)
  - Plains: 1.0x baseline
- **Strength/radius**: Keep current (unit=1x, city=3x; unit_radius=5, city_radius=7.5)

### 6. Delta Computation
- **Border line**: Send every calculated tick (every 10 ticks). Not every game tick.
- **Tension map**: Likely too large to send to client (full map × players × f32).
  - Optimization: Only send border line cells, not full tension map.
  - Future: Colour-coded map mode could use downsampled tension (e.g., 4x4 blocks).
- **Vision grid**: Send delta (only changed cells). Reuse existing partial update system.
  - Track `prev_vision_grid` and diff against `curr_vision_grid`.
  - Send `VisionDelta { added: Vec<(i32,i32)>, removed: Vec<(i32,i32)> }`.

---

## Open Questions (Remaining)

1. **Retreat damage multiplier**: 1.5x damage while retreating? Configurable per unit type?
2. **Retreat org drain**: -0.5 org/tick? Should it scale with how close the enemy is?
3. **Water connectivity**: Flood-fill of adjacent water cells (4-directional or 8-directional?)
4. **Border line format**: `Vec<(i32,i32)>` cells, or polyline segments for smoother rendering?
5. **Influence terrain costs**: Exact multipliers? (proposed: hills=0.7, forest=0.8, water=blocked)
6. **Combat distance check**: Euclidean `sqrt(dx²+dy²)` or Manhattan `|dx|+|dy|` for retreat constraint?
7. **Pursuit timeout**: If attacker stops moving for N ticks, does combat auto-disengage?
