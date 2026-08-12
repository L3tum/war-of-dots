//! Extension trait for hecs::World with game-specific spawn helpers.

use crate::game::components::*;
use crate::game::economy::{City, UnitType};
use hecs::World;
use std::collections::HashMap;

pub trait WorldExt {
    fn spawn_world(
        &mut self,
        terrain: Vec<Vec<crate::game::unit::Terrain>>,
        width: u32,
        height: u32,
        city_upgrade_settings: crate::config::CityUpgradeSettings,
        funds: HashMap<String, u32>,
        cities: Vec<City>,
    ) -> hecs::Entity;

    fn spawn_unit(
        &mut self,
        unit_type: &UnitType,
        owner: &str,
        position: (f32, f32),
    ) -> hecs::Entity;

    fn world_data(&self, world_entity: hecs::Entity) -> Option<hecs::Ref<'_, WorldBundle>>;

    fn world_data_clone(&self, world_entity: hecs::Entity) -> Option<WorldBundle>;

    fn world_data_mut(&mut self, world_entity: hecs::Entity) -> Option<&mut WorldBundle>;

    fn terrain_at(&self, world_entity: hecs::Entity, x: f32, y: f32) -> crate::game::unit::Terrain;

    fn player_funds(&self, world_entity: hecs::Entity, player_id: &str) -> u32;

    /// Record a detailed sub-part timing (only active when system is in detailed mode).
    #[cfg(any(debug_assertions, feature = "debug-overlay"))]
    fn record_detailed(&mut self, world_entity: hecs::Entity, key: &str, duration_ms: f64);

    #[cfg(not(any(debug_assertions, feature = "debug-overlay")))]
    fn record_detailed(&mut self, _world_entity: hecs::Entity, _key: &str, _duration_ms: f64) {}
}

impl WorldExt for World {
    fn spawn_world(
        &mut self,
        terrain: Vec<Vec<crate::game::unit::Terrain>>,
        width: u32,
        height: u32,
        city_upgrade_settings: crate::config::CityUpgradeSettings,
        funds: HashMap<String, u32>,
        cities: Vec<City>,
    ) -> hecs::Entity {
        let mut bundle = WorldBundle::new(terrain, width, height, city_upgrade_settings, funds);
        bundle.cities.cities = cities;
        self.spawn((bundle,))
    }

    fn spawn_unit(
        &mut self,
        unit_type: &UnitType,
        owner: &str,
        position: (f32, f32),
    ) -> hecs::Entity {
        self.spawn((
            UnitOwner {
                player_id: owner.to_string(),
            },
            UnitTypeId {
                name: unit_type.name.clone(),
            },
            UnitDisplay {
                symbol: unit_type.symbol.clone(),
            },
            UnitPosition {
                x: position.0,
                y: position.1,
            },
            UnitPath::default(),
            UnitStats {
                men: unit_type.men as f32,
                max_men: unit_type.men as f32,
                organisation: unit_type.max_organisation,
                max_organisation: unit_type.max_organisation,
            },
            UnitEquipment {
                equipment: unit_type.equipment.clone(),
                max_equipment: unit_type.equipment.clone(),
                supply_buffer_percent: unit_type.supply_buffer_percent,
                supply_rate: unit_type.supply_rate,
                supply: unit_type.max_supply as f32,
                max_supply: unit_type.max_supply as f32,
            },
            UnitFlags::default(),
            UnitWaterTransition::default(),
            // NOTE: no NearbyEnemies at spawn — its ABSENCE marks the unit as
            // "uninitialized"; Spatialty creates it on the first pass.
        ))
    }

    fn world_data(&self, world_entity: hecs::Entity) -> Option<hecs::Ref<'_, WorldBundle>> {
        self.get::<&WorldBundle>(world_entity).ok()
    }

    fn world_data_clone(&self, world_entity: hecs::Entity) -> Option<WorldBundle> {
        self.get::<&WorldBundle>(world_entity)
            .ok()
            .map(|r| (*r).clone())
    }

    fn world_data_mut(&mut self, world_entity: hecs::Entity) -> Option<&mut WorldBundle> {
        match self.query_one_mut::<(&mut WorldBundle,)>(world_entity) {
            Ok((wb,)) => Some(wb),
            Err(_) => None,
        }
    }

    fn terrain_at(&self, world_entity: hecs::Entity, x: f32, y: f32) -> crate::game::unit::Terrain {
        self.get::<&WorldBundle>(world_entity)
            .ok()
            .map(|wb| wb.terrain.get(x, y))
            .unwrap_or(crate::game::unit::Terrain::Plains)
    }

    fn player_funds(&self, world_entity: hecs::Entity, player_id: &str) -> u32 {
        self.get::<&WorldBundle>(world_entity)
            .ok()
            .and_then(|wb| wb.funds.funds.get(player_id).copied())
            .unwrap_or(0)
    }

    #[cfg(any(debug_assertions, feature = "debug-overlay"))]
    fn record_detailed(&mut self, world_entity: hecs::Entity, key: &str, duration_ms: f64) {
        let parent = key.split('/').next().unwrap_or("");
        if let Some(wb) = self.world_data_mut(world_entity) {
            if !wb.debug_stats.detailed_systems.contains(parent) {
                return;
            }
            let timings = &mut wb.debug_stats.detailed_timings;
            let entry = timings.entry(key.to_string()).or_insert_with(|| {
                crate::game::components::SystemTimings {
                    total_ms: 0.0,
                    call_count: 0,
                    last_ms: 0.0,
                    avg_ms: 0.0,
                    durations_ms: Vec::new(),
                }
            });
            entry.total_ms += duration_ms;
            entry.call_count += 1;
            entry.last_ms = duration_ms;
            entry.avg_ms = entry.total_ms / entry.call_count as f64;
            if entry.durations_ms.len() > 200 {
                entry.durations_ms.remove(0);
            }
            entry.durations_ms.push(duration_ms);
        }
    }
}
