// WebGPU rendering with fog of war, borders, unit symbols, and path preview

import { GameState } from './game_state';
import { marchingSquares, type TerrainType } from './terrain';
import { TerritoryFiller, TERRITORY_NONE } from './territory';
import { detectBorderEdgesFlat, type EdgeBuffers } from './geometry';
import {
  CELL_SIZE,
  UNIT_DOT_SIZE,
  SYMBOL_ATLAS_SIZE,
  SYMBOL_CELL_SIZE,
  SYMBOLS_PER_ROW,
  BORDER_LINE_WIDTH,
  PATH_LINE_WIDTH,
  BAR_HEIGHT,
  BAR_SPACING,
  BAR_WIDTH_MULTIPLIER,
  PLAYER_COLORS,
  BAR_COLORS,
  OUTLINE_ALPHA,
  OUTLINE_EXPAND,
  OUTLINE_COLOR,
  WATER_INDICATOR_COLOR,
  WATER_BORDER_COLOR,
  EMBARK_COLOR,
  BUFFER_SIZES,
  CITY_DOT_SIZE,
  CITY_COLORS,
  ZOOM_MIN,
  ZOOM_MAX,
} from './config';

export class Renderer {
  private canvas: HTMLCanvasElement;
  private device: GPUDevice;
  private context: GPUCanvasContext;
  private textureFormat: GPUTextureFormat;
  private updateCallback: (() => void) | null = null;

  // Main pipeline: solid color quads (terrain, unit dots)
  private mainPipeline: GPURenderPipeline;
  // Pipeline for unit symbols with texture
  private symbolPipeline: GPURenderPipeline;
  // Pipeline for borders and paths (thin quads)
  private linePipeline: GPURenderPipeline;
  // Pipeline for own-side outlines (semi-transparent halo, blend-enabled)
  private outlinePipeline: GPURenderPipeline;
  // Pipeline for fog overlay (NDC fullscreen quad with texture)
  private fogPipeline: GPURenderPipeline;
  // Fog bind group layout
  private fogBindGroupLayout!: GPUBindGroupLayout;

  camera: { x: number; y: number; zoom: number } = { x: 0, y: 0, zoom: 1.0 };

  // Offscreen canvas for unit symbols
  private symbolCanvas: OffscreenCanvas;
  private symbolCtx: OffscreenCanvasRenderingContext2D;
  private symbolTexture: GPUTexture;
  private symbolSampler: GPUSampler;

  // Fog texture
  private fogTexture: GPUTexture | null = null;
  // Fog cache — skip GPU write when visionGrid hasn't changed
  private fogCacheWidth = 0;
  private fogCacheHeight = 0;
  // Exposed dirty flags for external marking
  public fogDirty: boolean = true;

  // Terrain CPU-mesh cache — skip marchingSquares + buffer upload when the visible
  // cell range and terrain haven't changed (stationary camera). Terrain is static
  // during a match, so this is correct as long as the terrain reference and the
  // computed visible range are unchanged.
  private terrainMeshKey = '';
  private cachedTerrainRef: unknown = null;
  private cachedTerrainPositions: number[] | null = null;
  private cachedTerrainColors: number[] | null = null;
  // Uploaded-data cache — skip GPU writeBuffer calls while the mesh is cached
  private cachedTerrainPosData: Float32Array<ArrayBuffer> | null = null;
  private cachedTerrainColData: Float32Array<ArrayBuffer> | null = null;

  // Last render/validation error — surfaced via the debug overlay when active
  lastRenderError: string | null = null;
  // Number of frames whose full render pass reached queue.submit(). Test hook
  // for the E2E settle: polling this is stronger than a fixed timeout, which
  // could elapse with zero frames submitted. (Submission, not GPU completion —
  // no completion signal is wired; the name says so on purpose.)
  framesSubmitted = 0;

  // Fog overlay buffers (static fullscreen quad, created once)
  private fogPositionsBuffer: GPUBuffer;
  private fogColorsBuffer: GPUBuffer;

  // Fog-specific uniform buffer (camera + cellSize for UV mapping)
  private fogUniformBuffer!: GPUBuffer;
  private fogUniformBindGroup!: GPUBindGroup;

  // Border cache — store logical edges (camera-independent), recompute positions each frame
  // Exposed dirty flags for external marking
  public borderDirty: boolean = true;
  // Perf #3: flat, allocation-free territory/border pipeline. All scratch
  // buffers are grow-only and reused across passes — a dirty pass flattens the
  // grid into `borderFlatScratch`, compares it (plus city anchors + terrain)
  // against the committed snapshot, and only then runs the BFS.
  private territoryFiller = new TerritoryFiller();
  /** Last border grid that actually went through the BFS (change-detection snapshot). */
  private lastBorderFlat: Uint8Array = new Uint8Array(0);
  /** Current border grid, flattened row-major (rewritten each dirty pass). */
  private borderFlatScratch: Uint8Array = new Uint8Array(0);
  /** Water mask derived from terrain (terrain is static during a match). */
  private waterMask: Uint8Array = new Uint8Array(0);
  private waterMaskTerrainRef: unknown = null;
  /** City anchor scratch (packed parallel arrays) + change-detection key. */
  private anchorX = new Int32Array(0);
  private anchorY = new Int32Array(0);
  private anchorOwner = new Uint8Array(0);
  private lastCityKey: string | null = null;
  private lastTerrainRef: unknown = null;
  /** Logical edges (camera-independent), struct-of-arrays — see EdgeBuffers. */
  private edgeX = new Int32Array(0);
  private edgeY = new Int32Array(0);
  private edgeDir = new Uint8Array(0);
  private edgeOwnerA = new Uint8Array(0);
  private edgeOwnerB = new Uint8Array(0);
  private edgeCount = 0;
  private edgeBufCache: EdgeBuffers | null = null;
  /** Border positions/colors upload data — reused grow-only buffers. */
  private borderPosData: Float32Array<ArrayBuffer> = new Float32Array(0);
  private borderColData: Float32Array<ArrayBuffer> = new Float32Array(0);
  // Perf #3: fog change detection — last vision grid flattened/uploaded.
  private lastVisionFlat: Uint8Array = new Uint8Array(0);
  /** Current vision grid, flattened row-major (rewritten each pass). */
  private visionFlatScratch: Uint8Array = new Uint8Array(0);
  /** Fog RGBA upload data — reused grow-only buffer. */
  private fogData: Uint8Array<ArrayBuffer> = new Uint8Array(0);
  /** Test hook: full territory BFS passes actually executed (Perf #3 assertions). */
  public territoryRecomputeCount = 0;
  /** Sequence number of the last logical-edge regeneration (bumped when the
   *  BFS + edge scan actually run). Position data depends only on the edges
   *  plus zoom (the shader applies the camera), so uploads are gated on it. */
  private borderEdgeSeq = 0;
  /** (edgeSeq, zoom) for which the current border buffer content was generated. */
  private lastPosEdgeSeq = -1;
  private lastPosZoom: number = NaN;

  // Symbol cache — track when unit data changes
  private symbolCacheKey: string | null = null;

  // Frame dirty gate
  private dataVersion = 0;
  private lastFrameKey: string | null = null;

  // Hoisted bind groups (static, created once)
  private symbolBindGroup: GPUBindGroup | null = null;
  private fogBindGroup: GPUBindGroup | null = null;

  // Path data for preview
  currentPath: { x: number; y: number }[] = [];
  // Selected unit for movement
  selectedUnitId: string | null = null;

  // Vertex counts
  terrainVertexCount: number = 0;
  unitVertexCount: number = 0;
  borderVertexCount: number = 0;
  pathVertexCount: number = 0;

  // Buffers
  terrainPositionsBuffer: GPUBuffer;
  terrainColorBuffer: GPUBuffer;
  unitPositionsBuffer: GPUBuffer;
  unitColorBuffer: GPUBuffer;
  symbolPositionBuffer: GPUBuffer;
  symbolColorBuffer: GPUBuffer;
  borderPositionsBuffer: GPUBuffer;
  borderColorBuffer: GPUBuffer;
  pathPositionsBuffer: GPUBuffer;
  pathColorBuffer: GPUBuffer;
  // Unit bars (health, org, equipment) — thin quads
  barPositionsBuffer: GPUBuffer;
  barColorBuffer: GPUBuffer;
  barVertexCount: number = 0;

  // City dot buffers
  cityPositionsBuffer: GPUBuffer;
  cityColorBuffer: GPUBuffer;
  cityVertexCount: number = 0;

  // Own-side outline buffers (drawn behind own units/cities)
  outlinePositionsBuffer: GPUBuffer;
  outlineColorBuffer: GPUBuffer;
  outlineVertexCount: number = 0;

  // Uniform buffer for camera state (shared across all pipelines)
  uniformBuffer: GPUBuffer;
  uniformBindGroup: GPUBindGroup;
  // Shared symbol/fog bind group layout for group(1) (symbolPipeline only)
  symbolBindGroupLayout: GPUBindGroupLayout;

  constructor(canvas: HTMLCanvasElement, device: GPUDevice) {
    this.canvas = canvas;
    this.device = device;

    this.context = canvas.getContext('webgpu') as GPUCanvasContext;
    this.textureFormat = navigator.gpu!.getPreferredCanvasFormat();

    // Set canvas pixel dimensions to fill the window (buffer-independent)
    this.setCanvasDimensions();

    this.context.configure({
      device,
      format: this.textureFormat,
    });

    // Create offscreen canvas for unit symbols
    this.symbolCanvas = new OffscreenCanvas(SYMBOL_ATLAS_SIZE, SYMBOL_ATLAS_SIZE);
    this.symbolCtx = this.symbolCanvas.getContext('2d')!;
    this.symbolTexture = this.device.createTexture({
      size: [SYMBOL_ATLAS_SIZE, SYMBOL_ATLAS_SIZE],
      format: 'rgba8unorm',
      usage: GPUTextureUsage.TEXTURE_BINDING | GPUTextureUsage.COPY_DST,
    });
    this.symbolSampler = this.device.createSampler({
      magFilter: 'nearest',
      minFilter: 'nearest',
    });

    // Shared uniform bind group layout for group(0) across all pipelines
    // Uniform struct: cameraX, cameraY, zoom, canvasWidth, canvasHeight (20 bytes padded to 24)
    const uniformBindGroupLayout = device.createBindGroupLayout({
      entries: [
        {
          binding: 0,
          visibility: GPUShaderStage.VERTEX,
          buffer: { type: 'uniform' },
        },
      ],
    });

    // Shared symbol/fog bind group layout for group(1) (symbol pipeline only)
    this.symbolBindGroupLayout = device.createBindGroupLayout({
      entries: [
        { binding: 0, visibility: GPUShaderStage.FRAGMENT, texture: {} },
        { binding: 1, visibility: GPUShaderStage.FRAGMENT, sampler: {} },
      ],
    });
    // Symbol bind group depends only on the static atlas texture/sampler — create once.
    this.symbolBindGroup = this.createSymbolBindGroup();

    // Uniform buffer: cameraX, cameraY, zoom, canvasWidth, canvasHeight (5 floats = 20 bytes, align to 24)
    this.uniformBuffer = device.createBuffer({
      size: 24,
      usage: GPUBufferUsage.UNIFORM | GPUBufferUsage.COPY_DST,
    });
    // Initial uniform data
    device.queue.writeBuffer(
      this.uniformBuffer,
      0,
      new Float32Array([0, 0, 1.0, this.canvas.width, this.canvas.height]),
    );
    this.uniformBindGroup = device.createBindGroup({
      layout: uniformBindGroupLayout,
      entries: [{ binding: 0, resource: { buffer: this.uniformBuffer } }],
    });

    // Pipelines — all share the same group(0) uniform layout
    this.mainPipeline = this.createMainPipeline(uniformBindGroupLayout);
    this.symbolPipeline = this.createSymbolPipeline(uniformBindGroupLayout);
    this.linePipeline = this.createLinePipeline(uniformBindGroupLayout);
    this.outlinePipeline = this.createOutlinePipeline(uniformBindGroupLayout);
    this.fogPipeline = this.createFogPipeline();

    // Buffers
    this.outlinePositionsBuffer = device.createBuffer({
      size: BUFFER_SIZES.unit,
      usage: GPUBufferUsage.VERTEX | GPUBufferUsage.COPY_DST,
    });
    this.outlineColorBuffer = device.createBuffer({
      size: BUFFER_SIZES.unit,
      usage: GPUBufferUsage.VERTEX | GPUBufferUsage.COPY_DST,
    });

    this.terrainPositionsBuffer = device.createBuffer({
      size: BUFFER_SIZES.terrain,
      usage: GPUBufferUsage.VERTEX | GPUBufferUsage.COPY_DST,
    });
    this.terrainColorBuffer = device.createBuffer({
      size: BUFFER_SIZES.terrain,
      usage: GPUBufferUsage.VERTEX | GPUBufferUsage.COPY_DST,
    });

    this.unitPositionsBuffer = device.createBuffer({
      size: BUFFER_SIZES.unit,
      usage: GPUBufferUsage.VERTEX | GPUBufferUsage.COPY_DST,
    });
    this.unitColorBuffer = device.createBuffer({
      size: BUFFER_SIZES.unit,
      usage: GPUBufferUsage.VERTEX | GPUBufferUsage.COPY_DST,
    });

    // Symbol buffer
    this.symbolPositionBuffer = device.createBuffer({
      size: BUFFER_SIZES.symbol,
      usage: GPUBufferUsage.VERTEX | GPUBufferUsage.COPY_DST,
    });
    this.symbolColorBuffer = device.createBuffer({
      size: BUFFER_SIZES.symbol,
      usage: GPUBufferUsage.VERTEX | GPUBufferUsage.COPY_DST,
    });

    // Border and path buffers
    this.borderPositionsBuffer = device.createBuffer({
      size: BUFFER_SIZES.border,
      usage: GPUBufferUsage.VERTEX | GPUBufferUsage.COPY_DST,
    });
    this.borderColorBuffer = device.createBuffer({
      size: BUFFER_SIZES.border,
      usage: GPUBufferUsage.VERTEX | GPUBufferUsage.COPY_DST,
    });
    this.pathPositionsBuffer = device.createBuffer({
      size: BUFFER_SIZES.border,
      usage: GPUBufferUsage.VERTEX | GPUBufferUsage.COPY_DST,
    });
    this.pathColorBuffer = device.createBuffer({
      size: BUFFER_SIZES.border,
      usage: GPUBufferUsage.VERTEX | GPUBufferUsage.COPY_DST,
    });

    // Bar buffers (one bar per unit per stat = 3 bars per unit)
    this.barPositionsBuffer = device.createBuffer({
      size: BUFFER_SIZES.bar,
      usage: GPUBufferUsage.VERTEX | GPUBufferUsage.COPY_DST,
    });
    this.barColorBuffer = device.createBuffer({
      size: BUFFER_SIZES.bar,
      usage: GPUBufferUsage.VERTEX | GPUBufferUsage.COPY_DST,
    });

    // Fog overlay buffers — fullscreen quad with NDC positions
    this.fogPositionsBuffer = device.createBuffer({
      size: 32, // 4 vertices * 8 bytes (pos only; frag shader recomputes UVs)
      usage: GPUBufferUsage.VERTEX | GPUBufferUsage.COPY_DST,
    });
    this.fogColorsBuffer = device.createBuffer({
      size: 64, // minimal allocation, unused by fog pipeline
      usage: GPUBufferUsage.VERTEX | GPUBufferUsage.COPY_DST,
    });
    // Write fog quad data with NDC positions
    this.updateFogQuad();

    // City dot buffers
    this.cityPositionsBuffer = device.createBuffer({
      size: BUFFER_SIZES.city,
      usage: GPUBufferUsage.VERTEX | GPUBufferUsage.COPY_DST,
    });
    this.cityColorBuffer = device.createBuffer({
      size: BUFFER_SIZES.city,
      usage: GPUBufferUsage.VERTEX | GPUBufferUsage.COPY_DST,
    });
  }

  /** Generate fullscreen NDC fog quad. UVs are recomputed in the fragment shader from world position */
  updateFogQuad(): void {
    // Fullscreen quad in NDC space: 4 vertices, pos(x,y) only = 2 floats per vertex, stride 8
    const fogPositions = new Float32Array([-1, -1, 1, -1, -1, 1, 1, 1]);
    this.device.queue.writeBuffer(this.fogPositionsBuffer, 0, fogPositions);
  }

  private createMainPipeline(uniformLayout: GPUBindGroupLayout): GPURenderPipeline {
    const shaderModule = this.device.createShaderModule({
      code: `
        struct Uniforms {
          cameraX: f32,
          cameraY: f32,
          zoom: f32,
          canvasWidth: f32,
          canvasHeight: f32,
        };

        @group(0) @binding(0) var<uniform> uniforms: Uniforms;

        struct VertexOutput {
          @builtin(position) position: vec4<f32>,
          @location(0) color: vec3<f32>,
        };

        @vertex
        fn vert(@location(0) pos: vec2<f32>, @location(1) color: vec3<f32>) -> VertexOutput {
          var output: VertexOutput;
          let screenX = (pos.x - uniforms.cameraX) * uniforms.zoom / uniforms.canvasWidth * 2.0;
          let screenY = (uniforms.cameraY - pos.y) * uniforms.zoom / uniforms.canvasHeight * 2.0;
          output.position = vec4<f32>(screenX, screenY, 0.0, 1.0);
          output.color = color;
          return output;
        }

        @fragment
        fn frag(input: VertexOutput) -> @location(0) vec4<f32> {
          return vec4<f32>(input.color, 1.0);
        }
      `,
    });

    const pipelineLayout = this.device.createPipelineLayout({
      bindGroupLayouts: [uniformLayout],
    });

    return this.device.createRenderPipeline({
      layout: pipelineLayout,
      vertex: {
        module: shaderModule,
        entryPoint: 'vert',
        buffers: [
          {
            arrayStride: 8,
            attributes: [{ format: 'float32x2', offset: 0, shaderLocation: 0 }],
          },
          {
            arrayStride: 12,
            attributes: [{ format: 'float32x3', offset: 0, shaderLocation: 1 }],
          },
        ],
      },
      primitive: { topology: 'triangle-strip' },
      fragment: {
        module: shaderModule,
        entryPoint: 'frag',
        targets: [{ format: this.textureFormat }],
      },
    });
  }

  private createSymbolPipeline(uniformLayout: GPUBindGroupLayout): GPURenderPipeline {
    const shaderModule = this.device.createShaderModule({
      code: `
        struct Uniforms {
          cameraX: f32,
          cameraY: f32,
          zoom: f32,
          canvasWidth: f32,
          canvasHeight: f32,
        };

        @group(0) @binding(0) var<uniform> uniforms: Uniforms;
        @group(1) @binding(0) var symbolTex: texture_2d<f32>;
        @group(1) @binding(1) var texSampler: sampler;

        struct VertexOutput {
          @builtin(position) position: vec4<f32>,
          @location(0) uv: vec2<f32>,
          @location(1) color: vec3<f32>,
        };

        @vertex
        fn vert(@location(0) pos: vec2<f32>, @location(1) uv: vec2<f32>, @location(2) color: vec3<f32>) -> VertexOutput {
          var output: VertexOutput;
          let screenX = (pos.x - uniforms.cameraX) * uniforms.zoom / uniforms.canvasWidth * 2.0;
          let screenY = (uniforms.cameraY - pos.y) * uniforms.zoom / uniforms.canvasHeight * 2.0;
          output.position = vec4<f32>(screenX, screenY, 0.0, 1.0);
          output.uv = uv;
          output.color = color;
          return output;
        }

        @fragment
        fn frag(input: VertexOutput) -> @location(0) vec4<f32> {
          let texColor = textureSample(symbolTex, texSampler, input.uv);
          let alpha = texColor.a;
          let color = mix(input.color * vec3<f32>(texColor.rgb), texColor.rgb, texColor.a);
          return vec4<f32>(color, alpha);
        }
      `,
    });

    const pipelineLayout = this.device.createPipelineLayout({
      bindGroupLayouts: [uniformLayout, this.symbolBindGroupLayout],
    });

    return this.device.createRenderPipeline({
      layout: pipelineLayout,
      vertex: {
        module: shaderModule,
        entryPoint: 'vert',
        buffers: [
          {
            arrayStride: 16,
            attributes: [
              { format: 'float32x2', offset: 0, shaderLocation: 0 },
              { format: 'float32x2', offset: 8, shaderLocation: 1 },
            ],
          },
          {
            arrayStride: 12,
            attributes: [{ format: 'float32x3', offset: 0, shaderLocation: 2 }],
          },
        ],
      },
      primitive: { topology: 'triangle-strip' },
      fragment: {
        module: shaderModule,
        entryPoint: 'frag',
        targets: [
          {
            format: this.textureFormat,
            blend: {
              color: {
                operation: 'add',
                srcFactor: 'src-alpha',
                dstFactor: 'one-minus-src-alpha',
              },
              alpha: {
                operation: 'add',
                srcFactor: 'one',
                dstFactor: 'one-minus-src-alpha',
              },
            },
          },
        ],
      },
    });
  }

  private createLinePipeline(uniformLayout: GPUBindGroupLayout): GPURenderPipeline {
    const shaderModule = this.device.createShaderModule({
      code: `
        struct Uniforms {
          cameraX: f32,
          cameraY: f32,
          zoom: f32,
          canvasWidth: f32,
          canvasHeight: f32,
        };

        @group(0) @binding(0) var<uniform> uniforms: Uniforms;

        struct VertexOutput {
          @builtin(position) position: vec4<f32>,
          @location(0) color: vec3<f32>,
        };

        @vertex
        fn vert(@location(0) pos: vec2<f32>, @location(1) color: vec3<f32>) -> VertexOutput {
          var output: VertexOutput;
          let screenX = (pos.x - uniforms.cameraX) * uniforms.zoom / uniforms.canvasWidth * 2.0;
          let screenY = (uniforms.cameraY - pos.y) * uniforms.zoom / uniforms.canvasHeight * 2.0;
          output.position = vec4<f32>(screenX, screenY, 0.0, 1.0);
          output.color = color;
          return output;
        }

        @fragment
        fn frag(input: VertexOutput) -> @location(0) vec4<f32> {
          return vec4<f32>(input.color, 0.7);
        }
      `,
    });

    const pipelineLayout = this.device.createPipelineLayout({
      bindGroupLayouts: [uniformLayout],
    });

    return this.device.createRenderPipeline({
      layout: pipelineLayout,
      vertex: {
        module: shaderModule,
        entryPoint: 'vert',
        buffers: [
          {
            arrayStride: 8,
            attributes: [{ format: 'float32x2', offset: 0, shaderLocation: 0 }],
          },
          {
            arrayStride: 12,
            attributes: [{ format: 'float32x3', offset: 0, shaderLocation: 1 }],
          },
        ],
      },
      primitive: { topology: 'triangle-strip' },
      fragment: {
        module: shaderModule,
        entryPoint: 'frag',
        targets: [
          {
            format: this.textureFormat,
            blend: {
              color: {
                operation: 'add',
                srcFactor: 'src-alpha',
                dstFactor: 'one-minus-src-alpha',
              },
              alpha: {
                operation: 'add',
                srcFactor: 'src-alpha',
                dstFactor: 'one-minus-src-alpha',
              },
            },
          },
        ],
      },
    });
  }

  private createOutlinePipeline(uniformLayout: GPUBindGroupLayout): GPURenderPipeline {
    const shaderModule = this.device.createShaderModule({
      code: `
        struct Uniforms {
          cameraX: f32,
          cameraY: f32,
          zoom: f32,
          canvasWidth: f32,
          canvasHeight: f32,
        };

        @group(0) @binding(0) var<uniform> uniforms: Uniforms;

        struct VertexOutput {
          @builtin(position) position: vec4<f32>,
          @location(0) color: vec3<f32>,
        };

        @vertex
        fn vert(@location(0) pos: vec2<f32>, @location(1) color: vec3<f32>) -> VertexOutput {
          var output: VertexOutput;
          let screenX = (pos.x - uniforms.cameraX) * uniforms.zoom / uniforms.canvasWidth * 2.0;
          let screenY = (uniforms.cameraY - pos.y) * uniforms.zoom / uniforms.canvasHeight * 2.0;
          output.position = vec4<f32>(screenX, screenY, 0.0, 1.0);
          output.color = color;
          return output;
        }

        @fragment
        fn frag(input: VertexOutput) -> @location(0) vec4<f32> {
          return vec4<f32>(input.color, ${OUTLINE_ALPHA.toFixed(2)});
        }
      `,
    });

    const pipelineLayout = this.device.createPipelineLayout({
      bindGroupLayouts: [uniformLayout],
    });

    return this.device.createRenderPipeline({
      layout: pipelineLayout,
      vertex: {
        module: shaderModule,
        entryPoint: 'vert',
        buffers: [
          {
            arrayStride: 8,
            attributes: [{ format: 'float32x2', offset: 0, shaderLocation: 0 }],
          },
          {
            arrayStride: 12,
            attributes: [{ format: 'float32x3', offset: 0, shaderLocation: 1 }],
          },
        ],
      },
      primitive: { topology: 'triangle-strip' },
      fragment: {
        module: shaderModule,
        entryPoint: 'frag',
        targets: [
          {
            format: this.textureFormat,
            blend: {
              color: {
                operation: 'add',
                srcFactor: 'src-alpha',
                dstFactor: 'one-minus-src-alpha',
              },
              alpha: {
                operation: 'add',
                srcFactor: 'src-alpha',
                dstFactor: 'one-minus-src-alpha',
              },
            },
          },
        ],
      },
    });
  }

  private createFogPipeline(): GPURenderPipeline {
    // Fog pipeline: fullscreen NDC quad with UVs computed in fragment shader
    // to sample the fog texture (which covers only the terrain area)
    // Fog uniform layout: cameraX, cameraY, zoom, canvasWidth, canvasHeight, cellSize (6 floats = 24 bytes)
    const fogUniformLayout = this.device.createBindGroupLayout({
      entries: [
        {
          binding: 0,
          visibility: GPUShaderStage.FRAGMENT,
          buffer: { type: 'uniform' },
        },
      ],
    });

    // Fog-specific uniform buffer
    this.fogUniformBuffer = this.device.createBuffer({
      size: 24, // 6 floats
      usage: GPUBufferUsage.UNIFORM | GPUBufferUsage.COPY_DST,
    });
    this.fogUniformBindGroup = this.device.createBindGroup({
      layout: fogUniformLayout,
      entries: [{ binding: 0, resource: { buffer: this.fogUniformBuffer } }],
    });

    this.fogBindGroupLayout = this.device.createBindGroupLayout({
      entries: [
        { binding: 0, visibility: GPUShaderStage.FRAGMENT, texture: {} },
        { binding: 1, visibility: GPUShaderStage.FRAGMENT, sampler: {} },
      ],
    });

    const shaderModule = this.device.createShaderModule({
      code: `
        struct FogUniforms {
          cameraX: f32,
          cameraY: f32,
          zoom: f32,
          canvasWidth: f32,
          canvasHeight: f32,
          cellSize: f32,
        };
        @group(0) @binding(0) var<uniform> fogUniforms: FogUniforms;
        @group(1) @binding(0) var fogTex: texture_2d<f32>;
        @group(1) @binding(1) var fogSampler: sampler;

        struct VertexOutput {
          @builtin(position) position: vec4<f32>,
        };

        @vertex
        fn vert(@location(0) pos: vec2<f32>) -> VertexOutput {
          var output: VertexOutput;
          output.position = vec4<f32>(pos, 0.0, 1.0);
          return output;
        }

        @fragment
        fn frag(input: VertexOutput) -> @location(0) vec4<f32> {
          // Inverse of shader NDC transform to get world-space position
          let ndcX = input.position.x;
          let ndcY = input.position.y;
          let worldX = fogUniforms.cameraX + ndcX * (fogUniforms.canvasWidth / 2.0) / fogUniforms.zoom;
          let worldY = fogUniforms.cameraY - ndcY * (fogUniforms.canvasHeight / 2.0) / fogUniforms.zoom;

          // Fog texture UVs: world (0..texSize*CELL_SIZE) maps to texture (0..1)
          // Each fog texel covers one CELL_SIZE unit, so UV = world / (texelCount * CELL_SIZE)
          let texSize = vec2<f32>(textureDimensions(fogTex, 0));
          let uv = vec2<f32>(worldX / (texSize.x * fogUniforms.cellSize), worldY / (texSize.y * fogUniforms.cellSize));

          let fog = textureSample(fogTex, fogSampler, uv);
          // Outside terrain area = no fog. The sampler clamps out-of-range UVs,
          // so build a 0/1 in-range mask with step() — textureSample must stay
          // in uniform control flow (no per-pixel branch around it).
          let inRange = step(0.0, uv.x) * step(uv.x, 1.0) * step(0.0, uv.y) * step(uv.y, 1.0);
          return vec4<f32>(0.0, 0.0, 0.05, fog.a * inRange);
        }
      `,
    });

    const pipelineLayout = this.device.createPipelineLayout({
      bindGroupLayouts: [fogUniformLayout, this.fogBindGroupLayout],
    });

    return this.device.createRenderPipeline({
      layout: pipelineLayout,
      vertex: {
        module: shaderModule,
        entryPoint: 'vert',
        buffers: [
          {
            arrayStride: 8,
            attributes: [{ format: 'float32x2', offset: 0, shaderLocation: 0 }],
          },
        ],
      },
      primitive: { topology: 'triangle-strip' },
      fragment: {
        module: shaderModule,
        entryPoint: 'frag',
        targets: [
          {
            format: this.textureFormat,
            blend: {
              color: {
                operation: 'add',
                srcFactor: 'src-alpha',
                dstFactor: 'one-minus-src-alpha',
              },
              alpha: {
                operation: 'add',
                srcFactor: 'src-alpha',
                dstFactor: 'one-minus-src-alpha',
              },
            },
          },
        ],
      },
    });
  }
  resizeCanvas(): void {
    this.setCanvasDimensions();
    // Update canvas dimensions in uniform buffer (offset 12 = 3 floats)
    this.device.queue.writeBuffer(
      this.uniformBuffer,
      12,
      new Float32Array([this.canvas.width, this.canvas.height]),
    );
    // Update fog fullscreen quad to match new canvas dimensions
    this.updateFogQuad();
  }

  /** Set canvas HTML attributes to match CSS display size (buffer-independent) */
  setCanvasDimensions(): void {
    const dpr = window.devicePixelRatio || 1;
    const rect = this.canvas.getBoundingClientRect();
    this.canvas.width = rect.width * dpr;
    this.canvas.height = rect.height * dpr;
  }

  setUpdateCallback(callback: () => void) {
    this.updateCallback = callback;
  }

  startRenderLoop() {
    const loop = () => {
      const key = this.frameKey();
      if (key !== this.lastFrameKey) {
        this.lastFrameKey = key;
        this.updateCallback?.();
        this.renderFrame();
      }
      requestAnimationFrame(loop);
    };
    requestAnimationFrame(loop);
  }

  public markDataDirty(): void {
    this.dataVersion++;
  }

  private frameKey(): string {
    return (
      `${this.camera.x},${this.camera.y},${this.camera.zoom}|` +
      `${this.canvas.width}x${this.canvas.height}|${this.dataVersion}|` +
      `${this.currentPath.length}|${this.fogDirty ? 1 : 0}${this.borderDirty ? 1 : 0}`
    );
  }

  public fitMapZoom(mapW: number, mapH: number): void {
    if (this.canvas.width === 0 || this.canvas.height === 0 || mapW === 0 || mapH === 0) return;
    const zoom = Math.min(this.canvas.width / mapW, this.canvas.height / mapH) * 0.9;
    this.camera.zoom = Math.max(ZOOM_MIN, Math.min(ZOOM_MAX, zoom));
    this.markDataDirty();
  }

  /** Zoom out so the whole terrain map fits the viewport (10% margin). */
  private fitMapToTerrain(gameState: GameState): void {
    const terrain = gameState.terrain;
    if (!terrain || terrain.length === 0 || terrain[0].length === 0) return;
    let maxCol = 0;
    for (const row of terrain) {
      if (row.length > 0) maxCol = Math.max(maxCol, row.length - 1);
    }
    this.fitMapZoom((maxCol + 1) * CELL_SIZE, terrain.length * CELL_SIZE);
  }

  /**
   * Center the camera on the player's own territory (own units + cities),
   * or on the terrain center when there is no own player (spectators).
   * Used on FullState arrival.
   */
  centerCameraOn(gameState: GameState): void {
    const ownId = gameState.ownPlayerId;
    if (ownId) {
      let sumX = 0;
      let sumY = 0;
      let count = 0;
      for (const unit of gameState.units.values()) {
        if (unit.owner === ownId) {
          sumX += unit.position.x * CELL_SIZE;
          sumY += unit.position.y * CELL_SIZE;
          count++;
        }
      }
      for (const [, city] of gameState.cities) {
        if (city.owner === ownId) {
          sumX += city.position[0] * CELL_SIZE;
          sumY += city.position[1] * CELL_SIZE;
          count++;
        }
      }
      if (count > 0) {
        this.camera.x = sumX / count;
        this.camera.y = sumY / count;
        this.fitMapToTerrain(gameState);
        return;
      }
    }
    this.centerOnTerrain(gameState);
  }

  /** Center the camera on the terrain bounding box (ragged rows handled). */
  centerOnTerrain(gameState: GameState): void {
    const terrain = gameState.terrain;
    if (!terrain || terrain.length === 0 || terrain[0].length === 0) return;

    // Find the bounding box of non-empty terrain rows (handle ragged grids)
    let minCol = Infinity,
      maxCol = -Infinity;
    for (const row of terrain) {
      if (row.length > 0) {
        minCol = Math.min(minCol, 0);
        maxCol = Math.max(maxCol, row.length - 1);
      }
    }
    const minRow = 0;
    const maxRow = terrain.length - 1;

    this.camera.x = ((minCol + maxCol) / 2 + 0.5) * CELL_SIZE;
    this.camera.y = ((minRow + maxRow) / 2 + 0.5) * CELL_SIZE;
    this.fitMapToTerrain(gameState);
  }

  renderFrame() {
    try {
      // Update uniform buffer with current camera state and canvas dimensions
      const uniformData = new Float32Array([
        this.camera.x,
        this.camera.y,
        this.camera.zoom,
        this.canvas.width,
        this.canvas.height,
      ]);
      this.device.queue.writeBuffer(this.uniformBuffer, 0, uniformData);

      const commandEncoder = this.device.createCommandEncoder();
      const textureView = this.context.getCurrentTexture().createView();
      const renderPass = commandEncoder.beginRenderPass({
        colorAttachments: [
          {
            view: textureView,
            clearValue: { r: 0.05, g: 0.05, b: 0.1, a: 1 },
            loadOp: 'clear',
            storeOp: 'store',
          },
        ],
      });

      // Bind uniforms for all draw calls (group 0)
      renderPass.setBindGroup(0, this.uniformBindGroup);

      // Draw terrain
      if (this.terrainVertexCount > 0) {
        renderPass.setPipeline(this.mainPipeline);
        renderPass.setVertexBuffer(0, this.terrainPositionsBuffer);
        renderPass.setVertexBuffer(1, this.terrainColorBuffer);
        renderPass.draw(this.terrainVertexCount);
      }

      // Draw borders
      if (this.borderVertexCount > 0) {
        renderPass.setPipeline(this.linePipeline);
        renderPass.setVertexBuffer(0, this.borderPositionsBuffer);
        renderPass.setVertexBuffer(1, this.borderColorBuffer);
        renderPass.draw(this.borderVertexCount);
      }

      // Draw path preview
      if (this.pathVertexCount > 0) {
        renderPass.setPipeline(this.linePipeline);
        renderPass.setVertexBuffer(0, this.pathPositionsBuffer);
        renderPass.setVertexBuffer(1, this.pathColorBuffer);
        renderPass.draw(this.pathVertexCount);
      }

      // Draw own-side outlines (semi-transparent halo behind own units/cities)
      if (this.outlineVertexCount > 0) {
        renderPass.setPipeline(this.outlinePipeline);
        renderPass.setVertexBuffer(0, this.outlinePositionsBuffer);
        renderPass.setVertexBuffer(1, this.outlineColorBuffer);
        renderPass.draw(this.outlineVertexCount);
      }

      // Draw city dots (on top of terrain/borders/path, below units)
      if (this.cityVertexCount > 0) {
        renderPass.setPipeline(this.mainPipeline);
        renderPass.setVertexBuffer(0, this.cityPositionsBuffer);
        renderPass.setVertexBuffer(1, this.cityColorBuffer);
        renderPass.draw(this.cityVertexCount);
      }

      // Draw unit dots
      if (this.unitVertexCount > 0) {
        renderPass.setPipeline(this.mainPipeline);
        renderPass.setVertexBuffer(0, this.unitPositionsBuffer);
        renderPass.setVertexBuffer(1, this.unitColorBuffer);
        renderPass.draw(this.unitVertexCount);
      }

      // Draw unit bars (health, org, equipment)
      if (this.barVertexCount > 0) {
        renderPass.setPipeline(this.mainPipeline);
        renderPass.setVertexBuffer(0, this.barPositionsBuffer);
        renderPass.setVertexBuffer(1, this.barColorBuffer);
        renderPass.draw(this.barVertexCount);
      }

      // Draw unit symbols
      if (this.unitVertexCount > 0) {
        renderPass.setPipeline(this.symbolPipeline);
        renderPass.setVertexBuffer(0, this.symbolPositionBuffer);
        renderPass.setVertexBuffer(1, this.symbolColorBuffer);
        if (this.symbolBindGroup) renderPass.setBindGroup(1, this.symbolBindGroup);
        renderPass.draw(this.unitVertexCount);
      }

      // Draw fog overlay (fullscreen quad with NDC positions)
      if (this.fogTexture) {
        // Update fog uniform buffer with current camera + CELL_SIZE
        const fogUniformData = new Float32Array([
          this.camera.x,
          this.camera.y,
          this.camera.zoom,
          this.canvas.width,
          this.canvas.height,
          CELL_SIZE,
        ]);
        this.device.queue.writeBuffer(this.fogUniformBuffer, 0, fogUniformData);
        renderPass.setPipeline(this.fogPipeline);
        renderPass.setVertexBuffer(0, this.fogPositionsBuffer);
        renderPass.setBindGroup(0, this.fogUniformBindGroup);
        if (this.fogBindGroup) renderPass.setBindGroup(1, this.fogBindGroup);
        renderPass.draw(4);
      }

      renderPass.end();
      this.device.queue.submit([commandEncoder.finish()]);
      this.framesSubmitted++;
    } catch (err) {
      // Record the error so the debug overlay can surface it (instead of a silent blank frame).
      this.lastRenderError = err instanceof Error ? err.message : String(err);
      console.error('Render frame error:', err);
      // Don't crash — continue trying to render next frame
    }
  }

  private createSymbolBindGroup(): GPUBindGroup {
    return this.device.createBindGroup({
      layout: this.symbolBindGroupLayout,
      entries: [
        { binding: 0, resource: this.symbolTexture.createView() },
        { binding: 1, resource: this.symbolSampler },
      ],
    });
  }

  private createFogBindGroup(): GPUBindGroup {
    return this.device.createBindGroup({
      layout: this.fogBindGroupLayout,
      entries: [
        { binding: 0, resource: this.fogTexture!.createView() },
        { binding: 1, resource: this.symbolSampler },
      ],
    });
  }

  /** Recreate a vertex buffer with a larger size when the frame needs more capacity. */
  private ensureBuffer(buffer: GPUBuffer, neededBytes: number): GPUBuffer {
    if (neededBytes <= buffer.size) return buffer;
    const size = Math.max(neededBytes, Math.ceil(buffer.size * 1.5));
    return this.device.createBuffer({ size, usage: buffer.usage });
  }

  render(gameState: GameState) {
    // Guard: don't render if no game data yet (lobby state)
    if (!gameState.terrain || gameState.terrain.length === 0) {
      return;
    }

    try {
      // Update terrain buffers
      // Cache the CPU mesh keyed on the visible cell range (mirrors marchingSquares'
      // own viewport culling math) + the terrain reference. Terrain is static during a
      // match and the camera only changes the visible subset, so a stationary camera
      // reuses the already-computed mesh and skips both marchingSquares and the upload.
      const grid = gameState.terrain;
      const gHeight = grid.length;
      const gWidth = grid[0].length;
      const tZoom = this.camera.zoom;
      const camWorldX = this.camera.x - this.canvas.width / (2 * tZoom);
      const camWorldY = this.camera.y - this.canvas.height / (2 * tZoom);
      const visibleWorldW = this.canvas.width / tZoom;
      const visibleWorldH = this.canvas.height / tZoom;
      const tStartCol = Math.max(0, Math.floor(camWorldX / CELL_SIZE) - 1);
      const tEndCol = Math.min(gWidth, Math.ceil((camWorldX + visibleWorldW) / CELL_SIZE) + 1);
      const tStartRow = Math.max(0, Math.floor(camWorldY / CELL_SIZE) - 1);
      const tEndRow = Math.min(gHeight, Math.ceil((camWorldY + visibleWorldH) / CELL_SIZE) + 1);
      const terrainKey = `${tStartCol},${tEndCol},${tStartRow},${tEndRow},${this.canvas.width}x${this.canvas.height}`;
      const terrainCached =
        this.terrainMeshKey === terrainKey &&
        this.cachedTerrainRef === grid &&
        this.cachedTerrainPositions !== null;
      let positions: number[];
      let colors: number[];
      if (terrainCached) {
        positions = this.cachedTerrainPositions!;
        colors = this.cachedTerrainColors!;
      } else {
        const mesh = marchingSquares(
          grid,
          CELL_SIZE,
          this.camera,
          tZoom,
          this.canvas.width,
          this.canvas.height,
        );
        positions = mesh.positions;
        colors = mesh.colors;
        this.terrainMeshKey = terrainKey;
        this.cachedTerrainRef = grid;
        this.cachedTerrainPositions = positions;
        this.cachedTerrainColors = colors;
      }
      if (positions.length > 0) {
        this.terrainVertexCount = positions.length / 2;
        // Only (re)build the upload data and re-upload when the mesh was
        // rebuilt this frame; a cache hit means the buffer already holds it.
        if (!terrainCached) {
          this.cachedTerrainPosData = new Float32Array(positions);
          this.cachedTerrainColData = new Float32Array(colors);
          this.terrainPositionsBuffer = this.ensureBuffer(
            this.terrainPositionsBuffer,
            this.cachedTerrainPosData.byteLength,
          );
          this.terrainColorBuffer = this.ensureBuffer(
            this.terrainColorBuffer,
            this.cachedTerrainColData.byteLength,
          );
          this.device.queue.writeBuffer(this.terrainPositionsBuffer, 0, this.cachedTerrainPosData);
          this.device.queue.writeBuffer(this.terrainColorBuffer, 0, this.cachedTerrainColData);
        }
      } else {
        this.terrainVertexCount = 0;
      }

      // Update unit buffers and symbols
      const unitPositions: number[] = [];
      const unitColors: number[] = [];
      const symbolPositions: number[] = [];
      const symbolUVs: number[] = [];
      const symbolColors: number[] = [];
      const barPositions: number[] = [];
      const barColors: number[] = [];

      // Compute a cache key for the symbol canvas — only redraw when unit composition changes
      let symbolKey = '';
      for (const unit of gameState.units.values()) {
        symbolKey += `${unit.id}:${unit.symbol}:${unit.onWater}:${unit.embarking}:${unit.disembarking};`;
      }

      // Only redraw symbols on the 2D canvas when unit types/water status changed
      const symbolChanged = symbolKey !== this.symbolCacheKey;
      if (symbolChanged) {
        this.symbolCtx.clearRect(0, 0, SYMBOL_ATLAS_SIZE, SYMBOL_ATLAS_SIZE);
        this.symbolCacheKey = symbolKey;
      }

      let unitIndex = 0;
      for (const unit of gameState.units.values()) {
        // Send WORLD-SPACE positions; shader applies camera + zoom + NDC
        const wx = unit.position.x * CELL_SIZE;
        const wy = unit.position.y * CELL_SIZE;
        const halfSizeWorld = UNIT_DOT_SIZE / 2 / this.camera.zoom;
        unitPositions.push(
          wx - halfSizeWorld,
          wy - halfSizeWorld,
          wx + halfSizeWorld,
          wy - halfSizeWorld,
          wx + halfSizeWorld,
          wy + halfSizeWorld,
          wx - halfSizeWorld,
          wy + halfSizeWorld,
        );
        // Color by owner (player), not by unit type
        const colorIdx = gameState.colorIndexForOwner(unit.owner);
        const color = PLAYER_COLORS[colorIdx % PLAYER_COLORS.length] || { r: 1, g: 1, b: 1 };
        for (let i = 0; i < 4; i++) {
          unitColors.push(color.r, color.g, color.b);
        }

        // Symbol quad — use world-space size so shader transforms correctly
        const symHalfSizeWorld = halfSizeWorld * 0.8;
        const cellX = (unitIndex % SYMBOLS_PER_ROW) * SYMBOL_CELL_SIZE;
        const cellY = Math.floor(unitIndex / SYMBOLS_PER_ROW) * SYMBOL_CELL_SIZE;
        const uvW = SYMBOL_CELL_SIZE / SYMBOL_ATLAS_SIZE;
        const uvH = SYMBOL_CELL_SIZE / SYMBOL_ATLAS_SIZE;
        const uvX = cellX / SYMBOL_ATLAS_SIZE;
        const uvY = cellY / SYMBOL_ATLAS_SIZE;
        symbolPositions.push(
          wx - symHalfSizeWorld,
          wy - symHalfSizeWorld,
          wx + symHalfSizeWorld,
          wy - symHalfSizeWorld,
          wx + symHalfSizeWorld,
          wy + symHalfSizeWorld,
          wx - symHalfSizeWorld,
          wy + symHalfSizeWorld,
        );
        symbolUVs.push(uvX, uvY, uvX + uvW, uvY, uvX + uvW, uvY + uvH, uvX, uvY + uvH);
        for (let i = 0; i < 4; i++) {
          symbolColors.push(1, 1, 1);
        }

        // Bars: health (men), org, equipment — thin quads above the symbol
        // Reuse symHalfSizeWorld from symbol quad above; convert bar dims to world-space
        const barWidth = symHalfSizeWorld * BAR_WIDTH_MULTIPLIER;
        const barHeight = BAR_HEIGHT; // world-space, shader applies zoom
        const barSpacingWorld = BAR_SPACING / this.camera.zoom;
        const renderData = unit.unitRenderData();
        let barY = wy - symHalfSizeWorld - barSpacingWorld;

        // Three bars, each with different colors
        const barData = [
          { pct: renderData.menPercent, color: BAR_COLORS.health },
          { pct: renderData.orgPercent, color: BAR_COLORS.org },
          { pct: renderData.eqPercent, color: BAR_COLORS.equip },
        ];

        for (const bar of barData) {
          const actualWidth = (barWidth * bar.pct) / 100;
          const bx = wx - barWidth / 2;
          const by = barY;
          barPositions.push(
            bx,
            by,
            bx + actualWidth,
            by,
            bx + actualWidth,
            by + barHeight,
            bx,
            by + barHeight,
          );
          for (let i = 0; i < 4; i++) {
            barColors.push(bar.color.r, bar.color.g, bar.color.b);
          }
          barY -= barSpacingWorld;
        }

        // Draw symbol on canvas (only when the atlas was invalidated this
        // frame). NOTE: must be `symbolChanged`, not a re-comparison against
        // `this.symbolCacheKey` — that key is already committed above, so the
        // re-comparison would always be false and no glyphs would ever draw.
        if (symbolChanged) {
          this.symbolCtx.save();

          // Water unit indicator: blue border/tint
          if (unit.onWater || unit.embarking || unit.disembarking) {
            this.symbolCtx.beginPath();
            this.symbolCtx.arc(cellX + 16, cellY + 16, 14, 0, Math.PI * 2);
            this.symbolCtx.fillStyle = WATER_INDICATOR_COLOR;
            this.symbolCtx.fill();
            this.symbolCtx.strokeStyle = WATER_BORDER_COLOR;
            this.symbolCtx.lineWidth = 2;
            this.symbolCtx.stroke();
          }

          this.symbolCtx.font = 'bold 14px sans-serif';
          this.symbolCtx.textAlign = 'center';
          this.symbolCtx.textBaseline = 'middle';
          this.symbolCtx.fillStyle = '#ffffff';

          // Draw unit symbol
          this.symbolCtx.fillText(unit.symbol || '?', cellX + 16, cellY + 16);

          // Draw anchor icon for water units
          if (unit.onWater) {
            this.symbolCtx.font = 'bold 8px sans-serif';
            this.symbolCtx.fillStyle = WATER_BORDER_COLOR;
            this.symbolCtx.fillText('⚓', cellX + 16, cellY + 4);
          }

          // Draw embark/disembark indicator
          if (unit.embarking || unit.disembarking) {
            this.symbolCtx.font = 'bold 8px sans-serif';
            this.symbolCtx.fillStyle = EMBARK_COLOR;
            this.symbolCtx.fillText('⛴', cellX + 16, cellY + 4);
          }

          this.symbolCtx.restore();
        }

        unitIndex++;
      }

      // Upload symbol texture and buffers — only upload texture when cache invalidated
      if (unitIndex > 0) {
        if (symbolChanged) {
          const imageBitmap = this.symbolCanvas.transferToImageBitmap();
          this.device.queue.copyExternalImageToTexture(
            { source: imageBitmap },
            { texture: this.symbolTexture },
            [SYMBOL_ATLAS_SIZE, SYMBOL_ATLAS_SIZE, 1],
          );
        }
        // Interleave position + UV into single buffer (stride 16: pos x,y + uv x,y)
        const symbolData = new Float32Array(unitIndex * 4 * 4); // 4 vertices per unit, 4 floats each
        for (let i = 0; i < unitIndex * 4; i++) {
          symbolData[i * 4 + 0] = symbolPositions[i * 2 + 0]; // pos x
          symbolData[i * 4 + 1] = symbolPositions[i * 2 + 1]; // pos y
          symbolData[i * 4 + 2] = symbolUVs[i * 2 + 0]; // uv x
          symbolData[i * 4 + 3] = symbolUVs[i * 2 + 1]; // uv y
        }
        const symbolColData = new Float32Array(symbolColors);
        this.symbolPositionBuffer = this.ensureBuffer(
          this.symbolPositionBuffer,
          symbolData.byteLength,
        );
        this.symbolColorBuffer = this.ensureBuffer(
          this.symbolColorBuffer,
          symbolColData.byteLength,
        );
        this.device.queue.writeBuffer(this.symbolPositionBuffer, 0, symbolData);
        this.device.queue.writeBuffer(this.symbolColorBuffer, 0, symbolColData);
      }

      if (unitPositions.length > 0) {
        this.unitVertexCount = unitPositions.length / 2;
        const upData = new Float32Array(unitPositions);
        const ucData = new Float32Array(unitColors);
        this.unitPositionsBuffer = this.ensureBuffer(this.unitPositionsBuffer, upData.byteLength);
        this.unitColorBuffer = this.ensureBuffer(this.unitColorBuffer, ucData.byteLength);
        this.device.queue.writeBuffer(this.unitPositionsBuffer, 0, upData);
        this.device.queue.writeBuffer(this.unitColorBuffer, 0, ucData);
      } else {
        this.unitVertexCount = 0;
      }

      // Upload unit bars
      if (barPositions.length > 0) {
        this.barVertexCount = barPositions.length / 2;
        const bpData = new Float32Array(barPositions);
        const bcData = new Float32Array(barColors);
        this.barPositionsBuffer = this.ensureBuffer(this.barPositionsBuffer, bpData.byteLength);
        this.barColorBuffer = this.ensureBuffer(this.barColorBuffer, bcData.byteLength);
        this.device.queue.writeBuffer(this.barPositionsBuffer, 0, bpData);
        this.device.queue.writeBuffer(this.barColorBuffer, 0, bcData);
      } else {
        this.barVertexCount = 0;
      }

      // Update borders — cache logical edges (camera-independent), recompute positions each frame
      const borderGrid = gameState.borderGrid;

      // Rebuild logical edges when grid content changes
      if (this.borderDirty) {
        this.borderDirty = false;
        if (borderGrid.length > 0) {
          const rows = borderGrid.length;
          const cols = borderGrid[0].length;
          const cells = rows * cols;

          // W5: Fill territory so borders form contiguous regions instead of city-connecting blobs.
          // Perf #3: the dirty flag fires on every delta, but most deltas don't change the
          // grid content — flatten into a reused buffer, then skip the full BFS when the
          // grid, city anchors, and terrain are all unchanged since the last pass.
          this.refreshWaterMask(gameState.terrain, rows, cols);

          if (this.borderFlatScratch.length < cells) this.borderFlatScratch = new Uint8Array(cells);
          const flat = this.borderFlatScratch;
          for (let y = 0; y < rows; y++) {
            const row = borderGrid[y];
            let i = y * cols;
            for (let x = 0; x < cols; x++, i++) flat[i] = row[x] ?? TERRITORY_NONE;
          }

          // City anchors → packed scratch + change key (position + owner index).
          let anchorCount = 0;
          let cityKey = '';
          for (const city of gameState.cities.values()) {
            if (anchorCount === this.anchorX.length) this.growAnchors();
            this.anchorX[anchorCount] = Math.floor(city.position[0]);
            this.anchorY[anchorCount] = Math.floor(city.position[1]);
            this.anchorOwner[anchorCount] = gameState.colorIndexForOwner(city.owner);
            cityKey += `${this.anchorX[anchorCount]},${this.anchorY[anchorCount]},${this.anchorOwner[anchorCount]};`;
            anchorCount++;
          }

          const changed =
            this.lastBorderFlat.length !== cells ||
            this.lastCityKey !== cityKey ||
            this.lastTerrainRef !== gameState.terrain ||
            this.bytesDiffer(this.lastBorderFlat, flat, cells);

          if (changed) {
            const display = this.territoryFiller.fill(
              flat,
              this.waterMask,
              rows,
              cols,
              this.anchorX,
              this.anchorY,
              this.anchorOwner,
              anchorCount,
            );
            this.ensureEdgeCapacity(cells);
            this.edgeCount = detectBorderEdgesFlat(display, rows, cols, this.edgeBuf());
            this.borderEdgeSeq++;
            this.territoryRecomputeCount++;
            // Commit the input snapshot for next pass's change detection.
            if (this.lastBorderFlat.length !== cells) this.lastBorderFlat = new Uint8Array(cells);
            this.lastBorderFlat.set(flat.subarray(0, cells), 0);
            this.lastCityKey = cityKey;
            this.lastTerrainRef = gameState.terrain;
          }
          // !changed: the edgeCount + edge arrays from the last pass are still
          // valid (identical grid content) — keep them.
        } else {
          // Grid cleared → drop the cached edges (original behaviour: no
          // borders) and reset the committed snapshot so the next non-empty
          // pass always recomputes — even if its bytes match the pre-clear grid.
          this.edgeCount = 0;
          this.lastBorderFlat = new Uint8Array(0);
          this.lastCityKey = null;
          this.lastTerrainRef = null;
        }
      }

      // Compute WORLD-SPACE positions from cached logical edges; shader applies camera + zoom
      // Each owner boundary is rendered as TWO adjacent thin quads, one in each owner's
      // colour (the "player colour / other player colour" two-line request).
      // Perf #3: write into reused grow-only typed arrays (no per-frame JS arrays).
      // Positions depend only on the logical edges + zoom (world space; the
      // shader applies the camera), so an unchanged (edgeSeq, zoom) pair skips
      // both the ~1.2M-value regeneration and the GPU upload — the buffer
      // already holds the exact content.
      const lw = BORDER_LINE_WIDTH / this.camera.zoom;
      const posGenNeeded =
        this.edgeCount > 0 &&
        (this.lastPosEdgeSeq !== this.borderEdgeSeq || this.lastPosZoom !== this.camera.zoom);
      if (posGenNeeded) {
        const posLen = this.edgeCount * 16; // 2 quads × 4 verts × 2 coords
        const colLen = this.edgeCount * 24; // 2 quads × 4 verts × 3 colors
        if (this.borderPosData.length < posLen) this.borderPosData = new Float32Array(posLen);
        if (this.borderColData.length < colLen) this.borderColData = new Float32Array(colLen);
        const bp = this.borderPosData;
        const bc = this.borderColData;
        let pi = 0;
        let ci = 0;
        for (let e = 0; e < this.edgeCount; e++) {
          const cA = PLAYER_COLORS[this.edgeOwnerA[e] % PLAYER_COLORS.length] || {
            r: 0.5,
            g: 0.5,
            b: 0.5,
          };
          const cB = PLAYER_COLORS[this.edgeOwnerB[e] % PLAYER_COLORS.length] || {
            r: 0.5,
            g: 0.5,
            b: 0.5,
          };
          if (this.edgeDir[e] === 0) {
            // 'h': horizontal border between rows (gridY) and (gridY+1). ownerA is the upper cell.
            const wx = this.edgeX[e] * CELL_SIZE;
            const wy = (this.edgeY[e] + 1) * CELL_SIZE;
            // Quad A (ownerA) — upper half, [wy - lw, wy]
            bp[pi++] = wx;
            bp[pi++] = wy - lw;
            bp[pi++] = wx + CELL_SIZE;
            bp[pi++] = wy - lw;
            bp[pi++] = wx + CELL_SIZE;
            bp[pi++] = wy;
            bp[pi++] = wx;
            bp[pi++] = wy;
            // Quad B (ownerB) — lower half, [wy, wy + lw]
            bp[pi++] = wx;
            bp[pi++] = wy;
            bp[pi++] = wx + CELL_SIZE;
            bp[pi++] = wy;
            bp[pi++] = wx + CELL_SIZE;
            bp[pi++] = wy + lw;
            bp[pi++] = wx;
            bp[pi++] = wy + lw;
          } else {
            // 'v': vertical border between cols (gridX) and (gridX+1). ownerA is the left cell.
            const wx = (this.edgeX[e] + 1) * CELL_SIZE;
            const wy = this.edgeY[e] * CELL_SIZE;
            // Quad A (ownerA) — left half, [wx - lw, wx]
            const xa = wx - lw;
            bp[pi++] = xa;
            bp[pi++] = wy;
            bp[pi++] = wx;
            bp[pi++] = wy;
            bp[pi++] = wx;
            bp[pi++] = wy + CELL_SIZE;
            bp[pi++] = xa;
            bp[pi++] = wy + CELL_SIZE;
            // Quad B (ownerB) — right half, [wx, wx + lw]
            bp[pi++] = wx;
            bp[pi++] = wy;
            bp[pi++] = wx + lw;
            bp[pi++] = wy;
            bp[pi++] = wx + lw;
            bp[pi++] = wy + CELL_SIZE;
            bp[pi++] = wx;
            bp[pi++] = wy + CELL_SIZE;
          }
          for (let i = 0; i < 4; i++) {
            bc[ci++] = cA.r;
            bc[ci++] = cA.g;
            bc[ci++] = cA.b;
          }
          for (let i = 0; i < 4; i++) {
            bc[ci++] = cB.r;
            bc[ci++] = cB.g;
            bc[ci++] = cB.b;
          }
        }
        this.borderVertexCount = this.edgeCount * 8; // 2 quads × 4 verts
        const bordP = bp.subarray(0, posLen);
        const bordC = bc.subarray(0, colLen);
        this.borderPositionsBuffer = this.ensureBuffer(
          this.borderPositionsBuffer,
          bordP.byteLength,
        );
        this.borderColorBuffer = this.ensureBuffer(this.borderColorBuffer, bordC.byteLength);
        this.device.queue.writeBuffer(this.borderPositionsBuffer, 0, bordP);
        this.device.queue.writeBuffer(this.borderColorBuffer, 0, bordC);
        this.lastPosEdgeSeq = this.borderEdgeSeq;
        this.lastPosZoom = this.camera.zoom;
      } else {
        // Buffer content already current (or no edges) — keep the draw count.
        this.borderVertexCount = this.edgeCount * 8; // 0 when no edges
      }

      // Update path preview
      if (this.currentPath.length > 1) {
        const pathPositions: number[] = [];
        const pathColors: number[] = [];
        for (let i = 0; i < this.currentPath.length - 1; i++) {
          const p1 = this.currentPath[i];
          const p2 = this.currentPath[i + 1];
          // Send WORLD-SPACE positions; shader applies camera + zoom
          const wx1 = p1.x * CELL_SIZE;
          const wy1 = p1.y * CELL_SIZE;
          const wx2 = p2.x * CELL_SIZE;
          const wy2 = p2.y * CELL_SIZE;
          pathPositions.push(
            wx1 - PATH_LINE_WIDTH,
            wy1,
            wx1 + PATH_LINE_WIDTH,
            wy1,
            wx2 + PATH_LINE_WIDTH,
            wy2,
            wx2 - PATH_LINE_WIDTH,
            wy2,
          );
          for (let j = 0; j < 4; j++) pathColors.push(1, 1, 1);
        }
        if (pathPositions.length > 0) {
          this.pathVertexCount = pathPositions.length / 2;
          const pathP = new Float32Array(pathPositions);
          const pathC = new Float32Array(pathColors);
          this.pathPositionsBuffer = this.ensureBuffer(this.pathPositionsBuffer, pathP.byteLength);
          this.pathColorBuffer = this.ensureBuffer(this.pathColorBuffer, pathC.byteLength);
          this.device.queue.writeBuffer(this.pathPositionsBuffer, 0, pathP);
          this.device.queue.writeBuffer(this.pathColorBuffer, 0, pathC);
        } else {
          this.pathVertexCount = 0;
        }
      } else {
        this.pathVertexCount = 0;
      }

      // Update city dots — colored markers at city positions
      const cityPositions: number[] = [];
      const cityColorsArr: number[] = [];
      for (const city of gameState.cities.values()) {
        const wx = city.position[0] * CELL_SIZE;
        const wy = city.position[1] * CELL_SIZE;
        // W4: screen-space constant size (diameter in screen pixels)
        let dotSize = CITY_DOT_SIZE;
        // W6: own cities are 1.5x larger
        if (city.owner === gameState.ownPlayerId) {
          dotSize *= 1.5;
        }
        const halfSizeWorld = dotSize / 2 / this.camera.zoom;
        cityPositions.push(
          wx - halfSizeWorld,
          wy - halfSizeWorld,
          wx + halfSizeWorld,
          wy - halfSizeWorld,
          wx + halfSizeWorld,
          wy + halfSizeWorld,
          wx - halfSizeWorld,
          wy + halfSizeWorld,
        );
        // Color by owner; W6: own cities get 50% white mix for brightness
        const colorIdx = gameState.colorIndexForOwner(city.owner);
        let color = CITY_COLORS[colorIdx % CITY_COLORS.length] || { r: 0.5, g: 0.5, b: 0.5 };
        if (city.owner === gameState.ownPlayerId) {
          color = {
            r: color.r * 0.5 + 0.5,
            g: color.g * 0.5 + 0.5,
            b: color.b * 0.5 + 0.5,
          };
        }
        for (let i = 0; i < 4; i++) {
          cityColorsArr.push(color.r, color.g, color.b);
        }
      }

      if (cityPositions.length > 0) {
        this.cityVertexCount = cityPositions.length / 2;
        const cityP = new Float32Array(cityPositions);
        const cityC = new Float32Array(cityColorsArr);
        this.cityPositionsBuffer = this.ensureBuffer(this.cityPositionsBuffer, cityP.byteLength);
        this.cityColorBuffer = this.ensureBuffer(this.cityColorBuffer, cityC.byteLength);
        this.device.queue.writeBuffer(this.cityPositionsBuffer, 0, cityP);
        this.device.queue.writeBuffer(this.cityColorBuffer, 0, cityC);
      } else {
        this.cityVertexCount = 0;
      }

      // Update own-side outlines — larger semi-transparent halo behind own units/cities.
      const outlinePositions: number[] = [];
      const outlineColors: number[] = [];
      const ownId = gameState.ownPlayerId;
      if (ownId) {
        const pushOutline = (wx: number, wy: number, dotSize: number) => {
          const halfSize = dotSize + OUTLINE_EXPAND;
          const halfSizeWorld = halfSize / this.camera.zoom;
          outlinePositions.push(
            wx - halfSizeWorld,
            wy - halfSizeWorld,
            wx + halfSizeWorld,
            wy - halfSizeWorld,
            wx + halfSizeWorld,
            wy + halfSizeWorld,
            wx - halfSizeWorld,
            wy + halfSizeWorld,
          );
          for (let i = 0; i < 4; i++) {
            outlineColors.push(OUTLINE_COLOR.r, OUTLINE_COLOR.g, OUTLINE_COLOR.b);
          }
        };
        for (const unit of gameState.units.values()) {
          if (unit.owner === ownId) {
            pushOutline(
              unit.position.x * CELL_SIZE,
              unit.position.y * CELL_SIZE,
              UNIT_DOT_SIZE / 2,
            );
          }
        }
        for (const [, city] of gameState.cities) {
          if (city.owner === ownId) {
            pushOutline(
              city.position[0] * CELL_SIZE,
              city.position[1] * CELL_SIZE,
              CITY_DOT_SIZE / 2,
            );
          }
        }
      }
      if (outlinePositions.length > 0) {
        this.outlineVertexCount = outlinePositions.length / 2;
        const olP = new Float32Array(outlinePositions);
        const olC = new Float32Array(outlineColors);
        this.outlinePositionsBuffer = this.ensureBuffer(
          this.outlinePositionsBuffer,
          olP.byteLength,
        );
        this.outlineColorBuffer = this.ensureBuffer(this.outlineColorBuffer, olC.byteLength);
        this.device.queue.writeBuffer(this.outlinePositionsBuffer, 0, olP);
        this.device.queue.writeBuffer(this.outlineColorBuffer, 0, olC);
      } else {
        this.outlineVertexCount = 0;
      }

      // Update fog texture — only when visionGrid actually changed
      const visionGrid = gameState.visionGrid;
      if (visionGrid.length > 0) {
        const width = visionGrid[0].length;
        const height = visionGrid.length;
        const cells = width * height;

        // Perf #3: flatten into a reused buffer and compare against the last
        // uploaded content — the dirty flag fires on any vision delta, but most
        // deltas leave the uploaded grid unchanged (or patch a cell to its
        // current value). A content-identical pass skips the 4*cells-byte
        // rebuild and the GPU writeTexture.
        if (this.visionFlatScratch.length < cells) this.visionFlatScratch = new Uint8Array(cells);
        const vis = this.visionFlatScratch;
        for (let y = 0; y < height; y++) {
          const row = visionGrid[y];
          let i = y * width;
          for (let x = 0; x < width; x++, i++) vis[i] = row?.[x] ? 1 : 0;
        }
        const contentChanged =
          this.lastVisionFlat.length !== cells || this.bytesDiffer(this.lastVisionFlat, vis, cells);

        // Existing contract (see "dirty-flag and cache gating" tests):
        // markFogDirty forces a re-upload, and a size change does too. The
        // contentChanged term is a safety net for callers that mutate
        // visionGrid without marking fog dirty — it only ever adds rewrites,
        // never removes ones the flag requires.
        const needsUpdate =
          this.fogDirty ||
          !this.fogTexture ||
          this.fogCacheWidth !== width ||
          this.fogCacheHeight !== height ||
          contentChanged;

        if (needsUpdate) {
          if (this.fogData.length < cells * 4) this.fogData = new Uint8Array(cells * 4);
          const fogData = this.fogData;
          for (let i = 0; i < cells; i++) {
            fogData[i * 4] = 0;
            fogData[i * 4 + 1] = 0;
            fogData[i * 4 + 2] = 0;
            fogData[i * 4 + 3] = vis[i] ? 0 : 255;
          }
          if (!this.fogTexture) {
            this.fogTexture = this.device.createTexture({
              size: [width, height],
              format: 'rgba8unorm',
              usage: GPUTextureUsage.TEXTURE_BINDING | GPUTextureUsage.COPY_DST,
            });
            // Fog bind group depends only on the (static) fog texture — create once.
            this.fogBindGroup = this.createFogBindGroup();
          }
          this.device.queue.writeTexture(
            { texture: this.fogTexture },
            fogData.subarray(0, cells * 4),
            { offset: 0, bytesPerRow: width * 4 },
            [width, height, 1] as [number, number, number],
          );
          this.fogCacheWidth = width;
          this.fogCacheHeight = height;
          this.fogDirty = false;
          // Commit the uploaded content for next pass's change detection.
          if (this.lastVisionFlat.length !== cells) this.lastVisionFlat = new Uint8Array(cells);
          this.lastVisionFlat.set(vis.subarray(0, cells), 0);
        }
      }
    } catch (err) {
      console.error('Render update error:', err);
    }
  }

  // ------------------------------------------------------------------
  // Perf #3 helpers — flat scratch management for the territory/fog pipeline
  // ------------------------------------------------------------------

  /** (Re)build the flat water mask when the terrain reference or size changed. */
  private refreshWaterMask(terrain: TerrainType[][], rows: number, cols: number): void {
    const cells = rows * cols;
    if (this.waterMaskTerrainRef === terrain && this.waterMask.length >= cells) return;
    const mask = new Uint8Array(cells);
    for (let y = 0; y < rows; y++) {
      const tRow = terrain[y];
      if (!tRow) continue;
      for (let x = 0; x < cols; x++) {
        if (tRow[x] === 'Water') mask[y * cols + x] = 1;
      }
    }
    this.waterMask = mask;
    this.waterMaskTerrainRef = terrain;
  }

  /** Grow the packed city-anchor scratch (×2, minimum 8). */
  private growAnchors(): void {
    const n = Math.max(8, this.anchorX.length * 2);
    const x = new Int32Array(n);
    x.set(this.anchorX);
    const y = new Int32Array(n);
    y.set(this.anchorY);
    const o = new Uint8Array(n);
    o.set(this.anchorOwner);
    this.anchorX = x;
    this.anchorY = y;
    this.anchorOwner = o;
  }

  /** Reusable EdgeBuffers view over the edge arrays (rebuilt after growth). */
  private edgeBuf(): EdgeBuffers {
    if (!this.edgeBufCache) {
      this.edgeBufCache = {
        x: this.edgeX,
        y: this.edgeY,
        dir: this.edgeDir,
        ownerA: this.edgeOwnerA,
        ownerB: this.edgeOwnerB,
      };
    }
    return this.edgeBufCache;
  }

  /** Grow the edge storage to hold up to 2 edges per cell (right + lower). */
  private ensureEdgeCapacity(cells: number): void {
    const need = cells * 2;
    if (this.edgeX.length < need) {
      this.edgeX = new Int32Array(need);
      this.edgeY = new Int32Array(need);
      this.edgeDir = new Uint8Array(need);
      this.edgeOwnerA = new Uint8Array(need);
      this.edgeOwnerB = new Uint8Array(need);
    }
    this.edgeBufCache = null; // invalidate the cached view (arrays may have moved)
  }

  /** Cheap change detection: are two flat byte buffers different over `len` cells? */
  private bytesDiffer(a: Uint8Array, b: Uint8Array, len: number): boolean {
    for (let i = 0; i < len; i++) {
      if (a[i] !== b[i]) return true;
    }
    return false;
  }

  /** Mark the fog texture as needing a rebuild (call after visionGrid changes) */
  markFogDirty(): void {
    this.fogDirty = true;
  }

  /** Mark border edges as needing a rebuild (call after borderGrid changes) */
  markBorderDirty(): void {
    this.borderDirty = true;
  }
}
