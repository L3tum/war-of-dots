//! SupplySystem — global resupply from the player pools.
//!
//! Each tick, every non-encircled unit draws from its player's GLOBAL pools:
//! - **Men** at `recruitment_rate` per tick from the global manpower pool.
//! - **Equipment** restocked toward its supply buffer (`100% +
//!   supply_buffer_percent` of required, e.g. 110% at buffer 10) from funds
//!   (`cost_per_item`), first-come-first-served.
//!
//! There is **no range requirement**. Pools are contributed to by every owned
//! city each tick (economy), so city control determines how much of the army is
//! fed/equipped. Encircled units get nothing (cut supply lines).

use std::collections::HashMap;

use crate::game::components::*;
use crate::game::economy::UnitType;
use crate::game::game::GameSettings;
use crate::game::world_ext::WorldExt;
use hecs::World;

/// Cost + per-slot delivery plan to restock one unit's equipment toward its
/// supply buffer, capped at `equipment.supply_rate` items per tick.
/// Returns ([(slot_idx, items_to_add)], total_cost).
fn plan_delivery(equipment: &UnitEquipment) -> (Vec<(usize, u32)>, u32) {
    let buffer = (100 + equipment.supply_buffer_percent) as f32 / 100.0;
    let mut remaining = equipment.supply_rate;
    let mut plan: Vec<(usize, u32)> = Vec::new();
    let mut cost: u32 = 0;
    for (i, (eq, max_eq)) in equipment
        .equipment
        .iter()
        .zip(&equipment.max_equipment)
        .enumerate()
    {
        if remaining == 0 {
            break;
        }
        let target = ((max_eq.max_count as f32) * buffer).ceil() as u32;
        if eq.count >= target {
            continue;
        }
        let add = (target - eq.count).min(remaining);
        remaining -= add;
        if add > 0 {
            plan.push((i, add));
            cost += (add as f32 * max_eq.cost_per_item).ceil() as u32;
        }
    }
    (plan, cost)
}

/// Equipment ratio: carried / required (>= 1.0 when buffered/fully supplied).
fn equipment_ratio(equipment: &UnitEquipment) -> f32 {
    let needed: f32 = equipment
        .max_equipment
        .iter()
        .map(|e| e.max_count as f32)
        .sum();
    if needed <= 0.0 {
        return 1.0;
    }
    let carried: f32 = equipment.equipment.iter().map(|e| e.count as f32).sum();
    carried / needed
}

/// Combined attrition + supply. For each unit it computes the FULL net change
/// in a single pass and applies it once — one scan, one apply, instead of two
/// separate systems:
///
/// **Attrition**: equipment wear (1 item/tick), water/boat/org drain, and the
/// base manpower drain (1/600 × up to 10 when under-supplied).
/// **Supply**: men refill at `recruitment_rate` from the manpower pool, and
/// equipment refill toward the supply buffer from funds (capped by
/// `supply_rate`).
///
/// Returns the units that reached 0 men this tick (for death cleanup).
pub fn run(
    world: &mut World,
    world_entity: hecs::Entity,
    settings: &GameSettings,
) -> Vec<hecs::Entity> {
    let unit_types_by_name: HashMap<&str, &UnitType> = settings
        .unit_types
        .iter()
        .map(|ut| (ut.name.as_str(), ut))
        .collect();
    let recruitment_rate = settings.recruitment_rate;

    // Snapshot each player's global pools (manpower + funds) for FCFS gating.
    let (remaining_mp0, remaining_funds0) = {
        let wb = match world.world_data(world_entity) {
            Some(wb) => wb,
            None => return Vec::new(),
        };
        (wb.funds.manpower.clone(), wb.funds.funds.clone())
    };
    let mut remaining_mp = remaining_mp0;
    let mut remaining_funds = remaining_funds0;

    // Collected net changes (attrition + supply applied here), applied once.
    // (entity, new_stats, new_equipment, new_water_transition)
    type Change = (
        hecs::Entity,
        UnitStats,
        Option<UnitEquipment>,
        Option<UnitWaterTransition>,
    );
    let mut changes: Vec<Change> = Vec::new();
    let mut mp_cost: HashMap<String, u32> = HashMap::new();
    let mut fund_cost: HashMap<String, u32> = HashMap::new();

    // SINGLE PASS over every unit.
    for (entity, (utype, owner, stats, equipment, flags, wt)) in world
        .query::<(
            &UnitTypeId,
            &UnitOwner,
            &UnitStats,
            &UnitEquipment,
            &UnitFlags,
            &UnitWaterTransition,
        )>()
        .iter()
    {
        let pid = &owner.player_id;
        // Dead-waiting-for-surrender units are not mutated here.
        if stats.men <= 0.0 {
            continue;
        }
        let base_attrition = unit_types_by_name
            .get(utype.name.as_str())
            .map(|u| u.base_attrition)
            .unwrap_or(1.0);

        let mut new_stats = *stats;
        let mut new_equipment: Option<UnitEquipment> = None;
        let mut new_wt: Option<UnitWaterTransition> = None;
        let mut changed = false;

        // Attrition — water: org drain + boat supply consumption.
        if flags.on_water && !wt.embarking && !wt.disembarking {
            if new_stats.organisation > 0.0 {
                new_stats.organisation = (new_stats.organisation - 0.03).max(0.0);
                changed = true;
            }
            if !wt.boat_supply.is_empty() {
                let mut wtc = (*wt).clone();
                let drain = (base_attrition * 2.0).max(1.0);
                for b in wtc.boat_supply.iter_mut() {
                    if b.count > 0 {
                        b.count = b.count.saturating_sub(drain.ceil() as u32);
                    }
                }
                if wtc.boat_supply.iter().map(|e| e.count).sum::<u32>() == 0 {
                    wtc.boat_supply.clear();
                    wtc.max_boat_supply.clear();
                }
                new_wt = Some(wtc);
                changed = true;
            }
        }

        // Attrition — equipment wear (1 item/tick per stocked slot; applies to
        // everyone, even encircled). Then Supply — refill toward the supply
        // buffer (capped by supply_rate, gated on funds, non-encircled only).
        let mut eq_copy = (*equipment).clone();
        let mut eq_changed = false;
        for eq in eq_copy.equipment.iter_mut() {
            if eq.count > 0 {
                eq.count = eq.count.saturating_sub(1);
                eq_changed = true;
            }
        }
        let (delivery, cost) = plan_delivery(&eq_copy);
        let mut delivered = false;
        if cost > 0 && !flags.encircled {
            let avail = remaining_funds.get(pid).copied().unwrap_or(0);
            if avail >= cost {
                if let Some(f) = remaining_funds.get_mut(pid) {
                    *f -= cost;
                }
                for (i, add) in delivery {
                    if let Some(e) = eq_copy.equipment.get_mut(i) {
                        e.count += add;
                    }
                }
                *fund_cost.entry(pid.clone()).or_insert(0) += cost;
                delivered = true;
            }
        }
        if eq_changed || delivered {
            new_equipment = Some(eq_copy);
            changed = true;
        }

        // Men: attrition drain (base 1/600, scaled 2x..10x when under-supplied)
        // minus supply gain at `recruitment_rate` from the manpower pool.
        let ratio = equipment_ratio(equipment);
        let mult = if ratio < 1.0 { 10.0 - 8.0 * ratio } else { 1.0 };
        let drain = (1.0 / 600.0) * mult;
        let mut men_gain = 0.0;
        if !flags.encircled && new_stats.men < new_stats.max_men {
            let gain = recruitment_rate.min(new_stats.max_men - new_stats.men);
            if gain > 0.0 {
                let avail = remaining_mp.get(pid).copied().unwrap_or(0) as f32;
                if avail >= gain {
                    if let Some(mp) = remaining_mp.get_mut(pid) {
                        *mp -= gain as u32;
                    }
                    men_gain = gain;
                    *mp_cost.entry(pid.clone()).or_insert(0) += gain as u32;
                }
            }
        }
        let new_men = (new_stats.men - drain + men_gain)
            .max(0.0)
            .min(new_stats.max_men);
        if (new_men - new_stats.men).abs() > f32::EPSILON {
            new_stats.men = new_men;
            changed = true;
        }

        // Organisation recovery (merged from reinforce.rs)
        // Units in combat (fighting or attacking) do NOT recover org.
        if !flags.fighting && !flags.attacking {
            let supply_pct = ratio * 100.0;
            let malus = if supply_pct > 50.0 {
                1.0
            } else {
                supply_pct / 50.0
            };
            let unit_reinforce_rate = unit_types_by_name
                .get(utype.name.as_str())
                .map(|u| u.reinforcement_rate)
                .unwrap_or(5.0);
            let reinforce = unit_reinforce_rate * malus;
            let new_org = (new_stats.organisation + reinforce).min(new_stats.max_organisation);
            if (new_org - new_stats.organisation).abs() > f32::EPSILON {
                new_stats.organisation = new_org;
                changed = true;
            }
        }

        if changed {
            changes.push((entity, new_stats, new_equipment, new_wt));
        }
    }

    // Phase 2: deduct from global pools (one write per player).
    {
        let wb = match world.world_data_mut(world_entity) {
            Some(wb) => wb,
            None => return Vec::new(),
        };
        for (pid, amount) in mp_cost {
            if let Some(mp) = wb.funds.manpower.get_mut(&pid) {
                *mp = mp.saturating_sub(amount);
            }
        }
        for (pid, amount) in fund_cost {
            if let Some(funds) = wb.funds.funds.get_mut(&pid) {
                *funds = funds.saturating_sub(amount);
            }
        }
    }

    // Phase 3: apply every unit's net change once; collect dead.
    let mut dead = Vec::new();
    for (entity, new_stats, new_eq, new_wt) in changes {
        if let Ok((ms,)) = world.query_one_mut::<(&mut UnitStats,)>(entity) {
            *ms = new_stats;
        }
        if let Some(eq) = new_eq {
            if let Ok((me,)) = world.query_one_mut::<(&mut UnitEquipment,)>(entity) {
                *me = eq;
            }
        }
        if let Some(wt) = new_wt {
            if let Ok((mwt,)) = world.query_one_mut::<(&mut UnitWaterTransition,)>(entity) {
                *mwt = wt;
            }
        }
        if new_stats.men <= 0.0 {
            dead.push(entity);
        }
    }
    dead
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::CityUpgradeSettings;
    use crate::game::components::{UnitTypeId, UnitWaterTransition};
    use crate::game::economy::City;
    use crate::game::game::GameSettings;
    use crate::game::unit::Equipment;
    use crate::game::unit::Terrain;

    fn make_world(funds: u32) -> (World, hecs::Entity, GameSettings) {
        let mut world = World::new();
        let mut wb = WorldBundle::new(
            vec![vec![Terrain::Plains; 10]; 10],
            10,
            10,
            CityUpgradeSettings::default(),
            HashMap::new(),
        );
        wb.cities.cities = vec![City {
            id: "c1".to_string(),
            position: (5.0, 5.0),
            owner: Some("P1".to_string()),
            level: 1,
            supply_range: 5.0,
        }];
        wb.funds.funds.insert("P1".to_string(), funds);
        wb.funds.manpower.insert("P1".to_string(), 10_000);
        let world_entity = world.spawn((wb,));
        let settings = GameSettings {
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
                base_attrition: 0.5,
                fighting_attrition: 1.0,
                supply_buffer_percent: 10,
                supply_rate: 5,
                max_supply: 2000,
            }],
            city_upgrade_settings: CityUpgradeSettings::default(),
            water_speed: 0.5,
        };
        (world, world_entity, settings)
    }

    fn spawn_unit_with_equipment(
        world: &mut World,
        flags: UnitFlags,
        count: u32,
        buffer_percent: u32,
    ) -> hecs::Entity {
        spawn_unit_with_equipment_and_org(world, flags, count, buffer_percent, 100.0)
    }

    fn spawn_unit_with_equipment_and_org(
        world: &mut World,
        flags: UnitFlags,
        count: u32,
        buffer_percent: u32,
        organisation: f32,
    ) -> hecs::Entity {
        let equipment = [Equipment {
            name: "rifle".to_string(),
            count,
            max_count: 10,
            cost_per_item: 0.5,
        }];
        world.spawn((
            UnitPosition { x: 9.0, y: 9.0 },
            UnitOwner {
                player_id: "P1".to_string(),
            },
            UnitTypeId {
                name: "infantry".to_string(),
            },
            flags,
            UnitStats {
                men: 1000.0,
                max_men: 1000.0,
                organisation,
                max_organisation: 100.0,
            },
            UnitEquipment {
                equipment: equipment.to_vec(),
                max_equipment: equipment.to_vec(),
                supply_buffer_percent: buffer_percent,
                supply_rate: 5,
                supply: 2000.0,
                max_supply: 2000.0,
            },
            UnitWaterTransition::default(),
        ))
    }

    /// Supply delivers up to supply_rate items per tick. Starting at 4 rifles,
    /// attrition wears 1 and supply replaces up to 5, so the unit visibly gains.
    #[test]
    fn supply_refills_equipment_at_rate_charged_to_funds() {
        let (mut world, world_entity, settings) = make_world(100_000);
        // 4/10 rifles, buffer 10% (target 11), supply_rate 5.
        // Attrition: 4 → 3 (1 item worn).
        // Supply: gap to target is 8, capped by supply_rate=5 → delivers 5 → 8.
        let unit = spawn_unit_with_equipment(&mut world, UnitFlags::default(), 4, 10);

        run(&mut world, world_entity, &settings);

        let eq = world.get::<&UnitEquipment>(unit).unwrap();
        assert_eq!(
            eq.equipment[0].count, 8,
            "supply delivered 5 items, net gain visible"
        );
        // cost = 5 rifles × 0.5 = 2.5 → rounded up to 3 funds.
        let funds = world
            .world_data(world_entity)
            .unwrap()
            .funds
            .funds
            .get("P1")
            .copied()
            .unwrap();
        assert_eq!(funds, 100_000 - 3, "3 funds deducted for 5 items");
    }

    /// From a fully-buffered unit, attrition wears 1 item and supply replaces 1,
    /// so the buffer is maintained each tick at the cost of funds.
    #[test]
    fn supply_maintains_buffered_unit() {
        let (mut world, world_entity, settings) = make_world(100_000);
        // 11/10 rifles (at buffer target with 10% buffer).
        // Attrition: 11 → 10. Supply: refills 1 → 11.
        let unit = spawn_unit_with_equipment(&mut world, UnitFlags::default(), 11, 10);

        run(&mut world, world_entity, &settings);

        let eq = world.get::<&UnitEquipment>(unit).unwrap();
        assert_eq!(
            eq.equipment[0].count, 11,
            "supply maintained buffer after attrition"
        );
        let funds = world
            .world_data(world_entity)
            .unwrap()
            .funds
            .funds
            .get("P1")
            .copied()
            .unwrap();
        assert_eq!(funds, 100_000 - 1, "1 fund spent to replace attrited item");
    }

    /// Encircled units are blocked from supply: they get 0 resupply even when
    /// fully out of equipment, while a non-encircled unit at the same state would
    /// receive items.
    #[test]
    fn supply_skips_encircled() {
        let (mut world, world_entity, settings) = make_world(100_000);
        let flags = UnitFlags {
            encircled: true,
            ..Default::default()
        };
        // 0 equipment — if supply worked, it would deliver 1 item.
        let unit = spawn_unit_with_equipment(&mut world, flags, 0, 10);

        run(&mut world, world_entity, &settings);

        let eq = world.get::<&UnitEquipment>(unit).unwrap();
        assert_eq!(
            eq.equipment[0].count, 0,
            "encircled unit receives no supply"
        );
        let funds = world
            .world_data(world_entity)
            .unwrap()
            .funds
            .funds
            .get("P1")
            .copied()
            .unwrap();
        assert_eq!(funds, 100_000, "funds untouched — no supply delivered");
    }

    /// A player without enough funds does not get free equipment, but attrition
    /// still wears equipment down.
    #[test]
    fn supply_skips_when_funds_short() {
        let (mut world, world_entity, settings) = make_world(0); // no funds to deliver anything
                                                                 // 5/10 rifles. Attrition: 5 → 4. Supply blocked (no funds).
        let unit = spawn_unit_with_equipment(&mut world, UnitFlags::default(), 5, 10);

        run(&mut world, world_entity, &settings);

        let eq = world.get::<&UnitEquipment>(unit).unwrap();
        assert_eq!(
            eq.equipment[0].count, 4,
            "attrition wore 1, supply blocked by 0 funds"
        );
        let funds = world
            .world_data(world_entity)
            .unwrap()
            .funds
            .funds
            .get("P1")
            .copied()
            .unwrap();
        assert_eq!(funds, 0, "funds untouched when refill skipped");
    }

    /// Organisation recovery: a non-fighting, fully supplied unit recovers org.
    /// reinforcement_rate=5, supply_pct >= 50 => malus=1.0 => +5 org.
    #[test]
    fn org_recoveries_when_fully_supplied() {
        let (mut world, world_entity, settings) = make_world(100_000);
        // 10/10 rifles = 100% supply => malus=1.0
        let unit =
            spawn_unit_with_equipment_and_org(&mut world, UnitFlags::default(), 10, 10, 50.0);

        run(&mut world, world_entity, &settings);

        let stats = world.get::<&UnitStats>(unit).unwrap();
        assert!(
            (stats.organisation - 55.0).abs() < 0.01,
            "org recovered by 5 (rate*malus)"
        );
    }

    /// Organisation recovery: under-supplied unit gets reduced recovery.
    /// 5/10 rifles = 50% supply => malus=1.0 (exactly at threshold)
    #[test]
    fn org_recoveries_at_supply_threshold() {
        let (mut world, world_entity, settings) = make_world(100_000);
        // 5/10 rifles = 50% supply => malus=1.0
        let unit = spawn_unit_with_equipment_and_org(&mut world, UnitFlags::default(), 5, 10, 50.0);

        run(&mut world, world_entity, &settings);

        let stats = world.get::<&UnitStats>(unit).unwrap();
        assert!(
            (stats.organisation - 55.0).abs() < 0.01,
            "org recovered by 5 at threshold"
        );
    }

    /// Organisation recovery: severely under-supplied unit gets reduced recovery.
    /// 3/10 rifles = 30% supply => malus=30/50=0.6 => +3.0 org
    #[test]
    fn org_recoveries_reduced_when_under_supplied() {
        let (mut world, world_entity, settings) = make_world(100_000);
        // 3/10 rifles = 30% supply => malus=0.6 => 5*0.6=3.0
        let unit = spawn_unit_with_equipment_and_org(&mut world, UnitFlags::default(), 3, 10, 50.0);

        run(&mut world, world_entity, &settings);

        let stats = world.get::<&UnitStats>(unit).unwrap();
        assert!(
            (stats.organisation - 53.0).abs() < 0.01,
            "org recovered by 3 (5*0.6)"
        );
    }

    /// Organisation recovery: unit at max org stays at max.
    #[test]
    fn org_capped_at_max() {
        let (mut world, world_entity, settings) = make_world(100_000);
        let unit =
            spawn_unit_with_equipment_and_org(&mut world, UnitFlags::default(), 10, 10, 100.0);

        run(&mut world, world_entity, &settings);

        let stats = world.get::<&UnitStats>(unit).unwrap();
        assert!(
            (stats.organisation - 100.0).abs() < 0.01,
            "org capped at max"
        );
    }

    /// Organisation recovery: fighting units do NOT recover org.
    #[test]
    fn org_no_recovery_when_fighting() {
        let (mut world, world_entity, settings) = make_world(100_000);
        let flags = UnitFlags {
            fighting: true,
            ..Default::default()
        };
        let unit = spawn_unit_with_equipment_and_org(&mut world, flags, 10, 10, 50.0);

        run(&mut world, world_entity, &settings);

        let stats = world.get::<&UnitStats>(unit).unwrap();
        assert!(
            (stats.organisation - 50.0).abs() < 0.01,
            "fighting unit does not recover org"
        );
    }

    /// Organisation recovery: attacking units do NOT recover org.
    #[test]
    fn org_no_recovery_when_attacking() {
        let (mut world, world_entity, settings) = make_world(100_000);
        let flags = UnitFlags {
            attacking: true,
            ..Default::default()
        };
        let unit = spawn_unit_with_equipment_and_org(&mut world, flags, 10, 10, 50.0);

        run(&mut world, world_entity, &settings);

        let stats = world.get::<&UnitStats>(unit).unwrap();
        assert!(
            (stats.organisation - 50.0).abs() < 0.01,
            "attacking unit does not recover org"
        );
    }

    /// Organisation recovery: zero-supply unit gets zero recovery.
    /// 0/10 rifles = 0% supply => malus=0.0 => +0 org
    #[test]
    fn org_no_recovery_when_no_supply() {
        let (mut world, world_entity, settings) = make_world(100_000);
        let unit = spawn_unit_with_equipment_and_org(&mut world, UnitFlags::default(), 0, 10, 50.0);

        run(&mut world, world_entity, &settings);

        let stats = world.get::<&UnitStats>(unit).unwrap();
        assert!(
            (stats.organisation - 50.0).abs() < 0.01,
            "zero supply gives zero org recovery"
        );
    }
}
