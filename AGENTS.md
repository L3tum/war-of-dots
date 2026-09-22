# AGENTS.md

Guidance for AI agents (and humans) working in this repository.

## Project

War of Dots — a LAN multiplayer RTS in Rust (axum WebSocket server + ECS-style game
engine + optional LLM agents) with a TypeScript/WebGPU frontend served from the
same binary. `src/lib.rs` is the single module tree; `src/main.rs` is a thin CLI
over it.

## Common commands

- `make all` — check + fmt-check + clippy + test + deny + frontend build/check
- `cargo test` — full Rust suite (lib + bins + integration)
- `cd frontend && npm run typecheck && npm test && npm run lint`
- `make release-avx2` — opt-in AVX2 release; the **default build is portable**
  (no target-cpu flags) and a lib test asserts that
- `WOD_SKIP_FRONTEND=1 cargo build` — skip the frontend bundle when iterating on
  Rust only (frontend/dist must already exist)
- CI lives in `.github/workflows/ci.yml`; e2e exit code 77 (skip) is a hard
  failure there, so a silently-skipped e2e cannot pass CI

## reports/ — keep tracked, never gitignore

**`reports/` is intentionally tracked. Do not add it to `.gitignore`, delete it,
or "clean it up" as generated artifacts.**

- It holds the committed benchmark/stress HTML+JSON reports. The benchmark
  harness (`run-benchmark.sh`, `run-stresstest.sh`) writes to the same file
  names on every run, and `--bench-prev reports/<file>` renders 🟢/🔴 deltas
  against that previous report — so the **committed copy in git is the stable
  performance baseline** future runs are compared against.
- Running the harnesses will modify files in `reports/`. That is expected.
  Commit the new reports only when the run was representative (quiet machine,
  release profile, same ticks/units as the baseline). If a run happened under
  load (parallel builds, other agents, CI), the numbers are inflated — restore
  the baseline with `git restore reports/` instead of committing noise, since a
  noisy run would become the baseline everything else is judged against.
- Comparisons are only meaningful when ticks/units match; the report header
  names the file it was compared against.

## Gotchas

- The dhat-based test `alloc_invariants::warm_tick_net_heap_growth_is_bounded`
  uses a process-global allocator: run it **in isolation**, not under
  `--all-targets` (CI does this split; `make test` mirrors it).
- The `microbenchmarks` and `bench-alloc` features gate all benchmark code out
  of default/prof builds — don't reference `benchmark::` items unconditionally.
- The wire protocol is byte-pinned: delta/full-state tests compare exact bytes,
  and `frontend/schema/protocol.schema.json` must stay in sync with
  `src/server/protocol.rs` (CI has a drift gate).
- The engine exposes plain snapshots; wire conversion + the unit baseline store
  live in the server layer (`src/server/delta.rs`). Keep that seam.
