# Threat Model — War of Dots Server

Scope: the Rust `axum` server (`src/server/`, `src/game/`, `src/llm/`).
This doc records the trust assumptions the server is designed under, the
controls in place, and the explicitly-accepted risks. It is not a
penetration test; it is the basis for deciding what "good enough" means for
a LAN-oriented game server.

Last updated with the Sec L1/L2 hardening round (debug endpoint gate,
connection/spectator caps).

---

## 1. Trust model

**The server is intended to run on a trusted local network** (a household
network, a LAN party, a single-host dev machine). It provides **no
transport encryption and no authentication**. Everything below is "safe"
under that assumption only.

- **Cleartext on the wire.** All protocols (WS `/ws`, raw-TCP game port,
  HTTP, WS replay) are unencrypted. Lobby passwords, player names, game
  commands, and the entire game stream are readable by anyone who can
  observe the network (a compromised or curious LAN peer). The lobby
  password comparison is constant-time
  (`subtle::ConstantTimeEq`) — that defends against *timing* side channels,
  not against eavesdropping. Do not rely on lobby passwords as a
  confidentiality boundary; they only stop casual lobby-mixing.
- **Default bind is `0.0.0.0` (all interfaces) with zero auth.** The bind
  address is a CLI flag, not a config file setting: there is no `host`/`bind`
  key in `config.toml`. To restrict exposure, start the server with
  `--bind 127.0.0.1` (localhost only) or run it on a network you trust.
  The default is intentionally `0.0.0.0` for LAN play; do not treat the
  server as internet-exposable.
- **Two listeners.** `port` serves HTTP (SPA, read-only endpoints) and the
  WS upgrade routes; `port + 1` is a raw-TCP game port (length-prefixed
  CBOR frames). Both are equally untrusted inputs and both are covered by
  the connection cap below.

## 2. Endpoint inventory and controls

### 2.1 Game connections (`/ws`, raw-TCP `port+1`) — capped

- **Cap:** `network.max_ws_connections` (config, default **64**) bounds all
  concurrent game connections across both transports. The counter
  (`GameServer::ws_connections`) is reserved at accept/handshake time and
  released exactly once on disconnect; over-cap connections are rejected:
  - WS: a clear `Error` message ("Server at maximum connections — try again
    later") followed by WS **close code 1013** ("Try Again Later"), before
    any per-connection task is spawned.
  - Raw TCP: the same `Error` frame (best-effort) and the connection is
    dropped.
- The join path itself still requires a valid lobby (create/join with the
  lobby's password if set); the cap is a resource bound, not an
  authorization mechanism.
- The lobby *member* cap is a separate work item owned by the lobby logic;
  it is not part of this round.

### 2.2 Spectator stream (`/ws/spectator`) — unauthenticated by design, capped

- **Unauthenticated by design for LAN:** spectating is read-only (the
  spectator receives `FullState` + deltas and can send nothing that the
  server acts on). On a trusted LAN that is an acceptable trade for
  friction-free "watch a friend's game".
- **Cap:** `network.max_spectators_per_game` (config, default **8**). The
  check and the `players.push` both happen in the game tick task (sole
  owner of the player list), so concurrent joins cannot race past the cap.
  Over-cap joins receive an `Error` message ("Spectator limit reached
  (N)"); earlier spectators are unaffected.
- Spectator connections count against `max_ws_connections` as well.

### 2.3 Replay streams (`/replays`, `/replays/:name`, `/ws/replay`) — read-only

- Serves recorded game data only (map, settings, player *names*,
  per-tick JSONL). No credentials, no write operations, no user data beyond
  what a player chose to broadcast in-game.
- **`/ws/replay` is not counted** against `max_ws_connections` (it
  self-terminates: file end → close), but it **is rejected while the cap is
  reached** (Error + close 1013), so replay streaming cannot bypass the
  connection bound.
- Replay file names are sanitized before filesystem access (path traversal
  rejected — `is_safe_replay_basename`).

### 2.4 Debug endpoints (`/debug/stats`, `/debug/benchmark`) — debug builds only

- `/debug/benchmark` (only compiled with the `microbenchmarks` feature) runs
  a synthetic game and is a CPU-heavy workload: in
  any exposed deployment it is an **unauthenticated CPU-DoS lever**.
- Two-layer gate (Sec L1):
  1. **Route registration** — the routes are only registered when
     `cfg!(any(debug_assertions, feature = "debug-overlay"))`.
  2. **Request-time gate** — every handler re-checks at request time:
     `cfg!(any(debug_assertions, feature = "debug-overlay")) &&
     config.debug.endpoints`. If either side is false, the handler returns
     **404** (the handler body never runs). So even if a route is ever
     exposed (misconfigured proxy, feature-flag slip, a test app registering
     it), it cannot execute debug logic.
- `config.debug.endpoints` (config `[debug] endpoints`, default follows the
  build: on in debug builds / `debug-overlay`, off in release) lets an
  operator disable the endpoints in a debug build without rebuilding.
- Release builds: routes unregistered, and the request-time gate is false —
  double cover.

### 2.5 Read-only HTTP + CORS

- `tower_http::cors::CorsLayer` is `Any/Any/Any` on the HTTP router.
  **Rationale:** the HTTP surface serves the embedded SPA and the read-only
  replay endpoints — **no secrets are ever served and there are no state
  mutating routes over HTTP** (all game input flows over the WS/TCP game
  protocols). Cross-origin fetches from any page can therefore only read
  replay data, which is acceptable on a trusted LAN (anyone on it could
  also just run `curl`).
- **What would change if secrets were ever served** (e.g. an auth token,
  LLM API key, admin endpoint): CORS must switch to an explicit origin
  allowlist and the endpoint must require authentication first. Do not add
  such routes without revisiting this section.

## 3. Input-validation controls (context)

Already in place and relied on by the model above:

- **Framing limits** — length-prefixed TCP frames and WS payloads are size
  bounded; a handshake timeout drops connections that never send `Hello`.
- **SQL** — storage layer is fully parameterized (no string-built queries).
- **Lobby passwords** — constant-time comparison.
- **Replay names** — path-traversal-safe basename check.
- **LLM providers** — request timeouts, bounded context; provider API keys
  never leave the server (config-only).
- **Frontend** — output sanitized for the browser (separate round).

## 4. Explicitly out of scope / accepted risks

- **No rate limiting per connection** (lobby-message spam, command spam,
  replay re-streaming). Accepted for the LAN threat model: the *number* of
  peers is bounded by the §2.1/§2.2 caps, and per-connection abuse on a
  trusted LAN is a social problem, not a confidentiality one. If the server
  is ever exposed beyond a trusted LAN, rate limiting (and auth) become
  required, not optional.
- **No transport encryption** (§1). Mitigation: run on a trusted network
  or behind a tunnel (e.g. WireGuard/Tailscale) — out of scope for this
  codebase.
- **No authentication for game play** beyond optional lobby passwords
  (§1). Mitigation: trusted network.
- **Replays readable by any network peer** (§2.3). Accepted: game data
  only.

## 5. When to revisit

Any of the following changes the assumptions above and requires updating
this doc *before* the change ships:

1. Binding/internet exposure or a reverse-proxy deployment.
2. Adding any secret or state-mutating route over HTTP (→ CORS + auth).
3. Removing/raising the connection caps without a new resource bound.
4. Adding a new WebSocket or TCP listener (→ cap + threat-model entry).
5. Enabling `debug.endpoints` in a non-debug deployment (do not).
