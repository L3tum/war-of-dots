# TODO Notes — Deferred for Later Review

## 1. `world_data_clone()` → What about CoW?

**Problem**: `world_data_clone()` does a full deep clone of `WorldBundle` (terrain grid, vision grids, border grids, cities, economy funds) to avoid `hecs::Ref` borrow conflicts. This is expensive — terrain alone is ~400×300 cells, vision/border grids are similar.

**Potential optimizations**:
- **Arc-based sharing**: Wrap grids in `Arc<Vec<Vec<T>>>` so cloning is just reference bump
- **Cow**: Use `Cow<'a, Vec<Vec<T>>>` for systems that read-only access
- **Split WorldBundle**: Only clone what's needed per system (e.g., terrain is static after init)
- **System-local snapshots**: Pre-compute per-tick snapshots outside the ECS world

**When to address**: After Phase 2+3 are done and profiling shows `world_data_clone()` as a hot path.

---

## 2. System Dispatch → Parallelization of Systems? Does hecs handle this?

**Problem**: We manually dispatch systems in `Game::tick()` sequentially:
```rust
EconomySystem::tick(...);
MovementSystem::tick(...);
WaterSystem::tick(...);
// ... 13 systems in strict order
```

**Question**: Does `hecs` have built-in parallel scheduling like `specs` or `specs_parallel`?

**Background**:
- `hecs` is intentionally minimal — no scheduler, no parallel iterator by default
- `hecs` provides `Query::par_iter()` for rayon-based parallel queries WITHIN a system
- No automatic system dependency graph or parallel system dispatch
- `specs` had `Dispatcher` + `BuildWorld` for this; `hecs` leaves it to the user

**Options to explore later**:
- Use `rayon` + `world.query::<(&A, &B)>().par_iter()` for read-only systems (Vision, Border, Debug)
- Manual parallel groups: e.g., run Vision + Border concurrently (they don't overlap writes)
- Consider `hecs` + `tickable` or custom scheduler if tick budget is tight
- Profile first — 13 sequential systems may be fast enough for 20 ticks/sec

**When to address**: After benchmarks (Phase 4) show tick time > 50ms.

---

## 3. Entity Registry Cleanup

**Current**: `HashMap<String, hecs::Entity>` maps string IDs to entities. On entity removal, we delete the entry but don't reclaim string IDs.

**TODO**: Consider a slab allocator for string IDs or an entity ID counter with generation check to avoid ID reuse bugs.

---

## 4. Debug Overlay (Phase 3)

- `/debug/stats` HTTP endpoint with tick timing, memory, concurrent games
- In-browser stats overlay div (debug builds only)
- `DebugStats` singleton component + `DebugSystem`
- Gated by `#[cfg(feature = "debug-overlay")]` or `cfg(debug_assertions)`

---

## 5. Wire Protocol Stability

- Verify `protocol.rs` is 100% unchanged (client-facing API)
- Entity IDs from `hecs` (`u32`) are mapped to string IDs via `EntityRegistry`
- Ensure no `Entity` types leak into `ServerMessage`/`ClientMessage`
