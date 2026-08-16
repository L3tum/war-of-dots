//! Game constants extracted from magic numbers.
//!
//! These values control tick rates, ranges, and timers throughout the game systems.
//! Adjusting these values changes game balance.

/// Economy tick interval: funds generated every N ticks.
pub const ECONOMY_TICK_INTERVAL: u64 = 1;

/// Influence tick interval: border influence calculated every N ticks.
pub const INFLUENCE_TICK_INTERVAL: u64 = 3;

/// City capture tick interval: capture progress evaluated every N ticks.
pub const CITY_CAPTURE_TICK_INTERVAL: u64 = 30;

/// Combat range squared: units within this distance engage (distance 2.0).
pub const COMBAT_RANGE_SQ: f32 = 4.0;

/// How much supply a single soldier can carry (kg).
/// max_supply = men * CARRY_WEIGHT
pub const CARRY_WEIGHT: f32 = 2.0;

/// Embark timer: ticks required to board a unit onto water.
pub const EMBARK_TICKS: u32 = 10;

/// Disembark timer: ticks required to leave water.
pub const DISEMBARK_TICKS: u32 = 10;
