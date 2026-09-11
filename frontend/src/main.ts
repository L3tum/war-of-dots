//! Main frontend entry

import { GameState } from './game/game_state';
import { Renderer } from './game/renderer';
import { Connection } from './net/connection';
import { LobbyUI } from './ui/lobby';
import { Controls } from './ui/controls';
import { Spectator } from './ui/spectator';
import { Advisor } from './ui/advisor';
import { UnitInfoUI } from './ui/unit_info';
import { ReplayPlayer } from './replay/replay_player';
import { SpectatorReplay } from './replay/spectator';
import {
  ZOOM_MIN,
  ZOOM_MAX,
  ZOOM_INCREMENT,
  KEYBOARD_PAN_SPEED,
  CITY_LABEL_UPDATE_INTERVAL,
  STATS_PANEL_UPDATE_INTERVAL,
  CELL_SIZE,
} from './game/config';

async function main() {
  const canvas = document.getElementById('game-canvas') as HTMLCanvasElement;

  // Check route
  const path = window.location.pathname;

  // Replay list page
  if (path === '/replay' || path === '/replay/') {
    const urlParams = new URLSearchParams(window.location.search);
    const gameId = urlParams.get('game_id');
    if (gameId) {
      // Connect to replay WebSocket
      connectReplayGame(canvas, gameId);
    } else {
      // Show replay list
      const spectatorReplay = new SpectatorReplay(canvas);
      await spectatorReplay.start();
    }
    return;
  }

  // Check for replay mode (legacy: ?replay=file.jsonl)
  const urlParams = new URLSearchParams(window.location.search);
  const replayFile = urlParams.get('replay');
  if (replayFile) {
    let replay: ReplayPlayer;
    try {
      replay = new ReplayPlayer(canvas);
      await replay.loadReplay(replayFile);
      await replay.start();
      // Small no-server test hook (read by frontend/e2e/webgpu.spec.ts) so the
      // replay path can assert renderer.lastRenderError without a game server.
      (window as unknown as { __replayRenderError?: () => string | null }).__replayRenderError =
        () => replay.renderError;
    } catch (err) {
      showErrorMessage(
        `Failed to load replay: ${err instanceof Error ? err.message : 'Unknown error'}`,
      );
    }
    return;
  }

  // Normal game page
  if (!canvas) {
    console.error('Game canvas not found');
    return;
  }

  // Check WebGPU availability
  if (!navigator.gpu) {
    showErrorMessage(
      'WebGPU is not supported in your browser. Please use a browser that supports WebGPU (Chrome 113+, Edge 113+, or Firefox Nightly with WebGPU enabled).',
    );
    return;
  }

  // Show loading overlay during WebGPU initialization
  showLoading('Initializing graphics...');

  let device: GPUDevice;
  try {
    const adapter = await navigator.gpu.requestAdapter({ powerPreference: 'high-performance' });
    if (!adapter) {
      throw new Error('No WebGPU adapter available. Your device may not support WebGPU.');
    }
    device = await adapter.requestDevice();
  } catch (err) {
    hideLoading();
    showErrorMessage(
      `Failed to initialize WebGPU: ${err instanceof Error ? err.message : 'Unknown error'}`,
    );
    return;
  }

  let renderer: Renderer;
  try {
    renderer = new Renderer(canvas, device);
  } catch (err) {
    hideLoading();
    showErrorMessage(
      `Failed to initialize renderer: ${err instanceof Error ? err.message : 'Unknown error'}`,
    );
    return;
  }

  // Hide loading overlay now that WebGPU is initialized
  hideLoading();

  // Expose the renderer to the debug overlay (for surfacing WebGPU render errors).
  debugRenderer = renderer;

  // Resize canvas when window is resized
  window.addEventListener('resize', () => renderer.resizeCanvas());

  const gameState = new GameState();

  // Initialize UI modules
  const spectator = new Spectator();
  const advisor = new Advisor();
  advisor.setupToggle(); // adds the toggle button to the stats panel

  // Connection needs references to all UI modules
  // lobbyUI passed as null to break circular dependency; set after creation
  const connection = new Connection(gameState, renderer, spectator, advisor, null);

  // Establish WebSocket connection to the server
  connection.connect();

  const lobbyUI = new LobbyUI(connection);
  connection.setLobbyUI(lobbyUI);

  const unitInfo = new UnitInfoUI(gameState, connection);
  const controls = new Controls(canvas, renderer.camera, renderer, connection, gameState, unitInfo);

  // Setup keyboard controls
  setupKeyboardControls(canvas, renderer.camera, renderer, controls);

  // Listen for advisor toggle events
  window.addEventListener('advisor-toggle', (event: Event) => {
    const detail = (event as CustomEvent).detail;
    connection.send({
      ToggleAdvisor: { enabled: detail.enabled },
    });
  });

  // Throttle city label updates to ~4fps to avoid excessive DOM operations
  let lastCityLabelUpdate = 0;

  // Throttle stats panel updates to ~10fps
  let lastStatsUpdate = 0;

  // Stats panel — show funds and unit count
  const statsPanel = document.getElementById('stats-panel');
  if (statsPanel) statsPanel.classList.remove('hidden');

  // Game loop
  renderer.setUpdateCallback(() => {
    renderer.render(gameState);

    // Update stats panel at ~10fps to avoid excessive DOM writes
    const now = performance.now();
    if (statsPanel && now - lastStatsUpdate >= STATS_PANEL_UPDATE_INTERVAL) {
      statsPanel.innerHTML = `
        <div style="margin-bottom:4px;">💰 Funds: <strong>${gameState.player.funds.toLocaleString()}</strong></div>
        <div>⚔️ Units: <strong>${gameState.units.size}</strong></div>
      `;
      lastStatsUpdate = now;
    }

    if (now - lastCityLabelUpdate >= CITY_LABEL_UPDATE_INTERVAL) {
      updateCityLabels(gameState, renderer.camera, canvas);
      lastCityLabelUpdate = now;
    }
    // Refresh the unit info panel at ~10fps to stay in sync with delta updates
    if (now - lastStatsUpdate >= STATS_PANEL_UPDATE_INTERVAL) {
      unitInfo.refresh();
    }
  });

  renderer.startRenderLoop();
}

// Connect to the replay WebSocket and render game, reusing the normal Connection flow
// as a read-only spectator (no lobby, no controls). This gives the replay full parity with
// the live path: enum terrain, camera centering, unit construction, fog/border dirty flags,
// delta handling, and GameOver — all handled by Connection.
async function connectReplayGame(canvas: HTMLCanvasElement, gameId: string) {
  const adapter = await navigator.gpu!.requestAdapter({ powerPreference: 'high-performance' });
  const device = await adapter!.requestDevice();
  const renderer = new Renderer(canvas, device);
  const gameState = new GameState();

  // Resize canvas when window is resized
  window.addEventListener('resize', () => renderer.resizeCanvas());
  debugRenderer = renderer;
  const spectator = new Spectator();
  const advisor = new Advisor();
  const connection = new Connection(gameState, renderer, spectator, advisor, null, true);
  connection.connectTo(`/ws/replay?game_id=${encodeURIComponent(gameId)}`);

  // Render loop
  renderer.setUpdateCallback(() => {
    renderer.render(gameState);
  });
  renderer.startRenderLoop();
}

/** Update city level labels positioned over the game canvas */
function updateCityLabels(
  gameState: GameState,
  camera: { x: number; y: number; zoom: number },
  canvas: HTMLCanvasElement,
) {
  const container = document.getElementById('city-labels');
  if (!container) return;

  const rect = canvas.getBoundingClientRect();
  const cellSize = CELL_SIZE;

  // Build a set of city IDs that should be visible
  const visibleIds = new Set<string>();

  for (const [id, city] of gameState.cities) {
    visibleIds.add(id);

    // Get or create label element
    let label = document.getElementById(`city-label-${id}`) as HTMLDivElement;
    if (!label) {
      label = document.createElement('div');
      label.id = `city-label-${id}`;
      label.style.cssText = `
        position: absolute;
        background: rgba(0,0,0,0.7);
        color: #fff;
        font-size: 10px;
        font-weight: bold;
        padding: 1px 4px;
        border-radius: 3px;
        pointer-events: none;
        transform: translate(-50%, -50%);
      `;
      container.appendChild(label);
    }

    // Convert game position to screen position (world-space → canvas pixels)
    const worldX = city.position[0] * cellSize;
    const worldY = city.position[1] * cellSize;
    const screenX = ((worldX - camera.x) * (camera.zoom / canvas.width) + 0.5) * rect.width;
    const screenY = (0.5 + (worldY - camera.y) * (camera.zoom / canvas.height)) * rect.height;

    // Check if on screen
    const onScreen =
      screenX > -50 && screenX < rect.width + 50 && screenY > -50 && screenY < rect.height + 50;

    if (!onScreen) {
      label.style.display = 'none';
      continue;
    }

    const level = city.level ?? 1;
    label.style.display = 'block';
    label.style.left = `${screenX}px`;
    label.style.top = `${screenY - 8}px`;
    label.textContent = `${level}`;

    // Color based on level
    if (level >= 10) label.style.color = '#ff0';
    else if (level >= 5) label.style.color = '#4f4';
    else label.style.color = '#fff';
  }

  // Remove labels for cities that no longer exist
  for (const child of container.children) {
    const el = child as HTMLDivElement;
    const cityId = el.id.replace('city-label-', '');
    if (!visibleIds.has(cityId)) {
      container.removeChild(el);
    }
  }
}

// ============================================================================
// Debug Overlay — polls /debug/stats and displays performance metrics
// Only active if the server supports debug endpoints (debug builds)
// ============================================================================
let debugOverlayInterval: ReturnType<typeof setInterval> | null = null;
let frameCount = 0;
let lastFpsTime = performance.now();

// Renderer whose WebGPU errors are surfaced via the debug overlay (set once initialized).
let debugRenderer: Renderer | null = null;

// Module-level state for loading style injection
let loadingStyleInjected = false;

interface SystemTiming {
  name: string;
  avg_ms: number;
}

function startDebugOverlay() {
  const overlay = document.getElementById('debug-overlay');
  if (!overlay) return;

  // Poll for debug stats every 500ms
  debugOverlayInterval = setInterval(async () => {
    try {
      const resp = await fetch('/debug/stats');
      if (!resp.ok) {
        // Endpoint not available (release build), hide overlay
        overlay.classList.add('hidden');
        if (debugOverlayInterval) {
          clearInterval(debugOverlayInterval);
          debugOverlayInterval = null;
        }
        return;
      }

      const data = await resp.json();
      overlay.classList.remove('hidden');

      // Update stats from server
      const tickEl = document.getElementById('debug-tick');
      const lastTickEl = document.getElementById('debug-last-tick');
      const avgTickEl = document.getElementById('debug-avg-tick');
      const entitiesEl = document.getElementById('debug-entities');
      const archetypesEl = document.getElementById('debug-archetypes');
      const gamesEl = document.getElementById('debug-games');
      const fpsEl = document.getElementById('debug-fps');

      if (tickEl) tickEl.textContent = data.tick_count ?? '-';
      if (lastTickEl) lastTickEl.textContent = (data.last_tick_ms ?? 0).toFixed(2) + 'ms';
      if (avgTickEl) avgTickEl.textContent = (data.avg_tick_ms ?? 0).toFixed(2) + 'ms';
      if (entitiesEl) entitiesEl.textContent = data.entity_count ?? '-';
      if (archetypesEl) archetypesEl.textContent = data.archetype_count ?? '-';
      if (gamesEl) gamesEl.textContent = data.active_games ?? '-';

      // Update system timings
      const systemsDiv = document.getElementById('debug-systems');
      if (systemsDiv && data.system_timings && Array.isArray(data.system_timings)) {
        // Sort by avg_ms descending
        const sorted = [...data.system_timings].sort(
          (a: SystemTiming, b: SystemTiming) => (b.avg_ms ?? 0) - (a.avg_ms ?? 0),
        );
        systemsDiv.innerHTML = sorted
          .slice(0, 10)
          .map(
            (s: SystemTiming) =>
              `<div class="stat-row" style="font-size:11px">` +
              `<span class="stat-label">${s.name ?? '?'}</span>` +
              `<span class="stat-value">${(s.avg_ms ?? 0).toFixed(3)}ms</span>` +
              `</div>`,
          )
          .join('');
      }

      // Calculate client-side FPS
      frameCount++;
      const now = performance.now();
      if (now - lastFpsTime >= 1000) {
        if (fpsEl) fpsEl.textContent = frameCount.toString();
        frameCount = 0;
        lastFpsTime = now;
      }

      // Surface any WebGPU render error via the debug overlay (reuses debug-overlay feature).
      const renderErrEl = document.getElementById('debug-render-error');
      if (renderErrEl) {
        if (debugRenderer?.lastRenderError) {
          renderErrEl.textContent = debugRenderer.lastRenderError;
          renderErrEl.style.color = '#f66';
        } else {
          renderErrEl.textContent = 'ok';
          renderErrEl.style.color = '#4f4';
        }
      }
    } catch {
      // Ignore network errors during polling
    }
  }, 500);
}

main().catch(console.error);
startDebugOverlay();

// Toggle the debug overlay with the backtick key (or Ctrl+F3) so it's usable even
// when the /debug/stats endpoint is unavailable or hidden by default.
window.addEventListener('keydown', (e) => {
  if (e.key === '`' || (e.key === 'F3' && e.ctrlKey)) {
    e.preventDefault();
    const overlay = document.getElementById('debug-overlay');
    if (overlay) overlay.classList.toggle('hidden');
  }
});

// ============================================================================
// Loading & Error Overlays
// ============================================================================

/** Show a loading overlay with the given message */
function showLoading(message: string) {
  // Inject the spin animation style once
  if (!loadingStyleInjected) {
    const style = document.createElement('style');
    style.id = 'loading-spinner-style';
    style.textContent = '@keyframes spin{to{transform:rotate(360deg)}}';
    document.head.appendChild(style);
    loadingStyleInjected = true;
  }

  let overlay = document.getElementById('loading-overlay');
  if (!overlay) {
    overlay = document.createElement('div');
    overlay.id = 'loading-overlay';
    overlay.style.cssText = `
      position: fixed; top: 0; left: 0; width: 100%; height: 100%;
      background: rgba(0, 0, 0, 0.85); display: flex; align-items: center;
      justify-content: center; z-index: 10000; font-size: 18px; color: #fff;
      flex-direction: column; gap: 16px;
    `;
    document.body.appendChild(overlay);
  }
  overlay.innerHTML = `
    <div style="width:40px;height:40px;border:4px solid #555;border-top-color:#4af;border-radius:50%;animation:spin 1s linear infinite;"></div>
    <span>${message}</span>
  `;
}

/** Hide the loading overlay */
function hideLoading() {
  const overlay = document.getElementById('loading-overlay');
  if (overlay) overlay.remove();
  // Clean up the injected style
  const style = document.getElementById('loading-spinner-style');
  if (style) style.remove();
  loadingStyleInjected = false;
}

/** Show an error message overlay */
function showErrorMessage(message: string) {
  let overlay = document.getElementById('error-overlay');
  if (!overlay) {
    overlay = document.createElement('div');
    overlay.id = 'error-overlay';
    overlay.style.cssText = `
      position: fixed; top: 0; left: 0; width: 100%; height: 100%;
      background: rgba(0, 0, 0, 0.9); display: flex; align-items: center;
      justify-content: center; z-index: 10001;
    `;
    document.body.appendChild(overlay);
  }
  overlay.innerHTML = `
    <div style="background:#1a1a2e;border:1px solid #f44;border-radius:12px;padding:32px;max-width:500px;text-align:center;">
      <h2 style="color:#f44;margin:0 0 16px;">⚠️ Error</h2>
      <p style="color:#ccc;line-height:1.5;margin:0;">${message}</p>
    </div>
  `;
}

// ============================================================================
// Keyboard Controls
// ============================================================================

function setupKeyboardControls(
  _canvas: HTMLCanvasElement,
  camera: { x: number; y: number; zoom: number },
  renderer: Renderer,
  controls: Controls,
): () => void {
  const keyState: Record<string, boolean> = {};
  let rafId = 0;

  window.addEventListener('keydown', (e) => {
    // Prevent default for game control keys
    if (['ArrowUp', 'ArrowDown', 'ArrowLeft', 'ArrowRight', ' ', 'Escape'].includes(e.key)) {
      e.preventDefault();
    }
    keyState[e.key] = true;

    // Escape: deselect unit and clear path
    if (e.key === 'Escape') {
      renderer.currentPath = [];
      if (controls.deselect) {
        controls.deselect();
      }
    }

    // +/-/= for zoom
    if (e.key === '+' || e.key === '=') {
      camera.zoom = Math.min(ZOOM_MAX, camera.zoom * ZOOM_INCREMENT);
    } else if (e.key === '-' || e.key === '_') {
      camera.zoom = Math.max(ZOOM_MIN, camera.zoom / ZOOM_INCREMENT);
    }
  });

  window.addEventListener('keyup', (e) => {
    keyState[e.key] = false;
  });

  // Pan with arrow keys — run on each frame via a separate loop
  // Arrow keys pan the viewport in the pressed direction (content follows the key).
  // Pan speed scales with zoom so world-distance per tick stays consistent.
  const panLoop = () => {
    const dz = camera.zoom;
    if (keyState['ArrowLeft']) camera.x -= KEYBOARD_PAN_SPEED / dz;
    if (keyState['ArrowRight']) camera.x += KEYBOARD_PAN_SPEED / dz;
    if (keyState['ArrowUp']) camera.y -= KEYBOARD_PAN_SPEED / dz;
    if (keyState['ArrowDown']) camera.y += KEYBOARD_PAN_SPEED / dz;
    requestAnimationFrame(panLoop);
  };
  rafId = requestAnimationFrame(panLoop);

  // Return cleanup function
  return () => {
    cancelAnimationFrame(rafId);
  };
}
