/**
 * Live lobby E2E (Layer 2): real browsers ↔ real `war-of-dots` server.
 *
 * This is the "E2E" in the make target's name — the full stack:
 * real Rust server (axum + real /ws WebSocket), real browser page (the
 * frontend baked into the binary), real CBOR-over-WebSocket traffic,
 * including the WP3 Hello handshake that connection.ts performs on open
 * (if the handshake gate were broken, LobbyCreated would never arrive and
 * every expectation below would time out).
 */
import type { Page } from '@playwright/test';
import { test, expect } from '../harness/live-server';

type GameStats = { error: string | null; frames: number };

function gameStats(p: Page): Promise<GameStats> {
  return p.evaluate(
    () =>
      (window as unknown as { __gameStats?: () => GameStats }).__gameStats?.() ?? {
        error: '__gameStats hook missing — server binary predates the E2E hook',
        frames: 0,
      },
  );
}

test.describe('live lobby: two humans, one server', () => {
  test('create → join → start: both clients receive state and render frames', async ({ page }) => {
    // ── 1) Host: open the page (Hello handshake happens on WS open) ────────
    await page.goto('/');

    await page.fill('#create-player-name', 'E2E-Host');
    await page.click('#create-lobby-btn');

    // LobbyCreated arrives only after the handshake completed.
    await expect(page.locator('#lobby-info')).toBeVisible({ timeout: 15_000 });
    const code = ((await page.locator('#lobby-code-display').textContent()) ?? '').trim();
    expect(code.length, 'lobby code should be non-empty').toBeGreaterThan(0);
    await expect(page.locator('#lobby-players')).toContainText('E2E-Host');

    // ── 2) Guest: a second page = an independent connection + handshake ────
    const guest = await page.context().newPage();
    await guest.goto('/');

    await guest.click('#tab-join');
    await guest.fill('#lobby-code', code);
    await guest.fill('#join-player-name', 'E2E-Guest');
    await guest.click('#join-lobby-btn');

    await expect(guest.locator('#lobby-info')).toBeVisible({ timeout: 15_000 });
    const guestCode = ((await guest.locator('#lobby-code-display').textContent()) ?? '').trim();
    expect(guestCode, 'guest must see the same lobby code').toBe(code);

    // ── 3) LobbyUpdate broadcast: both clients converge on the roster ──────
    await expect(page.locator('#lobby-players')).toContainText('E2E-Guest', { timeout: 15_000 });
    await expect(guest.locator('#lobby-players')).toContainText('E2E-Host');
    await expect(guest.locator('#lobby-players')).toContainText('E2E-Guest');

    // ── 4) Start: both receive FullState and the render loop runs ──────────
    await page.click('#start-game-btn');

    // SwiftShader rasterizes on CPU — poll generously. Reaching a small
    // number of SUBMITTED frames proves FullState was applied (data dirty)
    // and real render passes executed on both clients.
    await expect
      .poll(() => gameStats(page).then((s) => s.frames), { timeout: 60_000 })
      .toBeGreaterThanOrEqual(5);
    await expect
      .poll(() => gameStats(guest).then((s) => s.frames), { timeout: 60_000 })
      .toBeGreaterThanOrEqual(5);

    const hostStats = await gameStats(page);
    const guestStats = await gameStats(guest);
    expect(hostStats.error, 'host render error').toBeNull();
    expect(guestStats.error, 'guest render error').toBeNull();
  });
});
