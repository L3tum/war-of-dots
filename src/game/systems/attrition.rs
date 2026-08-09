//! AttritionSystem — equipment attrition, org drain, starvation.
//!
//! Uses two-phase pattern: query + collect changes → apply mutations in batch.
//! Eliminates entity-collect-then-refetch anti-pattern.

use crate::game::components::*;
use crate::game::economy::UnitType;
use crate::game::game::GameSettings;
use hecs::World;
use std::collections::HashMap;

/// Collected changes to apply in phase 2.
struct AttritionChanges {
    new_stats: UnitStats,
    new_equipment: Option<UnitEquipment>,
    new_water_transition: Option<UnitWaterTransition>,
}

pub fn run(world: &mut World, _world_entity: hecs::Entity, settings: &GameSettings) {
    // Precompute unit-type lookup (name -> &UnitType) once per tick to avoid a
    // linear scan over settings.unit_types per unit per tick.
    let unit_types_by_name: HashMap<&str, &UnitType> = settings
        .unit_types
        .iter()
        .map(|ut| (ut.name.as_str(), ut))
        .collect();

    // Phase 1: query all needed components at once, compute changes
    let changes: Vec<(hecs::Entity, AttritionChanges)> = world
        .query::<(
            &UnitTypeId,
            &UnitStats,
            &UnitEquipment,
            &UnitFlags,
            &UnitWaterTransition,
        )>()
        .iter()
        .filter_map(|(entity, (utype, stats, equipment, flags, wt))| {
            // Skip units already at 0 men: they are dead-waiting-for-surrender and should
            // no longer be mutated by attrition (prevents junk ECS writes on dead units).
            // Such units are removed by the surrender sweep in tick_game.
            if stats.men <= 0.0 {
                return None;
            }
            compute_attrition(utype, stats, equipment, flags, wt, &unit_types_by_name)
                .map(|change| (entity, change))
        })
        .collect();

    // Phase 2: apply changes in batch
    for (entity, change) in changes {
        // Stats always change (at least org drain or men loss)
        if let Ok((ms,)) = world.query_one_mut::<(&mut UnitStats,)>(entity) {
            *ms = change.new_stats;
        }
        if let Some(new_eq) = change.new_equipment {
            if let Ok((me,)) = world.query_one_mut::<(&mut UnitEquipment,)>(entity) {
                *me = new_eq;
            }
        }
        if let Some(new_wt) = change.new_water_transition {
            if let Ok((mwt,)) = world.query_one_mut::<(&mut UnitWaterTransition,)>(entity) {
                *mwt = new_wt;
            }
        }
    }
}

fn compute_attrition(
    utype: &UnitTypeId,
    stats: &UnitStats,
    equipment: &UnitEquipment,
    flags: &UnitFlags,
    wt: &UnitWaterTransition,
    unit_types_by_name: &HashMap<&str, &UnitType>,
) -> Option<AttritionChanges> {
    let mut new_stats = *stats;
    let mut new_equipment: Option<UnitEquipment> = None;
    let mut new_water_transition: Option<UnitWaterTransition> = None;
    let mut changed = false;

    // Water bonus: org drain + boat supply consumption
    if flags.on_water && !wt.embarking && !wt.disembarking {
        if stats.organisation > 0.0 {
            new_stats.organisation = (new_stats.organisation - 0.03).max(0.0);
            changed = true;
        }
        if !wt.boat_supply.is_empty() {
            let mut new_wt_copy = (*wt).clone();
            let drain_rate: f32 = if flags.fighting { 2.0 } else { 1.0 };
            for boat_eq in new_wt_copy.boat_supply.iter_mut() {
                if boat_eq.count > 0 {
                    boat_eq.count = boat_eq.count.saturating_sub(drain_rate.ceil() as u32);
                }
            }
            if new_wt_copy.boat_supply.iter().map(|e| e.count).sum::<u32>() == 0 {
                new_wt_copy.boat_supply.clear();
                new_wt_copy.max_boat_supply.clear();
            }
            new_water_transition = Some(new_wt_copy);
            changed = true;
        }
    }

    // Equipment attrition + starvation
    if let Some(unit_type) = unit_types_by_name.get(utype.name.as_str()).copied() {
        let mut attrition_rate = if flags.fighting {
            unit_type.fighting_attrition
        } else {
            unit_type.base_attrition
        };

        let total_weight: u32 = equipment.equipment.iter().map(|e| e.count).sum();
        let capacity = stats.men as u32;
        if total_weight > capacity && capacity > 0 {
            attrition_rate = attrition_rate.max(unit_type.base_attrition * 1.5);
        }

        let mut new_eq_copy = (*equipment).clone();
        let mut eq_changed = false;
        for eq in &mut new_eq_copy.equipment {
            if eq.count > 0 {
                let old_count = eq.count;
                eq.count = eq
                    .count
                    .saturating_sub((attrition_rate.ceil() as u32).max(1));
                if eq.count != old_count {
                    eq_changed = true;
                }
            }
        }

        // Slight manpower attrition: ~1 man every 600 ticks (~10s at 60fps)
        // This keeps a standing army costing the economy continuously
        let manpower_drain = 1.0 / 600.0;
        new_stats.men = (new_stats.men - manpower_drain).max(0.0);
        changed = true;

        let supply_pct = supply_percent(&equipment.equipment, &equipment.max_equipment);
        let mut men_changed = false;
        if supply_pct <= 0.0 {
            let manpower_loss = (attrition_rate * 3.0).max(0.5);
            let new_men = (new_stats.men - manpower_loss).max(0.0);
            if new_men != new_stats.men {
                new_stats.men = new_men;
                men_changed = true;
            }
        }

        if eq_changed {
            new_equipment = Some(new_eq_copy);
            changed = true;
        }
        if men_changed {
            changed = true;
        }
    }

    if changed {
        Some(AttritionChanges {
            new_stats,
            new_equipment,
            new_water_transition,
        })
    } else {
        None
    }
}

fn supply_percent(
    eq: &[crate::game::unit::Equipment],
    max_eq: &[crate::game::unit::Equipment],
) -> f32 {
    if max_eq.is_empty() {
        return 100.0;
    }
    let total_needed: f32 = max_eq.iter().map(|me| (me.max_count as f32) * 0.9).sum();
    let total_carried: f32 = eq.iter().map(|e| e.count as f32).sum();
    if total_needed <= 0.0 {
        return 100.0;
    }
    (total_carried / total_needed * 100.0).clamp(0.0, 100.0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::game::unit::Equipment;

    /// Build the unit-type lookup map used by compute_attrition (mirrors run()).
    fn unit_types_map(settings: &GameSettings) -> HashMap<&str, &UnitType> {
        settings
            .unit_types
            .iter()
            .map(|ut| (ut.name.as_str(), ut))
            .collect()
    }

    // ---------------------------------------------------------------------------
    // supply_percent
    // ---------------------------------------------------------------------------

    #[test]
    fn supply_percent_empty_max_is_100() {
        let eq: Vec<Equipment> = vec![Equipment {
            name: "rifle".to_string(),
            count: 50,
            max_count: 100,
            cost_per_item: 2.5,
        }];
        let max_eq: Vec<Equipment> = vec![];
        assert_eq!(supply_percent(&eq, &max_eq), 100.0);
    }

    #[test]
    fn supply_percent_full_equipment() {
        let eq = vec![Equipment {
            name: "rifle".to_string(),
            count: 100,
            max_count: 100,
            cost_per_item: 2.5,
        }];
        let max_eq = vec![Equipment {
            name: "rifle".to_string(),
            count: 100,
            max_count: 100,
            cost_per_item: 2.5,
        }];
        // carried = 100, needed = 90 (0.9 * 100) -> 111.11% -> clamped to 100
        let pct = supply_percent(&eq, &max_eq);
        assert!((pct - 100.0).abs() < 0.01);
    }

    #[test]
    fn supply_percent_half_equipment() {
        let eq = vec![Equipment {
            name: "rifle".to_string(),
            count: 50,
            max_count: 100,
            cost_per_item: 2.5,
        }];
        let max_eq = vec![Equipment {
            name: "rifle".to_string(),
            count: 100,
            max_count: 100,
            cost_per_item: 2.5,
        }];
        // carried = 50, needed = 90 -> 55.56%
        let pct = supply_percent(&eq, &max_eq);
        assert!((pct - 55.56).abs() < 0.1);
    }

    #[test]
    fn supply_percent_no_equipment() {
        let eq = vec![Equipment {
            name: "rifle".to_string(),
            count: 0,
            max_count: 100,
            cost_per_item: 2.5,
        }];
        let max_eq = vec![Equipment {
            name: "rifle".to_string(),
            count: 100,
            max_count: 100,
            cost_per_item: 2.5,
        }];
        // carried = 0, needed = 90 -> 0%
        let pct = supply_percent(&eq, &max_eq);
        assert!((pct - 0.0).abs() < 0.01);
    }

    #[test]
    fn supply_percent_multiple_equipment() {
        let eq = vec![
            Equipment {
                name: "rifle".to_string(),
                count: 50,
                max_count: 100,
                cost_per_item: 2.5,
            },
            Equipment {
                name: "ammo".to_string(),
                count: 90,
                max_count: 100,
                cost_per_item: 0.5,
            },
        ];
        let max_eq = vec![
            Equipment {
                name: "rifle".to_string(),
                count: 100,
                max_count: 100,
                cost_per_item: 2.5,
            },
            Equipment {
                name: "ammo".to_string(),
                count: 100,
                max_count: 100,
                cost_per_item: 0.5,
            },
        ];
        // carried = 140, needed = 180 (90 + 90) -> 77.78%
        let pct = supply_percent(&eq, &max_eq);
        assert!((pct - 77.78).abs() < 0.1);
    }

    // ---------------------------------------------------------------------------
    // compute_attrition
    // ---------------------------------------------------------------------------

    fn test_settings() -> GameSettings {
        GameSettings {
            starting_funds: 1000,
            initial_manpower: 10_000,
            funds_per_city: 1.0,
            recruitment_rate: 1.0,
            game_speed: 1.0,
            use_standard_ruleset: true,
            unit_types: vec![crate::game::economy::UnitType {
                name: "infantry".to_string(),
                symbol: "I".to_string(),
                men: 1000,
                equipment: vec![],
                soft_attack: 5.0,
                soft_defense: 4.0,
                hard_attack: 3.0,
                hard_defense: 2.0,
                recruitment_cost: 200,
                max_organisation: 100.0,
                reinforcement_rate: 5.0,
                speed: 1.0,
                base_attrition: 1.0,
                fighting_attrition: 2.0,
                supply_buffer_percent: 20,
                max_supply: 2000,
            }],
            city_upgrade_settings: crate::config::CityUpgradeSettings::default(),
            water_speed: 0.5,
        }
    }

    #[test]
    fn attrition_reduces_equipment() {
        let utype = UnitTypeId {
            name: "infantry".to_string(),
        };
        let stats = UnitStats {
            men: 1000.0,
            max_men: 1000.0,
            organisation: 100.0,
            max_organisation: 100.0,
            reinforcement_rate: 5.0,
        };
        let equipment = UnitEquipment {
            equipment: vec![Equipment {
                name: "rifle".to_string(),
                count: 100,
                max_count: 100,
                cost_per_item: 2.5,
            }],
            max_equipment: vec![Equipment {
                name: "rifle".to_string(),
                count: 100,
                max_count: 100,
                cost_per_item: 2.5,
            }],
            supply_buffer_percent: 20,
            supply: 2000.0,
            max_supply: 2000.0,
        };
        let flags = UnitFlags {
            healing: false,
            fighting: false,
            moved: false,
            encircled: false,
            resupply_enabled: true,
            on_water: false,
        };
        let wt = UnitWaterTransition::default();
        let settings = test_settings();

        let change = compute_attrition(
            &utype,
            &stats,
            &equipment,
            &flags,
            &wt,
            &unit_types_map(&settings),
        );

        assert!(change.is_some());
        let change = change.unwrap();
        assert!(change.new_equipment.is_some());
        let new_eq = change.new_equipment.unwrap();
        assert!(
            new_eq.equipment[0].count < 100,
            "equipment should be reduced by attrition"
        );
    }

    #[test]
    fn attrition_fighting_has_higher_rate() {
        let utype = UnitTypeId {
            name: "infantry".to_string(),
        };
        let stats = UnitStats {
            men: 1000.0,
            max_men: 1000.0,
            organisation: 100.0,
            max_organisation: 100.0,
            reinforcement_rate: 5.0,
        };
        let equipment = UnitEquipment {
            equipment: vec![Equipment {
                name: "rifle".to_string(),
                count: 100,
                max_count: 100,
                cost_per_item: 2.5,
            }],
            max_equipment: vec![Equipment {
                name: "rifle".to_string(),
                count: 100,
                max_count: 100,
                cost_per_item: 2.5,
            }],
            supply_buffer_percent: 20,
            supply: 2000.0,
            max_supply: 2000.0,
        };
        let wt = UnitWaterTransition::default();
        let settings = test_settings();

        let flags_peace = UnitFlags {
            fighting: false,
            ..Default::default()
        };
        let flags_fight = UnitFlags {
            fighting: true,
            ..Default::default()
        };

        let peace_change = compute_attrition(
            &utype,
            &stats,
            &equipment,
            &flags_peace,
            &wt,
            &unit_types_map(&settings),
        )
        .unwrap();
        let fight_change = compute_attrition(
            &utype,
            &stats,
            &equipment,
            &flags_fight,
            &wt,
            &unit_types_map(&settings),
        )
        .unwrap();

        let peace_eq = peace_change.new_equipment.unwrap().equipment[0].count;
        let fight_eq = fight_change.new_equipment.unwrap().equipment[0].count;

        assert!(
            fight_eq < peace_eq,
            "fighting attrition ({}) should reduce more than peace ({})",
            fight_eq,
            peace_eq
        );
    }

    #[test]
    fn attrition_starvation_reduces_men() {
        let utype = UnitTypeId {
            name: "infantry".to_string(),
        };
        let stats = UnitStats {
            men: 1000.0,
            max_men: 1000.0,
            organisation: 100.0,
            max_organisation: 100.0,
            reinforcement_rate: 5.0,
        };
        // Zero equipment = starvation
        let equipment = UnitEquipment {
            equipment: vec![Equipment {
                name: "rifle".to_string(),
                count: 0,
                max_count: 100,
                cost_per_item: 2.5,
            }],
            max_equipment: vec![Equipment {
                name: "rifle".to_string(),
                count: 100,
                max_count: 100,
                cost_per_item: 2.5,
            }],
            supply_buffer_percent: 20,
            supply: 2000.0,
            max_supply: 2000.0,
        };
        let flags = UnitFlags::default();
        let wt = UnitWaterTransition::default();
        let settings = test_settings();

        let change = compute_attrition(
            &utype,
            &stats,
            &equipment,
            &flags,
            &wt,
            &unit_types_map(&settings),
        );

        assert!(change.is_some());
        let change = change.unwrap();
        assert!(
            change.new_stats.men < 1000.0,
            "starvation should reduce men"
        );
    }

    #[test]
    fn attrition_men_does_not_go_negative() {
        let utype = UnitTypeId {
            name: "infantry".to_string(),
        };
        let stats = UnitStats {
            men: 1.0,
            max_men: 1000.0,
            organisation: 100.0,
            max_organisation: 100.0,
            reinforcement_rate: 5.0,
        };
        let equipment = UnitEquipment {
            equipment: vec![Equipment {
                name: "rifle".to_string(),
                count: 0,
                max_count: 100,
                cost_per_item: 2.5,
            }],
            max_equipment: vec![Equipment {
                name: "rifle".to_string(),
                count: 100,
                max_count: 100,
                cost_per_item: 2.5,
            }],
            supply_buffer_percent: 20,
            supply: 2000.0,
            max_supply: 2000.0,
        };
        let flags = UnitFlags::default();
        let wt = UnitWaterTransition::default();
        let settings = test_settings();

        let change = compute_attrition(
            &utype,
            &stats,
            &equipment,
            &flags,
            &wt,
            &unit_types_map(&settings),
        );

        assert!(change.is_some());
        let change = change.unwrap();
        assert!(change.new_stats.men >= 0.0, "men should not be negative");
    }

    #[test]
    fn attrition_unknown_unit_type_no_change() {
        let utype = UnitTypeId {
            name: "nonexistent_unit".to_string(),
        };
        let stats = UnitStats {
            men: 1000.0,
            max_men: 1000.0,
            organisation: 100.0,
            max_organisation: 100.0,
            reinforcement_rate: 5.0,
        };
        let equipment = UnitEquipment {
            equipment: vec![],
            max_equipment: vec![],
            supply_buffer_percent: 20,
            supply: 2000.0,
            max_supply: 2000.0,
        };
        let flags = UnitFlags::default();
        let wt = UnitWaterTransition::default();
        let settings = test_settings();

        let change = compute_attrition(
            &utype,
            &stats,
            &equipment,
            &flags,
            &wt,
            &unit_types_map(&settings),
        );

        // No unit type found, so no attrition computed
        assert!(change.is_none());
    }
}
