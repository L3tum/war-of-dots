# War of Dots — Bug Fix Plan

## Context

Four rendering/UI bugs plus weak test coverage need fixing:

1. **Background in top-left corner** — terrain renders at screen edge instead of centered
2. **HTML overlays** — city labels/stats use DOM instead of WebGPU where possible
3. **No city dots** — cities have HTML labels but no rendered markers on the map
4. **Lobby UI persists** — lobby panel never hides when game starts
5. **Tests & architecture** — renderer tests mock no WebGPU; no integration tests for critical flows

---

## Root Cause Analysis

### Issue 1: Background in Top-Left

**Root cause**: Camera starts at `{x: 0, y: 0, zoom: 1}`. The WGSL shader transforms world position `(0, 0)` to NDC `(-1, 1)` which is the top-left corner. The map's origin is at the screen edge.

Additionally, `SCREEN_WIDTH`/`SCREEN_HEIGHT` (500×300) are hardcoded constants used for the fog fullscreen quad. The fog quad vertices use these as world-space coordinates, but the actual canvas is e.g. 1920×1080, so the fog quad maps to a tiny region near the top-left instead of fullscreen. This breaks fog-of-war.

**Bug sites**:
- `frontend/src/game/renderer.ts` — constructor: fog quad uses `SCREEN_WIDTH`/`SCREEN_HEIGHT` (500×300)
- `frontend/src/game/config.ts` — `SCREEN_WIDTH = 500`, `SCREEN_HEIGHT = 300`
- `frontend/src/net/connection.ts` — `applyFullState()` doesn't center the camera

### Issue 2: HTML Overlays

City labels are DOM elements repositioned every frame in `main.ts` `updateCityLabels()`. This is expensive DOM manipulation. The stats panel and unit info panel are also DOM.

Not everything can be WebGPU (text rendering is hard), but city dots and fog should be. City labels can stay as DOM for now (text), but the city *dots* should be WebGPU-rendered.

**Bug sites**:
- `frontend/src/main.ts` — `updateCityLabels()` does per-frame DOM manipulation
- `frontend/src/game/renderer.ts` — no city dot rendering pass

### Issue 3: No City Dots

The renderer's `render()` method draws terrain, borders, unit dots, unit bars, unit symbols, and fog. There is **no** rendering pass for city markers. Cities only appear as HTML labels.

**Bug sites**:
- `frontend/src/game/renderer.ts` — `render()` method missing city rendering
- `frontend/src/game/config.ts` — no `CITY_DOT_SIZE` or `CITY_COLORS` constants

### Issue 4: Lobby UI Persists

The server sends `FullState` when the game starts, but the `handleServerMessage()` in `connection.ts` only applies game data — it never hides the lobby.

**Bug sites**:
- `frontend/src/net/connection.ts` — `applyFullState()` doesn't hide lobby
- `frontend/src/ui/lobby.ts` — no `hide()` method exposed
- `frontend/src/ui/lobby.test.ts` — no test for lobby hiding on game start

### Issue 5: Tests & Architecture

Current test weaknesses:
- `renderer.test.ts` — tests isolated math (border detection, vertex gen) but not the actual renderer class or its render pipeline
- No tests verify that `applyFullState` hides the lobby
- No tests for camera centering logic
- No tests for city dot rendering
- No integration tests connecting GameState → Renderer

---

## Approach

### Fix 1: Camera Centering + Fog Fullscreen Quad

**A. Center camera on terrain when FullState arrives:**
- In `connection.ts` `applyFullState()`, after setting terrain, compute the center and set `renderer.camera`
- Formula: `camera.x = (terrain[0].length * CELL_SIZE) / 2`, `camera.y = (terrain.length * CELL_SIZE) / 2`

**B. Fix fog fullscreen quad to use dynamic canvas dimensions:**
- Remove hardcoded `SCREEN_WIDTH`/`SCREEN_HEIGHT` usage in fog quad initialization
- Instead, use `this.canvas.width` and `this.canvas.height` at construction time
- Re-write fog quad on resize (add method to update fog positions)
- Remove `SCREEN_WIDTH`/`SCREEN_HEIGHT` from config.ts (or repurpose them)

### Fix 2: City Dots in WebGPU

Add a city rendering pass to the renderer, drawing colored quads at each city position:
- Add `CITY_DOT_SIZE` and `CITY_COLORS` config constants
- Add city position/color buffers to Renderer
- Add a draw call in `renderFrame()` for city dots
- In `render()`, iterate over `gameState.cities` and generate city quad vertices
- Color cities by owner (reuse PLAYER_COLORS)

### Fix 3: Lobby UI Hide on Game Start

- Add `hide()` method to `LobbyUI` that hides the lobby panel
- In `connection.ts` `applyFullState()`, call `this.lobbyUI?.hide()` and show stats panel
- Show the stats panel (currently hidden by default)

### Fix 4: Tests

- Add camera centering tests
- Add city dot vertex generation tests
- Add lobby hide test in lobby.test.ts
- Add connection message handler integration test (mock DOM, verify FullState → lobby hidden)
- Add terrain marching squares edge-case tests (empty grid, irregular grid)

---

## Files to Modify

### Core fixes:
| File | Changes |
|------|---------|
| `frontend/src/game/renderer.ts` | Fix fog fullscreen quad (dynamic dims), add city dot buffers + render pass, add resize fog quad update |
| `frontend/src/game/config.ts` | Add `CITY_DOT_SIZE`, `CITY_COLORS`; remove/repurpose `SCREEN_WIDTH`/`SCREEN_HEIGHT` |
| `frontend/src/net/connection.ts` | Center camera on terrain in `applyFullState()`, hide lobby on game start |
| `frontend/src/ui/lobby.ts` | Add `hide()` public method, add `showGameUI()` helper |

### Tests:
| File | Changes |
|------|---------|
| `frontend/src/game/renderer.test.ts` | Add camera centering, city dot vertex gen, fog quad dimension tests |
| `frontend/src/ui/lobby.test.ts` | Add `hide()` test, add game-start transition test |
| `frontend/src/net/connection.test.ts` | Add FullState → lobby hide integration test |
| `frontend/src/game/terrain.test.ts` | Add edge cases (irregular grid) |

---

## Steps

### Phase 1: Camera Centering + Fog Quad Fix (Issue 1)

- [ ] **1.1** Remove `SCREEN_WIDTH`/`SCREEN_HEIGHT` from `config.ts` (or rename to clarify they're not used)
- [ ] **1.2** In `renderer.ts` constructor: use `this.canvas.width` / `this.canvas.height` for fog quad positions instead of `SCREEN_WIDTH`/`SCREEN_HEIGHT`
- [ ] **1.3** Add `updateFogQuad()` method to Renderer that rewrites fog positions buffer with current canvas dimensions
- [ ] **1.4** Call `updateFogQuad()` from `resizeCanvas()`
- [ ] **1.5** In `connection.ts` `applyFullState()`: after setting terrain, compute center and set `renderer.camera` to center on terrain
- [ ] **1.6** Also center camera when terrain is first received via FullState

### Phase 2: City Dots in WebGPU (Issues 2 & 3)

- [ ] **2.1** Add `CITY_DOT_SIZE = 6` and `CITY_COLORS` array to `config.ts`
- [ ] **2.2** Add `cityPositionsBuffer`, `cityColorBuffer`, `cityVertexCount` to Renderer class
- [ ] **2.3** Create city buffers in Renderer constructor (similar to unit buffers)
- [ ] **2.4** In `render()`: iterate `gameState.cities`, generate quad vertices at city positions, color by owner
- [ ] **2.5** In `renderFrame()`: add draw call for city dots using mainPipeline (before fog)
- [ ] **2.6** Ensure city dots render on top of terrain but below units (order: terrain → borders → path → cities → unit dots → bars → symbols → fog)

### Phase 3: Lobby UI Hide (Issue 4)

- [ ] **3.1** Add `hide()` method to `LobbyUI` that:
  - Hides `#lobby-panel`
  - Removes any inline styles
- [ ] **3.2** In `connection.ts` `applyFullState()`: call `this.lobbyUI?.hide()`
- [ ] **3.3** In `connection.ts` `applyFullState()`: show `#stats-panel` and `#city-labels`
- [ ] **3.4** Also handle `ClearSpectator` message to reset UI state properly

### Phase 4: Tests (Issue 5)

- [ ] **4.1** Add camera centering test: verify formula produces correct center for known terrain size
- [ ] **4.2** Add city dot vertex generation test: verify quad positions and colors for a city at known grid position
- [ ] **4.3** Add fog quad dimension test: verify fog quad matches canvas dimensions (not hardcoded 500×300)
- [ ] **4.4** Add `LobbyUI.hide()` test in `lobby.test.ts`
- [ ] **4.5** Add connection integration test: mock DOM + connection, send FullState, verify lobby hidden and camera centered
- [ ] **4.6** Add terrain edge case test: irregular (ragged) grid handling

---

## Reuse

- **`PLAYER_COLORS`** from `config.ts` — reuse for city dot colors by owner index
- **Unit dot quad generation** pattern in `renderer.ts` — same approach for city dots
- **Border edge detection tests** pattern — same structure for city dot vertex tests
- **`LobbyUI.showTab()`** DOM manipulation pattern — reuse for `hide()` method
- **`mouseToGameCoords()`** in `controls.ts` — inverse of shader transform, verify camera centering with same math

---

## Verification

### Manual testing:
1. Start a game — terrain should be centered on screen, not in top-left corner
2. Pan around — fog of war should cover entire screen correctly
3. Cities should show colored dots on the map (in addition to HTML level labels)
4. Lobby panel should disappear when game starts (FullState received)
5. Stats panel should appear when game starts

### Automated testing:
```bash
cd frontend && npx vitest run
```

All existing tests must pass, plus the new tests for:
- Camera centering
- City dot rendering
- Fog fullscreen quad
- Lobby hide on game start
- Connection message dispatch

### Edge cases:
- Very small terrain grids (2×2) — still center correctly
- Very large terrain grids — camera centers on full extent
- Zero cities — no crash from empty city iteration
- Zero units — no crash (existing)
- Window resize — fog quad updates, camera stays centered
- Reconnect after disconnect — lobby should reappear if needed
