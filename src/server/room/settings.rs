//! Lobby custom-ruleset helpers: config -> engine unit type conversion,
//! default unit types, and client-controlled lobby-settings validation.

use crate::config::{EquipmentConfig, UnitTypeConfig as ConfigUnitType};
use crate::game::economy::UnitType;
use crate::game::unit::Equipment;
use crate::server::protocol::{CustomUnitType, LobbySettings};

/// Convert standard ruleset config unit types to game unit types
pub(crate) fn convert_unit_types(unit_types: &[ConfigUnitType]) -> Vec<UnitType> {
    unit_types
        .iter()
        .map(|ut| {
            let equipment: Vec<Equipment> = ut
                .equipment
                .iter()
                .map(|s| {
                    // Parse "name:count:cost_per_item" format
                    let ecfg = EquipmentConfig::from_str(s);
                    Equipment {
                        name: ecfg.name,
                        count: ecfg.count,
                        max_count: ecfg.count,
                        cost_per_item: ecfg.cost_per_item,
                    }
                })
                .collect();
            // Derive recruitment cost from equipment if not specified
            let recruitment_cost = ut.recruitment_cost.unwrap_or_else(|| {
                // Base manpower cost (1 fund per 10 men) + equipment costs
                let base_manpower_cost = ut.men / 10;
                let equipment_cost: u32 = ut
                    .equipment
                    .iter()
                    .map(|s| {
                        let ecfg = EquipmentConfig::from_str(s);
                        (ecfg.count as f32 * ecfg.cost_per_item).ceil() as u32
                    })
                    .sum();
                base_manpower_cost + equipment_cost
            });
            UnitType {
                name: ut.name.clone(),
                symbol: ut.symbol.clone(),
                men: ut.men,
                equipment,
                soft_attack: ut.soft_attack,
                soft_defense: ut.soft_defense,
                hard_attack: ut.hard_attack,
                hard_defense: ut.hard_defense,
                speed: ut.speed,
                max_organisation: ut.max_organisation,
                reinforcement_rate: ut.reinforcement_rate,
                base_attrition: ut.base_attrition,
                fighting_attrition: ut.fighting_attrition,
                recruitment_cost,
                supply_buffer_percent: ut.supply_buffer_percent,
                supply_rate: 5,
                max_supply: crate::config::derive_max_supply(ut.men),
            }
        })
        .collect()
}

/// Convert custom unit types from lobby settings to game unit types
pub(crate) fn convert_custom_unit_types(unit_types: &[CustomUnitType]) -> Vec<UnitType> {
    unit_types
        .iter()
        .map(|ut| UnitType {
            name: ut.name.clone(),
            symbol: ut.symbol.clone(),
            men: ut.men,
            equipment: ut
                .equipment
                .iter()
                .map(|(name, count)| Equipment {
                    name: name.clone(),
                    count: *count,
                    max_count: *count,
                    cost_per_item: 0.0, // Custom types don't have per-item cost
                })
                .collect(),
            soft_attack: ut.soft_attack,
            soft_defense: ut.soft_defense,
            hard_attack: ut.hard_attack,
            hard_defense: ut.hard_defense,
            speed: ut.speed,
            max_organisation: ut.max_organisation,
            reinforcement_rate: ut.reinforcement_rate,
            base_attrition: 0.5,
            fighting_attrition: 1.0,
            recruitment_cost: ut.recruitment_cost,
            supply_buffer_percent: 10, // Default
            supply_rate: 5,
            max_supply: ut.men * 2, // men * CARRY_WEIGHT
        })
        .collect()
}

pub(crate) fn build_default_unit_types() -> Vec<UnitType> {
    crate::game::economy::default_unit_types()
}

/// G-3: Validate custom ruleset settings to prevent unreasonable values.
/// Returns an error message if validation fails, None if valid.
pub(crate) fn validate_custom_settings(settings: &LobbySettings) -> Option<String> {
    if settings.starting_funds == 0 {
        return Some("starting_funds must be > 0".into());
    }
    if settings.starting_funds > 100_000 {
        return Some("starting_funds must be <= 100000".into());
    }
    if settings.funds_per_city < 0.0 || settings.funds_per_city > 10000.0 {
        return Some("funds_per_city must be in range 0-10000".into());
    }
    if settings.game_speed < 0.1 || settings.game_speed > 10.0 {
        return Some("game_speed must be in range 0.1-10.0".into());
    }
    if settings.max_players == 0 || settings.max_players > 16 {
        return Some("max_players must be in range 1-16".into());
    }
    if settings.recruitment_rate < 0.0 || settings.recruitment_rate > 100.0 {
        return Some("recruitment_rate must be in range 0-100".into());
    }
    // NaN fails both range comparisons, so finiteness is checked explicitly.
    if !settings.water_speed.is_finite()
        || settings.water_speed < 0.1
        || settings.water_speed > 10.0
    {
        return Some("water_speed must be finite and in range 0.1-10.0".into());
    }
    // Validate custom unit types (fully client-controlled). Degenerate types
    // are game-breaking or DoS vectors: speed 0 yields 0/0 = NaN on the first
    // movement tick, and men 0 / recruitment_cost 0 yields an unbounded free
    // army. `men` is capped because it is the army-size field.
    for (i, ut) in settings.custom_unit_types.iter().enumerate() {
        if ut.men == 0 {
            return Some(format!(
                "custom unit type {i} ({}): men must be >= 1",
                ut.name
            ));
        }
        if ut.men > 100_000 {
            return Some(format!(
                "custom unit type {i} ({}): men must be <= 100000",
                ut.name
            ));
        }
        if ut.recruitment_cost == 0 {
            return Some(format!(
                "custom unit type {i} ({}): recruitment_cost must be > 0",
                ut.name
            ));
        }
        if !ut.speed.is_finite() || ut.speed <= 0.0 {
            return Some(format!(
                "custom unit type {i} ({}): speed must be finite and > 0",
                ut.name
            ));
        }
        if ut.speed > 100.0 {
            return Some(format!(
                "custom unit type {i} ({}): speed must be <= 100",
                ut.name
            ));
        }
        // Item 7b: finiteness + sane bounds for the remaining client floats.
        // NaN/inf here would poison combat resolution and organisation/regen
        // math for the entire game (a single NaN unit corrupts every future
        // battle it fights). Negative combat stats and a non-positive
        // max_organisation are degenerate; the 1e6 cap keeps squared combat
        // products far below f32 overflow.
        for (label, value) in [
            ("soft_attack", ut.soft_attack),
            ("soft_defense", ut.soft_defense),
            ("hard_attack", ut.hard_attack),
            ("hard_defense", ut.hard_defense),
        ] {
            if !value.is_finite() || !(0.0..=1_000_000.0).contains(&value) {
                return Some(format!(
                    "custom unit type {i} ({}): {label} must be finite and in range 0-1000000",
                    ut.name
                ));
            }
        }
        if !ut.max_organisation.is_finite()
            || ut.max_organisation <= 0.0
            || ut.max_organisation > 1_000_000.0
        {
            return Some(format!(
                "custom unit type {i} ({}): max_organisation must be finite and in range (0, 1000000]",
                ut.name
            ));
        }
        if !ut.reinforcement_rate.is_finite()
            || ut.reinforcement_rate < 0.0
            || ut.reinforcement_rate > 1_000_000.0
        {
            return Some(format!(
                "custom unit type {i} ({}): reinforcement_rate must be finite and in range 0-1000000",
                ut.name
            ));
        }
    }
    // Validate city upgrade settings
    let cs = &settings.city_upgrade_settings;
    if cs.max_level > 50 {
        return Some("city max_level must be <= 50".into());
    }
    if cs.base_upgrade_cost > 100_000 {
        return Some("base_upgrade_cost must be <= 100000".into());
    }
    None
}

#[cfg(test)]
mod validate_custom_settings_tests {
    use super::validate_custom_settings;
    use crate::config::CityUpgradeSettings;
    use crate::server::protocol::{CustomUnitType, LobbySettings};

    /// Baseline custom ruleset that passes every lobby-level check, so each
    /// test can isolate one unit-type (or water_speed) violation.
    fn base_settings() -> LobbySettings {
        // The type default (max_level 100) would itself trip the <= 50 rule,
        // so override it inline.
        let city = CityUpgradeSettings {
            max_level: 10,
            ..CityUpgradeSettings::default()
        };
        LobbySettings {
            map: "default".to_string(),
            starting_funds: 1000,
            initial_manpower: 10_000,
            funds_per_city: 1.0,
            game_speed: 1.0,
            use_standard_ruleset: false,
            llm_thinking_delay: 3.0,
            max_players: 4,
            custom_unit_types: vec![],
            spectator_vision: false,
            recruitment_rate: 10.0,
            water_speed: 1.2,
            city_upgrade_settings: city,
        }
    }

    fn unit_type(speed: f32, men: u32, recruitment_cost: u32) -> CustomUnitType {
        CustomUnitType {
            name: "infantry".to_string(),
            symbol: "I".to_string(),
            men,
            equipment: vec![],
            soft_attack: 5.0,
            soft_defense: 5.0,
            hard_attack: 3.0,
            hard_defense: 2.0,
            speed,
            max_organisation: 100.0,
            reinforcement_rate: 1.0,
            recruitment_cost,
        }
    }

    #[test]
    fn valid_custom_unit_type_passes() {
        let mut s = base_settings();
        s.custom_unit_types = vec![unit_type(1.0, 1000, 20)];
        assert_eq!(validate_custom_settings(&s), None);
    }

    #[test]
    fn zero_or_non_finite_speed_is_rejected() {
        for speed in [0.0, f32::NAN, f32::INFINITY] {
            let mut s = base_settings();
            s.custom_unit_types = vec![unit_type(speed, 1000, 20)];
            let err = validate_custom_settings(&s)
                .unwrap_or_else(|| panic!("speed {speed} must be rejected"));
            assert!(err.contains("speed"), "unexpected error: {err}");
        }
    }

    #[test]
    fn zero_men_is_rejected() {
        let mut s = base_settings();
        s.custom_unit_types = vec![unit_type(1.0, 0, 20)];
        let err = validate_custom_settings(&s).expect("men 0 must be rejected");
        assert!(err.contains("men"), "unexpected error: {err}");
    }

    #[test]
    fn oversized_men_is_rejected() {
        let mut s = base_settings();
        s.custom_unit_types = vec![unit_type(1.0, 100_001, 20)];
        let err = validate_custom_settings(&s).expect("men > 100000 must be rejected");
        assert!(err.contains("men"), "unexpected error: {err}");
    }

    #[test]
    fn free_recruitment_is_rejected() {
        // The reported DoS: recruitment_cost 0 + men 0 (unbounded free army).
        // Both degenerate fields are rejected individually.
        let mut s = base_settings();
        s.custom_unit_types = vec![unit_type(1.0, 0, 0)];
        let err = validate_custom_settings(&s).expect("men 0 + cost 0 must be rejected");
        assert!(err.contains("men"), "unexpected error: {err}");

        s.custom_unit_types = vec![unit_type(1.0, 1000, 0)];
        let err = validate_custom_settings(&s).expect("cost 0 with men > 0 must be rejected");
        assert!(err.contains("recruitment_cost"), "unexpected error: {err}");
    }

    #[test]
    fn nan_water_speed_is_rejected() {
        let mut s = base_settings();
        s.water_speed = f32::NAN; // NaN passes a bare range check
        let err = validate_custom_settings(&s).expect("NaN water_speed must be rejected");
        assert!(err.contains("water_speed"), "unexpected error: {err}");
    }

    // ------------------------------------------------------------------
    // Item 7b: finiteness/bounds for the combat + regen floats
    // ------------------------------------------------------------------

    fn unit_type_f(
        soft_attack: f32,
        soft_defense: f32,
        hard_attack: f32,
        hard_defense: f32,
        max_organisation: f32,
        reinforcement_rate: f32,
    ) -> CustomUnitType {
        CustomUnitType {
            name: "infantry".to_string(),
            symbol: "I".to_string(),
            men: 1000,
            equipment: vec![],
            soft_attack,
            soft_defense,
            hard_attack,
            hard_defense,
            speed: 1.0,
            max_organisation,
            reinforcement_rate,
            recruitment_cost: 20,
        }
    }

    #[test]
    fn nan_combat_stats_are_rejected() {
        // A single NaN unit would poison every future battle it fights.
        for (label, ut) in [
            (
                "soft_attack",
                unit_type_f(f32::NAN, 5.0, 3.0, 2.0, 100.0, 1.0),
            ),
            (
                "soft_defense",
                unit_type_f(5.0, f32::NAN, 3.0, 2.0, 100.0, 1.0),
            ),
            (
                "hard_attack",
                unit_type_f(5.0, 5.0, f32::NAN, 2.0, 100.0, 1.0),
            ),
            (
                "hard_defense",
                unit_type_f(5.0, 5.0, 3.0, f32::NAN, 100.0, 1.0),
            ),
        ] {
            let mut s = base_settings();
            s.custom_unit_types = vec![ut];
            let Some(err) = validate_custom_settings(&s) else {
                panic!("NaN {label} must be rejected");
            };
            assert!(err.contains(label), "unexpected error for {label}: {err}");
        }
    }

    #[test]
    fn inf_combat_stats_are_rejected() {
        for (label, ut) in [
            (
                "soft_attack",
                unit_type_f(f32::INFINITY, 5.0, 3.0, 2.0, 100.0, 1.0),
            ),
            (
                "hard_defense",
                unit_type_f(5.0, 5.0, 3.0, f32::INFINITY, 100.0, 1.0),
            ),
        ] {
            let mut s = base_settings();
            s.custom_unit_types = vec![ut];
            let Some(err) = validate_custom_settings(&s) else {
                panic!("inf {label} must be rejected");
            };
            assert!(err.contains(label), "unexpected error for {label}: {err}");
        }
    }

    #[test]
    fn negative_combat_stats_are_rejected() {
        for (label, ut) in [
            ("soft_attack", unit_type_f(-1.0, 5.0, 3.0, 2.0, 100.0, 1.0)),
            ("soft_defense", unit_type_f(5.0, -1.0, 3.0, 2.0, 100.0, 1.0)),
            ("hard_attack", unit_type_f(5.0, 5.0, -1.0, 2.0, 100.0, 1.0)),
            ("hard_defense", unit_type_f(5.0, 5.0, 3.0, -1.0, 100.0, 1.0)),
        ] {
            let mut s = base_settings();
            s.custom_unit_types = vec![ut];
            let Some(err) = validate_custom_settings(&s) else {
                panic!("negative {label} must be rejected");
            };
            assert!(err.contains(label), "unexpected error for {label}: {err}");
        }
    }

    #[test]
    fn degenerate_organisation_and_regen_are_rejected() {
        // max_organisation <= 0: a unit with no organisation bar (broken HP).
        // NaN/inf regen: organisation math corruption / unbounded healing.
        let cases = [
            (
                "max_organisation zero",
                unit_type_f(5.0, 5.0, 3.0, 2.0, 0.0, 1.0),
            ),
            (
                "max_organisation NaN",
                unit_type_f(5.0, 5.0, 3.0, 2.0, f32::NAN, 1.0),
            ),
            (
                "reinforcement_rate negative",
                unit_type_f(5.0, 5.0, 3.0, 2.0, 100.0, -1.0),
            ),
            (
                "reinforcement_rate NaN",
                unit_type_f(5.0, 5.0, 3.0, 2.0, 100.0, f32::NAN),
            ),
            (
                "soft_attack over cap",
                unit_type_f(1_000_001.0, 5.0, 3.0, 2.0, 100.0, 1.0),
            ),
        ];
        for (label, ut) in cases {
            let mut s = base_settings();
            s.custom_unit_types = vec![ut];
            assert!(
                validate_custom_settings(&s).is_some(),
                "{label} must be rejected"
            );
        }
    }

    #[test]
    fn finite_bounded_combat_stats_pass() {
        // Boundary values the validator must ACCEPT (no over-rejection):
        // zero is legal for combat stats and regen, the 1e6 caps are
        // inclusive.
        let mut s = base_settings();
        s.custom_unit_types = vec![unit_type_f(
            0.0,         // soft_attack
            0.0,         // soft_defense
            0.0,         // hard_attack
            0.0,         // hard_defense
            1_000_000.0, // max_organisation (inclusive upper bound)
            0.0,         // reinforcement_rate
        )];
        assert_eq!(validate_custom_settings(&s), None);
    }

    // ------------------------------------------------------------------
    // Item 7a: the validator is flag-agnostic — custom types are validated
    // when PRESENT, even with use_standard_ruleset = true. The StartGame
    // call site relies on exactly this: it invokes validation whenever
    // custom_unit_types is non-empty, regardless of the flag.
    // ------------------------------------------------------------------

    #[test]
    fn custom_types_validated_even_with_standard_flag() {
        let mut s = base_settings();
        s.use_standard_ruleset = true; // the bypass the call site now closes
        s.custom_unit_types = vec![unit_type(0.0, 1000, 20)]; // speed 0 = NaN on move
        let err = validate_custom_settings(&s)
            .expect("custom types must be validated even with the standard flag");
        assert!(err.contains("speed"), "unexpected error: {err}");

        // And a well-formed custom type with the flag set still passes.
        let mut s = base_settings();
        s.use_standard_ruleset = true;
        s.custom_unit_types = vec![unit_type(1.0, 1000, 20)];
        assert_eq!(validate_custom_settings(&s), None);
    }
}
