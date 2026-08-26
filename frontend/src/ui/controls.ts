//! Camera controls, unit selection, and path drawing UI

import { Renderer } from '../game/renderer';
import { Connection } from '../net/connection';
import { GameState } from '../game/game_state';
import { UnitInfoUI } from './unit_info';
import { CELL_SIZE, UNIT_DOT_SIZE, ZOOM_MIN, ZOOM_MAX, CITY_HIT_RADIUS, PAN_SPEED_MULTIPLIER } from '../game/config';

export class Controls {
  private canvas: HTMLCanvasElement;
  private camera: { x: number; y: number; zoom: number };
  private renderer: Renderer;
  private connection: Connection;
  private gameState: GameState;
  private unitInfo: UnitInfoUI;

  private isDragging = false;
  private rightDragStart = { x: 0, y: 0 };
  private lastPointer: { x: number; y: number } | null = null;
  private canvasRect: DOMRect | null = null;
  private panFrameId: number | null = null;
  private selectedUnitId: string | null = null;
  private onWindowPointerMove: ((e: PointerEvent) => void) | null = null;
  private onWindowPointerUp: ((e: PointerEvent) => void) | null = null;

  constructor(
    canvas: HTMLCanvasElement,
    camera: { x: number; y: number; zoom: number },
    renderer: Renderer,
    connection: Connection,
    gameState: GameState,
    unitInfo: UnitInfoUI,
  ) {
    this.canvas = canvas;
    this.camera = camera;
    this.renderer = renderer;
    this.connection = connection;
    this.gameState = gameState;
    this.unitInfo = unitInfo;
    this.setupEventListeners();
  }

  private refreshRect(): DOMRect {
    this.canvasRect = this.canvas.getBoundingClientRect();
    return this.canvasRect;
  }

  /** Convert screen (client) coordinates to world-space coordinates (no floor). */
  private screenToWorld(clientX: number, clientY: number): { x: number; y: number } {
    const rect = this.canvasRect ?? this.refreshRect();
    const ndcX = ((clientX - rect.left) / rect.width) * 2 - 1;
    const ndcY = 1 - ((clientY - rect.top) / rect.height) * 2;
    const worldX = this.camera.x + (ndcX * this.canvas.width) / (2 * this.camera.zoom);
    const worldY = this.camera.y - (ndcY * this.canvas.height) / (2 * this.camera.zoom);
    return { x: worldX, y: worldY };
  }

  private mouseToGameCoords(clientX: number, clientY: number): { x: number; y: number } {
    const world = this.screenToWorld(clientX, clientY);
    return { x: Math.floor(world.x / CELL_SIZE), y: Math.floor(world.y / CELL_SIZE) };
  }

  private hitTestUnit(clientX: number, clientY: number): string | null {
    const world = this.screenToWorld(clientX, clientY);
    const hitSize = UNIT_DOT_SIZE / 2;

    for (const unit of this.gameState.units.values()) {
      const uwx = unit.position.x * CELL_SIZE;
      const uwy = unit.position.y * CELL_SIZE;
      if (Math.abs(world.x - uwx) < hitSize && Math.abs(world.y - uwy) < hitSize) {
        return unit.id;
      }
    }
    return null;
  }

  private hitTestCity(clientX: number, clientY: number): string | null {
    const world = this.screenToWorld(clientX, clientY);
    const hitSize = CITY_HIT_RADIUS;

    for (const [id, city] of this.gameState.cities) {
      const cwx = city.position[0] * CELL_SIZE;
      const cwy = city.position[1] * CELL_SIZE;
      if (Math.abs(world.x - cwx) < hitSize && Math.abs(world.y - cwy) < hitSize) {
        return id;
      }
    }
    return null;
  }

  /** rAF loop that applies the pending camera pan once per frame. */
  private startPanLoop(): void {
    if (this.panFrameId !== null) return;
    const step = () => {
      if (this.isDragging && this.lastPointer) {
        const rect = this.canvasRect ?? this.refreshRect();
        const dx = this.lastPointer.x - this.rightDragStart.x;
        const dy = this.lastPointer.y - this.rightDragStart.y;
        // Camera moves opposite the drag; world units per CSS px = scalePx / zoom
        const scaleX = this.canvas.width / rect.width;
        const scaleY = this.canvas.height / rect.height;
        this.camera.x -= ((dx * scaleX) / this.camera.zoom) * PAN_SPEED_MULTIPLIER;
        this.camera.y -= ((dy * scaleY) / this.camera.zoom) * PAN_SPEED_MULTIPLIER;
        this.rightDragStart = { x: this.lastPointer.x, y: this.lastPointer.y };
      }
      this.panFrameId = requestAnimationFrame(step);
    };
    this.panFrameId = requestAnimationFrame(step);
  }

  private endPan(): void {
    if (this.panFrameId !== null) {
      cancelAnimationFrame(this.panFrameId);
      this.panFrameId = null;
    }
    this.isDragging = false;
    this.lastPointer = null;
  }

  /** Install window-level listeners for right-button pan tracking. */
  private attachWindowPanListeners(): void {
    if (this.onWindowPointerMove || this.onWindowPointerUp) return;

    this.onWindowPointerMove = (e: PointerEvent) => {
      // Re-arm pan if pointercancel was received but button is still held
      if (!this.isDragging && (e.buttons & 2)) {
        this.isDragging = true;
        this.rightDragStart = { x: e.clientX, y: e.clientY };
        this.lastPointer = { x: e.clientX, y: e.clientY };
        this.startPanLoop();
      }
      if (this.isDragging) {
        this.lastPointer = { x: e.clientX, y: e.clientY };
      }
    };

    this.onWindowPointerUp = (e: PointerEvent) => {
      if (e.button === 2) {
        this.endPan();
        if (this.canvas.hasPointerCapture(e.pointerId)) {
          this.canvas.releasePointerCapture(e.pointerId);
        }
      }
    };

    window.addEventListener('pointermove', this.onWindowPointerMove);
    window.addEventListener('pointerup', this.onWindowPointerUp);
  }

  /** Remove window-level pan listeners (for cleanup). */
  private detachWindowPanListeners(): void {
    if (this.onWindowPointerMove) {
      window.removeEventListener('pointermove', this.onWindowPointerMove);
      this.onWindowPointerMove = null;
    }
    if (this.onWindowPointerUp) {
      window.removeEventListener('pointerup', this.onWindowPointerUp);
      this.onWindowPointerUp = null;
    }
  }

  /** Zoom-to-cursor: zoom while keeping the point under the cursor fixed. */
  private zoomToCursor(clientX: number, clientY: number, zoomDelta: number): void {
    // Capture the world point under the cursor before zooming
    const worldBefore = this.screenToWorld(clientX, clientY);

    // Apply zoom
    this.camera.zoom *= zoomDelta;
    this.camera.zoom = Math.max(ZOOM_MIN, Math.min(ZOOM_MAX, this.camera.zoom));

    // Recompute NDC for the cursor position
    const rect = this.canvasRect ?? this.refreshRect();
    const ndcX = ((clientX - rect.left) / rect.width) * 2 - 1;
    const ndcY = 1 - ((clientY - rect.top) / rect.height) * 2;

    // Re-aim camera so the same world point stays under the cursor
    this.camera.x = worldBefore.x - (ndcX * this.canvas.width) / (2 * this.camera.zoom);
    this.camera.y = worldBefore.y + (ndcY * this.canvas.height) / (2 * this.camera.zoom);
  }

  private setupEventListeners() {
    window.addEventListener('resize', () => this.refreshRect());
    this.attachWindowPanListeners();

    this.canvas.addEventListener('pointerdown', (e) => {
      this.refreshRect();
      if (e.button === 0) {
        // First check if we clicked on a unit to select it
        const unitId = this.hitTestUnit(e.clientX, e.clientY);
        if (unitId) {
          this.selectedUnitId = unitId;
          this.renderer.currentPath = [];
          this.unitInfo.selectUnit(unitId);
          if (location.hostname === 'localhost' || location.hostname === '127.0.0.1') {
            console.log('Selected unit:', unitId);
          }
        } else {
          // Check if we clicked on a city
          const cityId = this.hitTestCity(e.clientX, e.clientY);
          if (cityId) {
            this.selectedUnitId = null;
            this.renderer.currentPath = [];
            this.unitInfo.selectCity(cityId);
            if (location.hostname === 'localhost' || location.hostname === '127.0.0.1') {
              console.log('Selected city:', cityId);
            }
          } else {
            // Deselect and start a new path from clicked position
            this.selectedUnitId = null;
            this.unitInfo.selectUnit(null);
            this.unitInfo.selectCity(null);
            const gamePos = this.mouseToGameCoords(e.clientX, e.clientY);
            this.renderer.currentPath = [gamePos];
          }
        }
      } else if (e.button === 2) {
        e.preventDefault();
        this.isDragging = true;
        this.rightDragStart = { x: e.clientX, y: e.clientY };
        this.lastPointer = { x: e.clientX, y: e.clientY };
        // Capture the pointer so the right-click+drag never triggers browser gestures
        // (e.g. back/forward navigation) and keeps tracking outside the canvas.
        try {
          this.canvas.setPointerCapture(e.pointerId);
        } catch {
          // pointer no longer active — fall back to plain move tracking
        }
        this.startPanLoop();
      }
    });

    // Canvas pointermove handles path drawing for button 0 only;
    // button-2 pan movement is handled on window (see attachWindowPanListeners)
    this.canvas.addEventListener('pointermove', (e) => {
      if (!this.isDragging && this.renderer.currentPath.length > 0) {
        const gamePos = this.mouseToGameCoords(e.clientX, e.clientY);
        this.renderer.currentPath.push(gamePos);
      }
    });

    this.canvas.addEventListener('pointerup', (e) => {
      if (e.button === 0 && this.renderer.currentPath.length > 0 && this.selectedUnitId) {
        const path = this.renderer.currentPath;
        this.renderer.currentPath = [];
        if (path.length > 0) {
          this.connection.send({
            MoveUnit: {
              unitId: this.selectedUnitId,
              path: path,
            },
          });
        }
        this.unitInfo.selectUnit(null);
      }
      // button-2 pointerup is handled on window
    });

    // W9 fix: pointercancel should NOT end the pan.
    // Just release capture; the window-level pointermove with buttons&2 will re-arm.
    this.canvas.addEventListener('pointercancel', (e) => {
      if (this.canvas.hasPointerCapture(e.pointerId)) {
        this.canvas.releasePointerCapture(e.pointerId);
      }
      // Do NOT call endPan() — the window pointermove re-arms if button still held
    });

    this.canvas.addEventListener('wheel', (e) => {
      e.preventDefault();
      const zoomDelta = e.deltaY > 0 ? 0.9 : 1.1;
      this.zoomToCursor(e.clientX, e.clientY, zoomDelta);
    });

    this.canvas.addEventListener('contextmenu', (e) => e.preventDefault());
  }

  /** Cleanup — remove window-level listeners. Call on component unmount. */
  destroy(): void {
    this.detachWindowPanListeners();
    this.endPan();
  }

  /** Deselect any selected unit/city and clear the path */
  deselect(): void {
    this.selectedUnitId = null;
    this.renderer.currentPath = [];
    this.unitInfo.selectUnit(null);
    this.unitInfo.selectCity(null);
  }
}
