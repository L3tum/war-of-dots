# Research Report — Frontend Bugs

## Bug 1 — Lobby creation UI not removed

**Root cause:** `src/server/game_server.rs:907-917` — `serve_static` is typed `Result<axum::response::Html<String>>` and returns `Html(content)` for **every** file in the baked-in `FRONTEND` dir (`include_dir!("$CARGO_MANIFEST_DIR/frontend/dist")`, line 41). `Html` sets `Content-Type: text/html`, so `/main.js` (referenced by `<script src="main.js">` in `frontend/index.html`) is served as HTML. All modern browsers enforce MIME-type checking for scripts and block it. Result: no event listeners, no WebSocket, no canvas init — the lobby form is static and clicking "Create Lobby" does nothing.

**The frontend logic is correct** (would work once JS runs): `lobby.ts` `createLobby()` → server replies `ServerMessage::LobbyCreated` (`game_server.rs:1449`) → `connection.ts` `msg.LobbyCreated` branch → `lobbyUI.showTab("info")` → waiting room shown, tabs hidden.

**Fix:** Rewrite `serve_static` to return `Result<Response, StatusCode>` with a `Content-Type` derived from the file extension (`.js` → `application/javascript`, `.html` → `text/html`, `.css` → `text/css`, fallback `application/octet-stream`), using `file.contents()` (bytes) instead of `contents_utf8()`. Use the `mime_guess` crate or a small `match` to avoid a new dependency. `index_handler` (line 854) already serves `index.html` correctly — leave it.

## Bug 2 — "Enable city upgrades & supply system" label

**Root cause:** `frontend/index.html:473` labels the checkbox "Enable city upgrades & **supply system**", but the backend `CityUpgradeSettings.enabled` flag (`src/config.rs:37`, default `false`) gates **only** city upgrades:
- `src/game/economy.rs:218` — `upgrade_city()` errors with "City upgrades are not enabled" when `!enabled`
- `src/game/game.rs:1234-1235` — same gate
- `src/game/systems/supply.rs` `run()` (called unconditionally every tick at `game.rs:900-908`) **never reads the flag** (zero matches for `enabled` in the file) — attrition + resupply always run.

**Fix:** `frontend/index.html:473` → label becomes "Enable city upgrades"; add a small note: *"Supply (unit resupply & attrition) is always active and cannot be disabled."* No backend change — the toggle correctly controls what it controls.

## Bug 3 — Game rendering doesn't work

**Primary cause:** identical to Bug 1 — blocked `main.js` means no WebGPU context, no render loop, blank canvas. The MIME fix resolves it.

**Secondary real bug (surfaces once JS runs):** fog vertex buffer overflow in `frontend/src/game/renderer.ts`:
- `fogPositionsBuffer` created at **32 bytes** (~line 304)
- `updateFogQuad()` (~line 355) writes **16 floats = 64 bytes** into it → WebGPU validation error, write discarded, buffer stays zeroed, fog quad degenerates
- `createFogPipeline()` vertex layout (~line 700): `arrayStride: 8` with two `float32x2` attributes at offsets 0 and 4 — attributes **overlap** (each is 8 bytes) and the stride can't hold both; inconsistent with the 4-floats-per-vertex data
- The fragment shader recomputes UVs from world position and ignores vertex UVs (comment: "will be overwritten in frag"), so the UV attribute is vestigial

**Fix (recommended, minimal):** position-only fog layout — `arrayStride: 8` with a single `float32x2` attribute at offset 0; WGSL `vert` takes only `@location(0) pos: vec2<f32>`; `updateFogQuad()` writes just the 8 floats `[-1,-1, 1,-1, -1,1, 1,1]` = 32 bytes, matching the buffer. (Alternative: stride 16, uv at offset 8, 64-byte buffer — wasteful since the UV is discarded.)

## Relevant infrastructure

- Frontend dev/build: `frontend/package.json` scripts (`dev`, `build`, `typecheck`, `test`)
- Frontend dist is baked into the binary via `include_dir!("$CARGO_MANIFEST_DIR/frontend/dist")` — after frontend edits, must rebuild frontend then rebuild the Rust binary
- State management: simple module-level singletons, event-driven
- API/websocket: `frontend/src/net/connection.ts` handles WebSocket messages
- Settings UI: static HTML in `frontend/index.html`, not a component
