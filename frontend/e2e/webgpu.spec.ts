import { test, expect } from '@playwright/test';

export type ReplayStats = { error: string | null; frames: number };

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
 * `renderer.lastRenderError`, at least a few *completed* frames (polled via
 * the `__replayStats` hook — stronger than a fixed timeout, which could pass
 * with zero frames rendered), and no uncaught page errors.
 * Deliberately no pixel baseline (flake factory).
 */

// The fixture has 4 events; each applied event unconditionally bumps the data
// version, which changes the frame key and drives exactly one rendered frame.
// So a correct replay renders >= 4 frames on the software GPU. Polling for 4
// proves the replay ticks applied AND full render passes ran — a fixed timeout
// could elapse with zero frames, and the count can't be faked by the idle
// frame. (SwiftShader is slow, so the poll timeout is generous.)
const MIN_RENDERED_FRAMES = 4;
const FRAME_POLL_MS = 60_000;
const HOOK_WAIT_MS = 30_000;

test('replay renders with no WebGPU render errors', async ({ page }) => {
  const pageErrors: string[] = [];
  page.on('pageerror', (err) => pageErrors.push(String(err)));

  await page.goto('/?replay=replay_smoke.jsonl', { waitUntil: 'load' });

  // 1) The no-server test hook (installed by main.ts after replay.start())
  //    proves loadReplay + start() succeeded and exposes renderer health
  //    without a game server.
  await page.waitForFunction(
    () =>
      typeof (window as unknown as { __replayStats?: () => unknown }).__replayStats === 'function',
    undefined,
    { timeout: HOOK_WAIT_MS, polling: 500 },
  );

  // 2) Poll for frames that actually completed on the software GPU — the
  //    render loop only advances when a replay event bumps the data version,
  //    so reaching MIN_RENDERED_FRAMES proves the replay ticks applied AND
  //    full render passes ran (a fixed timeout could elapse with zero frames).
  //    The predicate returns the frame count once the threshold is met (so
  //    jsonValue() yields the real number) and false until then.
  const frames = await page
    .waitForFunction(
      (min: number) => {
        const n = (window as unknown as { __replayStats: () => ReplayStats }).__replayStats()
          .frames;
        return n >= min ? n : false;
      },
      MIN_RENDERED_FRAMES,
      { timeout: FRAME_POLL_MS, polling: 250 },
    )
    .then((h) => h.jsonValue() as number);
  expect(frames).toBeGreaterThanOrEqual(MIN_RENDERED_FRAMES);

  // 3) No error overlay — showErrorMessage() creates #error-overlay on any
  //    init/load/render-setup failure (missing adapter, shader compile,
  //    fetch). Poll it for a short window after the frames landed so a late
  //    failure during the replayed ticks is still caught.
  await expect.poll(() => page.locator('#error-overlay').count(), { timeout: 5_000 }).toBe(0);

  // 4) Clean render error across the replayed frames.
  const lastRenderError = await page.evaluate(
    () => (window as unknown as { __replayStats: () => ReplayStats }).__replayStats().error,
  );
  expect(
    lastRenderError,
    'renderer.lastRenderError must be null after the replayed frames',
  ).toBeNull();

  // 5) No uncaught exceptions surfaced during the replayed frames.
  expect(pageErrors, 'uncaught page errors during replay render').toEqual([]);
});
