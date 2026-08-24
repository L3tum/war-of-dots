# Deep-plan brief

User report (verbatim): "The frontend is pretty broken. The lobby creation doesn't remove the lobby creation UI, the settings still say to enable the City Upgrade & Supply system (Supply is always active!), the game rendering itself doesn't work at all either."

## Repo layout (known)

- Rust backend: `src/`, `Cargo.toml`, `config.toml`
- Frontend: `frontend/` (Vite + TypeScript), sources in `frontend/src/`:
  - `game/`, `generated/`, `main.ts`, `net/`, `replay/`, `types/`, `ui/`
  - `frontend/schema/` exists
  - Tests: `vitest.config.ts`, eslint config present

## Research tasks

Produce a structured research report with concrete file paths and line numbers.

### Bug 1 — Lobby creation UI not removed
- Find where the lobby-creation UI is defined (likely `frontend/src/ui/`).
- Trace the full flow: form submit → API call → state update → render.
- Determine why the UI persists after a successful creation (missing state toggle, wrong handler, missing navigation, missing API response handling, ...).
- Deliver: root cause with file:line, short code evidence, proposed fix direction.

### Bug 2 — Settings advertise "enable City Upgrade & Supply system"
- Find the settings screen text/option mentioning "City Upgrade & Supply" in `frontend/src/ui/`.
- Check the backend (`src/`, `config.toml`) for the actual feature flags: is Supply always active? Is City Upgrade a real toggle?
- Decide the correct fix: remove the row, change to informational text, or wire a real toggle. Justify with backend citations.
- Deliver: root cause, evidence from both frontend and backend, proposed fix.

### Bug 3 — Game rendering doesn't work at all
- Find the canvas / render loop in `frontend/src/game/`.
- Identify why nothing renders: game loop wiring, websocket/message handling, type mismatches between frontend schema (`frontend/src/generated/`, `frontend/schema/`) and backend serialization, asset loading, build/serving issues (`frontend/index.html`, `dist/`, dev proxy, how the backend serves the frontend).
- Deliver: exact failure points with file:line, evidence, proposed fix direction.

### Infrastructure section
- Dev/build/test commands from `frontend/package.json` (and root `Makefile`/`package.json` if relevant).
- Key files and patterns the fixes must follow: state management, API client, websocket message handling, code generation (if `generated/` is checked in or produced by a script).

## Output format

Markdown report, sections per bug (root cause / evidence / proposed fix) plus "Relevant infrastructure". Keep evidence snippets short (≤ ~10 lines each).
