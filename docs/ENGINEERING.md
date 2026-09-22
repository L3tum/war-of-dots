# Error strategy by layer (engineering convention)

The codebase converges on one error-strategy shape per layer; new code follows it, and the worked examples below are the reference fixes (M3/M5 waves).

## The convention

- **Engine (`src/game/`)** — pure simulation: returns plain `Result`s where a
  caller can react; where reacting is impossible or not worth it, the work is
  skipped and `tracing` says why. Never panics on config/map values it can
  degrade around (e.g. oversized radii clamp to the span-table max).
- **Server (`src/server/`)** — maps engine/IO errors to wire `Error` frames
  for clients; fatal startup failures (listener binds) log
  `tracing::error!` with the exact consequence and abort. Never panics on
  client input; client-controlled values are validated at the boundary
  (connection caps, lobby limits, payload sizes).
- **LLM (`src/llm/`)** — bails with `anyhow::Result` (e.g.
  `tool_call_to_command` is fallible); never panics on external data, because
  provider output is untrusted input. A missing/unavailable provider is a
  normal state, not an error state.
- **Frontend (`frontend/src/`)** — `Connection` surfaces protocol errors to
  the UI (visible state/error) instead of dropping the stream silently.

## Worked examples

- **Silent circle-radius clamp** — the influence hot path clamps configured
  radii to the span-table max (10) by design; `config::resolve` now warns
  once per load, naming the setting, the provided value, and the clamped
  value (`influence_radius_warnings` + `influence::clamp_circle_radius`).
- **Opaque port+1 bind failure** — the raw-TCP game listener binds `port+1`;
  a failed bind now logs `tracing::error!` naming both ports and the
  consequence (TCP clients can't join, WS still works) and aborts startup —
  consistent with the HTTP bind — instead of running half a server.
- **Fake-key LLM calls on first run** — `client_factory` refuses empty,
  placeholder, or unexpanded `${ENV}` API keys with a single "LLM disabled:
  placeholder credentials for \<provider\>" warn and follows the existing
  unavailable-provider skip path; no provider call ever fires with dummy
  credentials.
- **Partial CBOR frames** — the encoder core (`encode_message_result`) is
  fallible and discards partial buffers; socket owners log + close instead
  of ever sending a half-frame.
