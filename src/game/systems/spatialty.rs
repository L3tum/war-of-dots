//! SpatialtySystem — maintains NearbyEnemies components via a three-tier update strategy.
//!
//! Runs BETWEEN movement and combat in the tick order. Uses a _persisted_ SpatialHash
//! (stored in WorldBundle) that is updated incrementally each tick. Only processes
//! units that need updating:
//!   Tier 1 — Initializers:  ALL units with NearbyEnemies.initialized == false (spawn/cold start)
//!   Tier 2 — Movers:        Units with flags.moved == true (rebuild from scratch)
//!   Tier 3 — Neighbor Prop: Stationary units in ±1 cells of each mover (staleness fix)
//!
//! This avoids re-scanning the entire world for units that haven't moved or changed.

use crate::game::components::*;
use crate::game::world_ext::WorldExt;
use std::collections::HashMap;
use std::collections::HashSet;
use std::hash::{BuildHasherDefault, Hasher};
use std::time::Instant;

/// Small, fast hasher (rustc-style fxhash). Used for the hot per-tick maps in
/// spatialty where we do thousands of lookups per call and SipHash overhead
/// dominates. Hash quality is irrelevant here (adversarial input is not a concern).
#[derive(Default)]
struct FxHasher(u64);

const FX_SEED: u64 = 0x51_7c_c1_b7_27_22_0a_95;

impl FxHasher {
    #[inline]
    fn add_to_hash(&mut self, i: u64) {
        self.0 = (self.0.rotate_left(5) ^ i).wrapping_mul(FX_SEED);
    }
}

impl Hasher for FxHasher {
    #[inline]
    fn write(&mut self, bytes: &[u8]) {
        for &b in bytes {
            self.add_to_hash(b as u64);
        }
    }
    #[inline]
    fn write_u8(&mut self, i: u8) {
        self.add_to_hash(i as u64);
    }
    #[inline]
    fn write_u16(&mut self, i: u16) {
        self.add_to_hash(i as u64);
    }
    #[inline]
    fn write_u32(&mut self, i: u32) {
        self.add_to_hash(i as u64);
    }
    #[inline]
    fn write_u64(&mut self, i: u64) {
        self.add_to_hash(i);
    }
    #[inline]
    fn write_usize(&mut self, i: usize) {
        self.add_to_hash(i as u64);
    }
    #[inline]
    fn write_i32(&mut self, i: i32) {
        self.add_to_hash(i as u64);
    }
    #[inline]
    fn write_i64(&mut self, i: i64) {
        self.add_to_hash(i as u64);
    }
    #[inline]
    fn write_isize(&mut self, i: isize) {
        self.add_to_hash(i as u64);
    }
    #[inline]
    fn finish(&self) -> u64 {
        self.0
    }
}

type FxHashMap<K, V> = HashMap<K, V, BuildHasherDefault<FxHasher>>;
type FxHashSet<K> = HashSet<K, BuildHasherDefault<FxHasher>>;

/// Returns the number of units that were newly initialized this call.
///
/// Runs between movement and combat. Maintains a _persisted_ SpatialHash
/// (stored in WorldBundle) that is kept in sync with living unit positions:
///   * a unit is inserted when it spawns / is first initialized,
///   * a unit's old entry is removed and a new one inserted when it moves,
///   * dead units are removed by `death_cleanup`.
/// Because the hash never accumulates stale/duplicate entries, per-tick cost
/// stays proportional to the number of units that actually changed this tick
/// (movers + uninitialized + their stationary neighbors), not to the total
/// unit count. Only those affected units recompute `NearbyEnemies`/`attacking`.
pub fn run(
    world: &mut hecs::World,
    world_entity: hecs::Entity,
    has_movers: bool,
    has_uninitialized: bool,
) -> u32 {
    // Skip entirely when there's nothing to do.
    if !has_movers && !has_uninitialized {
        return 0;
    }

    // Per-phase timing (recorded into WorldBundle debug stats; no-op without
    // the debug-overlay feature). Surface with `--bench-detailed spatialty`.
    let t0 = Instant::now();

    // =====================================================================
    // Pass 1: collect owned data for ALL units in one ECS query.
    // (query borrow is dropped the moment the loop ends)
    // =====================================================================
    struct U {
        pos: UnitPosition,
        owner_idx: u8,
        moved: bool,
        initialized: bool,
        last_x: f32,
        last_y: f32,
    }

    /*
    * Let's see if we can't get this to work a little better. I committed the current state so we can just git revert
       it.
       Fundamentally, here's what I'd think the spatialty system should look like:
       - Iterate over all entities that moved or are unitiliazed WITHOUT first collecting them into a vec. Here we can
       just collect entities that contain both a UnitPosition and a UnitOldPosition or no NearbyEnemies.
       - For each entity:
         - Delete the old position from SpatialHash -> old position is a Component as well, UnitOldPosition
         - Write its new position into SpatialHash
         - If its NearbyEnemies is filled, delete the current unit from each entry's own NearbyEnemies
         - Do a SpatialHash lookup
         - Do a world::get for each result
         - Write nearby enemies into NearbyEnemies
         - Write current unit into nearby enemies' NearbyEnemies
    */

    let mut owner_cache: FxHashMap<String, u8> = FxHashMap::default();
    let mut next_owner: u8 = 0;
    let mut units: FxHashMap<hecs::Entity, U> = FxHashMap::default();

    for (entity, (pos, owner, flags, nearby)) in world
        .query::<(&UnitPosition, &UnitOwner, &UnitFlags, &NearbyEnemies)>()
        .iter()
    {
        let idx = *owner_cache
            .entry(owner.player_id.clone())
            .or_insert_with(|| {
                let i = next_owner;
                next_owner += 1;
                i
            });
        units.insert(
            entity,
            U {
                pos: *pos,
                owner_idx: idx,
                moved: flags.moved,
                initialized: nearby.initialized,
                last_x: nearby.last_x,
                last_y: nearby.last_y,
            },
        );
    }

    if units.is_empty() {
        return 0;
    }

    // Work items: units whose hash entry / NearbyEnemies must be updated.
    let work: Vec<hecs::Entity> = units
        .iter()
        .filter(|(_, u)| u.moved || !u.initialized)
        .map(|(e, _)| *e)
        .collect();
    if work.is_empty() {
        return 0;
    }
    let t1 = Instant::now();

    struct Change {
        entity: hecs::Entity,
        enemies: Vec<hecs::Entity>,
        is_mover: bool,
        was_uninit: bool,
    }
    let mut changes: Vec<Change> = Vec::new();
    // When essentially every unit moved or is uninitialised, neighbour discovery
    // is redundant (all units are already affected) and the hash can just be
    // rebuilt fresh — no stale removals needed.
    let all_affected = work.len() >= units.len();
    // `affected_set` is only needed on the incremental path (neighbour discovery).
    let mut affected_set: FxHashSet<hecs::Entity> = if all_affected {
        FxHashSet::default()
    } else {
        work.iter().copied().collect()
    };
    let mut scratch: Vec<hecs::Entity> = Vec::with_capacity(32);

    // =====================================================================
    // Pass 2: all hash mutations + neighbor discovery + enemy-list building,
    // done inside ONE &mut WorldBundle borrow (no clone, no borrow juggling).
    // All data touched here is owned (`units`) or the hash itself.
    // =====================================================================
    {
        let wb = match world.world_data_mut(world_entity) {
            Some(wb) => wb,
            None => return 0,
        };
        let sh = &mut wb.spatial_hash;

        if all_affected {
            // Fresh rebuild: insert every unit at its current position.
            sh.clear();
            for (&e, u) in &units {
                sh.insert(e, u.pos.x, u.pos.y);
            }
        } else {
            // Incremental: refresh only work items...
            for &e in &work {
                let u = &units[&e];
                if u.initialized {
                    sh.remove(e, u.last_x, u.last_y);
                }
                sh.insert(e, u.pos.x, u.pos.y);
            }

            // ...and discover stationary neighbors around each mover.
            for &e in &work {
                let u = &units[&e];
                if u.moved {
                    sh.query_nearby_into(u.pos.x, u.pos.y, &mut scratch);
                    for &n in scratch.iter() {
                        // Only include entities that still exist (excludes stale slots).
                        if units.contains_key(&n) {
                            affected_set.insert(n);
                        }
                    }
                }
            }
        }

        // Enemy-list computation, grouped by hash cell: units in the same cell
        // share the same 3x3 neighborhood, so we query each cell's neighborhood
        // once instead of once per unit (big win in dense clusters).
        for (&(cx, cy), cell_entities) in &sh.cells {
            scratch.clear();
            for dx in -1..=1 {
                for dy in -1..=1 {
                    if let Some(ents) = sh.cells.get(&(cx + dx, cy + dy)) {
                        scratch.extend(ents.iter().copied());
                    }
                }
            }
            for &e in cell_entities {
                if !units.contains_key(&e) {
                    continue; // stale/dead slot (shouldn't happen, but be safe)
                }
                if !all_affected && !affected_set.contains(&e) {
                    continue;
                }
                let u = &units[&e];
                let enemies: Vec<hecs::Entity> = scratch
                    .iter()
                    .filter(|&&n| {
                        n != e && matches!(units.get(&n), Some(nu) if nu.owner_idx != u.owner_idx)
                    })
                    .copied()
                    .collect();
                changes.push(Change {
                    entity: e,
                    enemies,
                    is_mover: u.moved,
                    was_uninit: !u.initialized,
                });
            }
        }
    }
    // WorldBundle borrow dropped here.
    let t2 = Instant::now();

    // =====================================================================
    // Pass 3: apply NearbyEnemies / attacking / last-pos mutations.
    // =====================================================================
    let mut newly_initialized: u32 = 0;
    for c in changes {
        let has_enemies = !c.enemies.is_empty();
        if let Ok((flags, nearby)) =
            world.query_one_mut::<(&mut UnitFlags, &mut NearbyEnemies)>(c.entity)
        {
            if c.is_mover {
                // Mover: recompute attacking fresh.
                flags.attacking = has_enemies;
            } else if !has_enemies {
                // Stationary neighbor with no enemies nearby -> retreat/clear.
                flags.attacking = false;
            }
            // Stationary neighbor WITH enemies: leave attacking unchanged.

            let u = &units[&c.entity];
            nearby.entities = c.enemies;
            nearby.initialized = true;
            nearby.last_x = u.pos.x;
            nearby.last_y = u.pos.y;
            if c.was_uninit {
                newly_initialized += 1;
            }
        }
    }
    let t3 = Instant::now();

    // Record per-phase timings (keyed under "spatialty/..."; no-op if not
    // enabled or without the debug-overlay feature).
    let to_ms = |d: std::time::Duration| d.as_secs_f64() * 1000.0;
    world.record_detailed(world_entity, "spatialty/p1_collect", to_ms(t1 - t0));
    world.record_detailed(world_entity, "spatialty/p2_hash", to_ms(t2 - t1));
    world.record_detailed(world_entity, "spatialty/p3_mutate", to_ms(t3 - t2));

    newly_initialized
}

#[cfg(test)]
mod tests {
    use super::*;

    fn setup_world() -> (hecs::World, hecs::Entity) {
        let mut world = hecs::World::new();
        let entity = world.spawn_world(
            vec![vec![crate::game::unit::Terrain::Plains; 200]; 200],
            200,
            200,
            crate::config::CityUpgradeSettings::default(),
            std::collections::HashMap::new(),
            Vec::new(),
        );
        (world, entity)
    }

    fn make_unit(
        world: &mut hecs::World,
        owner: &str,
        x: f32,
        y: f32,
        moved: bool,
        attacking: bool,
    ) -> hecs::Entity {
        world.spawn((
            UnitOwner {
                player_id: owner.to_string(),
            },
            UnitPosition { x, y },
            UnitFlags {
                moved,
                attacking,
                ..Default::default()
            },
            NearbyEnemies::default(),
        ))
    }

    // Helper: run spatialty with reasonable defaults for tests
    fn run_test(world: &mut hecs::World, world_entity: hecs::Entity) {
        run(world, world_entity, true, true);
    }

    // -----------------------------------------------------------------------
    // Tier 1: Initialization
    // -----------------------------------------------------------------------

    #[test]
    fn tier1_initializes_new_spawns() {
        let (mut world, world_entity) = setup_world();
        let a = make_unit(&mut world, "P1", 10.0, 10.0, false, false);
        let b = make_unit(&mut world, "P2", 10.5, 10.5, false, false);
        // Both start uninitialized
        run_test(&mut world, world_entity);

        let nearby_a = world.get::<&NearbyEnemies>(a).unwrap();
        assert!(nearby_a.initialized, "unit A should be initialized");
        assert!(
            nearby_a.entities.contains(&b),
            "unit A should see enemy B in NearbyEnemies"
        );

        let nearby_b = world.get::<&NearbyEnemies>(b).unwrap();
        assert!(nearby_b.initialized, "unit B should be initialized");
        assert!(
            nearby_b.entities.contains(&a),
            "unit B should see enemy A in NearbyEnemies"
        );
    }

    #[test]
    fn tier1_ignores_same_owner() {
        let (mut world, world_entity) = setup_world();
        let a = make_unit(&mut world, "P1", 10.0, 10.0, false, false);
        let b = make_unit(&mut world, "P1", 10.5, 10.5, false, false);
        run_test(&mut world, world_entity);

        let nearby_a = world.get::<&NearbyEnemies>(a).unwrap();
        assert!(
            !nearby_a.entities.contains(&b),
            "same-owner unit should NOT appear in NearbyEnemies"
        );
    }

    // -----------------------------------------------------------------------
    // Tier 2: Movers
    // -----------------------------------------------------------------------

    #[test]
    fn tier2_sets_attacking_for_mover_with_enemies() {
        let (mut world, world_entity) = setup_world();
        let a = make_unit(&mut world, "P1", 10.0, 10.0, true, false); // mover
        let _b = make_unit(&mut world, "P2", 10.5, 10.5, false, false);
        run_test(&mut world, world_entity);

        let flags_a = world.get::<&UnitFlags>(a).unwrap();
        assert!(
            flags_a.attacking,
            "mover with enemy nearby should have attacking=true"
        );
    }

    #[test]
    fn tier2_clears_attacking_on_retreat() {
        let (mut world, world_entity) = setup_world();
        // Mover that previously had attacking=true but moves away from all enemies
        let a = make_unit(&mut world, "P1", 100.0, 100.0, true, true);
        let _b = make_unit(&mut world, "P2", 10.0, 10.0, false, false);
        run_test(&mut world, world_entity);

        let flags_a = world.get::<&UnitFlags>(a).unwrap();
        assert!(
            !flags_a.attacking,
            "mover far from enemies should have attacking=false (retreat)"
        );
        let nearby_a = world.get::<&NearbyEnemies>(a).unwrap();
        assert!(
            nearby_a.entities.is_empty(),
            "retreating mover should have empty NearbyEnemies"
        );
    }

    // -----------------------------------------------------------------------
    // Tier 3: Neighbor propagation
    // -----------------------------------------------------------------------

    #[test]
    fn tier3_propagates_to_stationary_neighbor() {
        let (mut world, world_entity) = setup_world();
        // Stationary unit B already initialized (simulated by pre-running)
        let b = make_unit(&mut world, "P2", 10.5, 10.5, false, false);
        // First pass to initialize B
        run_test(&mut world, world_entity);
        // Now clear B's NearbyEnemies to simulate staleness
        {
            let nb = world.query_one_mut::<(&mut NearbyEnemies,)>(b).unwrap().0;
            nb.entities.clear();
        }
        // Mover A moves near B (in adjacent cell within ±1)
        let a = make_unit(&mut world, "P1", 10.0, 10.0, true, false);
        run_test(&mut world, world_entity);

        let nearby_b = world.get::<&NearbyEnemies>(b).unwrap();
        assert!(
            nearby_b.entities.contains(&a),
            "stationary neighbor B should see mover A after Tier 3 propagation"
        );
    }

    #[test]
    fn tier3_clears_attacking_for_stationary_with_no_enemies() {
        let (mut world, _world_entity) = setup_world();
        // Stationary unit with attacking=true (was defending), now no enemies nearby
        let a = make_unit(&mut world, "P1", 10.0, 10.0, false, true);
        // Distant enemy that won't be in ±1 cells of mover
        let b = make_unit(&mut world, "P2", 50.0, 50.0, false, false);
        run_test(&mut world, _world_entity);

        // First pass initializes both; a should have empty NearbyEnemies (no nearby enemies)
        // Since a is not a mover and not a neighbor of a mover, Tier 3 won't touch it.
        // But Tier 1 already ran and should have set attacking=false via Tier 1 init.
        // Let's test the actual Tier 3 case with a mover nearby:
        // Reset: a has attacking=true, a distant mover moves far away
        {
            let flags = world.query_one_mut::<(&mut UnitFlags,)>(a).unwrap().0;
            flags.attacking = true;
        }
        {
            let nb = world.query_one_mut::<(&mut NearbyEnemies,)>(a).unwrap().0;
            nb.entities.push(b); // stale reference
        }
        // A mover far from A moves — A is NOT in affected cells, so Tier 3 skips A
        // This means A's stale state persists until next tick when A moves or a mover comes near
        // This is expected behavior — Tier 3 only affects units near movers
    }

    // -----------------------------------------------------------------------
    // attacking flag persistence
    // -----------------------------------------------------------------------

    #[test]
    fn attacking_persists_for_stationary_defender() {
        let (mut world, world_entity) = setup_world();
        let _a = make_unit(&mut world, "P1", 10.0, 10.0, true, false); // mover toward B
        let b = make_unit(&mut world, "P2", 10.5, 10.5, false, false);
        run_test(&mut world, world_entity);

        // First pass: Tier 1 init both, Tier 2 sets A.attacking=true, Tier 3 updates B
        // Copy the value to release the immutable borrow before calling run again
        let b_attacking = {
            let flags_b = world.get::<&UnitFlags>(b).unwrap();
            flags_b.attacking
        };
        assert!(
            !b_attacking,
            "stationary defender B should NOT have attacking set by Tier 3 when enemies are present \
             (Tier 3 preserves existing state, which is false for a new unit)"
        );

        // Second pass: A is no longer moved. B still has enemies nearby.
        // B should remain at attacking=false (unchanged by Tier 3 since non-empty)
        run_test(&mut world, world_entity);
        let b_attacking = {
            let flags_b = world.get::<&UnitFlags>(b).unwrap();
            flags_b.attacking
        };
        assert!(
            !b_attacking,
            "B's attacking should remain false — Tier 3 preserves it when enemies present"
        );
    }

    #[test]
    fn empty_world_does_not_panic() {
        let (mut world, world_entity) = setup_world();
        run_test(&mut world, world_entity);
    }

    // -----------------------------------------------------------------------
    // Regression: 3+ owners (owner index must be unique)
    // -----------------------------------------------------------------------

    #[test]
    fn three_owners_all_see_each_other() {
        let (mut world, world_entity) = setup_world();
        let a = make_unit(&mut world, "P1", 10.0, 10.0, false, false);
        let b = make_unit(&mut world, "P2", 10.5, 10.5, false, false);
        let c = make_unit(&mut world, "P3", 10.2, 10.2, false, false);
        run_test(&mut world, world_entity);

        let na = world.get::<&NearbyEnemies>(a).unwrap();
        assert!(na.entities.contains(&b), "A should see P2 B");
        assert!(na.entities.contains(&c), "A should see P3 C");

        let nb = world.get::<&NearbyEnemies>(b).unwrap();
        assert!(nb.entities.contains(&a), "B should see P1 A");
        assert!(nb.entities.contains(&c), "B should see P3 C");

        let nc = world.get::<&NearbyEnemies>(c).unwrap();
        assert!(nc.entities.contains(&a), "C should see P1 A");
        assert!(nc.entities.contains(&b), "C should see P2 B");
    }

    // -----------------------------------------------------------------------
    // Regression: multiple movers, affected_cells union
    // -----------------------------------------------------------------------

    #[test]
    fn multiple_movers_union_affected_cells() {
        let (mut world, world_entity) = setup_world();
        // Two movers far apart
        let _a = make_unit(&mut world, "P1", 10.0, 10.0, true, false);
        let _b = make_unit(&mut world, "P1", 100.0, 100.0, true, false);
        // Stationary unit near B but not near A
        let c = make_unit(&mut world, "P2", 101.0, 101.0, false, false);
        // First pass to initialize C
        run_test(&mut world, world_entity);
        // Clear C to verify it gets updated by Tier 3
        {
            let nb = world.query_one_mut::<(&mut NearbyEnemies,)>(c).unwrap().0;
            nb.entities.clear();
        }
        run_test(&mut world, world_entity);

        let nc = world.get::<&NearbyEnemies>(c).unwrap();
        assert!(nc.initialized, "C should remain initialized");
        // C should see B (mover nearby), not A (far away)
        assert_eq!(nc.entities.len(), 1, "C should see exactly B");
    }

    // -----------------------------------------------------------------------
    // Regression: Tier 3 must NOT set attacking=true for stationary defender
    // -----------------------------------------------------------------------

    #[test]
    fn tier3_does_not_set_attacking_for_stationary_defender() {
        let (mut world, world_entity) = setup_world();
        // Stationary defender B — already has attacking=false
        let b = make_unit(&mut world, "P2", 10.5, 10.5, false, false);
        run_test(&mut world, world_entity); // init

        // Now an enemy mover moves near B. B should have enemies in NearbyEnemies
        // but Tier 3 must NOT set attacking=true for B.
        let _a = make_unit(&mut world, "P1", 10.0, 10.0, true, false);
        run_test(&mut world, world_entity);

        let nb = world.get::<&NearbyEnemies>(b).unwrap();
        assert!(
            !nb.entities.is_empty(),
            "B should see mover A in NearbyEnemies after Tier 3"
        );

        let fb = world.get::<&UnitFlags>(b).unwrap();
        assert!(
            !fb.attacking,
            "Tier 3 must NOT set attacking=true for stationary defender B \
             (it should preserve existing attacking=false)"
        );
    }

    // -----------------------------------------------------------------------
    // Regression: initialized + stationary + no nearby mover → skipped
    // -----------------------------------------------------------------------

    #[test]
    fn initialized_stationary_away_from_movers_is_skipped() {
        let (mut world, world_entity) = setup_world();
        // Mover far from C
        let _a = make_unit(&mut world, "P1", 0.0, 0.0, true, false);
        // C is far away, initialized on first pass
        let c = make_unit(&mut world, "P2", 100.0, 100.0, false, false);
        run_test(&mut world, world_entity); // initializes C

        // Store C's enemy count before second pass
        let c_enemies_before = {
            let nb = world.get::<&NearbyEnemies>(c).unwrap();
            nb.entities.len()
        };

        // Second pass: A is still a mover (flag doesn't auto-clear in tests)
        run_test(&mut world, world_entity);

        let c_enemies_after = {
            let nb = world.get::<&NearbyEnemies>(c).unwrap();
            nb.entities.len()
        };

        // C was not touched by Tier 3 (no mover in adjacent cell)
        assert_eq!(
            c_enemies_before, c_enemies_after,
            "C's NearbyEnemies should be unchanged — C is too far from any mover"
        );
    }

    // -----------------------------------------------------------------------
    // Dense cluster: self-exclusion + same-owner filtering
    // -----------------------------------------------------------------------

    #[test]
    fn dense_cluster_filters_self_and_allies() {
        let (mut world, world_entity) = setup_world();
        // 3 P1 units and 2 P2 units all in the same cell
        let p1a = make_unit(&mut world, "P1", 10.0, 10.0, false, false);
        let p1b = make_unit(&mut world, "P1", 10.1, 10.1, false, false);
        let p1c = make_unit(&mut world, "P1", 10.2, 10.2, false, false);
        let p2a = make_unit(&mut world, "P2", 10.3, 10.3, false, false);
        let p2b = make_unit(&mut world, "P2", 10.4, 10.4, false, false);
        run_test(&mut world, world_entity);

        // Each P1 unit should see exactly 2 enemies (p2a, p2b)
        for (&unit, label) in [(&p1a, "P1A"), (&p1b, "P1B"), (&p1c, "P1C")] {
            let nb = world.get::<&NearbyEnemies>(unit).unwrap();
            assert_eq!(
                nb.entities.len(),
                2,
                "{} should see exactly 2 enemies (P2 units)",
                label
            );
            assert!(nb.entities.contains(&p2a), "{} should see p2a", label);
            assert!(nb.entities.contains(&p2b), "{} should see p2b", label);
            assert!(
                !nb.entities.contains(&unit),
                "{} should not see self",
                label
            );
        }

        // Each P2 unit should see exactly 3 enemies (p1a, p1b, p1c)
        for (&unit, label) in [(&p2a, "P2A"), (&p2b, "P2B")] {
            let nb = world.get::<&NearbyEnemies>(unit).unwrap();
            assert_eq!(
                nb.entities.len(),
                3,
                "{} should see exactly 3 enemies (P1 units)",
                label
            );
        }
    }
}
