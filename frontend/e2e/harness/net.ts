/**
 * Shared helpers for the E2E server fixtures.
 */
import * as net from 'node:net';

/**
 * Reserve and release an OS-assigned free localhost port. There is a small
 * TOCTOU window between release and the fixture binding it — accepted here:
 * the runner controls this machine and the window is microseconds; a race
 * would surface as a loud bind error, never a false green.
 */
export async function freePort(): Promise<number> {
  return new Promise((resolve, reject) => {
    const srv = net.createServer();
    srv.once('error', reject);
    srv.listen(0, '127.0.0.1', () => {
      const port = (srv.address() as net.AddressInfo).port;
      srv.close(() => resolve(port));
    });
  });
}

/** Poll until a TCP connect to 127.0.0.1:port succeeds (or the deadline hits). */
export function waitForTcp(port: number, timeoutMs: number): Promise<void> {
  const deadline = Date.now() + timeoutMs;
  const attempt = (): Promise<void> =>
    new Promise((resolve, reject) => {
      const sock = net.connect(port, '127.0.0.1');
      sock.once('connect', () => {
        sock.end();
        resolve();
      });
      sock.once('error', reject);
    });
  return (async () => {
    let lastErr: unknown;
    for (;;) {
      try {
        await attempt();
        return;
      } catch (err) {
        lastErr = err;
        if (Date.now() >= deadline) break;
        await new Promise((r) => setTimeout(r, 100));
      }
    }
    throw new Error(`nothing listening on 127.0.0.1:${port} after ${timeoutMs}ms (${String(lastErr)})`);
  })();
}
