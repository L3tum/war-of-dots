#![allow(deprecated)]

//! Economy: funds, manpower, recruitment, healing, city upgrades
//!
//! NOTE: This module uses deprecated `Unit` and `Economy` types during ECS migration.

use std::collections::HashMap;

use crate::config::CityUpgradeSettings;
use crate::game::unit::{Equipment, Unit};

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
    /// Derived from men + equipment + buffer
    pub max_supply: u32,
}

/// [LEGACY — NOT USED AT RUNTIME] Economy struct was the pre-ECS game state manager.
/// The game now uses ECS components (FundsData, CityData, etc.) and `Game` methods directly.
/// Kept for tests only. See `src/game/game.rs` and `src/game/components.rs` for current impl.
#[derive(Debug, Clone)]
#[deprecated(since = "0.1.0", note = "Replaced by ECS components in Game/WorldData")]
pub struct Economy {
    pub funds: HashMap<String, u32>,
    /// Global manpower pool per player
    pub manpower: HashMap<String, u32>,
    pub cities: Vec<City>,
    /// City upgrade settings (loaded from config)
    pub city_upgrade_settings: CityUpgradeSettings,
}

#[allow(dead_code)]
impl Economy {
    pub fn new() -> Self {
        Economy {
            funds: HashMap::new(),
            manpower: HashMap::new(),
            cities: Vec::new(),
            city_upgrade_settings: CityUpgradeSettings::default(),
        }
    }
}

impl Default for Economy {
    fn default() -> Self {
        Self::new()
    }
}

#[allow(dead_code)]
impl Economy {
    pub fn new_with_settings(settings: &CityUpgradeSettings) -> Self {
        Economy {
            funds: HashMap::new(),
            manpower: HashMap::new(),
            cities: Vec::new(),
            city_upgrade_settings: settings.clone(),
        }
    }

    /// Generate income for all owned cities into global economy pools.
    /// - Funds: income_multiplier(level) * ticks
    /// - Manpower: manpower_regen_per_tick * level * ticks (added to global manpower pool)
    pub fn generate_funds(&mut self, ticks: f32) {
        let settings = &self.city_upgrade_settings;
        for city in &self.cities {
            if let Some(owner) = &city.owner {
                let income_multiplier = settings.income_multiplier(city.level);
                let funds_gain = income_multiplier * ticks;
                let funds = self.funds.entry(owner.clone()).or_insert(0);
                *funds = funds.saturating_add(funds_gain as u32);

                // Manpower generation into global pool
                let mp_gain = (settings.manpower_regen_per_tick * city.level as f32 * ticks) as u32;
                let manpower = self.manpower.entry(owner.clone()).or_insert(0);
                *manpower = manpower.saturating_add(mp_gain);
            }
        }
    }

    /// Upgrade a city to the next level. Returns Ok on success, Err with reason on failure.
    pub fn upgrade_city(&mut self, city_id: &str, owner: &str) -> Result<(), String> {
        let city = self
            .cities
            .iter_mut()
            .find(|c| c.id == city_id && c.owner.as_deref() == Some(owner))
            .ok_or_else(|| format!("City {} not found or not owned by {}", city_id, owner))?;

        let settings = &self.city_upgrade_settings;
        if !settings.enabled {
            return Err("City upgrades are not enabled".to_string());
        }

        let next_level = city.level + 1;
        if next_level > settings.max_level {
            return Err(format!(
                "City is already at maximum level ({})",
                settings.max_level
            ));
        }

        let cost = settings.upgrade_cost(next_level);

        // Deduct funds
        let funds = self
            .funds
            .get_mut(owner)
            .ok_or("Player has no funds entry")?;
        if *funds < cost {
            return Err(format!("Insufficient funds: need {}, have {}", cost, funds));
        }
        *funds -= cost;

        // Apply upgrade
        city.level = next_level;
        city.apply_level_settings(settings);

        Ok(())
    }

    pub fn recruit_unit(
        &mut self,
        city_id: &str,
        unit_type: &UnitType,
        owner: &str,
        position: (f32, f32),
    ) -> Option<Unit> {
        let city = self.cities.iter().find(|c| c.id == city_id)?;
        if !city.can_recruit(unit_type, 0, owner) {
            return None;
        }
        // Deduct manpower from global pool
        {
            let mp = self.manpower.get_mut(owner)?;
            if *mp < unit_type.men {
                return None;
            }
            *mp -= unit_type.men;
        }
        // Deduct funds
        if let Some(funds) = self.funds.get_mut(owner) {
            if *funds < unit_type.recruitment_cost {
                // Insufficient funds, refund manpower
                if let Some(mp) = self.manpower.get_mut(owner) {
                    *mp = mp.saturating_add(unit_type.men);
                }
                return None;
            }
            *funds -= unit_type.recruitment_cost;
        }
        // Create unit with full men but 0% organisation
        let unit = Unit::new(unit_type, owner, position);
        Some(unit)
    }

    pub fn heal_unit(&mut self, unit: &mut Unit, owner: &str, city_id: &str, rate: f32) {
        if !unit.healing {
            return;
        }
        if unit.encircled {
            return; // no supply lines
        }
        // Check city exists and is owned
        if self
            .cities
            .iter()
            .find(|c| c.id == city_id && c.owner.as_deref() == Some(owner))
            .is_none()
        {
            return;
        }
        let _manpower_needed = (unit.max_men - unit.men) as u32;
        // Healing from global manpower pool
        let available_mp = self.manpower.get(owner).copied().unwrap_or(0) as f32;
        let healing_amount = (rate * available_mp).min(unit.max_men - unit.men);
        if healing_amount > 0.0 {
            unit.men = (unit.men + healing_amount).min(unit.max_men);
            // Heal equipment proportionally to manpower healing, at 50% rate
            for (ueq, meq) in unit.equipment.iter_mut().zip(&unit.max_equipment) {
                let eq_needed = meq.max_count - ueq.count;
                let heal_eq = (healing_amount * 0.5).min(eq_needed as f32) as u32;
                ueq.count = (ueq.count + heal_eq).min(meq.max_count);
            }
            // Deduct from global manpower pool
            if let Some(mp) = self.manpower.get_mut(owner) {
                *mp = mp.saturating_sub(healing_amount as u32);
            }
        }
    }
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
            supply_buffer_percent: 20,
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

    // ---------------------------------------------------------------------------
    // Economy
    // ---------------------------------------------------------------------------

    #[test]
    fn economy_new_empty() {
        let eco = Economy::new();
        assert!(eco.funds.is_empty());
        assert!(eco.cities.is_empty());
    }

    #[test]
    fn generate_funds_unowned_city_no_income() {
        let mut eco = Economy::new();
        eco.cities.push(City::new("city_0", (0.0, 0.0)));
        eco.generate_funds(1.0);

        assert!(eco.funds.is_empty());
    }

    #[test]
    fn generate_funds_owned_city() {
        let mut eco = Economy::new();
        let mut city = City::new("city_0", (0.0, 0.0));
        city.owner = Some("p1".to_string());
        eco.cities.push(city);
        eco.generate_funds(1.0);

        let funds = eco.funds.get("p1").copied().unwrap_or(0);
        assert!(funds > 0, "should have generated funds");
    }

    #[test]
    fn generate_funds_multiple_cities() {
        let mut eco = Economy::new();
        for i in 0..3 {
            let mut city = City::new(&format!("city_{}", i), (0.0, 0.0));
            city.owner = Some("p1".to_string());
            eco.cities.push(city);
        }
        eco.generate_funds(1.0);

        let funds = eco.funds.get("p1").copied().unwrap_or(0);
        assert!(funds > 0);
    }

    #[test]
    fn generate_funds_scaled_by_ticks() {
        let mut eco = Economy::new();
        let mut city = City::new("city_0", (0.0, 0.0));
        city.owner = Some("p1".to_string());
        eco.cities.push(city);

        eco.generate_funds(10.0);
        let funds_10 = eco.funds.get("p1").copied().unwrap_or(0);

        eco = Economy::new();
        let mut city2 = City::new("city_0", (0.0, 0.0));
        city2.owner = Some("p1".to_string());
        eco.cities.push(city2);
        eco.generate_funds(1.0);
        let funds_1 = eco.funds.get("p1").copied().unwrap_or(0);

        assert!(
            funds_10 >= funds_1,
            "10 ticks should generate >= 1 tick funds"
        );
    }

    #[test]
    fn recruit_unit_success() {
        let mut eco = Economy::new();
        let mut city = City::new("city_0", (5.0, 5.0));
        city.owner = Some("p1".to_string());
        eco.cities.push(city);
        eco.funds.insert("p1".to_string(), 1000);
        eco.manpower.insert("p1".to_string(), 5000);

        let ut = basic_unit_type();
        let unit = eco.recruit_unit("city_0", &ut, "p1", (5.0, 5.0));

        assert!(unit.is_some());
        let u = unit.unwrap();
        assert_eq!(u.owner, "p1");
        assert_eq!(u.position, (5.0, 5.0));
        // Check funds deducted
        assert_eq!(*eco.funds.get("p1").unwrap(), 800);
    }

    #[test]
    fn recruit_unit_fails_insufficient_funds() {
        let mut eco = Economy::new();
        let mut city = City::new("city_0", (5.0, 5.0));
        city.owner = Some("p1".to_string());
        eco.cities.push(city);
        eco.funds.insert("p1".to_string(), 50); // not enough
        eco.manpower.insert("p1".to_string(), 5000);

        let ut = basic_unit_type(); // costs 200
        let unit = eco.recruit_unit("city_0", &ut, "p1", (5.0, 5.0));

        assert!(unit.is_none());
        // Funds should be unchanged
        assert_eq!(*eco.funds.get("p1").unwrap(), 50);
    }

    #[test]
    fn recruit_unit_fails_wrong_owner() {
        let mut eco = Economy::new();
        let mut city = City::new("city_0", (5.0, 5.0));
        city.owner = Some("p1".to_string());
        eco.cities.push(city);
        eco.funds.insert("p2".to_string(), 1000);

        let ut = basic_unit_type();
        let unit = eco.recruit_unit("city_0", &ut, "p2", (5.0, 5.0));

        assert!(unit.is_none());
    }

    #[test]
    fn recruit_unit_fails_no_such_city() {
        let mut eco = Economy::new();
        eco.funds.insert("p1".to_string(), 1000);

        let ut = basic_unit_type();
        let unit = eco.recruit_unit("nonexistent", &ut, "p1", (5.0, 5.0));

        assert!(unit.is_none());
    }

    #[test]
    fn heal_unit_basic_healing() {
        let mut eco = Economy::new();
        let mut city = City::new("city_0", (0.0, 0.0));
        city.owner = Some("p1".to_string());
        eco.cities.push(city);

        // Initialize global manpower pool for the test
        eco.manpower.insert("p1".to_string(), 5000);

        let ut = basic_unit_type();
        let mut unit = Unit::new(&ut, "p1", (0.0, 0.0));
        unit.men = 500.0; // half health
        unit.max_men = 1000.0;
        unit.healing = true;
        unit.encircled = false;

        eco.heal_unit(&mut unit, "p1", "city_0", 0.01);

        assert!(
            unit.men > 500.0,
            "unit should have healed from 500 to {}",
            unit.men
        );
    }

    #[test]
    fn heal_unit_no_healing_flag() {
        let mut eco = Economy::new();
        let mut city = City::new("city_0", (0.0, 0.0));
        city.owner = Some("p1".to_string());
        eco.cities.push(city);

        eco.manpower.insert("p1".to_string(), 5000);

        let mut unit = Unit::new(&basic_unit_type(), "p1", (0.0, 0.0));
        unit.men = 500.0;
        unit.max_men = 1000.0;
        unit.healing = false;

        eco.heal_unit(&mut unit, "p1", "city_0", 0.01);

        assert_eq!(unit.men, 500.0, "should not heal when healing flag is off");
    }

    #[test]
    fn heal_unit_encircled_no_healing() {
        let mut eco = Economy::new();
        let mut city = City::new("city_0", (0.0, 0.0));
        city.owner = Some("p1".to_string());
        eco.cities.push(city);

        eco.manpower.insert("p1".to_string(), 5000);

        let mut unit = Unit::new(&basic_unit_type(), "p1", (0.0, 0.0));
        unit.men = 500.0;
        unit.max_men = 1000.0;
        unit.healing = true;
        unit.encircled = true;

        eco.heal_unit(&mut unit, "p1", "city_0", 0.01);

        assert_eq!(unit.men, 500.0, "should not heal encircled unit");
    }

    #[test]
    fn heal_unit_capped_at_max_men() {
        let mut eco = Economy::new();
        let mut city = City::new("city_0", (0.0, 0.0));
        city.owner = Some("p1".to_string());
        eco.cities.push(city);

        eco.manpower.insert("p1".to_string(), 5000);

        let mut unit = Unit::new(&basic_unit_type(), "p1", (0.0, 0.0));
        unit.men = 999.0;
        unit.max_men = 1000.0;
        unit.healing = true;

        eco.heal_unit(&mut unit, "p1", "city_0", 1.0);

        assert!(unit.men <= 1000.0, "men should not exceed max_men");
    }

    #[test]
    fn heal_unit_equipment_heals_proportionally() {
        let mut eco = Economy::new();
        let mut city = City::new("city_0", (0.0, 0.0));
        city.owner = Some("p1".to_string());
        eco.cities.push(city);

        eco.manpower.insert("p1".to_string(), 5000);

        let ut = basic_unit_type();
        let mut unit = Unit::new(&ut, "p1", (0.0, 0.0));
        unit.men = 500.0;
        unit.max_men = 1000.0;
        unit.healing = true;
        unit.equipment[0].count = 50; // damaged equipment
        unit.max_equipment[0].max_count = 100;

        let initial_eq = unit.equipment[0].count;
        eco.heal_unit(&mut unit, "p1", "city_0", 0.01);

        assert!(
            unit.equipment[0].count >= initial_eq,
            "equipment should also heal"
        );
    }

    #[test]
    fn upgrade_city_disabled() {
        let mut eco = Economy::new();
        let mut city = City::new("city_0", (0.0, 0.0));
        city.owner = Some("p1".to_string());
        eco.cities.push(city);
        eco.funds.insert("p1".to_string(), 1000);

        // Default settings have enabled = false
        let result = eco.upgrade_city("city_0", "p1");
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("not enabled"));
    }

    #[test]
    fn upgrade_city_insufficient_funds() {
        let mut eco = Economy::new();
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
        eco.city_upgrade_settings = settings;
        let mut city = City::new("city_0", (0.0, 0.0));
        city.owner = Some("p1".to_string());
        eco.cities.push(city);
        eco.funds.insert("p1".to_string(), 100); // not enough for level 2 (200)

        let result = eco.upgrade_city("city_0", "p1");
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("Insufficient funds"));
    }

    #[test]
    fn upgrade_city_success() {
        let mut eco = Economy::new();
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
        eco.city_upgrade_settings = settings;
        let mut city = City::new("city_0", (0.0, 0.0));
        city.owner = Some("p1".to_string());
        eco.cities.push(city);
        eco.funds.insert("p1".to_string(), 1000);

        let result = eco.upgrade_city("city_0", "p1");
        assert!(result.is_ok());
        assert_eq!(eco.cities[0].level, 2);
        // Cost = base_upgrade_cost * level = 200 * 2 = 400
        assert_eq!(*eco.funds.get("p1").unwrap(), 600); // 1000 - 400
    }

    #[test]
    fn upgrade_city_max_level() {
        let mut eco = Economy::new();
        let settings = CityUpgradeSettings {
            enabled: true,
            max_level: 1,
            base_upgrade_cost: 200,
            income_growth_per_level: 0.5,
            base_supply_range: 5.0,
            supply_range_growth: 3.0,
            secondary_city_decay: 0.5,
            manpower_regen_per_tick: 10.0,
        };
        eco.city_upgrade_settings = settings;
        let mut city = City::new("city_0", (0.0, 0.0));
        city.owner = Some("p1".to_string());
        eco.cities.push(city);
        eco.funds.insert("p1".to_string(), 1000);

        let result = eco.upgrade_city("city_0", "p1");
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("maximum level"));
    }
}
