# War of Dots — Deployment Runbook

How to build, deploy, and operate a production server. CI (`.github/workflows/ci.yml`)
enforces everything stated here mechanically; this document is the operator-facing
runbook.

## 1. Build

Production builds must be **release** builds:

```bash
cargo build --release --bin war-of-dots
# -> target/release/war-of-dots
```

The frontend is **embedded** into the binary: the `frontend/dist/main.js` bundle is
inlined via `include_str!`. Rebuild the frontend before building the server so the
embedded bundle matches the sources:

```bash
make frontend-build   # esbuild -> frontend/dist/main.js
```

CI verifies the release build compiles cleanly (`cargo check --release --lib --bins`)
and that the binary embeds the current bundle.

## 2. Configure

Copy `config.toml` next to the binary and edit:

```toml
[server]
bind = "0.0.0.0:8080"        # public bind address

[llm]
enabled = true
provider = "openai"          # "openai" | "anthropic" | "ollama"
# model / api_key: set in config OR the OPENAI_API_KEY / ANTHROPIC_API_KEY env vars
# (env vars take precedence). LLM features are OFF by default when [llm] is absent.

[debug]
endpoints = false            # MUST be false in production
```

- **`[llm] api_key`**: operators should prefer the `OPENAI_API_KEY` /
  `ANTHROPIC_API_KEY` environment variables over the config file (keeps secrets out
  of the repo-tracked config template).
- **`[debug] endpoints`**: the `/debug/*` HTTP endpoints (stats, tick info,
  benchmark) exist only in debug builds / with the `debug-overlay` feature, and the
  flag additionally gates them at request time. In a production (release, no
  feature) deployment the routes are not registered **and** the handlers 404 even if
  a route slips through — defense in depth. Never set `endpoints = true` on a public
  server: the benchmark endpoint is CPU-heavy and the stats endpoint leaks
  per-tick timing.

## 3. Run

```bash
./target/release/war-of-dots
```

- Storage: `./data/` (SQLite DB + JSONL replays), created on first run. Point the
  binary at a persistent volume for anything that should survive restarts.
- The server serves the embedded SPA at `/`, the game WebSocket at `/ws`, and the
  replay endpoints (`/replays`, `/replays/:id`).
- No external services are required. LLM calls go out to the configured provider
  over HTTPS.

## 4. Operations checklist

- [ ] Release build (see §1), frontend bundle rebuilt before the server build
- [ ] `config.toml`: public `bind`, `endpoints = false`, LLM credentials via env
- [ ] `./data/` on persistent storage (back it up for leaderboards + replays)
- [ ] TLS terminated at a reverse proxy in front of the bind address
- [ ] Watch logs for `encode failed` lines — a CBOR encode failure closes the
      connection by design (clients see a reconnect); a burst of them indicates a
      protocol/schema mismatch between server and a stale client
