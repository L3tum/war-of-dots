# Deep Plan: Fix war-of-dots rendering defects, camera UX, and idle websocket traffic

## Problem Statement
Five user-reported defects in the war-of-dots server + WebGPU frontend: janky right-drag pan with lost mouse tracking, quads rendering only one triangle (half black), invisible cities, default view too zoomed in, and slow rendering with ~60 msg/s idle websocket traffic.

## Context
- **Repo layout**: Rust server in `src/` (axum + tokio + hecs ECS, CBOR protocol in `src/server/protocol.rs`), TypeScript WebGPU frontend in `frontend/src/` bundled by esbuild. `frontend/dist` is **baked into the binary at compile time** via `include_dir!("$CARGO_MANIFEST_DIR/frontend/dist")` (`src/server/game_server.rs:41`, `src/frontend/mod.rs:6`) — every frontend change requires `make frontend-build` **before** `cargo build/run`.
- **Render pipeline** (`frontend/src/game/renderer.ts`, ~1500 lines): 5 `createRenderPipeline` calls (lines 352, 422, 504, 583, 706) — **none sets `primitives.topology`**, so all default to `triangle-list`. Every vertex builder (terrain cells in `terrain.ts:120,131`, unit dots `renderer.ts:1016+`, symbol quads `~1033`, bars `~1076`, border quads `~1225+`, path segments `~1310`, city dots `~1348`, halos `~1385`, fog quad `updateFogQuad()` `~307-310`) pushes exactly **4 vertices in strip order TL,TR,BR,BL** → only (v0,v1,v2) renders; the missing half shows the dark clear color (0.05,0.05,0.1). This single root cause explains #2 and makes city dots (12 world units) half-black/invisible (#3) and fog cover only the bottom-left half (part of #2).
- Bonus GPU waste: `renderer.ts:864` — `renderPass.draw(this.unitVertexCount * 4)` where the symbol buffer holds 4 verts/unit and `unitVertexCount` already counts them; correct count is `draw(this.unitVertexCount)`.
- **Camera pan** (`frontend/src/ui/controls.ts`): pointermove applies camera delta synchronously per event, calls `getBoundingClientRect()` on every event, and relies on one-shot `setPointerCapture` (line ~128) with no re-capture if capture is implicitly released. Pointer handlers starve because the main thread is saturated by the render loop (no dirty check) plus 60 CBOR decodes/s (no server-side idle skip).
- **Camera init**: default `camera = { x: 0, y: 0, zoom: 1.0 }` (`renderer.ts:53`, mirrored in `game_state.ts:17`). `centerCameraOnTerrain()` (`frontend/src/net/connection.ts:322`) sets x/y to map center only — never zoom. Default map is 200×200 × `CELL_SIZE` 32 = 6400 world units (`src/map/map_config.rs`, cities at (10,10),(190,10),(10,190),(190,190),(100,45)); at zoom 1.0 a 1920px canvas shows ~1920 world units, so all 5 cities (55–90 cells out) are off-screen. `ZOOM_MIN = 0.5` (`frontend/src/game/config.ts:21`) is below what's needed to fit the map (≈0.27).
- **Server tick** (`src/server/game_server.rs`): `game_speed = 60` (config.toml) → 16 ms interval (line 203, interval loop line 447). Every tick, for **every** player: `build_delta_inner` (`src/server/delta.rs`) re-snapshots `game.unit_states()` (clones all units incl. equipment Vecs, `src/game/game.rs:1777`) and `game.query_cities()`, and **always** returns `ServerMessage::DeltaState` — even with every vec empty — then it's CBOR-encoded and sent (lines 635-650). `ECONOMY_TICK_INTERVAL = 1` (`src/game/constants.rs:7`) means funds for any city owner change every tick → delta never fully empty in a live game. `game_server.rs:529` clones all unit states per tick just for `entity_count`; lines 543-545 clone them again for the active-player check. Replay loop (`game_server.rs:1228-1240`) has the same unconditional-send pattern.
- **Renderer hot path**: `startRenderLoop()` (`renderer.ts:765`) runs `updateCallback` (`renderer.render(gameState)` from `main.ts:155`) + `renderFrame()` **every rAF unconditionally** — rebuilding all `number[]` vertex arrays, `new Float32Array`, `writeBuffer` (incl. re-uploading cached terrain mesh), 9 draw calls, fresh uniform array, and fresh bind groups (`createSymbolBindGroup()` `~863`, `createFogBindGroup()` `~881`, each also doing `texture.createView()`) — all of which depend only on static textures/samplers.
- Existing dirty tracking: `fogDirty`/`borderDirty` flags, `terrainMeshKey` CPU mesh cache (but still re-uploads every frame), `symbolCacheKey` atlas cache. `game_state.test.ts:42` asserts default camera `{0,0,1.0}` — plan keeps that default untouched (fix is in `centerCamera*`), so the test stays valid.

## Design

### Issue 2 — missing triangle (root-cause fix)
Set `primitives: { topology: 'triangle-strip' }` on **all 5 pipelines** (lines 352, 422, 504, 583, 706). All vertex sources are 4-vert TL-TR-BR-BL quads, so strip topology is correct for every one (verified terrain.ts:120,131 and every builder in renderer.ts). Alternative considered: rewrite all builders to push 6 verts/triangle-list — rejected: 25% more vertex data for zero benefit. Also fix `draw(this.unitVertexCount * 4)` → `draw(this.unitVertexCount)` (line 864).

### Issue 1 — pan jank + lost focus
Two-part fix:
1. **Client (controls.ts)**: decouple input from camera mutation — pointermove only stores latest pointer pos (and re-captures pointer if capture was lost); a rAF-driven pan loop applies the accumulated camera delta **once per frame**. Cache `getBoundingClientRect()` (refresh on `window.resize` and at drag start). This removes per-event layout reads and guarantees 1:1 camera update per rendered frame.
2. **Main-thread relief**: the frame-level dirty gate (Issue 5b) plus server-side idle delta skip (Issue 5a) eliminate the constant render/decode load that starves pointermove handlers.

### Issue 3 — cities not visible
No data bug: cities are generated (`src/game/game.rs:542-546`), unconditionally in FullState (`src/server/game_server.rs:205-211` → `build_full_state` ~826), stored (`connection.ts` applyFullState) and drawn (`renderer.ts:~1348`). Causes are (a) half-quad rendering (fixed by Issue 2) and (b) default zoom 1.0 with all cities off-screen (fixed by Issue 4). No server change needed.

### Issue 4 — default zoom
Fit the map: `zoom = min(canvasW, canvasH) / (mapDim × CELL_SIZE) × 0.9`, clamped to `[ZOOM_MIN, ZOOM_MAX]`; lower `ZOOM_MIN` from 0.5 to 0.3 so the clamp doesn't kick in for the default map. Applied in `centerCameraOnTerrain()` and (for consistency) the own-territory branch of `centerCamera()`. Kept out of the default camera literal so `game_state.test.ts` is unaffected.

### Issue 5 — performance & WS traffic
**Server (5a), ordered by impact/risk:**
1. Skip encode+send when a player's delta is empty (units added/removed/updated, city_updates all empty ∧ grid diffs None ∧ funds + units_owned unchanged since last snapshot). `build_delta_inner` returns `Option<ServerMessage>`; on `None` the prev snapshots are **not** mutated (idempotent).
2. Hoist snapshots: `unit_states()` / `query_cities()` / `all_player_funds()` computed **once per tick** in the game loop and passed as slices/refs into `build_delta_inner` (new signature), instead of per-player-per-tick clones.
3. Cheap debug count: `game_server.rs:529` uses `game.unit_count()` (exists, `game.rs:653`, currently `#[allow(dead_code)]`) + `query_cities().len()`; the active-player check (543-545) reuses the hoisted snapshot.
4. `ECONOMY_TICK_INTERVAL: 1 → 10` (`constants.rs:7`): funds tick at ~6 Hz instead of 60 Hz. This is what makes "idle" (no units moving) actually idle for city owners. Gameplay effect: income accrues in coarser steps — visually a counter that updates 10× less often; flagged as the one behavior-affecting constant.
**Client (5b), ordered:**
1. Frame-level dirty gate in `startRenderLoop()`: a `frameKey()` (camera x/y/zoom + canvas size + data version + `currentPath.length` + fog/border dirty flags) gates `render()` + `renderFrame()` — nothing changes → zero work. Data version bumped via `markDataDirty()` from `connection.ts` on every FullState/DeltaState.
2. Line 864 draw-count fix (Issue 2).
3. Cache terrain `Float32Array`s alongside the mesh cache; `writeBuffer` only on rebuild (renderer.ts ~940-985).
4. Hoist symbol bind group to constructor; fog bind group cached at first fog-texture creation (fog texture itself is already created once and only written to).
- Deliberately **out of scope**: heartbeat ping/pong (WS-layer ping already ignored, no client ping — but not user-reported as a bug), 10–20 Hz delta throttling, per-row grid diffs (the stale "map is tiny 20x20" comment at `delta.rs` ~60 is noted but grid-diff machinery is a follow-up; at 200×200, full-grid resends on change are rare and ~180 KB each — acceptable for now), off-screen city indicators, `ECONOMY_TICK_INTERVAL` per-second income rebalance.

### Ordering & dependencies
1. Server changes (5a) are independent of frontend; do first (or in a separate commit).
2. Topology fix (Issue 2) **before** any visual verification of Issues 3/4 — everything renders as half-quads until then.
3. Zoom fix (Issue 4) before dirty gate (5b.1) — the gate makes verifying visual regressions harder if the camera logic is wrong.
4. Dirty gate depends on the server skip (2) for its "idle" guarantee, but works standalone.
5. `make frontend-build` before `cargo run` for any frontend change (include_dir).

## Implementation Steps

**Phase A — Server (Issue 5a)**

1. **`src/server/delta.rs` — `build_delta_inner`**: change signature to
   ```rust
   pub(crate) fn build_delta_inner(
       prev_units: &mut HashMap<String, UnitState>,
       prev_cities: &mut HashMap<String, CityState>,
       prev_funds: &mut HashMap<String, u32>,
       prev_vision_grid: &mut Option<Vec<Vec<bool>>>,
       prev_border_grid: &mut Option<Vec<Vec<u32>>>,
       current_units: &[UnitState],
       current_cities: &[CityState],
       all_player_funds: &HashMap<String, u32>,
       current_vision_grid: Option<Vec<Vec<bool>>>,
       current_border_grid: Option<Vec<Vec<u32>>>,
       player_id: &str,
   ) -> Option<ServerMessage>
   ```
   Delete the internal `game.unit_states()` / `game.query_cities()` / `game.player_funds()` calls (use `all_player_funds.get(player_id).copied().unwrap_or(0)`). After computing `units_added/removed/updates`, `city_updates`, both grid diffs and `player_state`, insert:
   ```rust
   let funds_changed = prev_funds.get(player_id).copied() != Some(player_state.funds);
   let prev_owned: u32 = current_units_prev_owned(prev_units, player_id); // count u.owner == player_id
   if units_added.is_empty() && units_removed.is_empty() && unit_updates.is_empty()
       && city_updates.is_empty() && vision_grid_diff.is_none() && border_grid_diff.is_none()
       && !funds_changed && prev_owned != player_state.units_owned { return None; }
   ```
   **before** the `*prev_* = ...` assignments (on `None`, leave snapshots untouched). Update the stale "map is tiny (20x20)" comment (~line 60).
2. **`src/server/game_server.rs` — tick loop (~505-650)**: after `game.tick_game()` and the debug-stats block, hoist once per tick:
   ```rust
   let current_units = game.unit_states();
   let current_cities: Vec<CityState> = game.query_cities().iter().map(|c| c.into()).collect();
   let all_funds = game.all_player_funds();
   ```
   Pass slices to `build_delta_inner`; wrap the send: `if let Some(delta) = build_delta_inner(...) { let encoded = encode_message(&delta); if player.tx().send(encoded).await.is_ok() { alive.push(player); } } else { alive.push(player); }`. Update `prev_funds` once after the loop: `prev_funds = all_funds;`.
3. **`src/server/game_server.rs:529`**: `gs.entity_count = game.unit_count() + game.query_cities().len();` (remove `#[allow(dead_code)]` at `src/game/game.rs:652` if now used). Rewrite the active-player check (543-545) to iterate `current_units`/`current_cities` instead of re-calling `unit_states()`/`query_cities()` per player.
4. **`src/server/game_server.rs` — replay loop (~1228-1240)**: same signature adaptation; snapshot per event, `if let Some(delta) = ... { send }`.
5. **`src/game/constants.rs:7`**: `ECONOMY_TICK_INTERVAL = 10`.

**Phase B — Frontend**

6. **`frontend/src/game/renderer.ts` — topology (Issue 2)**: add `primitives: { topology: 'triangle-strip' }` (after the `vertex` block, before `fragment`) to all 5 pipelines: lines 352, 422, 504, 583, 706.
7. **`renderer.ts:864`**: `renderPass.draw(this.unitVertexCount);` (drop `* 4`).
8. **`frontend/src/game/config.ts:21`**: `ZOOM_MIN = 0.3`.
9. **`frontend/src/net/connection.ts` — `centerCamera`/`centerCameraOnTerrain` (Issue 4)**: import `ZOOM_MIN, ZOOM_MAX` from `../game/config`; in `centerCameraOnTerrain()` after centering:
   ```ts
   const mapW = (maxCol - minCol + 1) * CELL_SIZE;
   const mapH = (maxRow - minRow + 1) * CELL_SIZE;
   const c = this.renderer.canvas; // canvas is public on Renderer
   this.renderer.camera.zoom = Math.max(ZOOM_MIN, Math.min(ZOOM_MAX,
     Math.min(c.width / mapW, c.height / mapH) * 0.9));
   ```
   In `centerCamera()`'s own-territory branch, apply the same fit zoom after setting x/y (extract a private `fitMapZoom()` helper to avoid duplicating the bbox math).
10. **`renderer.ts` — frame dirty gate (Issue 5b.1)**: add fields `private dataVersion = 0; private lastFrameKey: string | null = null;`, public `markDataDirty() { this.dataVersion++; }`, and:
    ```ts
    private frameKey(): string {
      return `${this.camera.x},${this.camera.y},${this.camera.zoom}|` +
        `${this.canvas.width}x${this.canvas.height}|${this.dataVersion}|` +
        `${this.currentPath.length}|${this.fogDirty ? 1 : 0}${this.borderDirty ? 1 : 0}`;
    }
    ```
    In `startRenderLoop()` (~765): compute key; if unchanged from `lastFrameKey`, skip `updateCallback` and `renderFrame()`; else set `lastFrameKey`, run both. (Key is computed before `render()` so flag consumption still triggers one more frame.)
11. **`frontend/src/net/connection.ts`**: call `this.renderer.markDataDirty()` in `applyFullState` (~line 261) and at the top of `applyDelta`'s try block (~line 359).
12. **`renderer.ts` — terrain upload cache (5b.3)**: alongside `cachedTerrainPositions/Colors` (~line 60), add `cachedTerrainPosData/cachedTerrainColData: Float32Array | null`, stored on mesh rebuild; in `render()` (~975-983) reuse the cached arrays and skip both `writeBuffer` calls when `terrainCached` (keep `ensureBuffer` + first-upload path).
13. **`renderer.ts` — bind-group hoist (5b.4)**: create `private symbolBindGroup!: GPUBindGroup;` in the constructor right after `symbolBindGroupLayout` (~187) using `symbolTexture`/`symbolSampler`; line ~863 uses it directly and `createSymbolBindGroup()` is deleted. For fog: add `private fogBindGroup: GPUBindGroup | null = null;`, set it where `fogTexture` is first created (`render()` ~1455), use `this.fogBindGroup!` at ~881, delete `createFogBindGroup()`.
14. **`frontend/src/ui/controls.ts` — pan rewrite (Issue 1)**:
    - New fields: `private lastPointer: { x: number; y: number } | null = null; private canvasRect: DOMRect | null = null; private panFrameId: number | null = null;`
    - `refreshRect()` helper: `this.canvasRect = this.canvas.getBoundingClientRect();`
    - pointerdown button 2 (~line 122-129): keep `preventDefault`; set `isDragging`, `rightDragStart`, `lastPointer = {x: e.clientX, y: e.clientY}`; `this.refreshRect();` wrap `setPointerCapture` in try/catch; call `startPanLoop()`.
    - pointermove while dragging (~132-146): `e.preventDefault(); this.lastPointer = { x: e.clientX, y: e.clientY }; if (!this.canvas.hasPointerCapture(e.pointerId)) { try { this.canvas.setPointerCapture(e.pointerId); } catch { /* ignore */ } }` — **no camera mutation here**.
    - New `startPanLoop()`: if `panFrameId !== null` return; `step = () => { if (this.isDragging && this.lastPointer) { const rect = this.canvasRect ?? this.refreshRect(); const dx = this.lastPointer.x - this.rightDragStart.x; const dy = this.lastPointer.y - this.rightDragStart.y; this.camera.x -= dx * (this.canvas.width / rect.width) / this.camera.zoom; this.camera.y -= dy * (this.canvas.height / rect.height) / this.camera.zoom; this.rightDragStart = { x: this.lastPointer.x, y: this.lastPointer.y }; } this.panFrameId = requestAnimationFrame(step); }` and start it.
    - `endPan()`: cancel `panFrameId`, `isDragging = false`, `lastPointer = null`, release capture if held; call from pointerup button 2 (~152) and pointercancel (~173).
    - `window.addEventListener('resize', () => this.refreshRect())` in `setupEventListeners` (line ~90).
    - `mouseToGameCoords`/`hitTestUnit`/`hitTestCity`: replace `getBoundingClientRect()` with `this.canvasRect ?? this.refreshRect()` (clicks refresh the rect first via pointerdown).

**Phase C — Rebuild & verify** (see Verification).

## Dependencies
- None new. Existing: esbuild, cbor-x, @webgpu/types (frontend); axum, hecs, serde_cbor/ciborium (server).

## Risks & Mitigations
- **Topology flip misrenders something that legitimately used triangle-list** — Mitigation: verified all 9 vertex producers emit 4-vert TL-TR-BR-BL quads (`terrain.ts:120,131` + 8 builder sites in `renderer.ts`); no index buffers exist, so no other topology semantics are in play.
- **Dirty gate drops a needed frame** (e.g., camera moved but key not part of key, or a field mutated without `markDataDirty`) — Mitigation: key includes camera/zoom/size/path-len/fog/border flags; any server message bumping `dataVersion` covers all `applyDelta`/`applyFullState` writes; `updateCityLabels`/stats are camera-data driven so they re-run on any rendered frame. If a missed-frame bug appears, the escape hatch is bumping `dataVersion` at the mutation site.
- **`setPointerCapture` re-capture throws** (pointer no longer active) — Mitigation: try/catch, fall back to plain move tracking.
- **Empty-delta skip desyncs a client** — Mitigation: skip condition requires *everything* (units, cities, grids, funds, units_owned) unchanged vs the last sent snapshot; FullState is resent on (re)join, and any future protocol change flows through the same fields.
- **`ECONOMY_TICK_INTERVAL = 10` changes income pacing** — income accrues in 10-tick batches instead of per-tick (e.g., 6 funds/600 ms vs 1/100 ms); totals are identical. Flagged to user as the one gameplay-visible constant; trivially revertible (1-line change).
- **Fit zoom on very small windows clamps to ZOOM_MIN=0.3** — acceptable: zoom 0.3 still shows a large portion of the default map, and wheel-zoom range now extends down to 0.3.
- **include_dir staleness** — a frontend change without `make frontend-build` ships the old bundle; build step is explicitly ordered in Verification.

## Tests Needed
- **New Rust unit tests** (`#[cfg(test)]` in `src/server/delta.rs` — none exist today; the new signature makes `build_delta_inner` trivially testable without a `Game`):
  - identical snapshots → returns `None` (and does not mutate prev state).
  - a single unit position change → `Some(DeltaState)` with exactly 1 `unit_updates` entry.
  - funds-only change for player A → `Some` for A, `None` for other players.
  - city owner change → `Some` with `city_updates`; grid change → grid diff present.
  - first tick from empty prev (units_added path) → `Some`.
- **Existing suites must stay green**: `cargo test --all-targets -- --skip alloc_invariants::warm_tick_net_heap_growth_is_bounded` + the isolated dhat test (Makefile `test` target), `cargo clippy -- -D warnings`, `cd frontend && npm run test` (vitest: `renderer.test.ts`, `game_state.test.ts` — its `{0,0,1.0}` default-camera assertion stays valid, `terrain.test.ts` 4-verts-per-cell assertions unchanged, `units.test.ts`).
- **Manual/edge cases**: 200×200 map (default) + a small custom map file through `load_map` for the zoom-fit math; drag right-button while pointer exits the window; rapid browser-tab/pointer-cancellation; zoom wheel at new 0.3 floor.

## Verification
1. **Server build/run**: `cargo build` (debug), run `cargo run -- --config config.toml` (port default 8080, `src/main.rs:32-37`). Debug-stats variant: `cargo run --features debug-overlay` (enables `/debug/stats` used by the frontend overlay).
2. **Full pipeline**: `make frontend-build` (bundles `frontend/dist/main.js`) **then** `cargo build` / `cargo run` — dist is baked in via `include_dir!` at compile time.
3. **Issue 2**: start game, observe terrain, unit symbols, bars, borders, path preview, and fog — all quads fully filled; fog now covers the entire screen (was bottom-left half). No more half-black quads.
4. **Issue 3**: on join, all 5 cities (4 corners + one at (100,45)) visible as full colored dots with DOM labels.
5. **Issue 4**: on join (spectator or fresh player), view is zoomed out to ~map-fit (≈0.3 zoom on 1920×1080 for the default map); wheel zoom can go down to 0.3.
6. **Issue 1**: right-drag pan is smooth at display refresh rate; dragging past the window edge / releasing outside keeps 1:1 tracking until release; no jank even with 60 Hz ticks running.
7. **Issue 5**:
   - Idle WS traffic: two connected clients with no commands — server should send **zero** DeltaState messages while idle (browser DevTools network/WS frames, or `console.debug('[WS] ←')` logs on localhost).
   - CPU: debug overlay (debug-overlay build) FPS rises and tick ms flat; with a static camera the renderer performs ~0 GPU work between server deltas (verify via browser performance monitor: draw calls / frame times).
   - `ECONOMY_TICK_INTERVAL=10`: funds counter for a city owner updates ~10× less frequently; income totals unchanged.
   - Replay mode (`/ws/replay?game_id=…`) still streams and ends correctly with a final FullState.
8. **Regression gate**: `make test` (cargo + vitest) and `make frontend-check` (fmt/lint/typecheck) clean.

## Out of Scope / Optional Follow-ups
- Off-screen city/unit indicator arrows.
- Per-row grid diffing for vision/border (fixes the 40 KB / 160 KB full-grid resends on change); update the stale 20×20 comment in `delta.rs` meanwhile.
- Delta delivery throttling (10–20 Hz coalescing) and server→client heartbeat ping/pong (`game_server.rs:1395-1397` ignores WS pings; no client ping in `connection.ts`) — not user-reported, add only if disconnects surface.
- Rebalancing income to per-second accrual if the 10-tick batches ever matter gameplay-wise.
- A `dispose()` on `Controls` for the resize listener (page-lifetime app; not worth the churn).
