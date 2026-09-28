// Bootstrap smoke test for main.ts.
//
// main.ts has no unit-testable seams — importing the module wires the whole
// app (canvas lookup, WebGPU device, renderer, connection, all UI modules)
// and starts the render loop. This smoke test only proves the entry module
// loads and its bootstrap path runs to a clean, user-visible error overlay
// when device acquisition fails (the fake WebGPU setup provides
// `navigator.gpu` without `requestAdapter`, so `requestWebGPUDevice()`
// rejects), instead of crashing with an unhandled rejection.

import { describe, it, expect, vi, afterEach } from 'vitest';

describe('main.ts bootstrap (smoke)', () => {
  afterEach(() => {
    document.getElementById('error-overlay')?.remove();
    document.getElementById('game-canvas')?.remove();
  });

  it('surfaces a WebGPU init failure as the error overlay', async () => {
    const canvas = document.createElement('canvas');
    canvas.id = 'game-canvas';
    document.body.appendChild(canvas);

    // Importing the module runs main() (fire-and-forget at module scope).
    await import('./main');

    // main() is async — wait for the bootstrap to settle on the error path.
    await vi.waitFor(() => {
      expect(document.getElementById('error-overlay')).not.toBeNull();
    });
    expect(document.getElementById('error-overlay')?.textContent).toContain(
      'Failed to initialize WebGPU',
    );
  });
});
