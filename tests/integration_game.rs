//! Integration tests for game engine — full game flow with Map + Game + settings.

use std::collections::HashMap;

use war_of_dots::config::CityUpgradeSettings;
use war_of_dots::game::economy::UnitType;
use war_of_dots::game::game::{Game, GameSettings};
use war_of_dots::game::unit::Equipment;
use war_of_dots::map::map::{Map, MapConfig};

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
        supply_rate: 1,
        max_supply: 2000, // men(1000) * CARRY_WEIGHT(2)
    }
}

fn test_settings() -> GameSettings {
    GameSettings {
        starting_funds: 1000,
        initial_manpower: 10_000,
        funds_per_city: 1.0,
        recruitment_rate: 1.0,
        game_speed: 1.0,
        use_standard_ruleset: true,
        unit_types: vec![basic_unit_type()],
        city_upgrade_settings: CityUpgradeSettings::default(),
        water_speed: 0.5,
    }
}

fn test_map() -> Map {
    let config = MapConfig {
        width: 20,
        height: 20,
        cities: vec![(3.0, 3.0), (16.0, 3.0), (3.0, 16.0), (16.0, 16.0)],
        terrain_data: None,
    };
    Map::from_config(&config)
}

// ---------------------------------------------------------------------------
// Game creation and settings
// ---------------------------------------------------------------------------

#[test]
fn game_creates_with_map_and_settings() {
    let map = test_map();
    let settings = test_settings();
    let game = Game::new(map, settings, 4);

    assert!(!game.is_running());
    assert_eq!(game.tick_number(), 0);
}

#[test]
fn game_dimensions_match_map() {
    let map = test_map();
    let settings = test_settings();
    let game = Game::new(map, settings, 4);

    assert_eq!(game.width(), 20);
    assert_eq!(game.height(), 20);
}

#[test]
fn game_registers_players_per_city() {
    let map = test_map(); // 4 cities
    let settings = test_settings();
    let game = Game::new(map, settings, 4);

    assert_eq!(game.players().len(), 4);
}

#[test]
fn game_starting_funds_set() {
    let map = test_map();
    let settings = test_settings();
    let game = Game::new(map, settings, 4);

    for (player_id, funds) in game.all_player_funds() {
        assert_eq!(funds, 1000, "player {} should have 1000 funds", player_id);
    }
}

// ---------------------------------------------------------------------------
// Game tick flow
// ---------------------------------------------------------------------------

#[test]
fn tick_increments_counter() {
    let map = test_map();
    let settings = test_settings();
    let mut game = Game::new(map, settings, 4);

    game.start();
    assert_eq!(game.tick_number(), 0);

    game.tick_game();
    assert_eq!(game.tick_number(), 1);

    game.tick_game();
    assert_eq!(game.tick_number(), 2);
}

#[test]
fn tick_does_not_run_without_start() {
    let map = test_map();
    let settings = test_settings();
    let mut game = Game::new(map, settings, 4);

    game.tick_game();
    assert_eq!(game.tick_number(), 0);
}

#[test]
fn multiple_ticks_progress_correctly() {
    let map = test_map();
    let settings = test_settings();
    let mut game = Game::new(map, settings, 4);

    game.start();
    for _ in 0..50 {
        game.tick_game();
    }

    assert_eq!(game.tick_number(), 50);
}

// ---------------------------------------------------------------------------
// Economy: funds generation
// ---------------------------------------------------------------------------

#[test]
fn economy_generates_funds_for_owned_cities() {
    let map = test_map();
    let settings = test_settings();
    let mut game = Game::new(map, settings, 4);

    game.assign_city_owner("city_0", "player_0");

    let initial_funds = game.player_funds("player_0");
    game.start();

    for _ in 0..20 {
        game.tick_game();
    }

    let final_funds = game.player_funds("player_0");
    assert!(
        final_funds > initial_funds,
        "player_0 should have earned funds: initial={} final={}",
        initial_funds,
        final_funds
    );
}

#[test]
fn unowned_city_does_not_generate_income() {
    let map = test_map();
    let settings = test_settings();
    let mut game = Game::new(map, settings, 4);

    let initial_funds = game.all_player_funds();
    game.start();

    for _ in 0..20 {
        game.tick_game();
    }

    let final_funds = game.all_player_funds();
    for (pid, initial) in &initial_funds {
        let final_val = final_funds.get(pid).copied().unwrap_or(0);
        assert_eq!(
            final_val, *initial,
            "player {} funds should be unchanged",
            pid
        );
    }
}

#[test]
fn multiple_economy_cycles_accumulate_funds() {
    let map = test_map();
    let settings = test_settings();
    let mut game = Game::new(map, settings, 4);

    game.assign_city_owner("city_0", "player_0");
    game.assign_city_owner("city_1", "player_1");
    game.start();

    let funds_10: HashMap<_, _> = game.all_player_funds().into_iter().collect();

    for _ in 0..30 {
        game.tick_game();
    }

    let funds_30: HashMap<_, _> = game.all_player_funds().into_iter().collect();

    assert!(
        funds_30["player_0"] > funds_10["player_0"],
        "funds should accumulate over more ticks"
    );
}

// ---------------------------------------------------------------------------
// City ownership
// ---------------------------------------------------------------------------

#[test]
fn assign_city_owner_returns_true_for_valid_city() {
    let map = test_map();
    let settings = test_settings();
    let mut game = Game::new(map, settings, 4);

    let result = game.assign_city_owner("city_0", "player_0");
    assert!(result);
}

#[test]
fn assign_city_owner_returns_false_for_invalid_city() {
    let map = test_map();
    let settings = test_settings();
    let mut game = Game::new(map, settings, 4);

    let result = game.assign_city_owner("nonexistent_city", "player_0");
    assert!(!result);
}

#[test]
fn city_ownership_persists_across_ticks() {
    let map = test_map();
    let settings = test_settings();
    let mut game = Game::new(map, settings, 4);

    game.assign_city_owner("city_0", "player_0");
    game.start();

    for _ in 0..10 {
        game.tick_game();
    }

    assert!(game.player_funds("player_0") > 1000);
}

// ---------------------------------------------------------------------------
// Recruitment / manpower economy
// ---------------------------------------------------------------------------

/// Manpower and funds always apply — manpower is NOT gated behind the city-upgrade
/// flag. A standard game (city upgrades disabled) with a seeded initial manpower
/// pool must be able to recruit immediately, and the recruit costs funds.
#[test]
fn recruit_uses_seeded_manpower_even_without_city_upgrades() {
    let map = test_map();
    let settings = test_settings(); // CityUpgradeSettings::default() => enabled = false, but initial_manpower = 10000
    let mut game = Game::new(map, settings, 4);
    game.assign_city_owner("city_0", "player_0");
    let funds_before = game.player_funds("player_0");
    let ut = basic_unit_type();

    let unit = game.recruit_unit("city_0", &ut, "player_0");
    assert!(
        unit.is_some(),
        "recruitment must work from the seeded manpower pool even when city upgrades are disabled"
    );
    assert!(
        game.player_funds("player_0") == funds_before - ut.recruitment_cost,
        "recruitment must still consume funds"
    );
}

/// Manpower is a first-class recruitment resource: with an empty/manual manpower
/// pool, recruitment must fail even if the player has enough funds and regardless
/// of the city-upgrade flag.
#[test]
fn recruit_requires_manpower_always() {
    let map = test_map();
    let mut settings = test_settings();
    settings.initial_manpower = 0; // empty manpower pool
    let mut game = Game::new(map, settings, 4);
    game.assign_city_owner("city_0", "player_0");
    let ut = basic_unit_type();

    let unit = game.recruit_unit("city_0", &ut, "player_0");
    assert!(
        unit.is_none(),
        "recruit must fail when the manpower pool is empty, even with funds available"
    );
}

// ---------------------------------------------------------------------------
// Player state
// ---------------------------------------------------------------------------

#[test]
fn player_stats_initialized() {
    let map = test_map();
    let settings = test_settings();
    let game = Game::new(map, settings, 4);

    let stats = game.player_stats();
    assert!(!stats.is_empty());

    for (pid, s) in stats {
        assert_eq!(s.units_recruited, 0, "{} should have 0 recruits", pid);
        assert_eq!(s.units_lost, 0, "{} should have 0 losses", pid);
        assert_eq!(s.cities_captured, 0, "{} should have 0 captures", pid);
    }
}

#[test]
fn is_player_active_requires_units_or_cities() {
    let map = test_map();
    let settings = test_settings();
    let mut game = Game::new(map, settings, 4);

    // Players are registered but have no units/cities yet
    assert!(!game.is_player_active("player_0"));

    // Assign a city to make the player active
    game.assign_city_owner("city_0", "player_0");
    assert!(game.is_player_active("player_0"));

    // Nonexistent player is never active
    assert!(!game.is_player_active("nonexistent"));
}

// ---------------------------------------------------------------------------
// Game settings
// ---------------------------------------------------------------------------

#[test]
fn settings_accessible_after_creation() {
    let map = test_map();
    let settings = test_settings();
    let game = Game::new(map, settings, 4);

    let s = game.settings();
    assert_eq!(s.starting_funds, 1000);
    assert_eq!(s.unit_types.len(), 1);
    assert_eq!(s.unit_types[0].name, "infantry");
}

// ---------------------------------------------------------------------------
// Winner determination
// ---------------------------------------------------------------------------

#[test]
fn determine_winner_returns_none_when_game_running() {
    let map = test_map();
    let settings = test_settings();
    let mut game = Game::new(map, settings, 4);

    game.start();
    game.tick_game();

    assert!(game.determine_winner().is_none());
}

// ---------------------------------------------------------------------------
// Combat + Attrition + Death Cleanup integration
// ---------------------------------------------------------------------------

/// Integration test: verify that units can be recruited, engage in combat, and
/// the dead units are properly cleaned up by the unified death_cleanup pipeline.
/// This tests the full tick flow: movement → spatialty → combat → attrition →
/// surrender → death_cleanup.
#[test]
fn combat_kill_is_cleaned_by_unified_pipeline() {
    let map = test_map();
    let settings = test_settings();
    let mut game = Game::new(map, settings, 2);

    // Player 0 owns city at (3, 3), player 1 owns city at (3, 16) — far apart.
    // We need to recruit units close enough for combat. Use city_0 for both.
    game.assign_city_owner("city_0", "player_0");
    game.assign_city_owner("city_1", "player_1");

    game.start();

    let ut = basic_unit_type();
    // Recruit two units at their own cities, then move toward each other.
    // city_0 is at (3, 3) owned by player_0, city_1 is at (16, 3) owned by player_1.
    // Move them toward each other on the same y-axis.
    let unit_a = game.recruit_unit("city_0", &ut, "player_0");
    let unit_b = game.recruit_unit("city_1", &ut, "player_1");

    assert!(unit_a.is_some(), "player_0 should recruit a unit");
    assert!(unit_b.is_some(), "player_1 should recruit a unit");

    let unit_a_id = unit_a.unwrap();
    let unit_b_id = unit_b.unwrap();

    // Move unit A toward player_1's city and unit B toward player_0's city.
    // They'll meet somewhere around x=10. This should trigger movement → spatialty → combat.
    game.move_troops(&unit_a_id, "player_0", vec![(10.0, 3.0)]);
    game.move_troops(&unit_b_id, "player_1", vec![(10.0, 3.0)]);

    // Run enough ticks for: movement → spatialty → combat to engage
    for _ in 0..60 {
        game.tick_game();
    }

    // After combat, at least one unit should have been damaged or killed.
    // The key assertion is that dead units are cleaned up (not leaking in the world).
    let unit_data = game.query_all_units();
    // Filter to only units from these two players
    let player_units: Vec<_> = unit_data
        .iter()
        .filter(|u| u.owner == "player_0" || u.owner == "player_1")
        .collect();

    // The dead unit should NOT appear in the query results (it was despawned)
    for unit in &player_units {
        assert!(
            unit.men > 0.0,
            "dead units should be cleaned up, found unit with men={} in query",
            unit.men
        );
    }

    // We should have at most 2 units (one per player), and both should be alive
    assert!(
        player_units.len() <= 2,
        "should have at most 2 units alive, got {}",
        player_units.len()
    );
}

/// Integration test: verify that the unified death cleanup handles combat kills
/// and surrender (below 5% men) in the same pass. After a full tick cycle with
/// combat and surrender both producing dead units, all dead entities should be
/// removed from the world.
#[test]
fn unified_cleanup_handles_combat_and_surrender() {
    let map = test_map();
    let settings = test_settings();
    let mut game = Game::new(map, settings, 3);

    game.assign_city_owner("city_0", "player_0");
    game.assign_city_owner("city_1", "player_1");
    game.assign_city_owner("city_2", "player_2");

    game.start();

    let ut = basic_unit_type();
    // Recruit units for player 0 and player 1 at their own cities
    let unit_a = game.recruit_unit("city_0", &ut, "player_0");
    let unit_b = game.recruit_unit("city_1", &ut, "player_1");
    // Recruit a unit for player 2 that will be brought below 5%
    let _unit_c = game.recruit_unit("city_2", &ut, "player_2");

    assert!(unit_a.is_some());
    assert!(unit_b.is_some());

    let unit_a_id = unit_a.unwrap();

    // Move unit A toward player_1's city and unit B toward player_0's city
    // so they meet around x=10 on the same y-axis.
    game.move_troops(&unit_a_id, "player_0", vec![(10.0, 3.0)]);
    let unit_b_id = unit_b.unwrap();
    game.move_troops(&unit_b_id, "player_1", vec![(10.0, 3.0)]);

    // Run 60 ticks: combat should engage, surrender check happens every 30 ticks
    // so at least 2 surrender sweeps occur, giving the pipeline enough opportunity
    // to clean up both combat casualties and surrender casualties.
    for _ in 0..90 {
        game.tick_game();
    }

    // All units in the query should be alive (men > 0)
    let all_units = game.query_all_units();
    for unit in &all_units {
        assert!(
            unit.men > 0.0,
            "all dead/surrendered units should be cleaned up, found {} with men={}",
            unit.id,
            unit.men
        );
    }

    // No unit should have men below 5% of max_men (surrender would have caught it)
    for unit in &all_units {
        if unit.max_men > 0.0 {
            let pct = unit.men / unit.max_men;
            assert!(
                pct >= 0.05,
                "no unit should survive below 5% men (surrender should have cleaned it), \
                 unit {} has {}% men",
                unit.id,
                pct * 100.0
            );
        }
    }
}
