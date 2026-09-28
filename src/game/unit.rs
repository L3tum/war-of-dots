//! Unit system: equipment and terrain value types.
//!
//! Units themselves are plain ECS components (UnitOwner, UnitStats,
//! UnitPosition, etc. — see `src/game/components.rs`); this module holds the
//! shared value types (Equipment, Terrain) they build on.

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Equipment {
    pub name: String,
    pub count: u32,
    pub max_count: u32,
    /// Cost in funds per item (used for recruitment and resupply)
    pub cost_per_item: f32,
}

impl Equipment {
    #[allow(dead_code)]
    #[allow(clippy::should_implement_trait)]
    pub fn from_str(s: &str) -> Self {
        let parts: Vec<&str> = s.split(':').collect();
        let name = parts[0].to_string();
        let count = parts
            .get(1)
            .and_then(|s| s.parse::<u32>().ok())
            .unwrap_or(0);
        let cost_per_item = parts
            .get(2)
            .and_then(|s| s.parse::<f32>().ok())
            .unwrap_or(0.0); // Legacy format: no cost
        Equipment {
            name,
            count,
            max_count: count,
            cost_per_item,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Terrain {
    Plains,
    Hills,
    Forest,
    Water,
    Mountain,
}

impl Terrain {
    #[allow(dead_code)] // Used in integration tests for Terrain enum
    pub fn attack_modifier(&self) -> f32 {
        match self {
            Terrain::Hills => 1.5,
            Terrain::Forest => 0.75,
            Terrain::Water => 0.5,
            _ => 1.0,
        }
    }

    #[allow(dead_code)]
    pub fn vision_bonus(&self) -> f32 {
        match self {
            Terrain::Hills => 2.0,
            _ => 1.0,
        }
    }

    #[allow(dead_code)]
    pub fn is_impassable(&self) -> bool {
        matches!(self, Terrain::Mountain)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // ---------------------------------------------------------------------------
    // Terrain
    // ---------------------------------------------------------------------------

    #[test]
    fn terrain_attack_modifier_plains() {
        assert_eq!(Terrain::Plains.attack_modifier(), 1.0);
    }

    #[test]
    fn terrain_attack_modifier_hills_bonus() {
        assert_eq!(Terrain::Hills.attack_modifier(), 1.5);
    }

    #[test]
    fn terrain_attack_modifier_forest_penalty() {
        assert_eq!(Terrain::Forest.attack_modifier(), 0.75);
    }

    #[test]
    fn terrain_attack_modifier_water_penalty() {
        assert_eq!(Terrain::Water.attack_modifier(), 0.5);
    }

    #[test]
    fn terrain_attack_modifier_mountain_normal() {
        assert_eq!(Terrain::Mountain.attack_modifier(), 1.0);
    }

    #[test]
    fn terrain_vision_bonus_hills() {
        assert_eq!(Terrain::Hills.vision_bonus(), 2.0);
    }

    #[test]
    fn terrain_vision_bonus_others() {
        assert_eq!(Terrain::Plains.vision_bonus(), 1.0);
        assert_eq!(Terrain::Forest.vision_bonus(), 1.0);
        assert_eq!(Terrain::Water.vision_bonus(), 1.0);
        assert_eq!(Terrain::Mountain.vision_bonus(), 1.0);
    }

    #[test]
    fn terrain_is_impassable_only_mountain() {
        assert!(!Terrain::Plains.is_impassable());
        assert!(!Terrain::Hills.is_impassable());
        assert!(!Terrain::Forest.is_impassable());
        assert!(!Terrain::Water.is_impassable());
        assert!(Terrain::Mountain.is_impassable());
    }

    // ---------------------------------------------------------------------------
    // Equipment
    // ---------------------------------------------------------------------------

    #[test]
    fn equipment_from_str_full_format() {
        let eq = Equipment::from_str("rifle:100:2.5");
        assert_eq!(eq.name, "rifle");
        assert_eq!(eq.count, 100);
        assert_eq!(eq.max_count, 100);
        assert_eq!(eq.cost_per_item, 2.5);
    }

    #[test]
    fn equipment_from_str_legacy_no_cost() {
        let eq = Equipment::from_str("rifle:50");
        assert_eq!(eq.name, "rifle");
        assert_eq!(eq.count, 50);
        assert_eq!(eq.cost_per_item, 0.0);
    }

    #[test]
    fn equipment_from_str_name_only() {
        let eq = Equipment::from_str("ammo");
        assert_eq!(eq.name, "ammo");
        assert_eq!(eq.count, 0);
        assert_eq!(eq.cost_per_item, 0.0);
    }

    #[test]
    fn equipment_from_str_invalid_count_defaults_zero() {
        let eq = Equipment::from_str("rifle:abc:2.5");
        assert_eq!(eq.count, 0);
        assert_eq!(eq.cost_per_item, 2.5);
    }
}
