import { test, expect } from '@playwright/test';

/**
 * No-server WebGPU smoke test (Layer 2).
 *
 * Plays the committed `replay_smoke.jsonl` fixture through the *real* in-page
 * replay pipeline (`ReplayPlayer` → real `Renderer` → real WGSL shaders
 * compiled by naga/tint and rasterized on CPU by Dawn/SwiftShader). No game
 * server and no WebSocket are involved — the page and fixture are served
 * statically (see the `frontend-e2e` make target).
 *
 * Asserts only render health: no error overlay, a clean
 * `renderer.lastRenderError`, and no uncaught page errors across the
 * replayed frames. Deliberately no pixel baseline (flake factory).
 */

// The replay fixture has 4 events (1 full_state + 3 deltas); each is applied on
// a requestAnimationFrame tick, and we then want several clean render passes.
// SwiftShader is slow, so allow generous headroom.
const REPLAY_SETTLE_MS = 8000;
const HOOK_WAIT_MS = 30_000;

test('replay renders with no WebGPU render errors', async ({ page }) => {
  const pageErrors: string[] = [];
  page.on('pageerror', (err) => pageErrors.push(String(err)));

  await page.goto('/?replay=replay_smoke.jsonl', { waitUntil: 'load' });

  // Let the device initialize, the render loop start, the four replay ticks
  // apply, and a handful of full frames render on the software GPU.
  await page.waitForTimeout(REPLAY_SETTLE_MS);

  // 1) No error overlay — showErrorMessage() creates #error-overlay on any
  //    init/load/render-setup failure (missing adapter, shader compile, fetch).
  const overlay = page.locator('#error-overlay');
  if ((await overlay.count()) > 0) {
    const text = (await overlay.innerText()).replace(/\s+/g, ' ').trim();
    throw new Error(`WebGPU error overlay present: ${text}`);
  }
  await expect(overlay).toHaveCount(0);

  // 2) The no-server test hook (installed by main.ts after replay.start())
  //    proves loadReplay + start() succeeded and exposes the renderer's
  //    lastRenderError without a game server.
  await page.waitForFunction(
    () =>
      typeof (window as unknown as { __replayRenderError?: () => string | null })
        .__replayRenderError === 'function',
    undefined,
    { timeout: HOOK_WAIT_MS, polling: 500 },
  );
  const lastRenderError = await page.evaluate(() =>
    (window as unknown as { __replayRenderError: () => string | null }).__replayRenderError(),
  );
  expect(
    lastRenderError,
    'renderer.lastRenderError must be null after the replayed frames',
  ).toBeNull();

  // 3) No uncaught exceptions surfaced during the replayed frames.
  expect(pageErrors, 'uncaught page errors during replay render').toEqual([]);
});
