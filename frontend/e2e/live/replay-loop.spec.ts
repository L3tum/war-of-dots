/**
 * Live replay loop E2E (Layer 2): the full record → serve → play cycle.
 *
 *   create → join → start → both clients render → both disconnect
 *   → the server records the game to {replay_path}/{game_id}.jsonl
 *     ({"full_state"} + {"delta"} per active tick + {"event":"game_end"})
 *   → /replays lists it, /replays/{id}.jsonl serves the file
 *   → a fresh page opens ?replay={id}.jsonl and the ReplayPlayer plays back
 *     SERVER-RECORDED events (not the hand-authored fixture) and renders.
 *
 * This closes the loop the no-server replay tests couldn't: until the server
 * gained recording, every replay in the suite was a hand-written fixture, so
 * "the server's stream is replayable" was never actually tested. The
 * recorded file is the first human player's wire view (full_state + that
 * player's deltas), which is exactly what the frontend ReplayPlayer consumes.
 */
import type { Page } from '@playwright/test';
import { test, expect } from '../harness/live-server';

type GameStats = { error: string | null; frames: number };
type ReplayStats = { error: string | null; frames: number; events: number };

function gameStats(p: Page): Promise<GameStats> {
  return p.evaluate(
    () =>
      (window as unknown as { __gameStats?: () => GameStats }).__gameStats?.() ?? {
        error: '__gameStats hook missing — server binary predates the E2E hook',
        frames: 0,
      },
  );
}

function replayStats(p: Page): Promise<ReplayStats> {
  return p.evaluate(
    () =>
      (window as unknown as { __replayStats?: () => ReplayStats }).__replayStats?.() ?? {
        error: '__replayStats hook missing — server binary predates the E2E hook',
        frames: 0,
        events: 0,
      },
  );
}

/**
 * GET /replays → the recorded game ids on this server.
 *
 * The server's `ReplayInfo` serializes `game_id` (not `id`) — the frontend's
 * replay list UI reads the same field, so the spec must too.
 */
async function listRecordedReplays(url: string): Promise<string[]> {
  const res = await fetch(`${url}/replays`);
  expect(res.status, 'GET /replays should be 200').toBe(200);
  const body = (await res.json()) as { game_id: string }[];
  return body.map((r) => r.game_id);
}

test.describe('live replay loop: record → serve → play', () => {
  // Record + render + replay a real game on CPU SwiftShader — comfortably
  // above the 90s default.
  test.describe.configure({ timeout: 180_000 });

  test(
    'a played game is recorded, served, and plays back in a fresh page',
    async ({ page, liveServer }) => {
      const { url } = liveServer;

      // ── 1) Two humans create, join, and start a real game ───────────────
      await page.goto('/');
      await page.fill('#create-player-name', 'E2E-Replay-Host');
      await page.click('#create-lobby-btn');
      await expect(page.locator('#lobby-info')).toBeVisible({ timeout: 15_000 });
      const code = ((await page.locator('#lobby-code-display').textContent()) ?? '').trim();
      expect(code.length, 'lobby code should be non-empty').toBeGreaterThan(0);

      const guest = await page.context().newPage();
      await guest.goto('/');
      await guest.click('#tab-join');
      await guest.fill('#lobby-code', code);
      await guest.fill('#join-player-name', 'E2E-Replay-Guest');
      await guest.click('#join-lobby-btn');
      await expect(guest.locator('#lobby-info')).toBeVisible({ timeout: 15_000 });

      // Wait for the roster to converge on both clients before starting.
      await expect(page.locator('#lobby-players')).toContainText('E2E-Replay-Guest', {
        timeout: 15_000,
      });
      await expect(guest.locator('#lobby-players')).toContainText('E2E-Replay-Host');

      // Baseline: games that already ended on this worker-scoped server
      // (lobby.spec runs earlier in the same worker) — our game is the NEW
      // id that appears after we disconnect.
      const before = await listRecordedReplays(url);

      await page.click('#start-game-btn');

      // SwiftShader rasterizes on CPU — poll generously, same as lobby.spec.
      await expect
        .poll(() => gameStats(page).then((s) => s.frames), { timeout: 60_000 })
        .toBeGreaterThanOrEqual(5);
      await expect
        .poll(() => gameStats(guest).then((s) => s.frames), { timeout: 60_000 })
        .toBeGreaterThanOrEqual(5);

      // ── 2) Both clients disconnect → the game loop exits as "Abandoned"
      //     and finalizes the replay file (full_state + deltas + game_end).
      await guest.close();
      await page.close();

      // ── 3) The recorded game appears in /replays ────────────────────────
      await expect
        .poll(async () => (await listRecordedReplays(url)).some((id) => !before.includes(id)), {
          timeout: 30_000,
          message: 'our game should be listed in /replays after both clients disconnect',
        })
        .toBe(true);
      const freshId = (await listRecordedReplays(url)).find((id) => !before.includes(id));
      expect(freshId, 'a new replay id should appear').toBeDefined();

      // ── 4) /replays/{id}.jsonl serves a well-formed recording ───────────
      // The DB row is written when the game ends; the file is flushed through
      // the same window, so poll briefly for the 200 rather than assuming it is
      // already on disk by the time the id surfaces in /replays.
      await expect
        .poll(
          async () => (await fetch(`${url}/replays/${freshId}.jsonl`)).status,
          { timeout: 10_000, message: `GET /replays/${freshId}.jsonl should be 200` },
        )
        .toBe(200);
      const fileRes = await fetch(`${url}/replays/${freshId}.jsonl`);
      const lines = (await fileRes.text())
        .split('\n')
        .map((l) => l.trim())
        .filter((l) => l.length > 0)
        .map((l) => JSON.parse(l) as Record<string, unknown>);

      expect(
        lines.length,
        'recording should have full_state + deltas + game_end',
      ).toBeGreaterThanOrEqual(3);
      // First line: the full state snapshot, in the wire shape (terrain is
      // plain enum strings — the shape the ReplayPlayer must accept).
      const first = lines[0] as { full_state?: Record<string, unknown> };
      expect(first.full_state, 'first line must be the full_state event').toBeDefined();
      const terrain = (first.full_state as { terrain: unknown[] }).terrain as unknown[][];
      expect(terrain.length, 'terrain should be a 2-D grid').toBeGreaterThan(0);
      expect(terrain[0].length, 'terrain rows should be non-empty').toBeGreaterThan(0);
      expect(typeof terrain[0][0], 'live terrain cells are plain strings').toBe('string');
      // Middle lines: per-tick deltas.
      const deltas = lines.slice(1, -1).filter((l) => 'delta' in l);
      expect(deltas.length, 'recording should contain at least one delta tick').toBeGreaterThan(0);
      // Last line: the game_end terminal marker.
      const last = lines[lines.length - 1] as { event?: string };
      expect(last.event, 'last line must be the game_end marker').toBe('game_end');

      // ── 5) A fresh page plays the recording back end-to-end ─────────────
      const viewer = await page.context().newPage();
      const pageErrors: string[] = [];
      viewer.on('pageerror', (err: Error) => pageErrors.push(String(err)));
      await viewer.goto(`/?replay=${freshId}.jsonl`);

      // SwiftShader: one event per rendered frame, so a large recording can
      // take a while to reach the end. Require a meaningful slice to have
      // PLAYED (well past the initial full_state) and to have caught up
      // (frames >= events), which proves the pipeline — parse → apply →
      // render — processed server-recorded events without error.
      const MIN_PLAYED = 25;
      await expect
        .poll(async () => {
          const s = await replayStats(viewer);
          return s.events >= MIN_PLAYED && s.frames >= s.events;
        }, {
          timeout: 120_000,
          message: 'replay should apply+render a meaningful slice of the recording',
        })
        .toBe(true);

      const stats = await replayStats(viewer);
      expect(stats.error, 'replay render error').toBeNull();
      expect(
        pageErrors,
        `page errors during replay: ${pageErrors.join(' | ')}`,
      ).toHaveLength(0);
      await viewer.close();
    },
  );
});
