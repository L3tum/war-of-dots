import { defineConfig } from 'vitest/config';

export default defineConfig({
  test: {
    globals: true,
    environment: 'happy-dom',
    environmentOptions: { happyDOM: { url: 'http://localhost/' } },
    include: ['src/**/*.test.ts'],
    setupFiles: ['src/test/webgpu-setup.ts'],
  },
});
