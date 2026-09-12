import { test, expect } from '@playwright/test';

type ReplayStats = { error: string | null; frames: number; events: number };

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
 * `renderer.lastRenderError`, and the frames >= events invariant polled via
 * the `__replayStats` hook — every applied replay event marks the data dirty
 * and drives at least one render-pass submission, so reaching all 4 fixture
 * events with >= 4 submitted frames proves the ticks applied AND full render
 * passes ran (a fixed timeout could elapse with zero frames, and a flat
 * frame minimum could be met by the pre-replay idle frame).
 * Deliberately no pixel baseline (flake factory).
 */

// The fixture has 4 events; each applied event unconditionally bumps the data
// version, which changes the frame key and drives at least one submitted
// frame. The settle waits for events >= 4 AND frames >= events. (SwiftShader
// is slow, so the poll timeout is generous.)
const MIN_EXPECTED_EVENTS = 4;
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

  // 2) Poll for the frames >= events invariant on the software GPU — the
  //    render loop only advances when a replay event bumps the data version,
  //    so events reaching MIN_EXPECTED_EVENTS with that many (or more)
  //    submitted frames proves the ticks applied AND full render passes ran.
  //    The predicate returns the stats once the invariant holds (so
  //    jsonValue() yields the real numbers) and false until then.
  const stats = await page
    .waitForFunction(
      (minEvents: number) => {
        const s = (window as unknown as { __replayStats: () => ReplayStats }).__replayStats();
        return s.events >= minEvents && s.frames >= s.events ? s : false;
      },
      MIN_EXPECTED_EVENTS,
      { timeout: FRAME_POLL_MS, polling: 250 },
    )
    .then((h) => h.jsonValue() as ReplayStats);
  expect(stats.events, 'all fixture events must have been applied').toBeGreaterThanOrEqual(
    MIN_EXPECTED_EVENTS,
  );
  expect(stats.frames, 'every applied event must have driven a render pass').toBeGreaterThanOrEqual(
    stats.events,
  );

  // 3) No error overlay — one-shot check, not a poll: init/load failures
  //    surface before the hook is installed (step 1) and render errors are
  //    asserted via __replayStats().error (step 4), so there is no race a
  //    poll would catch here — and on failure this surfaces the overlay text
  //    in the message (a poll timeout would drop it).
  const overlayCount = await page.locator('#error-overlay').count();
  if (overlayCount > 0) {
    const overlayText = await page.locator('#error-overlay').innerText();
    throw new Error(`error overlay appeared during replay: ${overlayText}`);
  }

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
