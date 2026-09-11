import { defineConfig } from '@playwright/test';
import * as fs from 'node:fs';
import * as os from 'node:os';
import * as path from 'node:path';

/**
 * No-server WebGPU E2E config (Layer 2).
 *
 * Runs the *real* GPU stack on CPU: full Chromium (not the headless shell,
 * which has no `navigator.gpu`) under `xvfb-run`, rasterized by Dawn/SwiftShader.
 * The Makefile target `frontend-e2e` builds the bundle, stages the replay
 * fixture at `dist/replays/`, serves `dist/` statically, and sets
 * `WEBGPU_E2E_URL` to point the suite at it.
 */

/**
 * Resolve the cached full Playwright Chromium build (skipping the headless
 * shell). Pinning `executablePath` here keeps the suite download-free and
 * independent of which `@playwright/test` minor version is installed — the
 * bundled binary already carries SwiftShader (`libvk_swiftshader.so`).
 */
function cachedChromium(): string | undefined {
  const root = path.join(os.homedir(), '.cache', 'ms-playwright');
  if (!fs.existsSync(root)) return undefined;
  const candidates = fs
    .readdirSync(root)
    .filter((n) => /^chromium-\d+$/.test(n) && !n.includes('headless'))
    .sort((a, b) => Number(b.split('-')[1]) - Number(a.split('-')[1]));
  for (const c of candidates) {
    const exe = path.join(root, c, 'chrome-linux64', 'chrome');
    if (fs.existsSync(exe)) return exe;
  }
  return undefined;
}

export default defineConfig({
  testDir: './e2e',
  // CPU rasterization of the replayed frames is slow; be generous.
  timeout: 90_000,
  expect: { timeout: 10_000 },
  retries: 0,
  workers: 1,
  fullyParallel: false,
  reporter: [['list']],
  use: {
    // WebGPU needs the full Chromium build; the page is driven under xvfb-run.
    headless: false,
    baseURL: process.env.WEBGPU_E2E_URL ?? 'http://127.0.0.1:8123',
    launchOptions: {
      executablePath: cachedChromium(),
      args: [
        // WebGPU's software (SwiftShader) adapter is HIDDEN by default in
        // headless / GPU-blocklisted environments; re-enable it. On Linux the
        // WebGPU backend is Vulkan, so we force the Vulkan path onto the
        // bundled SwiftShader software Vulkan ICD (libvk_swiftshader.so). Note:
        // this is the `--use-angle=vulkan` + `--use-vulkan=swiftshader` combo,
        // NOT `--use-angle=swiftshader` (which routes ANGLE to the GL path and
        // leaves WebGPU with no adapter).
        '--enable-unsafe-webgpu',
        '--enable-features=Vulkan',
        '--use-angle=vulkan',
        '--use-vulkan=swiftshader',
        '--use-webgpu-adapter=swiftshader',
        '--disable-vulkan-surface',
        // Container/root-friendly.
        '--no-sandbox',
        '--disable-gpu-sandbox',
        '--disable-dev-shm-usage',
        '--hide-scrollbars',
      ],
    },
  },
});
