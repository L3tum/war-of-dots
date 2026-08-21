//! Unit system: men, equipment, organisation, derived stats, supply effectiveness
//!
//! NOTE: This module uses deprecated `Unit` type during ECS migration.
#![allow(deprecated)]

use serde::{Deserialize, Serialize};
use uuid::Uuid;

/// [LEGACY — NOT USED AT RUNTIME] Unit struct was the pre-ECS unit data model.
/// The game now uses ECS components (UnitOwner, UnitStats, UnitPosition, etc.) directly.
/// Kept for tests only. See `src/game/components.rs` for current component definitions.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[deprecated(since = "0.1.0", note = "Replaced by ECS components")]
pub struct Unit {
    pub id: String,
    pub owner: String,
    pub unit_type: String,
    pub symbol: String,
    pub position: (f32, f32),
    pub path: Vec<(f32, f32)>,
    pub men: f32,
    pub max_men: f32,
    pub equipment: Vec<Equipment>,
    pub max_equipment: Vec<Equipment>,
    pub organisation: f32,
    pub max_organisation: f32,
    pub supply: f32,
    pub max_supply: f32,
    pub healing: bool,
    pub attacking: bool,
    pub encircled: bool,
    /// Whether this unit receives equipment resupply from cities (separate from healing)
    pub resupply_enabled: bool,
    /// Percentage of extra equipment carried above "needed" baseline
    pub supply_buffer_percent: u32,
    /// Whether this unit is currently traveling on water
    pub on_water: bool,
    /// Extra supply loaded on "the boat" (per equipment type, stored as total count)
    pub boat_supply: Vec<Equipment>,
    /// Max boat supply (set at embark, used to compute boat_supply_percent)
    pub max_boat_supply: Vec<Equipment>,
    /// In process of getting on boat (stands still during embark)
    pub embarking: bool,
    /// In process of getting off boat (stands still during disembark)
    pub disembarking: bool,
    /// Ticks remaining to complete embark (starts at 10)
    pub embarking_ticks_left: u32,
    /// Ticks remaining to complete disembark (starts at 10)
    pub disembarking_ticks_left: u32,
}

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

#[allow(dead_code)]
impl Unit {
    pub fn new(
        unit_type: &crate::game::economy::UnitType,
        owner: &str,
        position: (f32, f32),
    ) -> Self {
        let max_supply = unit_type.max_supply as f32;
        let supply_buffer_percent = unit_type.supply_buffer_percent;

        Unit {
            id: Uuid::new_v4().to_string(),
            owner: owner.to_string(),
            unit_type: unit_type.name.clone(),
            symbol: unit_type.symbol.clone(),
            position,
            path: Vec::new(),
            men: unit_type.men as f32,
            max_men: unit_type.men as f32,
            equipment: unit_type.equipment.clone(),
            max_equipment: unit_type.equipment.clone(),
            organisation: 0.0,
            max_organisation: unit_type.max_organisation,
            supply: max_supply,
            max_supply,
            healing: true,
            attacking: false,
            encircled: false,
            resupply_enabled: true,
            supply_buffer_percent,
            on_water: false,
            boat_supply: Vec::new(),
            max_boat_supply: Vec::new(),
            embarking: false,
            disembarking: false,
            embarking_ticks_left: 0,
            disembarking_ticks_left: 0,
        }
    }

    pub fn surrender_chance(&self) -> f32 {
        let men_ratio = self.men / self.max_men;
        let encircled_penalty = if self.encircled { 0.4 } else { 0.0 };
        // Surrender chance is proportional to how few men left
        let base_chance = (1.0 - men_ratio) * 0.5; // up to 50% from men alone
        base_chance + encircled_penalty
    }

    /// Returns the combat effectiveness multiplier for this unit.
    /// Returns 0.75 when on water (nerf), 1.0 otherwise.
    pub fn water_combat_modifier(&self) -> f32 {
        if self.on_water {
            0.75
        } else {
            1.0
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
    use crate::game::economy::UnitType;

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

    // ---------------------------------------------------------------------------
    // Unit construction
    // ---------------------------------------------------------------------------

    fn test_unit_type() -> UnitType {
        UnitType {
            name: "infantry".to_string(),
            symbol: "I".to_string(),
            men: 1000,
            equipment: vec![Equipment {
                name: "rifle".to_string(),
                count: 100,
                max_count: 100,
                cost_per_item: 2.5,
            }],
            soft_attack: 5.0,
            soft_defense: 5.5,
            hard_attack: 3.0,
            hard_defense: 2.0,
            recruitment_cost: 200,
            max_organisation: 100.0,
            reinforcement_rate: 5.0,
            speed: 1.0,
            base_attrition: 0.5,
            fighting_attrition: 1.0,
            supply_buffer_percent: 20,
            supply_rate: 5,
            max_supply: 2000,
        }
    }

    #[test]
    fn unit_new_sets_correct_values() {
        let ut = test_unit_type();
        let unit = Unit::new(&ut, "player1", (10.0, 20.0));

        assert_eq!(unit.owner, "player1");
        assert_eq!(unit.unit_type, "infantry");
        assert_eq!(unit.symbol, "I");
        assert_eq!(unit.position, (10.0, 20.0));
        assert_eq!(unit.men, 1000.0);
        assert_eq!(unit.max_men, 1000.0);
        assert_eq!(unit.organisation, 0.0);
        assert_eq!(unit.max_organisation, 100.0);
        assert_eq!(unit.supply, 2000.0);
        assert_eq!(unit.max_supply, 2000.0);
        assert!(unit.healing);
        assert!(!unit.attacking);
        assert!(!unit.encircled);
        assert!(unit.resupply_enabled);
        assert!(!unit.on_water);
        assert!(!unit.embarking);
        assert!(!unit.disembarking);
        assert_eq!(unit.equipment.len(), 1);
        assert_eq!(unit.max_equipment.len(), 1);
        assert!(!unit.id.is_empty());
    }

    // ---------------------------------------------------------------------------
    // Surrender chance
    // ---------------------------------------------------------------------------

    #[test]
    fn surrender_chance_full_men_zero_chance() {
        let mut unit = Unit::new(&test_unit_type(), "p1", (0.0, 0.0));
        unit.men = 1000.0;
        unit.max_men = 1000.0;
        unit.encircled = false;
        // (1 - 1.0) * 0.5 + 0 = 0
        assert_eq!(unit.surrender_chance(), 0.0);
    }

    #[test]
    fn surrender_chance_half_men() {
        let mut unit = Unit::new(&test_unit_type(), "p1", (0.0, 0.0));
        unit.men = 500.0;
        unit.max_men = 1000.0;
        unit.encircled = false;
        // (1 - 0.5) * 0.5 + 0 = 0.25
        assert_eq!(unit.surrender_chance(), 0.25);
    }

    #[test]
    fn surrender_chance_low_men_not_encircled() {
        let mut unit = Unit::new(&test_unit_type(), "p1", (0.0, 0.0));
        unit.men = 100.0;
        unit.max_men = 1000.0;
        unit.encircled = false;
        // (1 - 0.1) * 0.5 + 0 = 0.45
        assert_eq!(unit.surrender_chance(), 0.45);
    }

    #[test]
    fn surrender_chance_encircled_bonus() {
        let mut unit = Unit::new(&test_unit_type(), "p1", (0.0, 0.0));
        unit.men = 500.0;
        unit.max_men = 1000.0;
        unit.encircled = true;
        // (1 - 0.5) * 0.5 + 0.4 = 0.65
        assert_eq!(unit.surrender_chance(), 0.65);
    }

    #[test]
    fn surrender_chance_no_men_encircled() {
        let mut unit = Unit::new(&test_unit_type(), "p1", (0.0, 0.0));
        unit.men = 0.0;
        unit.max_men = 1000.0;
        unit.encircled = true;
        // (1 - 0.0) * 0.5 + 0.4 = 0.9
        assert_eq!(unit.surrender_chance(), 0.9);
    }

    // ---------------------------------------------------------------------------
    // Water combat modifier
    // ---------------------------------------------------------------------------

    #[test]
    fn water_combat_modifier_on_land() {
        let unit = Unit::new(&test_unit_type(), "p1", (0.0, 0.0));
        assert!(!unit.on_water);
        assert_eq!(unit.water_combat_modifier(), 1.0);
    }

    #[test]
    fn water_combat_modifier_on_water() {
        let mut unit = Unit::new(&test_unit_type(), "p1", (0.0, 0.0));
        unit.on_water = true;
        assert_eq!(unit.water_combat_modifier(), 0.75);
    }
}
