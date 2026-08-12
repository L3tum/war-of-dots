//! Server configuration

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

#[derive(Debug, Deserialize, Clone)]
pub struct Config {
    pub llm: LlmConfig,
    pub standard_ruleset: StandardRuleset,
    pub storage: StorageConfig,
}

#[derive(Debug, Deserialize, Clone)]
pub struct LlmConfig {
    /// LLM providers available
    #[serde(default)]
    pub providers: Vec<LlmProvider>,
    /// Default advisor provider (name from providers list)
    #[serde(default)]
    pub advisor_provider: Option<String>,
}

#[derive(Debug, Deserialize, Clone)]
pub struct LlmProvider {
    pub name: String,
    pub provider: String, // "openai", "anthropic", "ollama"
    pub model: String,
    pub api_key: Option<String>,
    pub base_url: Option<String>,
}

/// City upgrade and supply system settings.
/// Shared between game logic and the wire protocol (see protocol.rs re-export).
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct CityUpgradeSettings {
    /// Whether city upgrades and supply system are enabled
    #[serde(default = "default_city_upgrades_enabled")]
    pub enabled: bool,
    /// Maximum city level (default: 100)
    #[serde(default = "default_max_level")]
    pub max_level: u32,

    // --- Upgrade cost formula: cost = base_cost * level ---
    /// Base cost for upgrades; actual cost = base_cost * level
    #[serde(default = "default_base_upgrade_cost")]
    pub base_upgrade_cost: u32,

    // --- Income formula: multiplier = 1.0 + (level - 1) * income_growth_per_level ---
    /// Additional income multiplier per level above 1
    #[serde(default = "default_income_growth_per_level")]
    pub income_growth_per_level: f32,

    // --- Range formula: range = base_range + (level - 1) * range_growth_per_level ---
    /// Base supply range in grid cells at level 1
    #[serde(default = "default_base_supply_range")]
    pub base_supply_range: f32,
    /// Additional range per level above 1
    #[serde(default = "default_supply_range_growth")]
    pub supply_range_growth: f32,

    // --- Multi-city supply decay ---
    /// Decay multiplier for non-closest city contributions (0.0 = ignored, 1.0 = equal)
    /// When a unit is within range of multiple friendly cities, the secondary city
    /// contribution = supply_rate * secondary_city_decay
    #[serde(default = "default_secondary_city_decay")]
    pub secondary_city_decay: f32,

    /// Manpower generated per tick per city level.
    /// Added to global player manpower pool each economy tick.
    #[serde(default = "default_manpower_regen_per_tick")]
    pub manpower_regen_per_tick: f32,
}

impl CityUpgradeSettings {
    /// Returns the upgrade cost to reach `level` (1-indexed, level 1 is free)
    /// Linear scaling: cost = base_cost * level
    pub fn upgrade_cost(&self, level: u32) -> u32 {
        if level <= 1 {
            return 0;
        }
        self.base_upgrade_cost * level
    }

    /// Returns the income multiplier for a city at `level` (1-indexed)
    pub fn income_multiplier(&self, level: u32) -> f32 {
        1.0 + ((level.saturating_sub(1)) as f32) * self.income_growth_per_level
    }

    /// Returns the supply range for a city at `level` (1-indexed)
    pub fn supply_range_for(&self, level: u32) -> f32 {
        self.base_supply_range + ((level.saturating_sub(1)) as f32) * self.supply_range_growth
    }
}

fn default_city_upgrades_enabled() -> bool {
    false
}
fn default_max_level() -> u32 {
    100
}
fn default_base_upgrade_cost() -> u32 {
    200
}
fn default_income_growth_per_level() -> f32 {
    0.5
}
fn default_base_supply_range() -> f32 {
    5.0
}
fn default_supply_range_growth() -> f32 {
    3.0
}

fn default_secondary_city_decay() -> f32 {
    0.5
}
fn default_manpower_regen_per_tick() -> f32 {
    10.0
}

#[derive(Debug, Deserialize, Clone)]
pub struct StandardRuleset {
    pub unit_types: Vec<UnitTypeConfig>,
    pub enabled: bool,
    #[serde(default)]
    pub city_upgrades: Option<CityUpgradeSettings>,

    // --- Game-level defaults for standard ruleset ---
    /// Starting funds for each player (default: 2000, ~20 light-infantry units)
    #[serde(default = "default_starting_funds")]
    pub starting_funds: u32,
    /// Starting manpower pool for each player (default: 2000, ~20 light-infantry units).
    /// Manpower is always used when recruiting — it is not gated behind city upgrades.
    #[serde(default = "default_initial_manpower")]
    pub initial_manpower: u32,
    /// Funds generated per city per economy tick (default: 1.0)
    #[serde(default = "default_funds_per_city")]
    pub funds_per_city: f32,
    /// Game tick speed in ticks per second (default: 60)
    #[serde(default = "default_game_speed")]
    pub game_speed: f32,
    /// Ticks between recruitment actions, 0 = unlimited (default: 10)
    #[serde(default = "default_recruitment_rate")]
    pub recruitment_rate: f32,
    /// Movement speed multiplier for water tiles (default: 1.2)
    #[serde(default = "default_water_speed")]
    pub water_speed: f32,
}

fn default_starting_funds() -> u32 {
    2000
}
fn default_initial_manpower() -> u32 {
    2000
}
fn default_funds_per_city() -> f32 {
    20.0
}
fn default_game_speed() -> f32 {
    60.0
}
fn default_recruitment_rate() -> f32 {
    10.0
}
fn default_water_speed() -> f32 {
    1.2
}

#[derive(Debug, Deserialize, Clone)]
pub struct UnitTypeConfig {
    pub name: String,
    pub symbol: String,
    pub men: u32,
    /// Equipment definitions in format "name:count:cost_per_item" (e.g., "rifles:100:0.5")
    /// Legacy format "name:count" is also supported (cost defaults to 0.0)
    pub equipment: Vec<String>,
    pub soft_attack: f32,
    pub soft_defense: f32,
    pub hard_attack: f32,
    pub hard_defense: f32,
    pub speed: f32,
    pub max_organisation: f32,
    pub reinforcement_rate: f32,
    pub base_attrition: f32,
    pub fighting_attrition: f32,
    /// Recruitment cost in funds. If omitted, derived from equipment costs + base manpower.
    pub recruitment_cost: Option<u32>,
    /// Percentage of extra equipment carried above "needed" baseline (default: 10)
    #[serde(default = "default_supply_buffer_percent")]
    pub supply_buffer_percent: u32,
}

/// Derive max_supply from manpower.
/// Formula: men * CARRY_WEIGHT (kg per soldier)
/// Equipment is tracked separately as "needed" supply (max_equipment).
/// Buffer percentage is for embarked units only (boat_supply).
pub fn derive_max_supply(men: u32) -> u32 {
    (men as f32 * crate::game::constants::CARRY_WEIGHT) as u32
}

fn default_supply_buffer_percent() -> u32 {
    10
}

/// Parsed equipment entry with cost
#[derive(Debug, Clone)]
pub struct EquipmentConfig {
    pub name: String,
    pub count: u32,
    pub cost_per_item: f32,
}

impl EquipmentConfig {
    /// Parse "name:count:cost_per_item" or legacy "name:count" format
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
            .unwrap_or(0.0); // Legacy format: no cost specified
        EquipmentConfig {
            name,
            count,
            cost_per_item,
        }
    }
}

#[derive(Debug, Deserialize, Clone)]
pub struct StorageConfig {
    pub database_path: String,
    pub replay_path: String,
}

pub fn load(path: &str) -> Result<Config, crate::errors::ConfigError> {
    let content =
        std::fs::read_to_string(path).map_err(|_| crate::errors::ConfigError::NotFound {
            path: path.to_string(),
        })?;
    let config: Config =
        toml::from_str(&content).map_err(|source| crate::errors::ConfigError::Parse {
            source: Box::new(source),
        })?;
    Ok(config.resolve())
}

/// Expands environment variable references in a string.
/// Replaces ${VAR_NAME} with the value of the environment variable.
/// If the variable is not set, leaves the placeholder unchanged.
fn expand_env_vars(s: &str) -> String {
    let mut result = String::new();
    let mut remaining = s;
    while let Some(idx) = remaining.find("${") {
        if let Some(end) = remaining[idx..].find('}') {
            let full_match_start = idx;
            // end is relative to idx; var_name is between ${ and }
            let var_name = &remaining[idx + 2..idx + end];
            let rest = &remaining[idx + end + 1..];
            result.push_str(&remaining[..full_match_start]);
            result.push_str(&std::env::var(var_name).unwrap_or_else(|_| {
                tracing::warn!(
                    "Environment variable '{}' not set, leaving placeholder",
                    var_name
                );
                format!("${{{}}}", var_name)
            }));
            remaining = rest;
        } else {
            // Unmatched ${, just include as-is
            result.push_str(&remaining[..idx + 2]);
            remaining = &remaining[idx + 2..];
        }
    }
    result.push_str(remaining);
    result
}

impl Config {
    /// Resolves environment variable references in all string fields.
    pub fn resolve(mut self) -> Self {
        self.storage.database_path = expand_env_vars(&self.storage.database_path);
        self.storage.replay_path = expand_env_vars(&self.storage.replay_path);

        self.llm.advisor_provider = self
            .llm
            .advisor_provider
            .as_ref()
            .map(|s| expand_env_vars(s));

        for provider in &mut self.llm.providers {
            provider.name = expand_env_vars(&provider.name);
            provider.provider = expand_env_vars(&provider.provider);
            provider.model = expand_env_vars(&provider.model);
            provider.api_key = provider.api_key.as_ref().map(|s| expand_env_vars(s));
            provider.base_url = provider.base_url.as_ref().map(|s| expand_env_vars(s));
        }

        for ut in &mut self.standard_ruleset.unit_types {
            ut.name = expand_env_vars(&ut.name);
            ut.symbol = expand_env_vars(&ut.symbol);
            ut.equipment = ut.equipment.iter().map(|e| expand_env_vars(e)).collect();
        }

        self
    }

    /// Returns the city upgrade settings, or defaults if not configured.
    pub fn city_upgrade_settings(&self) -> CityUpgradeSettings {
        self.standard_ruleset
            .city_upgrades
            .clone()
            .unwrap_or_default()
    }
}

impl Default for CityUpgradeSettings {
    fn default() -> Self {
        CityUpgradeSettings {
            enabled: false,
            max_level: 100,
            base_upgrade_cost: 200,
            income_growth_per_level: 0.5,
            base_supply_range: 5.0,
            supply_range_growth: 3.0,
            secondary_city_decay: 0.5,
            manpower_regen_per_tick: 10.0,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // ---------------------------------------------------------------------------
    // CityUpgradeSettings
    // ---------------------------------------------------------------------------

    fn settings() -> CityUpgradeSettings {
        CityUpgradeSettings {
            enabled: true,
            max_level: 10,
            base_upgrade_cost: 200,
            income_growth_per_level: 0.5,
            base_supply_range: 5.0,
            supply_range_growth: 3.0,
            secondary_city_decay: 0.5,
            manpower_regen_per_tick: 10.0,
        }
    }

    #[test]
    fn upgrade_cost_level_1_is_zero() {
        let s = settings();
        assert_eq!(s.upgrade_cost(1), 0);
    }

    #[test]
    fn upgrade_cost_level_0_is_zero() {
        let s = settings();
        assert_eq!(s.upgrade_cost(0), 0);
    }

    #[test]
    fn upgrade_cost_level_2() {
        let s = settings();
        // base_cost * 2 = 400
        assert_eq!(s.upgrade_cost(2), 400);
    }

    #[test]
    fn upgrade_cost_level_5() {
        let s = settings();
        // base_cost * 5 = 1000
        assert_eq!(s.upgrade_cost(5), 1000);
    }

    #[test]
    fn income_multiplier_level_1() {
        let s = settings();
        // 1.0 + 0 * 0.5 = 1.0
        assert_eq!(s.income_multiplier(1), 1.0);
    }

    #[test]
    fn income_multiplier_level_3() {
        let s = settings();
        // 1.0 + 2 * 0.5 = 2.0
        assert_eq!(s.income_multiplier(3), 2.0);
    }

    #[test]
    fn income_multiplier_level_10() {
        let s = settings();
        // 1.0 + 9 * 0.5 = 5.5
        assert_eq!(s.income_multiplier(10), 5.5);
    }

    #[test]
    fn supply_range_for_level_1() {
        let s = settings();
        // 5.0 + 0 * 3 = 5.0
        assert_eq!(s.supply_range_for(1), 5.0);
    }

    #[test]
    fn supply_range_for_level_3() {
        let s = settings();
        // 5.0 + 2 * 3 = 11.0
        assert_eq!(s.supply_range_for(3), 11.0);
    }

    #[test]
    fn city_upgrade_settings_default() {
        let s = CityUpgradeSettings::default();
        assert!(!s.enabled);
        assert_eq!(s.max_level, 100);
        assert_eq!(s.base_upgrade_cost, 200);
        assert_eq!(s.income_growth_per_level, 0.5);
        assert_eq!(s.base_supply_range, 5.0);
        assert_eq!(s.supply_range_growth, 3.0);
    }

    // ---------------------------------------------------------------------------
    // EquipmentConfig
    // ---------------------------------------------------------------------------

    #[test]
    fn equipment_config_full_format() {
        let eq = EquipmentConfig::from_str("rifle:100:2.5");
        assert_eq!(eq.name, "rifle");
        assert_eq!(eq.count, 100);
        assert_eq!(eq.cost_per_item, 2.5);
    }

    #[test]
    fn equipment_config_legacy_no_cost() {
        let eq = EquipmentConfig::from_str("rifle:50");
        assert_eq!(eq.name, "rifle");
        assert_eq!(eq.count, 50);
        assert_eq!(eq.cost_per_item, 0.0);
    }

    #[test]
    fn equipment_config_name_only() {
        let eq = EquipmentConfig::from_str("ammo");
        assert_eq!(eq.name, "ammo");
        assert_eq!(eq.count, 0);
        assert_eq!(eq.cost_per_item, 0.0);
    }

    #[test]
    fn equipment_config_invalid_count() {
        let eq = EquipmentConfig::from_str("rifle:abc:2.5");
        assert_eq!(eq.count, 0);
        assert_eq!(eq.cost_per_item, 2.5);
    }

    // ---------------------------------------------------------------------------
    // expand_env_vars
    // ---------------------------------------------------------------------------

    #[test]
    fn expand_env_vars_no_vars() {
        assert_eq!(expand_env_vars("hello world"), "hello world");
    }

    #[test]
    fn expand_env_vars_set_variable() {
        std::env::set_var("WOD_TEST_VAR_1", "expanded_value");
        assert_eq!(
            expand_env_vars("prefix_${WOD_TEST_VAR_1}_suffix"),
            "prefix_expanded_value_suffix"
        );
    }

    #[test]
    fn expand_env_vars_unset_variable_keeps_placeholder() {
        std::env::remove_var("WOD_NONEXISTENT_VAR_XYZ");
        let result = expand_env_vars("before_${WOD_NONEXISTENT_VAR_XYZ}_after");
        // Unmatched var stays as placeholder
        assert_eq!(result, "before_${WOD_NONEXISTENT_VAR_XYZ}_after");
    }

    #[test]
    fn expand_env_vars_multiple_vars() {
        std::env::set_var("WOD_VAR_A", "A");
        std::env::set_var("WOD_VAR_B", "B");
        assert_eq!(expand_env_vars("${WOD_VAR_A}-${WOD_VAR_B}"), "A-B");
    }

    #[test]
    fn expand_env_vars_unmatched_brace() {
        let result = expand_env_vars("hello ${world");
        assert_eq!(result, "hello ${world");
    }

    #[test]
    fn expand_env_vars_empty_string() {
        assert_eq!(expand_env_vars(""), "");
    }
}
