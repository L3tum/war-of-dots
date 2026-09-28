// Vitest setup — install the fake WebGPU globals before any test file loads
// (GPUBufferUsage/GPUTextureUsage/GPUShaderStage enums, navigator.gpu,
// OffscreenCanvas). See fake_gpu.ts.

import { setupFakeGpu } from './fake_gpu';

setupFakeGpu();
