//! Tick work state — the tick loop's pre-allocated scratch, dirtiness flags,
//! and the persistent `TickContext`.
//!
//! Extracted from `game.rs` (Arch #3): every field `Game` used to service the
//! tick loop (scratch buffers, dirtiness flags, settings-derived unit-type
//! indexes, the persistent `TickContext`) is grouped here, together with the
//! construction logic that built them in `Game::new`.

use crate::game::game::{GameSettings, TickContext, TickContextDirty};
use crate::game::spatial;
use crate::game::systems;
use std::collections::HashMap;

/// Tick-loop work state: pre-allocated scratch buffers (cleared and reused
/// each tick — no per-tick heap allocation at steady state), persistent
/// dirtiness flags, and the persisted [`TickContext`].
pub(crate) struct TickWorkState {
    /// Pre-allocated combat scratch, cleared and reused each tick.
    pub(crate) combat_scratch: systems::combat::CombatScratch,
    /// Pre-allocated supply/attrition scratch, cleared and reused each tick.
    pub(crate) supply_scratch: systems::supply::SupplyScratch,
    /// Spatial grid buffers for the combat mover-scan (target acquisition),
    /// resized on growth and reused each tick (no per-tick heap allocation at
    /// steady state). Built/updated by movement, consumed by combat.
    pub(crate) grid_index_scratch: spatial::GridIndexScratch,
    /// Reused scratch buffers for the encirclement flood-fill.
    pub(crate) encirclement_scratch: systems::encirclement::EncirclementScratch,
    /// Economy scratch: (player_id, funds_earned) per player, cleared and reused
    /// each economy tick instead of building a fresh HashMap.
    pub(crate) economy_earned: Vec<(String, u32)>,
    /// Unit-type name -> index into `settings.unit_types`, built once at
    /// `Game::new` so the supply system does not rebuild the map every tick.
    pub(crate) unit_types_index: HashMap<String, usize>,
    /// Sum of `max_count` across each unit type's equipment template — one
    /// entry per index into `settings.unit_types`. The per-unit equipment
    /// `needed` denominator is invariant (built from this template at spawn,
    /// never mutated), so supply reads this instead of re-summing per unit per
    /// tick (S-1). Same order as `settings.unit_types`.
    pub(crate) unit_type_total_max_equipment: Vec<f32>,
    /// Pre-allocated movement scratch, cleared and reused each tick. Holds the
    /// mover records that movement system uses.
    pub(crate) movement_scratch: systems::movement::MovementScratch,
    /// Pre-allocated water scratch, cleared and reused each tick.
    pub(crate) water_scratch: systems::water::WaterScratch,
    /// Number of units that moved in the most recent tick (from movement.rs).
    /// Used by benchmark/status reporting to gauge spatialty work.
    pub(crate) last_mover_count: usize,
    /// Number of engaged pairs in the most recent tick (from combat.rs).
    /// Used by benchmark/status reporting to gauge combat activity.
    pub(crate) last_engaged_pairs: usize,
    /// Persistent "units_by_player no longer matches the ECS" flag — set by
    /// movement, unit death, and recruitment; cleared only when a rebuild
    /// actually runs. (Not part of the per-tick ctx_dirty, which is reset at
    /// the top of every tick and would lose sets made between ticks or after
    /// the rebuild point.)
    pub(crate) units_by_player_stale: bool,
    /// Persisted TickContext, built once and updated incrementally.
    pub(crate) ctx: TickContext,
    /// Tracks which TickContext fields need updating this tick.
    pub(crate) ctx_dirty: TickContextDirty,
    /// Persistent "game state changed since last influence run" flag. Set by
    /// movement, city capture, unit death, successful recruitment, and
    /// assign_city_owner. Cleared only when influence actually runs. NOT part
    /// of the per-tick ctx_dirty (which is reset at the top of every tick).
    pub(crate) influence_dirty: bool,
    /// Set when influence runs, cleared when encirclement runs. Encirclement's
    /// inputs (borders grids, cities, units_by_player) only change via an
    /// influence run, so this is a faithful "encirclement inputs changed since
    /// last run" signal. Invariants: (i) every cities_changed source also sets
    /// influence_dirty; (ii) CITY_CAPTURE_TICK_INTERVAL % INFLUENCE_TICK_INTERVAL
    /// == 0, so the influence block always services a change before the
    /// encirclement block in the same tick.
    pub(crate) encirclement_dirty: bool,
    /// Pre-allocated influence scratch, cleared and reused each influence run.
    pub(crate) influence_scratch: systems::influence::InfluenceScratch,
    /// Pre-allocated death-cleanup scratch, cleared and reused each run.
    pub(crate) death_cleanup_scratch: systems::death_cleanup::DeathCleanupScratch,
}

impl TickWorkState {
    /// Build the tick work state: the persistent scratch buffers and
    /// dirtiness flags, the initial [`TickContext`] (built once before the
    /// world is moved into `Game`), and the settings-derived unit-type
    /// indexes (built once here so the supply/combat systems do not rebuild
    /// them every tick).
    pub(crate) fn new(
        world: &hecs::World,
        world_entity: hecs::Entity,
        settings: &GameSettings,
    ) -> Self {
        // Build initial TickContext before world is moved into Self
        let ctx = TickContext::build_initial(world, world_entity);
        // Cache unit-type name -> index once (settings is moved into Self
        // below, so build the map first). Supply indexes straight into
        // settings.unit_types.
        let unit_types_index: HashMap<String, usize> = settings
            .unit_types
            .iter()
            .enumerate()
            .map(|(i, ut)| (ut.name.clone(), i))
            .collect();
        // S-1: precompute the invariant equipment `needed` denominator per unit
        // type (sum of the template's max_count). Supply reads this instead of
        // re-summing max_equipment for every unit every tick.
        let unit_type_total_max_equipment: Vec<f32> = settings
            .unit_types
            .iter()
            .map(|ut| ut.equipment.iter().map(|e| e.max_count as f32).sum())
            .collect();

        Self {
            combat_scratch: systems::combat::CombatScratch::new(),
            supply_scratch: systems::supply::SupplyScratch::new(),
            grid_index_scratch: spatial::GridIndexScratch::default(),
            encirclement_scratch: systems::encirclement::EncirclementScratch::new(),
            economy_earned: Vec::new(),
            unit_types_index,
            unit_type_total_max_equipment,
            movement_scratch: systems::movement::MovementScratch::default(),
            water_scratch: systems::water::WaterScratch::default(),
            last_mover_count: 0,
            last_engaged_pairs: 0,
            units_by_player_stale: false,
            ctx,
            ctx_dirty: TickContextDirty::default(),
            influence_dirty: true,
            encirclement_dirty: true,
            influence_scratch: systems::influence::InfluenceScratch::new(),
            death_cleanup_scratch: systems::death_cleanup::DeathCleanupScratch::default(),
        }
    }

    /// Reset the intra-tick dirtiness at the top of every tick.
    ///
    /// Only `ctx_dirty` is per-tick. The persistent flags
    /// (`units_by_player_stale`, `influence_dirty`, `encirclement_dirty`)
    /// carry sets made between ticks (or after the rebuild point) until a
    /// consumer actually runs, so they are cleared on consumption, not here.
    pub(crate) fn reset_per_tick(&mut self) {
        self.ctx_dirty = TickContextDirty::default();
    }
}
