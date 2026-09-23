//! I8: lazy spectator union vision grid cache (see struct docs).

use std::sync::Arc;

// ---------------------------------------------------------------------------
// I8: lazy spectator union vision grid cache
// ---------------------------------------------------------------------------

/// I8: cached union of every player's vision grid, for the spectator network
/// path. `union_vision_grid_flat` allocates a fresh ~1 MB `Vec` + P×1 MB OR
/// on every call; this cache recomputes it only when the pid list or any
/// source per-player vision grid `Arc` changes.
///
/// Correctness rests on the influence double-buffer invariant
/// (`components.rs`): published vision-grid `Arc`s are never mutated in
/// place — a dirty run allocates fresh grids and flips ALL published `Arc`
/// refs at once. So `Arc::ptr_eq` across the source list misses on every
/// grid-dirty run and hits exactly on grid-clean ticks.
pub(crate) struct SpectatorUnionCache {
    pids: Vec<String>,
    /// Per-pid vision grid `Arc` (or `None` for a pid with no grid), same
    /// order as `pids`. These are the cache keys.
    sources: Vec<Option<Arc<Vec<u8>>>>,
    /// Cached union, in the same flat layout as `union_vision_grid_flat`.
    union: Arc<Vec<u8>>,
}

impl SpectatorUnionCache {
    /// Returns the union grid for this tick: the cached `Arc` on a hit
    /// (pid list unchanged and every source `Arc` `Arc::ptr_eq`-equal), or
    /// `recompute()`'s result wrapped in a fresh cache entry on a miss.
    ///
    /// `sources` must be one entry per `pids` entry, in the same order — the
    /// per-pid `vision_grid_flat` results that `union_vision_grid_flat` ORs
    /// together.
    pub(crate) fn update(
        cache: &mut Option<Self>,
        pids: &[String],
        sources: &[Option<Arc<Vec<u8>>>],
        recompute: impl FnOnce() -> Option<Vec<u8>>,
    ) -> Option<Arc<Vec<u8>>> {
        let hit = cache.as_ref().is_some_and(|c| {
            c.pids == pids
                && c.sources.len() == sources.len()
                && c.sources
                    .iter()
                    .zip(sources.iter())
                    .all(|(a, b)| match (a, b) {
                        (None, None) => true,
                        (Some(x), Some(y)) => Arc::ptr_eq(x, y),
                        _ => false,
                    })
        });
        if hit {
            return Some(Arc::clone(&cache.as_ref().unwrap().union));
        }
        match recompute() {
            Some(union) => {
                let entry = Self {
                    pids: pids.to_vec(),
                    sources: sources.to_vec(),
                    union: Arc::new(union),
                };
                let out = Arc::clone(&entry.union);
                *cache = Some(entry);
                Some(out)
            }
            None => {
                *cache = None;
                None
            }
        }
    }
}

#[cfg(test)]
mod spectator_union_cache_tests {
    use super::SpectatorUnionCache;
    use std::sync::Arc;

    fn pid(s: &str) -> String {
        s.to_string()
    }

    /// Acceptance gate: cache hit when pids are identical and every source
    /// Arc is the same allocation — `recompute` must NOT run.
    #[test]
    fn cache_hit_on_identical_arcs_and_pids() {
        let grid_a: Arc<Vec<u8>> = Arc::new(vec![0, 1, 1, 0]);
        let grid_b: Arc<Vec<u8>> = Arc::new(vec![1, 0, 0, 1]);
        let pids = vec![pid("a"), pid("b")];
        let sources = vec![Some(Arc::clone(&grid_a)), Some(Arc::clone(&grid_b))];
        let mut cache: Option<SpectatorUnionCache> = None;

        let mut recomputes = 0;
        let make = |recomputes: &mut i32| -> Option<Vec<u8>> {
            *recomputes += 1;
            Some(vec![1, 1, 1, 1]) // a | b
        };

        let first =
            SpectatorUnionCache::update(&mut cache, &pids, &sources, || make(&mut recomputes));
        assert!(first.is_some());
        assert_eq!(recomputes, 1);

        // Same pids + same Arc allocations (fresh clones of the same Arcs).
        let sources2 = vec![Some(Arc::clone(&grid_a)), Some(Arc::clone(&grid_b))];
        let second =
            SpectatorUnionCache::update(&mut cache, &pids, &sources2, || make(&mut recomputes));
        assert!(Arc::ptr_eq(
            first.as_ref().unwrap(),
            second.as_ref().unwrap()
        ));
        assert_eq!(recomputes, 1, "hit must not recompute");
        assert_eq!(second.unwrap().as_slice(), &[1, 1, 1, 1]);
    }

    /// Acceptance gate: recompute when a source grid is a different
    /// allocation (grid-dirty run flips all published Arcs).
    #[test]
    fn recompute_on_source_arc_miss() {
        let pids = vec![pid("a"), pid("b")];
        let old_a: Arc<Vec<u8>> = Arc::new(vec![0, 1, 1, 0]);
        let old_b: Arc<Vec<u8>> = Arc::new(vec![1, 0, 0, 1]);
        let mut cache = Option::<SpectatorUnionCache>::None;

        let mut recomputes = 0;
        let make = |recomputes: &mut i32| -> Option<Vec<u8>> {
            *recomputes += 1;
            Some(vec![*recomputes as u8, 0, 0, 0]) // tag with the call count
        };

        let s_old = vec![Some(Arc::clone(&old_a)), Some(Arc::clone(&old_b))];
        let first =
            SpectatorUnionCache::update(&mut cache, &pids, &s_old, || make(&mut recomputes));
        assert_eq!(recomputes, 1);

        // New allocations (as a dirty run would produce): same VALUES, new Arcs.
        let new_a: Arc<Vec<u8>> = Arc::new(vec![0, 1, 1, 0]);
        let s_new = vec![Some(Arc::clone(&new_a)), Some(Arc::clone(&old_b))];
        assert!(!Arc::ptr_eq(&old_a, &new_a));
        let second =
            SpectatorUnionCache::update(&mut cache, &pids, &s_new, || make(&mut recomputes));
        assert_eq!(recomputes, 2, "any Arc miss must recompute");
        assert!(!Arc::ptr_eq(
            first.as_ref().unwrap(),
            second.as_ref().unwrap()
        ));

        // Cache now holds the NEW sources: identical inputs hit again.
        let third =
            SpectatorUnionCache::update(&mut cache, &pids, &s_new, || make(&mut recomputes));
        assert_eq!(recomputes, 2);
        assert!(Arc::ptr_eq(
            second.as_ref().unwrap(),
            third.as_ref().unwrap()
        ));
    }

    /// Acceptance gate: recompute on pid join (and leave) — the pid list is
    /// part of the key.
    #[test]
    fn recompute_on_pid_join_or_leave() {
        let grid_a: Arc<Vec<u8>> = Arc::new(vec![1, 0, 0, 0]);
        let grid_b: Arc<Vec<u8>> = Arc::new(vec![0, 1, 0, 0]);
        let pids_a = vec![pid("a")];
        let pids_ab = vec![pid("a"), pid("b")];
        let mut cache: Option<SpectatorUnionCache> = None;

        let mut recomputes = 0;
        let make = |recomputes: &mut i32| -> Option<Vec<u8>> {
            *recomputes += 1;
            Some(vec![*recomputes as u8; 4])
        };

        let _ =
            SpectatorUnionCache::update(&mut cache, &pids_a, &[Some(Arc::clone(&grid_a))], || {
                make(&mut recomputes)
            });
        assert_eq!(recomputes, 1);

        // b joins: even though a's Arc is unchanged, the union must refresh.
        let _ = SpectatorUnionCache::update(
            &mut cache,
            &pids_ab,
            &[Some(Arc::clone(&grid_a)), Some(Arc::clone(&grid_b))],
            || make(&mut recomputes),
        );
        assert_eq!(recomputes, 2, "pid join must recompute");

        // b leaves again.
        let _ =
            SpectatorUnionCache::update(&mut cache, &pids_a, &[Some(Arc::clone(&grid_a))], || {
                make(&mut recomputes)
            });
        assert_eq!(recomputes, 3, "pid leave must recompute");

        // Hit again on the (now cached) single-pid state.
        let _ =
            SpectatorUnionCache::update(&mut cache, &pids_a, &[Some(Arc::clone(&grid_a))], || {
                make(&mut recomputes)
            });
        assert_eq!(recomputes, 3);
    }

    /// A pid with no grid (`None` source) is part of the key: None → Some and
    /// Some → None must both recompute.
    #[test]
    fn none_sources_are_part_of_the_key() {
        let grid_a: Arc<Vec<u8>> = Arc::new(vec![1, 1, 0, 0]);
        let pids = vec![pid("a"), pid("b")];
        let mut cache: Option<SpectatorUnionCache> = None;

        let mut recomputes = 0;
        let make = |recomputes: &mut i32| -> Option<Vec<u8>> {
            *recomputes += 1;
            Some(vec![*recomputes as u8; 4])
        };

        let none_b = vec![Some(Arc::clone(&grid_a)), None];
        let _ = SpectatorUnionCache::update(&mut cache, &pids, &none_b, || make(&mut recomputes));
        assert_eq!(recomputes, 1);

        // b gains a grid.
        let grid_b: Arc<Vec<u8>> = Arc::new(vec![0, 0, 1, 1]);
        let some_b = vec![Some(Arc::clone(&grid_a)), Some(Arc::clone(&grid_b))];
        let _ = SpectatorUnionCache::update(&mut cache, &pids, &some_b, || make(&mut recomputes));
        assert_eq!(recomputes, 2, "None -> Some must recompute");

        // b loses it again.
        let _ = SpectatorUnionCache::update(&mut cache, &pids, &none_b, || make(&mut recomputes));
        assert_eq!(recomputes, 3, "Some -> None must recompute");

        // Hit again.
        let _ = SpectatorUnionCache::update(&mut cache, &pids, &none_b, || make(&mut recomputes));
        assert_eq!(recomputes, 3);
    }

    /// Recompute returning `None` (no world data / no players) clears the
    /// cache and returns `None`.
    #[test]
    fn recompute_none_clears_cache() {
        let grid_a: Arc<Vec<u8>> = Arc::new(vec![1, 0, 0, 0]);
        let pids = vec![pid("a")];
        let mut cache: Option<SpectatorUnionCache> = None;
        let _ =
            SpectatorUnionCache::update(&mut cache, &pids, &[Some(Arc::clone(&grid_a))], || {
                Some(vec![1; 4])
            });
        assert!(cache.is_some());

        // Empty pid list forces a miss; recompute -> None must clear the cache.
        let out = SpectatorUnionCache::update(&mut cache, &[], &[], || None);
        assert!(out.is_none());
        assert!(cache.is_none(), "recompute None must clear the cache");
    }
}
