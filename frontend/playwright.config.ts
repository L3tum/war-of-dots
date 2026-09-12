import { defineConfig } from '@playwright/test';
import * as fs from 'node:fs';
import * as os from 'node:os';
import * as path from 'node:path';

/**
 * E2E config — two layers, one runner.
 *
 * Both projects run the *real* GPU stack on CPU: full Chromium (not the
 * headless shell, which has no `navigator.gpu`) under `xvfb-run`, rasterized
 * by Dawn/SwiftShader. The Makefile target `frontend-e2e` (via
 * e2e/run_e2e.sh) builds the bundle + server binary, then runs this suite.
 *
 * Each layer's server lifecycle lives in a harness fixture (e2e/harness/),
 * which also overrides the `baseURL` fixture — so specs use relative
 * `page.goto('/')` and never touch ports or URLs:
 *
 *   - `no-server` (webgpu.spec.ts): static file server over dist/ — the fast
 *     render-pipeline smoke layer (replay mode, no Rust, no WebSocket).
 *   - `live` (live/*.spec.ts): the real war-of-dots binary — full E2E with
 *     the baked-in frontend, real /ws traffic, and the WP3 handshake.
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
  console.warn(
    '[playwright.config] No cached Playwright Chromium found under ~/.cache/ms-playwright — ' +
      'falling back to the version-matched browser from node_modules (it may trigger a ' +
      'download, and the run_e2e.sh skip-gate will exit 77 if it is absent).',
  );
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
  projects: [
    {
      name: 'no-server',
      testMatch: 'webgpu.spec.ts',
    },
    {
      name: 'live',
      testMatch: 'live/**/*.spec.ts',
    },
  ],
  use: {
    // WebGPU needs the full Chromium build; the page is driven under xvfb-run.
    headless: false,
    // NOTE: no top-level baseURL — each harness fixture overrides it (a
    // static server for `no-server`, the live binary for `live`).
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
