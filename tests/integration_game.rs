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
        supply_buffer_percent: 20,
        max_supply: 2000, // men(1000) * CARRY_WEIGHT(2)
    }
}

fn test_settings() -> GameSettings {
    GameSettings {
        starting_funds: 1000,
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
