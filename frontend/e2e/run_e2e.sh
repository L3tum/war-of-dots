#!/usr/bin/env bash
# No-server WebGPU E2E (Layer 2) runner — invoked by `make frontend-e2e`.
#
# Builds the frontend bundle, stages the committed replay fixture at
# dist/replays/, serves dist/ over a simple static file server, and runs the
# Playwright suite under xvfb-run with the Dawn/SwiftShader software GPU (CPU
# rendering — no GPU hardware required).
#
# Skips (exit 77 — the TAP/xunit standard "skip" code, so a missing-tools
# environment is distinguishable from a green run) when a required tool is
# absent. Deliberately NOT part of the default `make all` path.
set -u

FE_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
REPO_DIR="$(cd "$FE_DIR/.." && pwd)"

skip() { echo "SKIP frontend-e2e: $1"; exit 77; }

command -v xvfb-run >/dev/null 2>&1 || skip "xvfb-run not found (e.g. apt-get install xvfb)"
command -v python3 >/dev/null 2>&1 || skip "python3 not found (needed for the static file server)"
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

echo "==> Staging replay fixture at dist/replays/"
mkdir -p "$FE_DIR/dist/replays"
cp "$FE_DIR/e2e/fixtures/replay_smoke.jsonl" "$FE_DIR/dist/replays/"

# Pick a free localhost port (unless the caller pinned one).
PORT="${WEBGPU_E2E_PORT:-}"
if [ -z "$PORT" ]; then
  PORT="$(python3 - <<'PY'
import socket
s = socket.socket()
s.bind(("127.0.0.1", 0))
print(s.getsockname()[1])
s.close()
PY
)"
fi
URL="http://127.0.0.1:${PORT}"

echo "==> Serving $FE_DIR/dist at $URL"
HTTP_LOG="$(mktemp)"
python3 -m http.server "$PORT" --bind 127.0.0.1 --directory "$FE_DIR/dist" >"$HTTP_LOG" 2>&1 &
SERVER_PID=$!
cleanup() {
  kill "$SERVER_PID" >/dev/null 2>&1 || true
  rm -f "$HTTP_LOG"
}
trap cleanup EXIT

# Wait for the static server to accept connections.
for _ in $(seq 1 50); do
  if python3 -c "import socket; s=socket.socket(); s.settimeout(0.2); s.connect(('127.0.0.1',$PORT)); s.close()" 2>/dev/null; then
    break
  fi
  sleep 0.2
done

echo "==> Running Playwright (headless:false under xvfb-run, SwiftShader CPU rendering)"
cd "$FE_DIR"
# The exit status of the suite is this script's exit status — do NOT swallow it
# with `exit $?` (redundant, and fragile if a line is inserted below).
WEBGPU_E2E_URL="$URL" xvfb-run -a npx playwright test --config playwright.config.ts
