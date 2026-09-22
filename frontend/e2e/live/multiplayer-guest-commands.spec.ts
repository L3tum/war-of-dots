/**
 * Live multiplayer E2E (C1 regression): every NON-CREATOR player must be able
 * to act.
 *
 * Bug (C1): at game start the server re-mints in-game player IDs as
 * `player_{slot}` (slot i == lobby player i), but the StartGame handler
 * remapped only the STARTER's `player_map` entry to "player_0". Players 2..N
 * kept their lobby-era `player_<8hex>` IDs, and the engine's ownership gate
 * (move_troops / recruit_unit / upgrade_city / toggle_* / embark_*) silently
 * early-returns when the sender's ID != the unit/city owner — so every
 * non-creator could watch the game but never mutate it.
 *
 * This spec proves the guest's commands actually land:
 *   1. Host (real browser page, same UI flow as lobby.spec.ts) creates a
 *      lobby.
 *   2. Guest = a second, raw WebSocket connection (Node's WebSocket + cbor-x)
 *      speaking the same CBOR protocol: Hello → JoinLobby.
 *   3. Host clicks start; the guest receives a FullState scoped to its
 *      in-game slot id.
 *   4. The guest issues MoveTroop for all three of its starting units and
 *      ToggleHealing on one of them.
 *   5. Assertions on the guest's own delta stream:
 *        - each moved unit gets a non-empty `path` (MoveTroop was accepted
 *          by the engine — before the fix the ownership gate dropped it
 *          silently and no path would ever arrive);
 *        - the toggled unit's `healing` flag arrives as `true` (ToggleHealing
 *          was applied — same silent-drop failure mode before the fix).
 *
 * Both assertions are server-authoritative (the guest's per-player
 * DeltaState), so a false green would require the engine itself to lie.
 *
 * Why the assertions stop at "the engine accepted the command": the three
 * starting units spawn in a 2-cell row (city, city±2) while the engine's
 * occupancy gate (MIN_UNIT_DISTANCE = 8) blocks any step whose target is
 * within 8 cells of ANOTHER unit — so the starting formation never physically
 * disperses (the creator's identical formation stalls the same way; it is an
 * engine-wide property, not a C1/multiplayer one). A recruit at the city is
 * likewise rejected while a unit stands on it (cell_occupied gate). Waiting
 * for real displacement or a recruit here would make this test fail for
 * reasons unrelated to C1.
 */
import { WebSocket as NodeWebSocket } from 'node:http';
import { decode, encode } from 'cbor-x';
import { test, expect } from '../harness/live-server';

/** Instance type of the Node built-in (undici) WebSocket. */
type HttpWebSocket = InstanceType<typeof NodeWebSocket>;

// ── Minimal wire types (see src/server/protocol.rs) ──────────────────────────

interface UnitStateWire {
  id: string;
  owner: string;
  position: unknown;
  unit_type: string;
}

interface CityStateWire {
  id: string;
  position: unknown;
  owner: unknown;
}

interface PlayerStateWire {
  funds: number;
  units_owned: number;
  player_id: string;
}

interface FullStateWire {
  terrain: string[][];
  units: UnitStateWire[];
  cities: CityStateWire[];
  player: PlayerStateWire;
}

interface UnitUpdateWire {
  id: string;
  position?: number | null;
  path?: unknown;
  healing?: boolean | null;
}

interface DeltaWire {
  units_added?: UnitStateWire[];
  unit_updates?: UnitUpdateWire[];
  player?: PlayerStateWire;
}

type WsMsg = Record<string, unknown>;

function asPoint2(v: unknown): [number, number] {
  const a = v as number[];
  return [a[0], a[1]];
}


// ── Raw-WS guest client ──────────────────────────────────────────────────────

/**
 * A minimal game-protocol client over a real WebSocket, standing in for a
 * second human player. Messages are buffered and fan out to listeners, so
 * the spec can both wait for specific messages and track per-delta state.
 */
class GuestClient {
  private readonly ws: HttpWebSocket;
  private readonly queue: WsMsg[] = [];
  private readonly listeners = new Set<(m: WsMsg) => void>();
  private readonly notifiers = new Set<() => void>();
  private isOpen = false;
  private openWaiters = new Set<() => void>();
  private failReason: string | null = null;
  /** Server `Error` messages seen on this connection (diagnostics). */
  readonly serverErrors: string[] = [];

  constructor(wsUrl: string) {
    this.ws = new NodeWebSocket(wsUrl);
    // Node's WebSocket defaults to binaryType 'blob' (web spec) — force
    // arraybuffer so message data is plain bytes.
    this.ws.binaryType = 'arraybuffer';
    this.ws.onopen = () => {
      this.isOpen = true;
      for (const w of [...this.openWaiters]) w();
      this.openWaiters.clear();
    };
    this.ws.onerror = () => {
      this.failReason = 'guest websocket error (server closed or unreachable)';
      this.wake();
    };
    this.ws.onclose = (ev) => {
      if (!this.isOpen) this.failReason = `guest websocket failed (close code ${ev.code})`;
      else this.failReason = `guest websocket closed (code ${ev.code})`;
      this.wake();
      for (const w of [...this.openWaiters]) w();
      this.openWaiters.clear();
    };
    this.ws.onmessage = (ev) => {
      const data: unknown = ev.data;
      let bytes: Uint8Array;
      if (data instanceof ArrayBuffer) bytes = new Uint8Array(data);
      else if (data instanceof Uint8Array) bytes = data;
      else if (Buffer.isBuffer(data))
        bytes = new Uint8Array(data.buffer, data.byteOffset, data.byteLength);
      else bytes = new Uint8Array((data as ArrayBufferView).buffer);
      const msg = decode(bytes) as WsMsg;
      const err = msg.Error as { message?: unknown } | undefined;
      if (err && typeof err.message === 'string') this.serverErrors.push(err.message);
      this.queue.push(msg);
      for (const l of this.listeners) l(msg);
      this.wake();
    };
  }

  async waitOpen(timeoutMs: number): Promise<void> {
    if (this.isOpen) return;
    if (this.failReason) throw new Error(`guest websocket: ${this.failReason}`);
    await new Promise<void>((resolve, reject) => {
      const timer = setTimeout(() => {
        waiters.delete(wake);
        reject(new Error(`guest websocket did not open within ${timeoutMs}ms`));
      }, timeoutMs);
      const wake = (): void => {
        clearTimeout(timer);
        waiters.delete(wake);
        if (this.isOpen) resolve();
        else reject(new Error(`guest websocket: ${this.failReason ?? 'closed before open'}`));
      };
      const waiters = this.openWaiters;
      waiters.add(wake);
      // Re-check: onopen may have fired between the guard above and the add.
      if (this.isOpen) wake();
    });
  }

  send(msg: unknown): void {
    if (this.ws.readyState !== 1) throw new Error('guest ws not open (readyState ' + this.ws.readyState + ')');
    this.ws.send(encode(msg));
  }

  observe(l: (m: WsMsg) => void): void {
    this.listeners.add(l);
  }

  /** Scan the buffer (and future messages) until `pred` matches or the deadline. */
  async next(pred: (m: WsMsg) => boolean, timeoutMs: number, label: string): Promise<WsMsg> {
    const deadline = Date.now() + timeoutMs;
    for (;;) {
      if (this.failReason) throw new Error(`${label}: ${this.failReason}`);
      const idx = this.queue.findIndex(pred);
      if (idx >= 0) return this.queue.splice(idx, 1)[0];
      const remaining = deadline - Date.now();
      if (remaining <= 0) {
        const seen = this.queue
          .slice(0, 8)
          .map((m) => Object.keys(m).join('|'))
          .join(', ');
        throw new Error(
          `timed out after ${timeoutMs}ms waiting for ${label}` +
            (this.serverErrors.length ? `; server errors: ${this.serverErrors.join('; ')}` : '') +
            `; recent messages: ${seen}`,
        );
      }
      await new Promise<void>((resolve) => {
        const timer = setTimeout(() => {
          this.notifiers.delete(go);
          resolve();
        }, remaining);
        const go = (): void => {
          clearTimeout(timer);
          resolve();
        };
        this.notifiers.add(go);
      });
    }
  }

  /** Poll `cond` until true (woken by new messages), up to `timeoutMs`. */
  async waitFor(label: string, timeoutMs: number, cond: () => boolean): Promise<void> {
    const deadline = Date.now() + timeoutMs;
    for (;;) {
      if (cond()) return;
      if (this.failReason) throw new Error(`${label}: ${this.failReason}`);
      const remaining = deadline - Date.now();
      if (remaining <= 0) {
        throw new Error(
          `timed out after ${timeoutMs}ms waiting for ${label}` +
            (this.serverErrors.length ? `; server errors: ${this.serverErrors.join('; ')}` : ''),
        );
      }
      await new Promise<void>((resolve) => {
        const timer = setTimeout(() => {
          this.notifiers.delete(go);
          resolve();
        }, remaining);
        const go = (): void => {
          clearTimeout(timer);
          resolve();
        };
        this.notifiers.add(go);
      });
    }
  }

  close(): void {
    try {
      this.ws.close();
    } catch {
      // already closed
    }
  }

  private wake(): void {
    for (const n of [...this.notifiers]) n();
  }
}

// ── The regression test ──────────────────────────────────────────────────────

test.describe('live multiplayer: non-creator commands (C1 regression)', () => {
  test('guest MoveTroop + ToggleHealing mutate the game state', async ({
    page,
    liveServer,
  }) => {
    // ── 1) Host: real browser page creates the lobby ─────────────────────────
    await page.goto('/');
    await page.fill('#create-player-name', 'E2E-Host');
    await page.click('#create-lobby-btn');
    await expect(page.locator('#lobby-info')).toBeVisible({ timeout: 15_000 });
    const code = ((await page.locator('#lobby-code-display').textContent()) ?? '').trim();
    expect(code.length, 'lobby code should be non-empty').toBeGreaterThan(0);

    // ── 2) Guest: a second connection (raw WS) joins the same lobby ──────────
    const guest = new GuestClient(`ws://127.0.0.1:${liveServer.port}/ws`);
    await guest.waitOpen(10_000);
    try {
      guest.send({ Hello: { protocol_version: 1, features: 0 } });
      // CBOR unit variants (e.g. ClearSpectator) decode to bare strings —
      // the `in` operator throws on primitives, so guard the object shape.
      const isObj = (m: WsMsg): boolean => typeof m === 'object' && m !== null;
      const ack = await guest.next((m) => isObj(m) && 'HelloAck' in m, 10_000, 'HelloAck');
      expect((ack.HelloAck as { protocol_version: number }).protocol_version).toBe(1);

      guest.send({
        LobbyMessage: {
          JoinLobby: { lobby_code: code, password: null, player_name: 'E2E-Guest' },
        },
      });
      const joined = await guest.next((m) => isObj(m) && 'LobbyJoined' in m, 10_000, 'LobbyJoined');
      expect((joined.LobbyJoined as { lobby_code: string }).lobby_code).toBe(code);

      // ── 3) Host starts; the guest receives its FullState ───────────────────
      await page.click('#start-game-btn');
      const fullMsg = await guest.next((m) => isObj(m) && 'FullState' in m, 30_000, 'FullState');
      const gs = fullMsg.FullState as FullStateWire;
      const guestPid = gs.player.player_id;
      // The engine knows the guest by its in-game slot id (player_{i}).
      expect(guestPid, 'guest in-game id must be a slot id').toMatch(/^player_\d+$/);

      const myUnits = gs.units.filter((u) => u.owner === guestPid);
      expect(myUnits, 'guest owns the 3-unit starting force').toHaveLength(3);

      // The starting force sits on exactly one of the guest's cities (all
      // three units within 2.5 cells of it).
      const recruitCity = gs.cities.find(
        (c) =>
          c.owner === guestPid &&
          Math.hypot(
            asPoint2(c.position)[0] - asPoint2(myUnits[0].position)[0],
            asPoint2(c.position)[1] - asPoint2(myUnits[0].position)[1],
          ) <= 2.5,
      );
      expect(recruitCity, 'a guest-owned city hosts the starting force').toBeDefined();
      const cityPos = asPoint2((recruitCity as CityStateWire).position);

      // The map is 200x200 with the starting cities on corners: 25 cells
      // toward the map center along x stays on open plains. (The destination
      // is not asserted to be reached — see the file header: the starting
      // formation is locked by the engine-wide occupancy gate.) The +0.5/
      // +0.25 offsets keep the coordinates NON-integer: cbor-x encodes
      // whole-number floats as CBOR integers, which the server's ciborium
      // rejects for f32 fields (the same reason protocol.rs has float_deser
      // for the float settings).
      const mapW = gs.terrain[0].length;
      const tx = cityPos[0] + 25 * (cityPos[0] < mapW / 2 ? 1 : -1) + 0.5;
      const ty = cityPos[1] + 0.25;

      // Per-delta tracking (the guest's own, server-authoritative stream).
      const pathSeenFor = new Set<string>();
      const healingSeenFor = new Set<string>();
      guest.observe((m) => {
        const d = m.DeltaState as DeltaWire | undefined;
        if (!d) return;
        for (const up of d.unit_updates ?? []) {
          if (up.path && (up.path as unknown[]).length > 0) pathSeenFor.add(up.id);
          if (up.healing === true) healingSeenFor.add(up.id);
        }
      });

      // ── 4) Guest issues MoveTroop for all three starting units ──────────────
      expect(gs.player.units_owned, 'guest starts with 3 units').toBe(3);
      for (const u of myUnits) {
        guest.send({ MoveTroop: { troop_id: u.id, path: [[tx, ty]] } });
      }
      // The engine accepted every move: each unit got a path (before the C1
      // fix the ownership gate dropped the commands silently — no path would
      // ever arrive on the guest's own stream).
      for (const u of myUnits) {
        await guest.waitFor(
          `MoveTroop accepted for unit ${u.id}`,
          15_000,
          () => pathSeenFor.has(u.id),
        );
      }

      // ── 5) Guest toggles healing on one of its units ────────────────────────
      const healTarget = myUnits[1];
      guest.send({ ToggleHealing: { unit_id: healTarget.id, enabled: true } });
      await guest.waitFor(
        `ToggleHealing applied to unit ${healTarget.id}`,
        15_000,
        () => healingSeenFor.has(healTarget.id),
      );

      // The engine applied both command families on the guest's in-game slot
      // id: its commands mutated the game.
      expect(pathSeenFor.size, 'all 3 guest units received a path').toBe(3);
      expect(healingSeenFor.has(healTarget.id), 'healing toggle reached the unit').toBe(true);
    } finally {
      guest.close();
    }
  });
});
