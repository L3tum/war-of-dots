#!/usr/bin/env bash
# E2E runner — invoked by `make frontend-e2e`. One Playwright run, two layers:
#
#   no-server : dist/ via a static file server — fast render-pipeline smoke
#               (replay mode: real Renderer → WGSL → SwiftShader, no Rust)
#   live      : the real war-of-dots binary — full E2E: the baked-in frontend
#               (include_dir!), real /ws WebSocket traffic, WP3 handshake
#
# This script's job: tool gating (missing tool → exit 77 skip), the build
# pipeline (frontend bundle → server binary embedding that bundle), and xvfb.
# Server lifecycle itself lives in the Playwright harness fixtures
# (e2e/harness/*.ts) — free ports, readiness waits, teardown — so this runner
# stays thin and every testable behavior is testable in TypeScript.
#
# Skips (exit 77 — the TAP/xunit standard "skip" code, so a missing-tools
# environment is distinguishable from a green run) when a required tool is
# absent. Deliberately NOT part of the default `make all` path.
set -u

FE_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
REPO_DIR="$(cd "$FE_DIR/.." && pwd)"

skip() { echo "SKIP frontend-e2e: $1"; exit 77; }

command -v xvfb-run >/dev/null 2>&1 || skip "xvfb-run not found (e.g. apt-get install xvfb)"
command -v python3 >/dev/null 2>&1 || skip "python3 not found (needed by the no-server layer's file server)"
[ -d "$FE_DIR/node_modules/@playwright/test" ] || skip "@playwright/test not installed (cd frontend && npm install)"
# Gate on the ACTUAL chromium binary (not just the chromium-* directory): the
# Playwright config drives this exact path via launchOptions.executablePath.
# Keep the HIGHEST-versioned build, matching playwright.config.ts (which sorts
# by version descending) — an alphabetical first-match could gate on one build
# while the config launches another.
CHROME_BIN=""
CHROME_VER=0
for d in "$HOME"/.cache/ms-playwright/chromium-*/chrome-linux64/chrome; do
  [ -x "$d" ] || continue
  ver="${d%/*}"
  ver="${ver%/*}"
  ver="${ver##*/}"
  ver="${ver#chromium-}"
  if [ "$ver" -gt "$CHROME_VER" ] 2>/dev/null; then
    CHROME_BIN="$d"
    CHROME_VER="$ver"
  fi
done
[ -n "$CHROME_BIN" ] || skip "Playwright Chromium not found (cd frontend && npx playwright install chromium)"

echo "==> Building frontend bundle"
# Fail hard on build errors: with no `set -e`, a broken build would otherwise
# fall through and run Playwright against a stale dist/ (false-green E2E).
make -C "$REPO_DIR" frontend-build || exit 1

# ── Live layer: the server binary ─────────────────────────────────────────────
# The frontend is BAKED INTO THE BINARY at compile time (include_dir! over
# frontend/dist), so the binary must be rebuilt whenever the bundle is newer
# than it — otherwise the live layer would serve (and test) a stale page.
BIN="$REPO_DIR/target/debug/war-of-dots"
if command -v cargo >/dev/null 2>&1; then
  if [ ! -x "$BIN" ] || [ "$FE_DIR/dist/main.js" -nt "$BIN" ]; then
    echo "==> Building server binary (embeds the current frontend bundle)"
    (cd "$REPO_DIR" && cargo build --bin war-of-dots) || exit 1
  fi
elif [ ! -x "$BIN" ]; then
  echo "NOTE: cargo not found and no server binary at $BIN —"
  echo "      the live layer will skip itself (no-server layer still runs)."
fi

echo "==> Type-checking + linting the E2E harness"
# Gate on the harness's own code quality before running it: a broken or
# untypechecked fixture is a false green, same as a stale dist/.
(cd "$FE_DIR" && npx tsc --noEmit -p tsconfig.e2e.json \
  && npx eslint e2e/ playwright.config.ts --max-warnings=0) || exit 1

echo "==> Running Playwright (headless:false under xvfb-run, SwiftShader CPU rendering)"
cd "$FE_DIR"
# The exit status of the suite is this script's exit status — do NOT swallow it
# with `exit $?` (redundant, and fragile if a line is inserted below).
# WOZ_E2E_SERVER_BIN (optional) lets CI point the live layer at a custom build;
# by default the live fixture uses $BIN. The replay fixture is staged into
# dist/replays/ by the no-server harness fixture, not here.
xvfb-run -a npx playwright test --config playwright.config.ts
