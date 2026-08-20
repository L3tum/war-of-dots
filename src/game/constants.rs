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

/// Visual/physical radius of a unit's dot (a 10×10 dot). Global for now — no
/// per-unit-type field, so the client draws the same size with no protocol change.
pub const UNIT_RADIUS: f32 = 5.0;

/// Two units are in combat when their circles touch/intersect: center distance
/// < 2×UNIT_RADIUS (the two 5px dots tangent at distance 10). The mover is the
/// attacker.
pub const COMBAT_DISTANCE: f32 = UNIT_RADIUS * 2.0; // = 10.0
pub const COMBAT_DISTANCE_SQ: f32 = COMBAT_DISTANCE * COMBAT_DISTANCE; // = 100.0

/// Closest two unit centers may approach. 8 < COMBAT_DISTANCE, so units can close
/// to 8–10 (fighting; 10px dots overlap by up to 2px) and stop at 8.
pub const MIN_UNIT_DISTANCE: f32 = 8.0;
pub const MIN_UNIT_DISTANCE_SQ: f32 = MIN_UNIT_DISTANCE * MIN_UNIT_DISTANCE; // = 64.0

/// Spatial-lookup cell side. Set to COMBAT_DISTANCE so the 3×3 neighbourhood is
/// complete for both the combat (radius 10) and occupancy (radius 8) queries.
/// A locality tuning knob, decoupled from the rule.
pub const SPATIAL_CELL_SIZE: f32 = COMBAT_DISTANCE; // = 10.0

/// How much supply a single soldier can carry (kg).
/// max_supply = men * CARRY_WEIGHT
pub const CARRY_WEIGHT: f32 = 2.0;

/// Embark timer: ticks required to board a unit onto water.
pub const EMBARK_TICKS: u32 = 10;

/// Disembark timer: ticks required to leave water.
pub const DISEMBARK_TICKS: u32 = 10;

#[cfg(test)]
mod tests {
    use super::*;

    /// Units must be able to close to within combat range (otherwise the
    /// occupancy rule would block them *before* they could ever fight).
    #[test]
    fn min_distance_is_less_than_combat_distance() {
        let min = MIN_UNIT_DISTANCE;
        let combat = COMBAT_DISTANCE;
        assert!(
            min < combat,
            "units stop at {} but only fight within {} — they could never engage",
            min,
            combat
        );
    }

    /// Combat distance is exactly the two dots tangent (2× radius).
    #[test]
    fn combat_distance_is_two_radii() {
        assert_eq!(COMBAT_DISTANCE, 2.0 * UNIT_RADIUS);
        assert_eq!(COMBAT_DISTANCE_SQ, COMBAT_DISTANCE * COMBAT_DISTANCE);
        assert_eq!(MIN_UNIT_DISTANCE_SQ, MIN_UNIT_DISTANCE * MIN_UNIT_DISTANCE);
    }

    /// The 3×3 cell scan must cover the full combat radius, so a candidate at
    /// dist < COMBAT_DISTANCE is always in the same-or-adjacent cell.
    #[test]
    fn spatial_cell_size_covers_combat_distance() {
        let cell = SPATIAL_CELL_SIZE;
        let combat = COMBAT_DISTANCE;
        assert!(
            cell >= combat,
            "cell side {} < combat distance {} — 3×3 neighbourhood is incomplete",
            cell,
            combat
        );
    }
}
