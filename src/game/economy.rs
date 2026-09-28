//! Economy value types: cities, the unit roster, and the canonical default
//! roster shared by the standard ruleset, the benchmark, and tests.
//!
//! Funds/manpower live in ECS components (FundsData, CityData, etc.); see
//! `src/game/game.rs` for the live economy systems.

use crate::config::CityUpgradeSettings;
use crate::game::unit::Equipment;

#[derive(Debug, Clone)]
pub struct City {
    pub id: String,
    pub position: (f32, f32),
    pub owner: Option<String>,
    /// City level (starts at 1)
    pub level: u32,
    /// Maximum distance for supply in grid cells (grows with level)
    pub supply_range: f32,
}

impl City {
    pub fn new(id: &str, position: (f32, f32)) -> Self {
        City {
            id: id.to_string(),
            position,
            owner: None,
            level: 1,
            supply_range: 5.0, // Default for level 1
        }
    }

    /// Apply city upgrade settings for the current level
    pub fn apply_level_settings(&mut self, settings: &CityUpgradeSettings) {
        self.supply_range = settings.supply_range_for(self.level);
    }

    pub fn can_recruit(&self, _unit_type: &UnitType, _cost: u32, owner: &str) -> bool {
        self.owner.as_deref() == Some(owner)
        // Manpower/funds checked against global pool externally
    }
}

#[derive(Debug, Clone)]
#[allow(dead_code)]
pub struct UnitType {
    pub name: String,
    pub symbol: String,
    pub men: u32,
    pub equipment: Vec<Equipment>,
    pub soft_attack: f32,
    pub soft_defense: f32,
    pub hard_attack: f32,
    pub hard_defense: f32,
    pub recruitment_cost: u32,
    pub max_organisation: f32,
    pub reinforcement_rate: f32,
    pub speed: f32,
    pub base_attrition: f32,
    pub fighting_attrition: f32,
    /// Percentage of extra equipment carried above "needed" baseline
    pub supply_buffer_percent: u32,
    /// Max equipment items delivered to a unit per tick by the supply system.
    pub supply_rate: u32,
    /// Derived from men + equipment + buffer
    pub max_supply: u32,
}

/// Build a single equipment item (shared constructor so field additions don't
/// fan out to every literal).
pub fn make_equipment(name: &str, count: u32, cost_per_item: f32) -> crate::game::unit::Equipment {
    crate::game::unit::Equipment {
        name: name.to_string(),
        count,
        max_count: count,
        cost_per_item,
    }
}

/// Canonical default unit roster, shared by the standard ruleset, the
/// benchmark, and tests. One source of truth — adding a field here no longer
/// requires sweeping dozens of literals.
pub fn default_unit_types() -> Vec<UnitType> {
    vec![
        UnitType {
            name: "infantry".to_string(),
            symbol: "i".to_string(),
            men: 1000,
            equipment: vec![make_equipment("rifles", 1000, 0.5)],
            soft_attack: 5.0,
            soft_defense: 5.0,
            hard_attack: 1.0,
            hard_defense: 1.0,
            recruitment_cost: 20,
            max_organisation: 100.0,
            reinforcement_rate: 1.0,
            speed: 1.0,
            base_attrition: 0.5,
            fighting_attrition: 1.0,
            supply_buffer_percent: 10,
            supply_rate: 5,
            max_supply: 2000, // men(1000) * CARRY_WEIGHT(2)
        },
        UnitType {
            name: "armor".to_string(),
            symbol: "a".to_string(),
            men: 500,
            equipment: vec![make_equipment("tanks", 20, 5.0)],
            soft_attack: 8.0,
            soft_defense: 8.0,
            hard_attack: 8.0,
            hard_defense: 8.0,
            recruitment_cost: 50,
            max_organisation: 100.0,
            reinforcement_rate: 2.0,
            speed: 2.0,
            base_attrition: 1.0,
            fighting_attrition: 2.0,
            supply_buffer_percent: 10,
            supply_rate: 5,
            max_supply: 1000, // men(500) * CARRY_WEIGHT(2)
        },
        UnitType {
            name: "cavalry".to_string(),
            symbol: "C".to_string(),
            men: 80,
            equipment: vec![],
            soft_attack: 7.0,
            soft_defense: 4.0,
            hard_attack: 3.0,
            hard_defense: 2.0,
            recruitment_cost: 15,
            max_organisation: 100.0,
            reinforcement_rate: 1.0,
            speed: 1.5,
            base_attrition: 0.15,
            fighting_attrition: 0.6,
            supply_buffer_percent: 10,
            supply_rate: 5,
            max_supply: 160, // men(80) * CARRY_WEIGHT(2)
        },
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    fn basic_unit_type() -> UnitType {
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
            soft_defense: 4.0,
            hard_attack: 3.0,
            hard_defense: 2.0,
            recruitment_cost: 200,
            max_organisation: 100.0,
            reinforcement_rate: 5.0,
            speed: 1.0,
            base_attrition: 0.5,
            fighting_attrition: 1.0,
            supply_buffer_percent: 10,
            supply_rate: 5,
            max_supply: 2000,
        }
    }

    // ---------------------------------------------------------------------------
    // City
    // ---------------------------------------------------------------------------

    #[test]
    fn city_new_defaults() {
        let city = City::new("city_0", (5.0, 10.0));
        assert_eq!(city.id, "city_0");
        assert_eq!(city.position, (5.0, 10.0));
        assert_eq!(city.owner, None);
        assert_eq!(city.level, 1);
        assert_eq!(city.supply_range, 5.0);
    }

    #[test]
    fn city_can_recruit_owned_enough_manpower() {
        let mut city = City::new("city_0", (0.0, 0.0));
        city.owner = Some("p1".to_string());
        let ut = basic_unit_type();

        assert!(city.can_recruit(&ut, 200, "p1"));
    }

    #[test]
    fn city_cannot_recruit_wrong_owner() {
        let mut city = City::new("city_0", (0.0, 0.0));
        city.owner = Some("p1".to_string());
        let ut = basic_unit_type();

        assert!(!city.can_recruit(&ut, 200, "p2"));
    }

    #[test]
    fn city_can_recruit_when_owned() {
        let mut city = City::new("city_0", (0.0, 0.0));
        city.owner = Some("p1".to_string());
        let ut = basic_unit_type();

        assert!(city.can_recruit(&ut, 200, "p1"));
    }

    #[test]
    fn city_cannot_recruit_unowned() {
        let city = City::new("city_0", (0.0, 0.0));
        let ut = basic_unit_type();

        assert!(!city.can_recruit(&ut, 200, "p1"));
    }

    #[test]
    fn city_apply_level_settings() {
        let mut city = City::new("city_0", (0.0, 0.0));
        let settings = CityUpgradeSettings {
            enabled: true,
            max_level: 10,
            base_upgrade_cost: 200,
            income_growth_per_level: 0.5,
            base_supply_range: 5.0,
            supply_range_growth: 3.0,
            secondary_city_decay: 0.5,
            manpower_regen_per_tick: 10.0,
        };
        city.level = 3;
        city.apply_level_settings(&settings);

        // level 3: output = 30 + (3-1)*0 = 30 (base only, no growth param for output)
        // range = 5 + (3-1)*3 = 11
        assert_eq!(city.supply_range, settings.supply_range_for(3));
    }
}
