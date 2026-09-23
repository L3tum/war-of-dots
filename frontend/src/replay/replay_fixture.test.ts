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
// Live-recorded replay shape — the server writes the wire messages
// (GameStateMessage / GameDeltaMessage) as JSONL. Terrain there is the plain
// enum-string wire shape (no { type, height } objects), and the file ends
// with a `{ "event": "game_end" }` marker that the player must ignore
// gracefully. This block pins that the player accepts BOTH shapes.
// ============================================================================

describe('live-recorded replay shape (server-written JSONL)', () => {
  const text = [
    JSON.stringify({
      full_state: {
        terrain: [
          ['Forest', 'Plains'],
          ['Water', 'Hill'],
        ],
        units: [],
        cities: [],
        player: { funds: 100, units_owned: 0 },
      },
    }),
    JSON.stringify({
      delta: {
        units_added: [],
        units_removed: [],
        unit_updates: [],
        city_updates: [],
        player: { funds: 110, units_owned: 1 },
      },
    }),
    JSON.stringify({ event: 'game_end', time: 1789000000 }),
  ].join('\n');

  // Serve the given text for whatever `replays/` URL is requested, replacing
  // the file-scoped fixture stub.
  function serveText(body: string): void {
    vi.stubGlobal(
      'fetch',
      vi.fn(async (url: string) => {
        if (!url.startsWith('replays/')) throw new Error(`unexpected fetch: ${url}`);
        return {
          ok: true,
          status: 200,
          statusText: 'OK',
          text: async () => body,
        } as unknown as Response;
      }),
    );
  }

  it('applies string-terrain full_state + wire-shape delta, ignores game_end', async () => {
    serveText(text);
    const canvas = document.createElement('canvas');
    const player = new ReplayPlayer(canvas);
    await player.loadReplay('live_recorded.jsonl');
    player.tick(); // full_state
    player.tick(); // delta
    player.tick(); // game_end marker — must be a no-op, not a crash

    const gs = (
      player as unknown as { gameState: { terrain: string[][]; player: { funds: number } } }
    ).gameState;
    expect(gs.terrain).toEqual([
      ['Forest', 'Plains'],
      ['Water', 'Hill'],
    ]);
    expect(gs.player.funds).toBe(110);
    // All three lines counted as applied (the marker is a valid no-op event).
    expect(player.eventsApplied).toBe(3);
  });

  it('still parses the legacy { type, height } object terrain', async () => {
    const legacy = [
      JSON.stringify({
        full_state: {
          terrain: [
            [
              { type: 'Forest', height: 1 },
              { type: 'Plains', height: 0 },
            ],
          ],
          units: [],
          cities: [],
        },
      }),
    ].join('\n');
    serveText(legacy);
    const canvas = document.createElement('canvas');
    const player = new ReplayPlayer(canvas);
    await player.loadReplay('legacy_fixture.jsonl');
    player.tick();

    const gs = (player as unknown as { gameState: { terrain: string[][] } }).gameState;
    expect(gs.terrain).toEqual([['Forest', 'Plains']]);
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
