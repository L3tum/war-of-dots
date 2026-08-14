//! NearbyEnemies — grid-based neighbour rebuild every tick.
//!
//! Uses a coarse spatial grid (cell_size = COMBAT_RANGE = 2.0) so each unit only
//! checks the ~1-5 entities in its 3×3 cell neighbourhood instead of every
//! other unit. No HashMap-based SpatialHash, no incremental maintenance.
//!
//! Pipeline:
//!   1. Collect all unit data in one ECS query.
//!   2. Build grid index: each cell stores (entity, position, owner_idx).
//!   3. For each unit, query 3×3 cell neighbourhood, filter enemies within
//!      COMBAT_RANGE using the owner index (no linear scan!), write NearbyEnemies
//!      and `attacking` flag.
//!
//! Complexity: O(n × c) where c ≈ entities per 3×3 cell neighbourhood.
//! At game scale (500 units on 200×200 grid), c is very small (~2-3), so
//! this is ~150-250 distance checks vs 250,000 for brute-force O(n²).

use crate::game::components::*;
use crate::game::constants::COMBAT_RANGE_SQ;
use hecs::Entity;

/// Cell size equals COMBAT_RANGE (sqrt(4.0) = 2.0).
const CELL_SIZE: f32 = 2.0;

/// Data stored per entity in a grid cell.
/// owner_idx is the index into the units array — enables O(1) owner comparison
/// because units with the same owner share the same player_id string at the
/// same array position (deduplicated below).
#[derive(Clone, Copy)]
struct CellEntry {
    entity: Entity,
    pos: UnitPosition,
    owner_idx: usize, // index into units array
}

/// Coarse spatial grid: 1D array of cell buckets.
struct GridIndex {
    cells: Box<[Vec<CellEntry>]>,
    min_x: i32,
    min_y: i32,
    width: usize,
}

impl GridIndex {
    /// Build grid from units array.
    /// `units` is indexed; each entry's position in the array is its owner_idx.
    fn build(units: &[(Entity, UnitPosition, String, bool)]) -> Self {
        let mut min_x = i32::MAX;
        let mut min_y = i32::MAX;
        let mut max_x = i32::MIN;
        let mut max_y = i32::MIN;
        for &(_, pos, _, _) in units {
            let c = cell_for(pos);
            if c.0 < min_x { min_x = c.0; }
            if c.1 < min_y { min_y = c.1; }
            if c.0 > max_x { max_x = c.0; }
            if c.1 > max_y { max_y = c.1; }
        }
        let width = (max_x - min_x + 2) as usize;
        let height = (max_y - min_y + 2) as usize;
        let total = width * height;

        let mut cells: Vec<Vec<CellEntry>> = vec![Vec::with_capacity(4); total];

        for (i, &(e, pos, _, _)) in units.iter().enumerate() {
            let c = cell_for(pos);
            let cx = ((c.0 - min_x) as usize).min(width - 1);
            let cy = ((c.1 - min_y) as usize).min(height - 1);
            let idx = cy * width + cx;
            cells[idx].push(CellEntry {
                entity: e,
                pos,
                owner_idx: i,
            });
        }

        Self {
            cells: cells.into_boxed_slice(),
            min_x,
            min_y,
            width,
        }
    }

    #[inline]
    fn cell_idx(&self, cx: i32, cy: i32) -> Option<usize> {
        let x = (cx - self.min_x) as isize;
        let y = (cy - self.min_y) as isize;
        let w = self.width as isize;
        if (0..w).contains(&x) && (0..(self.cells.len() as isize / w)).contains(&y) {
            Some((y * w + x) as usize)
        } else {
            None
        }
    }
}

#[inline]
fn cell_for(pos: UnitPosition) -> (i32, i32) {
    (
        (pos.x / CELL_SIZE).floor() as i32,
        (pos.y / CELL_SIZE).floor() as i32,
    )
}

pub fn run(world: &mut hecs::World) {
    // 1. Collect all living unit data in one ECS query.
    //    Each entry: (entity, position, owner_string, is_moved).
    //    The array index IS the owner identity — same array index = same unit,
    //    so for owner comparison we just compare indices.
    let units: Vec<(Entity, UnitPosition, String, bool)> = world
        .query::<(Entity, &UnitPosition, &UnitOwner, &UnitFlags)>()
        .iter()
        .map(|(e, pos, owner, flags)| {
            (e, *pos, owner.player_id.clone(), flags.moved)
        })
        .collect();

    let n = units.len();
    if n == 0 {
        return;
    }

    // 2. Build grid index.
    let grid = GridIndex::build(&units);

    // Build owner lookup: for each unit index, which other indices share the
    // same owner? We need owner equality, not identity. Create a set of
    // owner indices grouped by player_id.
    // Simpler: just compare player_id strings. Since most units have different
    // owners (only same-player units share), we use a compact owner ID.
    //
    // Optimisation: build a parallel `owner_id` array that maps each unit index
    // to a small usize owner group ID. Then owner comparison is just usize equality.
    let owner_ids: Vec<usize> = {
        let mut owner_map: std::collections::HashMap<&str, usize> =
            std::collections::HashMap::with_capacity(16);
        let mut next_id: usize = 0;
        units
            .iter()
            .map(|(_, _, ref pid, _)| {
                *owner_map.entry(pid.as_str()).or_insert_with(|| {
                    let id = next_id;
                    next_id += 1;
                    id
                })
            })
            .collect()
    };

    debug_assert_eq!(owner_ids.len(), n);

    // Reuse buffer to avoid per-unit allocation.
    let mut enemies = Vec::with_capacity(16);

    // 3. For each unit, query 3×3 neighbourhood and build NearbyEnemies.
    for (my_idx, &(entity, pos, _, is_moved)) in units.iter().enumerate() {
        let my_owner = owner_ids[my_idx];
        let c = cell_for(pos);

        // Collect 3×3 neighbourhood entries — no HashMap lookups, no linear scans.
        enemies.clear();
        for dx in -1..=1 {
            for dy in -1..=1 {
                if let Some(idx) = grid.cell_idx(c.0 + dx, c.1 + dy) {
                    for entry in &grid.cells[idx] {
                        if entry.entity == entity {
                            continue;
                        }
                        // Fast owner comparison.
                        if owner_ids[entry.owner_idx] == my_owner {
                            continue;
                        }
                        let dx_pos = pos.x - entry.pos.x;
                        let dy_pos = pos.y - entry.pos.y;
                        if dx_pos * dx_pos + dy_pos * dy_pos < COMBAT_RANGE_SQ {
                            enemies.push(entry.entity);
                        }
                    }
                }
            }
        }

        // Write/update NearbyEnemies.
        let has_enemies = !enemies.is_empty();
        if let Ok((nb,)) = world.query_one_mut::<(&mut NearbyEnemies,)>(entity) {
            nb.entities = std::mem::take(&mut enemies);
            nb.initialized = true;
            enemies = Vec::with_capacity(16);
        } else {
            let _ = world.insert(
                entity,
                (NearbyEnemies {
                    entities: enemies.drain(..).collect(),
                    initialized: true,
                },),
            );
            enemies = Vec::with_capacity(16);
        }

        // Set `attacking` flag for movers only.
        if is_moved {
            if let Ok((mut_flags,)) = world.query_one_mut::<(&mut UnitFlags,)>(entity) {
                mut_flags.attacking = has_enemies;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::CityUpgradeSettings;
    use crate::game::world_ext::WorldExt;

    fn setup() -> (hecs::World, hecs::Entity) {
        let mut world = hecs::World::new();
        let entity = world.spawn_world(
            vec![vec![crate::game::unit::Terrain::Plains; 200]; 200],
            200,
            200,
            CityUpgradeSettings::default(),
            std::collections::HashMap::new(),
            Vec::new(),
        );
        (world, entity)
    }

    /// Spawn a unit with the given owner, position and moved flag.
    fn make_unit(
        world: &mut hecs::World,
        owner: &str,
        x: f32,
        y: f32,
        moved: bool,
    ) -> hecs::Entity {
        world.spawn((
            UnitOwner {
                player_id: owner.to_string(),
            },
            UnitPosition { x, y },
            UnitFlags {
                moved,
                ..UnitFlags::default()
            },
        ))
    }

    #[test]
    fn initializes_new_spawns_and_sees_each_other() {
        let (mut world, _world_entity) = setup();
        let a = make_unit(&mut world, "P1", 10.0, 10.0, false);
        let b = make_unit(&mut world, "P2", 10.5, 10.5, false);
        run(&mut world);

        let nearby_a = world.get::<&NearbyEnemies>(a).unwrap();
        assert!(nearby_a.initialized);
        assert!(
            nearby_a.entities.contains(&b),
            "A should see enemy B"
        );

        let nearby_b = world.get::<&NearbyEnemies>(b).unwrap();
        assert!(nearby_b.initialized);
        assert!(nearby_b.entities.contains(&a), "B should see enemy A");
    }

    #[test]
    fn ignores_same_owner() {
        let (mut world, _world_entity) = setup();
        let a = make_unit(&mut world, "P1", 10.0, 10.0, false);
        let b = make_unit(&mut world, "P1", 10.5, 10.5, false);
        run(&mut world);

        let nearby_a = world.get::<&NearbyEnemies>(a).unwrap();
        assert!(
            !nearby_a.entities.contains(&b),
            "same-owner must be excluded"
        );
        let nearby_b = world.get::<&NearbyEnemies>(b).unwrap();
        assert!(
            !nearby_b.entities.contains(&a),
            "same-owner must be excluded"
        );
    }

    #[test]
    fn mover_sets_attacking_with_enemy_nearby() {
        let (mut world, _world_entity) = setup();
        // Stationary defender (moved=false).
        let b = make_unit(&mut world, "P2", 10.5, 10.5, false);
        // Mover advancing toward B (moved=true).
        let a = make_unit(&mut world, "P1", 10.0, 10.0, true);
        run(&mut world);

        assert!(
            world.get::<&UnitFlags>(a).unwrap().attacking,
            "mover with a nearby enemy must have attacking=true"
        );
        let nearby_b = world.get::<&NearbyEnemies>(b).unwrap();
        assert!(
            nearby_b.entities.contains(&a),
            "stationary B should see mover A"
        );
    }

    #[test]
    fn mover_clears_attacking_when_no_enemy_nearby() {
        let (mut world, _world_entity) = setup();
        // Mover at a far distance from anyone.
        let a = make_unit(&mut world, "P1", 0.0, 0.0, true);
        let b = make_unit(&mut world, "P2", 100.0, 100.0, false);
        run(&mut world);

        assert!(
            !world.get::<&UnitFlags>(a).unwrap().attacking,
            "mover far from enemies should have attacking=false"
        );
    }

    #[test]
    fn stationary_defender_does_not_become_attacker() {
        let (mut world, _world_entity) = setup();
        let b = make_unit(&mut world, "P2", 10.5, 10.5, false);
        let a = make_unit(&mut world, "P1", 10.0, 10.0, true);
        run(&mut world);

        assert!(
            !world.get::<&UnitFlags>(b).unwrap().attacking,
            "stationary defender must NOT become an attacker"
        );
        assert!(
            !world.get::<&NearbyEnemies>(b).unwrap().entities.is_empty(),
            "B should see mover A"
        );
    }

    #[test]
    fn two_nearby_movers_see_each_other_and_attack() {
        let (mut world, _world_entity) = setup();
        let a = make_unit(&mut world, "P1", 10.0, 10.0, true);
        let b = make_unit(&mut world, "P2", 10.5, 10.5, true);
        run(&mut world);

        assert!(world
            .get::<&NearbyEnemies>(a)
            .unwrap()
            .entities
            .contains(&b));
        assert!(world
            .get::<&NearbyEnemies>(b)
            .unwrap()
            .entities
            .contains(&a));
        assert!(world.get::<&UnitFlags>(a).unwrap().attacking);
        assert!(world.get::<&UnitFlags>(b).unwrap().attacking);
    }

    #[test]
    fn three_owners_all_see_each_other() {
        let (mut world, _world_entity) = setup();
        let a = make_unit(&mut world, "P1", 10.0, 10.0, false);
        let b = make_unit(&mut world, "P2", 10.5, 10.5, false);
        let cc = make_unit(&mut world, "P3", 10.2, 10.2, false);
        run(&mut world);

        let na = world.get::<&NearbyEnemies>(a).unwrap();
        assert!(na.entities.contains(&b) && na.entities.contains(&cc));
        let nb = world.get::<&NearbyEnemies>(b).unwrap();
        assert!(nb.entities.contains(&a) && nb.entities.contains(&cc));
        let nc = world.get::<&NearbyEnemies>(cc).unwrap();
        assert!(nc.entities.contains(&a) && nc.entities.contains(&b));
    }

    #[test]
    fn dense_cluster_filters_self_and_allies() {
        let (mut world, _world_entity) = setup();
        let p1a = make_unit(&mut world, "P1", 10.0, 10.0, false);
        let p1b = make_unit(&mut world, "P1", 10.1, 10.1, false);
        let p1c = make_unit(&mut world, "P1", 10.2, 10.2, false);
        let p2a = make_unit(&mut world, "P2", 10.3, 10.3, false);
        let p2b = make_unit(&mut world, "P2", 10.4, 10.4, false);
        run(&mut world);

        for (unit, label) in [(&p1a, "P1A"), (&p1b, "P1B"), (&p1c, "P1C")] {
            let nb = world.get::<&NearbyEnemies>(*unit).unwrap();
            assert_eq!(
                nb.entities.len(),
                2,
                "{} should see exactly 2 P2 enemies",
                label
            );
            assert!(nb.entities.contains(&p2a) && nb.entities.contains(&p2b));
            assert!(
                !nb.entities.contains(unit),
                "{} must not see itself",
                label
            );
        }
        for (unit, label) in [(&p2a, "P2A"), (&p2b, "P2B")] {
            let nb = world.get::<&NearbyEnemies>(*unit).unwrap();
            assert_eq!(
                nb.entities.len(),
                3,
                "{} should see 3 P1 enemies",
                label
            );
        }
    }

    #[test]
    fn empty_world_does_not_panic() {
        let (mut world, _world_entity) = setup();
        run(&mut world);
    }

    #[test]
    fn units_beyond_combat_range_not_seen() {
        let (mut world, _world_entity) = setup();
        let a = make_unit(&mut world, "P1", 0.0, 0.0, false);
        let b = make_unit(&mut world, "P2", 10.0, 10.0, false);
        run(&mut world);

        let nearby_a = world.get::<&NearbyEnemies>(a).unwrap();
        assert!(nearby_a.entities.is_empty(), "far units should not be in NearbyEnemies");
        let nearby_b = world.get::<&NearbyEnemies>(b).unwrap();
        assert!(nearby_b.entities.is_empty(), "far units should not be in NearbyEnemies");
    }
}
