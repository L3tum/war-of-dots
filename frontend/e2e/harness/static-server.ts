/**
 * No-server harness (Layer 1: replay render pipeline).
 *
 * Serves the built `dist/` with a plain static file server — no Rust binary,
 * no WebSocket. This is the fast, dependency-light layer: it proves the real
 * in-page render stack (Renderer → WGSL → SwiftShader) works standalone.
 *
 * The server lifecycle lives in a worker-scoped Playwright fixture, so specs
 * only need:
 *
 *   import { test, expect } from '../harness/static-server';
 *   test('...', async ({ page }) => { await page.goto('/?replay=...'); });
 *
 * The `baseURL` fixture override is what makes relative `page.goto('/')`
 * work — specs never deal with URLs or ports.
 */
import { test as base, expect } from '@playwright/test';
import * as child from 'node:child_process';
import * as fs from 'node:fs';
import * as path from 'node:path';
import { fileURLToPath } from 'node:url';
import { freePort, waitForTcp } from './net';

// ESM (package.json "type": "module") — no __dirname.
const HERE = path.dirname(fileURLToPath(import.meta.url));
const DIST_DIR = path.resolve(HERE, '..', '..', 'dist');
const REPLAY_FIXTURE = path.resolve(HERE, '..', 'fixtures', 'replay_smoke.jsonl');

type StaticServerFixture = {
  /** Base URL of the static file server (e.g. http://127.0.0.1:53211). */
  staticUrl: string;
};

// staticUrl is worker-scoped (one server per worker), so it is declared in
// the second type parameter of extend. The empty first (test-args) param is a
// Playwright idiom — hence the targeted disable.
// eslint-disable-next-line @typescript-eslint/no-empty-object-type
export const test = base.extend<{}, StaticServerFixture>({
  staticUrl: [
    // eslint-disable-next-line no-empty-pattern
    async ({}, use) => {
      const port = await freePort();

      // Stage the committed replay fixture into dist/replays/ (the page
      // fetches it by relative path in ?replay= mode).
      const replaysDir = path.join(DIST_DIR, 'replays');
      fs.mkdirSync(replaysDir, { recursive: true });
      fs.copyFileSync(REPLAY_FIXTURE, path.join(replaysDir, 'replay_smoke.jsonl'));

      const proc = child.spawn(
        'python3',
        ['-m', 'http.server', String(port), '--bind', '127.0.0.1', '--directory', DIST_DIR],
        { stdio: ['ignore', 'ignore', 'pipe'] },
      );
      let stderr = '';
      proc.stderr?.on('data', (d: Buffer) => (stderr += d.toString()));
      const killed = { v: false };
      proc.on('exit', () => (killed.v = true));

      try {
        await waitForTcp(port, 10_000);
      } catch (err) {
        proc.kill();
        throw new Error(`static server failed to start: ${String(err)}\n${stderr}`);
      }

      await use(`http://127.0.0.1:${port}`);

      // Teardown: kill the static server (it is a plain python process —
      // SIGTERM suffices; no child processes of its own).
      if (!killed.v) proc.kill('SIGTERM');
    },
    { scope: 'worker' },
  ],

  // Relative page.goto('/') resolves against the static server for every
  // test in this harness. (baseURL is test-scoped in Playwright, so this
  // override is too — it may still depend on the worker-scoped fixture.)
  baseURL: async ({ staticUrl }, use) => {
    await use(staticUrl);
  },
});

export { expect };
