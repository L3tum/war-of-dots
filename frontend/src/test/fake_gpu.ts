// Recording fake WebGPU device for headless (happy-dom) renderer tests.
//
// There is no ready-made `GPUDevice` mock package (checked: vitest-webgl-canvas-mock
// covers WebGL only; `webgpu`/`bun-webgpu` are real Dawn-backed GPUs), so this
// doubles only the API surface the renderer actually calls and RECORDS the
// `queue.writeBuffer` / `queue.writeTexture` payloads and the render-pass draw
// sequence — the authoritative geometry output that assertions read.

// Test-only fake WebGPU surface: the fakes deliberately mirror the loose,
// browser-global shape of the WebGPU IDL (e.g. `globalThis as any` in
// setupFakeGpu) instead of re-declaring every member — the real, fully-typed
// definitions live in src/types/webgpu.d.ts.
/* eslint-disable @typescript-eslint/no-explicit-any */

export interface WriteBufferCall {
  buffer: FakeGPUBuffer;
  offset: number;
  data: Float32Array | Uint8Array;
}

export interface WriteTextureCall {
  texture: FakeGPUTexture;
  data: Uint8Array;
  layout: { offset: number; bytesPerRow: number };
  size: [number, number, number];
}

export interface CopyExternalImageCall {
  source: unknown;
  texture: FakeGPUTexture;
  size: [number, number, number];
}

export interface DrawCall {
  pipeline: FakeGPURenderPipeline;
  vertexCount: number;
}

export class FakeGPUBuffer {
  size: number;
  usage: number;
  constructor(size: number, usage: number) {
    this.size = size;
    this.usage = usage;
  }
}

export class FakeGPUTexture {
  width: number;
  height: number;
  format: string;
  constructor(width: number, height: number, format: string) {
    this.width = width;
    this.height = height;
    this.format = format;
  }
  createView(): FakeGPUTextureView {
    return new FakeGPUTextureView(this);
  }
}

export class FakeGPUTextureView {
  constructor(readonly texture: FakeGPUTexture) {}
}

export class FakeGPUSampler {}

export class FakeGPUShaderModule {
  constructor(readonly code: string) {}
}

export class FakeGPUPipelineLayout {
  constructor(readonly bindGroupLayouts: FakeGPUBindGroupLayout[]) {}
}

export class FakeGPUBindGroupLayout {}

export class FakeGPUBindGroup {}

export class FakeGPURenderPipeline {
  static nextId = 0;
  readonly id: number;
  constructor(
    readonly label: string,
    readonly description: any,
  ) {
    this.id = FakeGPURenderPipeline.nextId++;
    this.label = label;
  }
}

export class FakeGPURenderPassEncoder {
  draws: DrawCall[] = [];
  pipelines: FakeGPURenderPipeline[] = [];
  // Records the bind-group IDENTITY (not just the slot) so tests can assert
  // the exact group a section bound — `some(g => g.index === 1)` alone is
  // satisfied by whichever section binds slot 1 last (the fog section renders
  // after symbols, masking a missing symbol bind).
  bindGroups: Array<{ index: number; group: unknown }> = [];
  vertexBuffers: Array<{ index: number; buffer: FakeGPUBuffer }> = [];

  constructor(readonly clearValue: { r: number; g: number; b: number; a: number }) {}

  setPipeline(pipeline: FakeGPURenderPipeline): void {
    this.pipelines.push(pipeline);
  }
  setBindGroup(index: number, bindGroup: unknown): void {
    this.bindGroups.push({ index, group: bindGroup });
  }
  setVertexBuffer(index: number, buffer: FakeGPUBuffer): void {
    this.vertexBuffers.push({ index, buffer });
  }
  draw(vertexCount: number): void {
    const last = this.pipelines[this.pipelines.length - 1];
    this.draws.push({ pipeline: last, vertexCount });
  }
  end(): void {}
}

export class FakeGPUCommandEncoder {
  renderPasses: FakeGPURenderPassEncoder[] = [];
  finish(): {
    kind: 'fake-command-buffer';
    renderPasses: FakeGPURenderPassEncoder[];
  } {
    return { kind: 'fake-command-buffer', renderPasses: this.renderPasses };
  }
  beginRenderPass(info: {
    colorAttachments: Array<{
      clearValue: { r: number; g: number; b: number; a: number };
    }>;
  }): FakeGPURenderPassEncoder {
    const pass = new FakeGPURenderPassEncoder(
      info.colorAttachments[0].clearValue as {
        r: number;
        g: number;
        b: number;
        a: number;
      },
    );
    this.renderPasses.push(pass);
    return pass;
  }
}

/** Records every queue submission; `writeBuffer` payloads are kept for assertions. */
export class FakeGPUQueue {
  writeBufferCalls: WriteBufferCall[] = [];
  writeTextureCalls: WriteTextureCall[] = [];
  copyExternalImageCalls: CopyExternalImageCall[] = [];
  submitted: unknown[] = [];

  writeBuffer(buffer: FakeGPUBuffer, offset: number, data: Float32Array | Uint8Array): void {
    this.writeBufferCalls.push({ buffer, offset, data });
  }
  writeTexture(
    info: { texture: FakeGPUTexture },
    data: Uint8Array,
    layout: { offset: number; bytesPerRow: number },
    size: [number, number, number],
  ): void {
    this.writeTextureCalls.push({ texture: info.texture, data, layout, size });
  }
  copyExternalImageToTexture(
    source: { source: unknown },
    dest: { texture: FakeGPUTexture },
    size: [number, number, number],
  ): void {
    this.copyExternalImageCalls.push({
      source: source.source,
      texture: dest.texture,
      size,
    });
  }
  submit(commandBuffers: unknown[]): void {
    this.submitted.push(...commandBuffers);
  }
}

/** A `GPUCanvasContext` whose configure() is a no-op and whose current texture is fixed. */
export class FakeGPUCanvasContext {
  configured: { device: unknown; format: string } | null = null;
  private current: FakeGPUTexture;

  constructor(width: number, height: number, format: string) {
    this.current = new FakeGPUTexture(width, height, format);
  }

  configure(info: { device: unknown; format: string }): void {
    this.configured = { device: info.device, format: info.format };
  }
  getCurrentTexture(): FakeGPUTexture {
    return this.current;
  }
}

/** The fake device — the only thing `new Renderer(canvas, device)` actually needs. */
export class FakeGPUDevice {
  queue = new FakeGPUQueue();
  createdPipelines: FakeGPURenderPipeline[] = [];
  createdTextures: FakeGPUTexture[] = [];

  createShaderModule(info: { code: string }): FakeGPUShaderModule {
    return new FakeGPUShaderModule(info.code);
  }
  createPipelineLayout(info: {
    bindGroupLayouts: FakeGPUBindGroupLayout[];
  }): FakeGPUPipelineLayout {
    return new FakeGPUPipelineLayout(info.bindGroupLayouts);
  }
  createRenderPipeline(description: any): FakeGPURenderPipeline {
    const pipeline = new FakeGPURenderPipeline(description?.label ?? '', description);
    this.createdPipelines.push(pipeline);
    return pipeline;
  }
  createBindGroupLayout(_info: unknown): FakeGPUBindGroupLayout {
    return new FakeGPUBindGroupLayout();
  }
  createBindGroup(_info: unknown): FakeGPUBindGroup {
    return new FakeGPUBindGroup();
  }
  createSampler(_info: unknown): FakeGPUSampler {
    return new FakeGPUSampler();
  }
  createBuffer(info: { size: number; usage: number }): FakeGPUBuffer {
    return new FakeGPUBuffer(info.size, info.usage);
  }
  createTexture(info: { size: [number, number]; format: string; usage: number }): FakeGPUTexture {
    const texture = new FakeGPUTexture(info.size[0], info.size[1], info.format);
    this.createdTextures.push(texture);
    return texture;
  }
  createCommandEncoder(): FakeGPUCommandEncoder {
    return new FakeGPUCommandEncoder();
  }
}

/** No-op 2D context — records only the text actually painted (assertable). */
export class Fake2DContext {
  fillTextCalls: Array<{ text: string; x: number; y: number }> = [];
  clearRectCalls: number[] = [];
  fillStyle = '';
  strokeStyle = '';
  lineWidth = 0;
  font = '';
  textAlign = '';
  textBaseline = '';

  clearRect(x: number, y: number, w: number, h: number): void {
    this.clearRectCalls.push(x, y, w, h);
  }
  save(): void {}
  restore(): void {}
  beginPath(): void {}
  arc(..._args: number[]): void {}
  fill(): void {}
  stroke(): void {}
  fillText(text: string, x: number, y: number): void {
    this.fillTextCalls.push({ text, x, y });
  }
}

/** OffscreenCanvas stand-in: 2d context + transferToImageBitmap. */
export class FakeOffscreenCanvas {
  width: number;
  height: number;
  private ctx: Fake2DContext;
  /** 2D canvas-API methods the renderer called that the fake doesn't implement. */
  unknown2dCalls: string[] = [];
  transferredBitmaps: unknown[] = [];

  constructor(width: number, height: number) {
    this.width = width;
    this.height = height;
    this.ctx = new Fake2DContext();
  }
  getContext(_kind: string): Fake2DContext {
    // Hand back a trapping proxy so an un-faked canvas-API call in the
    // renderer fails loudly (recorded in unknown2dCalls) instead of no-oping.
    return trapUnknownCalls(this.ctx, this.unknown2dCalls, '2d context');
  }
  transferToImageBitmap(): {
    kind: 'fake-image-bitmap';
    width: number;
    height: number;
  } {
    const bitmap = {
      kind: 'fake-image-bitmap' as const,
      width: this.width,
      height: this.height,
    };
    this.transferredBitmaps.push(bitmap);
    return bitmap;
  }
}

/**
 * WebGPU enum constants the renderer references as globals (browser-provided;
 * happy-dom has none). Values follow the WebGPU IDL.
 */
function ensureGpuConstants(): void {
  const g = globalThis as any;
  if (!g.GPUBufferUsage) {
    g.GPUBufferUsage = {
      MAP_READ: 1,
      MAP_WRITE: 2,
      COPY_SRC: 4,
      COPY_DST: 8,
      INDEX: 16,
      VERTEX: 32,
      UNIFORM: 64,
      STORAGE: 128,
      INDIRECT: 256,
    };
  }
  if (!g.GPUTextureUsage) {
    g.GPUTextureUsage = {
      COPY_SRC: 1,
      COPY_DST: 2,
      TEXTURE_BINDING: 4,
      STORAGE_BINDING: 8,
      RENDER_ATTACHMENT: 16,
    };
  }
  if (!g.GPUShaderStage) {
    g.GPUShaderStage = { VERTEX: 1, FRAGMENT: 2, COMPUTE: 4 };
  }
}

/**
 * Stub `navigator.gpu` (with `getPreferredCanvasFormat`) + `OffscreenCanvas` for
 * the duration of the test process. Idempotent.
 */
export function setupFakeGpu(): void {
  const g = globalThis as any;
  ensureGpuConstants();

  // happy-dom defines OffscreenCanvas but its getContext('2d') returns null —
  // always replace it in the test environment.
  g.OffscreenCanvas = FakeOffscreenCanvas;

  const gNav = g.navigator;
  if (gNav) {
    if (typeof gNav.getPreferredCanvasFormat === 'undefined') {
      try {
        Object.defineProperty(gNav, 'getPreferredCanvasFormat', {
          value: () => 'bgra8unorm',
          writable: true,
          configurable: true,
        });
      } catch {
        // Some environments make navigator read-only — the tests can override
        // per-test with vi.stubGlobal if this throws.
      }
    }
    // Tests construct the Renderer with an explicit fake device, so the
    // navigator stub only needs the sync format query (no requestAdapter).
    if (typeof gNav.gpu === 'undefined') {
      try {
        Object.defineProperty(gNav, 'gpu', {
          value: { getPreferredCanvasFormat: () => 'bgra8unorm' },
          writable: true,
          configurable: true,
        });
      } catch {
        // Same fallback as above.
      }
    }
  }
}

export interface FakeGpuHarness {
  device: FakeGPUDevice;
  context: FakeGPUCanvasContext;
  canvas: HTMLCanvasElement;
}

/**
 * Wrap a fake in a Proxy that records and REJECTS calls to WebGPU methods the
 * fake doesn't implement. Without this, a future Renderer change that starts
 * calling an un-faked API would either no-op (silently green) or throw an
 * opaque TypeError; with it the test fails loudly and the missing method is
 * named. Existing methods/properties pass through untouched.
 */
export function trapUnknownCalls<T extends object>(
  target: T,
  unknownCalls: string[],
  kind: string,
): T {
  return new Proxy(target, {
    get(obj, prop) {
      const val = (obj as unknown as Record<PropertyKey, unknown>)[prop];
      if (
        typeof val !== 'function' &&
        typeof prop === 'string' &&
        !(prop in obj) &&
        prop !== 'constructor' &&
        prop !== 'toJSON'
      ) {
        unknownCalls.push(`${kind}.${prop}()`);
        throw new Error(
          `Fake ${kind}: method '${prop}' is not implemented — the ` +
            `Renderer now calls it. Add a recording fake to fake_gpu.ts rather ` +
            `than leaving this trap to fail the suite.`,
        );
      }
      return val;
    },
  });
}

/**
 * Create a canvas + fake device + fake webgpu context wired to `canvas` for
 * `new Renderer(canvas, device)`. The canvas gets an explicit pixel size (the
 * Renderer constructor's setCanvasDimensions() would otherwise zero it under
 * happy-dom, whose getBoundingClientRect() is 0×0).
 *
 * `device` is proxied: if the Renderer calls a WebGPU method the fake doesn't
 * implement, the call is recorded in `unknownGpuCalls` and throws (see
 * trapUnknownCalls) instead of silently no-oping.
 */
export function createFakeGpu(
  width = 1280,
  height = 720,
): FakeGpuHarness & { unknownGpuCalls: string[] } {
  setupFakeGpu();

  const canvas = document.createElement('canvas');
  canvas.width = width;
  canvas.height = height;

  const rawContext = new FakeGPUCanvasContext(width, height, 'bgra8unorm');
  (canvas as unknown as { getContext: (kind: string) => unknown }).getContext = (kind: string) => {
    if (kind === 'webgpu') return rawContext;
    return null;
  };

  const rawDevice = new FakeGPUDevice();
  const unknownGpuCalls: string[] = [];
  const device = trapUnknownCalls(rawDevice, unknownGpuCalls, 'device');
  // Trap the canvas context too — the one remaining surface where an
  // un-faked WebGPU call (e.g. a future configure/getCurrentTexture variant)
  // would have silently no-oped. Shares the harness's unknownGpuCalls list.
  const context = trapUnknownCalls(rawContext, unknownGpuCalls, 'canvas context');
  return { device, context, canvas, unknownGpuCalls };
}
