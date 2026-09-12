/**
 * Smoke test for the committed replay fixture
 * (`e2e/fixtures/replay_smoke.jsonl`) — runs it through the real
 * `ReplayPlayer` event path so a hand-authored (or regenerated) fixture
 * cannot silently drift from what the player actually consumes.
 *
 * Exercises: legacy full_state (terrain/vision/border/units/cities),
 * I3 packed unit position, I4 sparse border patch, and flat vision diff.
 */
import { describe, it, expect, vi, beforeEach } from 'vitest';
import { readFileSync } from 'node:fs';
import { fileURLToPath } from 'node:url';
import { dirname, join } from 'node:path';
import { ReplayPlayer } from './replay_player';

const here = dirname(fileURLToPath(import.meta.url));
const FIXTURE_PATH = join(here, '../../e2e/fixtures/replay_smoke.jsonl');

// File-scoped (not describe-scoped) so EVERY test — including the filename
// validation block below — gets a FRESH fetch mock per test.
beforeEach(() => {
  const text = readFileSync(FIXTURE_PATH, 'utf8');
  vi.stubGlobal(
    'fetch',
    vi.fn(async (url: string) => {
      if (!url.startsWith('replays/')) throw new Error(`unexpected fetch: ${url}`);
      return {
        ok: true,
        status: 200,
        statusText: 'OK',
        text: async () => text,
      } as unknown as Response;
    }),
  );
});

describe('replay_smoke.jsonl fixture', () => {
  it('applies full_state + all three deltas through the real player', async () => {
    const canvas = document.createElement('canvas');
    const player = new ReplayPlayer(canvas);
    await player.loadReplay('replay_smoke.jsonl');
    expect(fetch).toHaveBeenCalled();

    // Four events in the fixture → four ticks apply them all.
    for (let i = 0; i < 4; i++) player.tick();

    // gameState is private; the test inspects it the way the renderer would.
    const gs = (
      player as unknown as {
        gameState: {
          terrain: string[][];
          visionGrid: boolean[][];
          borderGrid: number[][];
          units: Map<
            string,
            {
              position: { x: number; y: number };
              men: number;
              onWater: boolean;
            }
          >;
          cities: Map<string, { manpower: number }>;
          player: { funds: number };
        };
      }
    ).gameState;

    // Full state
    expect(gs.terrain).toHaveLength(8);
    expect(gs.terrain[0][5]).toBe('Water'); // river column
    expect(gs.terrain[0][0]).toBe('Forest');
    expect(gs.units).toHaveLength(3);
    expect(gs.units.get('u3')?.onWater).toBe(true); // water unit from full state
    expect(gs.cities.get('c1')?.manpower).toBe(1000);
    expect(gs.player.funds).toBe(950); // after the tick-10 delta

    // I3: tick-10 delta moved u1 to (3,3) via a packed position on the 8x8 map
    expect(gs.units.get('u1')?.position).toEqual({ x: 3, y: 3 });
    expect(gs.units.get('u1')?.men).toBe(95);

    // I4: tick-20 delta flipped border cell (x=4, y=3) from 0 → 1
    expect(gs.borderGrid[3][4]).toBe(1);
    // ...and left the rest of the west bank untouched
    expect(gs.borderGrid[3][3]).toBe(0);
    expect(gs.borderGrid[3][5]).toBe(255); // river stays no-owner

    // Flat vision diff: tick-30 revealed the previously fogged corner
    expect(gs.visionGrid[6][7]).toBe(true);
    expect(gs.visionGrid[7][7]).toBe(true);
  });

  it('stops applying events once the fixture is exhausted', async () => {
    const canvas = document.createElement('canvas');
    const player = new ReplayPlayer(canvas);
    await player.loadReplay('replay_smoke.jsonl');

    const gs = (
      player as unknown as {
        gameState: {
          units: Map<string, { men: number }>;
          player: { funds: number };
        };
      }
    ).gameState;
    for (let i = 0; i < 4; i++) player.tick();
    const fundsAfter = gs.player.funds;
    // Extra ticks past the end of the fixture must be no-ops.
    player.tick();
    player.tick();
    expect(gs.player.funds).toBe(fundsAfter);
  });
});

// ============================================================================
// loadReplay filename validation — `?replay=` is user-controlled (URL query
// param), so the basename guard's REJECTION paths must be tested, not just
// the happy path above.
// ============================================================================

describe('loadReplay filename validation (?replay= is user-controlled)', () => {
  async function expectRejected(file: string): Promise<void> {
    const canvas = document.createElement('canvas');
    const player = new ReplayPlayer(canvas);
    await expect(
      player.loadReplay(file),
      `loadReplay(${JSON.stringify(file)}) must reject`,
    ).rejects.toThrow(/Invalid replay file name/);
    // Rejected BEFORE any network access — the whole point of validating
    // before fetch is that a hostile name never reaches the static server.
    expect(fetch, `no fetch for rejected name ${JSON.stringify(file)}`).not.toHaveBeenCalled();
  }

  it('rejects parent-directory traversal', () => expectRejected('../secrets.jsonl'));
  it('rejects double-dot traversal', () => expectRejected('../../etc/passwd.jsonl'));
  it('rejects nested paths', () => expectRejected('a/b.jsonl'));
  it('rejects URL-encoded traversal', () => expectRejected('%2e%2e/secrets.jsonl'));
  it('rejects non-jsonl extensions', () => expectRejected('evil.json'));
  it('rejects embedded spaces/control chars', () => expectRejected('bad name.jsonl'));
  it('rejects NUL bytes', () => expectRejected('nul\0name.jsonl'));
  it('rejects over-long names (>128 chars)', () => expectRejected(`${'a'.repeat(130)}.jsonl`));

  it('accepts a plain basename (happy path)', async () => {
    const canvas = document.createElement('canvas');
    const player = new ReplayPlayer(canvas);
    await player.loadReplay('replay_smoke.jsonl');
    expect(fetch).toHaveBeenCalledTimes(1);
  });
});
