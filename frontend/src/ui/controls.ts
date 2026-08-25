//! Camera controls, unit selection, and path drawing UI

import { Renderer } from '../game/renderer';
import { Connection } from '../net/connection';
import { GameState } from '../game/game_state';
import { UnitInfoUI } from './unit_info';
import { CELL_SIZE, UNIT_DOT_SIZE, ZOOM_MIN, ZOOM_MAX, CITY_HIT_RADIUS } from '../game/config';

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

  private mouseToGameCoords(clientX: number, clientY: number): { x: number; y: number } {
    const rect = this.canvasRect ?? this.refreshRect();
    // Mouse CSS pixel → NDC → world-space → grid cell
    const ndcX = ((clientX - rect.left) / rect.width) * 2 - 1;
    const ndcY = 1 - ((clientY - rect.top) / rect.height) * 2;
    // Inverse of shader: worldX = camera.x + ndcX * canvas.width / (2*zoom)
    const worldX = this.camera.x + (ndcX * this.canvas.width) / (2 * this.camera.zoom);
    const worldY = this.camera.y - (ndcY * this.canvas.height) / (2 * this.camera.zoom);
    const posGameX = worldX / CELL_SIZE;
    const posGameY = worldY / CELL_SIZE;
    return { x: Math.floor(posGameX), y: Math.floor(posGameY) };
  }

  private hitTestUnit(clientX: number, clientY: number): string | null {
    // Convert mouse to world-space
    const rect = this.canvasRect ?? this.refreshRect();
    const ndcX = ((clientX - rect.left) / rect.width) * 2 - 1;
    const ndcY = 1 - ((clientY - rect.top) / rect.height) * 2;
    // Inverse of shader: worldX = camera.x + ndcX * canvas.width / (2*zoom)
    const worldX = this.camera.x + (ndcX * this.canvas.width) / (2 * this.camera.zoom);
    const worldY = this.camera.y - (ndcY * this.canvas.height) / (2 * this.camera.zoom);
    const hitSize = UNIT_DOT_SIZE * this.camera.zoom;

    for (const unit of this.gameState.units.values()) {
      const uwx = unit.position.x * CELL_SIZE;
      const uwy = unit.position.y * CELL_SIZE;
      if (Math.abs(worldX - uwx) < hitSize && Math.abs(worldY - uwy) < hitSize) {
        return unit.id;
      }
    }
    return null;
  }

  private hitTestCity(clientX: number, clientY: number): string | null {
    // Convert mouse to world-space
    const rect = this.canvasRect ?? this.refreshRect();
    const ndcX = ((clientX - rect.left) / rect.width) * 2 - 1;
    const ndcY = 1 - ((clientY - rect.top) / rect.height) * 2;
    // Inverse of shader: worldX = camera.x + ndcX * canvas.width / (2*zoom)
    const worldX = this.camera.x + (ndcX * this.canvas.width) / (2 * this.camera.zoom);
    const worldY = this.camera.y - (ndcY * this.canvas.height) / (2 * this.camera.zoom);
    const hitSize = CITY_HIT_RADIUS * this.camera.zoom;

    for (const [id, city] of this.gameState.cities) {
      const cwx = city.position[0] * CELL_SIZE;
      const cwy = city.position[1] * CELL_SIZE;
      if (Math.abs(worldX - cwx) < hitSize && Math.abs(worldY - cwy) < hitSize) {
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
        this.camera.x -= (dx * scaleX) / this.camera.zoom;
        this.camera.y -= (dy * scaleY) / this.camera.zoom;
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

  private setupEventListeners() {
    window.addEventListener('resize', () => this.refreshRect());

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

    this.canvas.addEventListener('pointermove', (e) => {
      if (this.isDragging) {
        // Suppress browser gestures during a captured right-button drag
        // (e.g. back/forward navigation, scroll, touch gestures).
        e.preventDefault();
        this.lastPointer = { x: e.clientX, y: e.clientY };
        // Re-capture if the browser dropped it (e.g. after a focus/OS event)
        // so tracking never silently "loses the mouse" mid-drag.
        if (!this.canvas.hasPointerCapture(e.pointerId)) {
          try {
            this.canvas.setPointerCapture(e.pointerId);
          } catch {
            // ignore — move tracking still works while the pointer is inside
          }
        }
      } else if (this.renderer.currentPath.length > 0) {
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
      } else if (e.button === 2) {
        this.endPan();
        if (this.canvas.hasPointerCapture(e.pointerId)) {
          this.canvas.releasePointerCapture(e.pointerId);
        }
      }
    });

    this.canvas.addEventListener('pointercancel', (e) => {
      this.endPan();
      if (this.canvas.hasPointerCapture(e.pointerId)) {
        this.canvas.releasePointerCapture(e.pointerId);
      }
    });

    this.canvas.addEventListener('wheel', (e) => {
      e.preventDefault();
      const zoomDelta = e.deltaY > 0 ? 0.9 : 1.1;
      this.camera.zoom *= zoomDelta;
      this.camera.zoom = Math.max(ZOOM_MIN, Math.min(ZOOM_MAX, this.camera.zoom));
    });

    this.canvas.addEventListener('contextmenu', (e) => e.preventDefault());
  }

  /** Deselect any selected unit/city and clear the path */
  deselect(): void {
    this.selectedUnitId = null;
    this.renderer.currentPath = [];
    this.unitInfo.selectUnit(null);
    this.unitInfo.selectCity(null);
  }
}
