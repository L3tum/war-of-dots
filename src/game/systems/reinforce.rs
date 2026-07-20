//! ReinforcementSystem — org reinforcement with supply malus.
//!
//! Uses two-phase pattern: query + collect changes → apply mutations in batch.

use crate::game::components::*;
use hecs::World;

pub fn run(world: &mut World, _world_entity: hecs::Entity) {
    // Phase 1: query all components, compute new org values
    let changes: Vec<(hecs::Entity, f32)> = world
        .query::<(&UnitStats, &UnitEquipment)>()
        .iter()
        .filter_map(|(entity, (stats, equipment))| {
            let supply_pct = supply_percent(&equipment.equipment, &equipment.max_equipment);
            let malus = if supply_pct > 50.0 {
                1.0
            } else {
                supply_pct / 50.0
            };
            let reinforce = stats.reinforcement_rate * malus;
            let new_org = (stats.organisation + reinforce).min(stats.max_organisation);
            if new_org != stats.organisation {
                Some((entity, new_org))
            } else {
                None
            }
        })
        .collect();

    // Phase 2: apply changes
    for (entity, new_org) in changes {
        if let Ok((ms,)) = world.query_one_mut::<(&mut UnitStats,)>(entity) {
            ms.organisation = new_org;
        }
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
