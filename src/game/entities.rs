//! Entity registry — string unit ID <-> hecs::Entity bidirectional lookup.
//!
//! Extracted from `game.rs` (Arch #3): the two maps the `Game` facade used to
//! hold inline, grouped with the small helpers that manage them.

use std::collections::HashMap;

/// Bidirectional string unit ID <-> hecs::Entity registry.
///
/// Maps string unit IDs ("unit_123") -> hecs::Entity, with a reverse lookup
/// (hecs::Entity -> string unit ID) for O(1) dead entity cleanup.
#[derive(Default)]
pub(crate) struct EntityRegistry {
    /// Maps string unit IDs ("unit_123") -> hecs::Entity
    pub(crate) entity_registry: HashMap<String, hecs::Entity>,
    /// Reverse lookup: hecs::Entity -> string unit ID — O(1) dead entity cleanup
    pub(crate) entity_reverse: HashMap<hecs::Entity, String>,
}

impl EntityRegistry {
    /// Register a freshly spawned unit under its string ID (both directions).
    pub(crate) fn insert(&mut self, unit_id: String, entity: hecs::Entity) {
        self.entity_registry.insert(unit_id.clone(), entity);
        self.entity_reverse.insert(entity, unit_id);
    }

    /// Look up a unit's hecs handle by its string ID.
    pub(crate) fn get(&self, unit_id: &str) -> Option<hecs::Entity> {
        self.entity_registry.get(unit_id).copied()
    }
}
