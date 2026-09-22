//! Shared WebGPU bootstrap — the single adapter + device acquisition sequence
//! used by every page path (live game, live replay/spectator WebSocket).

/**
 * Request a high-performance WebGPU adapter and a device from it.
 *
 * Both former in-line copies (the live game path and the replay WebSocket
 * path in main.ts) requested the same things, so this is their exact union:
 * one adapter request with `powerPreference: 'high-performance'` and NO
 * feature requests and NO explicit limits (driver defaults apply), followed
 * by a plain `requestDevice()`. Do not add features/limits here without
 * auditing every call site — Renderer builds its pipelines against the
 * device it is handed.
 *
 * @throws if `navigator.gpu` is unavailable (browser without WebGPU), if no
 *         adapter is available, or if `requestDevice()` fails.
 */
export async function requestWebGPUDevice(): Promise<GPUDevice> {
  if (!navigator.gpu) {
    throw new Error(
      'WebGPU is not supported in your browser. Please use a browser that supports WebGPU (Chrome 113+, Edge 113+, or Firefox Nightly with WebGPU enabled).',
    );
  }
  const adapter = await navigator.gpu.requestAdapter({ powerPreference: 'high-performance' });
  if (!adapter) {
    throw new Error('No WebGPU adapter available. Your device may not support WebGPU.');
  }
  return adapter.requestDevice();
}
