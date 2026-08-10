//! Spatial hash grid for O(n) adjacency lookups.
//!
//! Used by CombatSystem to find adjacent enemy units without O(n²) pairwise checks.

use std::collections::HashMap;

/// A spatial hash grid that maps grid cells to entity IDs.
/// Cell size matches the combat range (2.0 units).
#[derive(Debug, Clone, Default)]
pub struct SpatialHash {
    /// Cell size in world units. Matches combat range.
    cell_size: f32,
    /// Maps (cell_x, cell_y) -> list of entity indices
    pub cells: HashMap<(i32, i32), Vec<hecs::Entity>>,
}

impl SpatialHash {
    /// Combat range is < 2.0, so cell size of 2.0 ensures adjacent enemies are in adjacent cells.
    pub fn new() -> Self {
        Self {
            cell_size: 2.0,
            cells: HashMap::new(),
        }
    }

    /// Clear all entries (call at start of each combat resolution).
    #[allow(dead_code)] // Used in unit tests
    pub fn clear(&mut self) {
        self.cells.clear();
    }

    /// Insert an entity at the given position.
    pub fn insert(&mut self, entity: hecs::Entity, x: f32, y: f32) {
        let (cx, cy) = self.world_to_cell(x, y);
        self.cells.entry((cx, cy)).or_default().push(entity);
    }

    /// Remove an entity from the cell it's in (given its old position).
    /// Used for incremental updates when a unit moves.
    pub fn remove(&mut self, entity: hecs::Entity, x: f32, y: f32) {
        let (cx, cy) = self.world_to_cell(x, y);
        if let Some(cell) = self.cells.get_mut(&(cx, cy)) {
            cell.retain(|&e| e != entity);
            if cell.is_empty() {
                self.cells.remove(&(cx, cy));
            }
        }
    }

    /// Update an entity's position (remove from old cell, insert into new).
    pub fn update(&mut self, entity: hecs::Entity, old_x: f32, old_y: f32, new_x: f32, new_y: f32) {
        self.remove(entity, old_x, old_y);
        self.insert(entity, new_x, new_y);
    }

    /// Query all entities within the combat range of (x, y).
    /// Returns entities from the same cell and all 8 neighboring cells.
    /// Prefer [`Self::query_nearby_into`] to avoid per-call allocations.
    #[allow(dead_code)] // Used in unit tests
    pub fn query_nearby(&self, x: f32, y: f32) -> Vec<hecs::Entity> {
        let mut results = Vec::new();
        self.query_nearby_into(x, y, &mut results);
        results
    }

    /// Like [`Self::query_nearby`] but writes into `out`, clearing it first.
    /// Reuses the buffer to avoid a Vec allocation per call.
    pub fn query_nearby_into(&self, x: f32, y: f32, out: &mut Vec<hecs::Entity>) {
        let (cx, cy) = self.world_to_cell(x, y);
        out.clear();
        for dx in -1..=1 {
            for dy in -1..=1 {
                if let Some(entities) = self.cells.get(&(cx + dx, cy + dy)) {
                    out.extend(entities.iter().copied());
                }
            }
        }
    }

    fn world_to_cell(&self, x: f32, y: f32) -> (i32, i32) {
        (
            (x / self.cell_size).floor() as i32,
            (y / self.cell_size).floor() as i32,
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_entity(id: u64) -> hecs::Entity {
        // Create a minimal world just to get a valid entity handle
        let mut world = hecs::World::new();
        for _ in 0..id {
            world.spawn(((),));
        }
        world.spawn(((),))
    }

    #[test]
    fn new_is_empty() {
        let sh = SpatialHash::new();
        assert!(sh.query_nearby(0.0, 0.0).is_empty());
    }

    #[test]
    fn insert_and_query_same_cell() {
        let mut sh = SpatialHash::new();
        let e = make_entity(0);
        sh.insert(e, 0.5, 0.5);

        let results = sh.query_nearby(0.5, 0.5);
        assert_eq!(results, vec![e]);
    }

    #[test]
    fn query_adjacent_cell() {
        let mut sh = SpatialHash::new();
        let e = make_entity(0);
        // cell_size is 2.0, so (2.0, 0.0) is in adjacent cell
        sh.insert(e, 2.0, 0.0);

        let results = sh.query_nearby(0.5, 0.0);
        assert!(results.contains(&e));
    }

    #[test]
    fn query_diagonal_adjacent_cell() {
        let mut sh = SpatialHash::new();
        let e = make_entity(0);
        sh.insert(e, 2.5, 2.5); // cell (1, 1)

        let results = sh.query_nearby(0.0, 0.0); // cell (0, 0)
        assert!(results.contains(&e));
    }

    #[test]
    fn query_two_cells_away_not_found() {
        let mut sh = SpatialHash::new();
        let e = make_entity(0);
        // cell_size 2.0, so (4.0, 0.0) is cell (2, 0) — 2 cells away from (0, 0)
        sh.insert(e, 4.0, 0.0);

        let results = sh.query_nearby(0.0, 0.0);
        assert!(!results.contains(&e));
    }

    #[test]
    fn multiple_entities_same_cell() {
        let mut sh = SpatialHash::new();
        // Need unique entities from the same world
        let mut world = hecs::World::new();
        let e1 = world.spawn(((),));
        let e2 = world.spawn(((),));
        let e3 = world.spawn(((),));
        sh.insert(e1, 0.0, 0.0);
        sh.insert(e2, 0.5, 0.5);
        sh.insert(e3, 1.0, 1.0);

        let results = sh.query_nearby(0.5, 0.5);
        assert_eq!(results.len(), 3);
        assert!(results.contains(&e1));
        assert!(results.contains(&e2));
        assert!(results.contains(&e3));
    }

    #[test]
    fn clear_removes_all() {
        let mut sh = SpatialHash::new();
        sh.insert(make_entity(0), 0.0, 0.0);
        sh.clear();

        assert!(sh.query_nearby(0.0, 0.0).is_empty());
    }

    #[test]
    fn negative_coordinates() {
        let mut sh = SpatialHash::new();
        let e = make_entity(0);
        sh.insert(e, -1.0, -1.0);

        let results = sh.query_nearby(-1.0, -1.0);
        assert!(results.contains(&e));
    }

    #[test]
    fn query_returns_entities_from_all_9_cells() {
        let mut sh = SpatialHash::new();
        // Place entities in all 9 cells around center cell (0, 0)
        let mut world = hecs::World::new();
        let mut entities = Vec::new();
        for dx in -1..=1 {
            for dy in -1..=1 {
                let ex = (dx as f32) * 2.0 + 0.5;
                let ey = (dy as f32) * 2.0 + 0.5;
                let e = world.spawn(((),));
                sh.insert(e, ex, ey);
                entities.push(e);
            }
        }

        let results = sh.query_nearby(0.5, 0.5);
        assert_eq!(results.len(), 9);
        for e in &entities {
            assert!(results.contains(e));
        }
    }

    #[test]
    fn empty_query_returns_empty() {
        let sh = SpatialHash::new();
        let results = sh.query_nearby(99.0, 99.0);
        assert!(results.is_empty());
    }
}
