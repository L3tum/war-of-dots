/**
 * Live-server harness (Layer 2: full E2E against the real Rust server).
 *
 * Spawns the actual `war-of-dots` binary — the one that serves the embedded
 * frontend (include_dir!) and the real `/ws` game socket — on a free port
 * with an isolated config (temp-dir storage, no LLM advisor). Specs get a
 * ready, version-locked server via:
 *
 *   import { test, expect } from '../harness/live-server';
 *   test('...', async ({ page }) => { await page.goto('/'); });
 *
 * Important: the frontend is BAKED INTO THE BINARY at compile time
 * (`include_dir!("frontend/dist")`). The runner (run_e2e.sh) therefore
 * rebuilds the binary whenever `dist/main.js` is newer than it — a stale
 * binary would serve an old page and the suite would test the wrong code.
 *
 * If the binary is absent the whole layer is skipped (not failed) with a
 * reason — matching the runner's "missing tools → skip" philosophy.
 */
import { test as base, expect } from '@playwright/test';
import * as child from 'node:child_process';
import * as fs from 'node:fs';
import * as os from 'node:os';
import * as path from 'node:path';
import { fileURLToPath } from 'node:url';
import { freePort, waitForTcp } from './net';

// ESM (package.json "type": "module") — no __dirname.
const HERE = path.dirname(fileURLToPath(import.meta.url));
// frontend/e2e/harness → repo root is three levels up.
const REPO_ROOT = path.resolve(HERE, '..', '..', '..');
const DEFAULT_BIN = path.join(REPO_ROOT, 'target', 'debug', 'war-of-dots');
const DEFAULT_CONFIG = path.join(REPO_ROOT, 'config.toml');
const STARTUP_TIMEOUT_MS = 30_000;

type LiveServerFixture = {
  /** Base URL of the live server (e.g. http://127.0.0.1:48213). */
  url: string;
  /** Port the server listens on. */
  port: number;
};

/**
 * Derive the E2E config from the real repo config.toml so the server runs
 * the production ruleset/unit types, with two changes:
 *  - storage paths → temp dir (E2E must never touch the dev database);
 *  - advisor_provider removed (no LLM calls; the advisor is a manual in-game
 *    toggle and the lobby is human-only).
 */
function writeE2eConfig(baseConfigPath: string, tmpDir: string): string {
  const cfg = fs.readFileSync(baseConfigPath, 'utf8');
  const dbPath = path.join(tmpDir, 'e2e.sqlite');
  const replayPath = path.join(tmpDir, 'replays');
  const e2e = cfg
    .replace(/^database_path\s*=.*$/m, `database_path = "${dbPath}"`)
    .replace(/^replay_path\s*=.*$/m, `replay_path = "${replayPath}"`)
    .replace(/^advisor_provider\s*=.*$/m, '');
  // The rewrites are regex best-effort over an unvalidated TOML: if a key is
  // ever renamed or reformatted, a replace silently no-ops and the live server
  // (spawned with cwd = repo root) would write its database straight into the
  // repo, and/or keep the LLM advisor live. Fail loudly instead of running green.
  if (
    !e2e.includes(`database_path = "${dbPath}"`) ||
    !e2e.includes(`replay_path = "${replayPath}"`)
  ) {
    throw new Error(
      `E2E config rewrite failed to redirect storage into ${tmpDir}: database_path/replay_path no ` +
        'longer match config.toml. Refusing to run (would clobber the dev database).',
    );
  }
  if (/^\s*advisor_provider\s*=/m.test(e2e)) {
    throw new Error(
      'E2E config rewrite failed to remove advisor_provider: the live E2E server must never be able to fire real LLM calls.',
    );
  }
  const configPath = path.join(tmpDir, 'e2e-config.toml');
  fs.writeFileSync(configPath, e2e);
  return configPath;
}

/** Wait until GET / returns 200 (axum router fully up, not just the socket). */
async function waitForHttp(url: string, timeoutMs: number): Promise<void> {
  const deadline = Date.now() + timeoutMs;
  for (;;) {
    try {
      const res = await fetch(url);
      if (res.status === 200) return;
    } catch {
      // not up yet
    }
    if (Date.now() >= deadline) break;
    await new Promise((r) => setTimeout(r, 100));
  }
  throw new Error(`server at ${url} did not return HTTP 200 within ${timeoutMs}ms`);
}

// liveServer is worker-scoped (one binary per worker), so it is declared in
// the second type parameter of extend. The empty first (test-args) param is a
// Playwright idiom — hence the targeted disable.
// eslint-disable-next-line @typescript-eslint/no-empty-object-type
export const test = base.extend<{}, { liveServer: LiveServerFixture }>({
  liveServer: [
    // eslint-disable-next-line no-empty-pattern
    async ({}, use) => {
      const bin = process.env.WOZ_E2E_SERVER_BIN ?? DEFAULT_BIN;
      if (!fs.existsSync(bin)) {
        test.skip(
          true,
          `live E2E skipped: server binary not found at ${bin} (run \`cargo build\`)`,
        );
      }
      const baseConfig = process.env.WOZ_E2E_CONFIG ?? DEFAULT_CONFIG;

      const tmpDir = fs.mkdtempSync(path.join(os.tmpdir(), 'wod-e2e-'));
      const configPath = writeE2eConfig(baseConfig, tmpDir);
      const port = await freePort();

      const proc = child.spawn(
        bin,
        // --bind 127.0.0.1 keeps the test server loopback-only: no LAN
        // exposure of /ws, /replays, /maps, or the debug-build /debug/*
        // endpoints for the whole (unauthenticated) run.
        ['--port', String(port), '--bind', '127.0.0.1', '--config', configPath],
        // cwd = repo root: the map registry scans a cwd-relative `maps/`
        // directory (the built-in "default" map does not depend on it).
        { cwd: REPO_ROOT, stdio: ['ignore', 'pipe', 'pipe'] },
      );
      let log = '';
      proc.stdout?.on('data', (d: Buffer) => (log += d.toString()));
      proc.stderr?.on('data', (d: Buffer) => (log += d.toString()));

      const url = `http://127.0.0.1:${port}`;
      try {
        await waitForTcp(port, STARTUP_TIMEOUT_MS);
        await waitForHttp(url, STARTUP_TIMEOUT_MS);
      } catch (err) {
        proc.kill('SIGKILL');
        throw new Error(
          `live server failed to become ready: ${String(err)}\n--- server log (last 4KB) ---\n${log.slice(-4096)}`,
        );
      }

      await use({ url, port });

      // Teardown: SIGTERM, brief grace, then SIGKILL; drop the temp config dir.
      const exited = new Promise<void>((resolve) => proc.once('exit', () => resolve()));
      if (proc.exitCode === null) proc.kill('SIGTERM');
      await Promise.race([exited, new Promise((r) => setTimeout(r, 5_000))]);
      if (proc.exitCode === null) proc.kill('SIGKILL');
      fs.rmSync(tmpDir, { recursive: true, force: true });
    },
    { scope: 'worker' },
  ],

  // Relative page.goto('/') resolves against the live server for every test.
  // (baseURL is test-scoped in Playwright, so this override is too — it may
  // still depend on the worker-scoped fixture.)
  baseURL: async ({ liveServer }, use) => {
    await use(liveServer.url);
  },
});

export { expect };
