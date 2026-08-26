//! Shared configuration constants for the game frontend

/** Size of each terrain cell in world units */
export const CELL_SIZE = 32;

/** Unit dot size multiplier */
export const UNIT_DOT_SIZE = 8;

/** Symbol atlas size */
export const SYMBOL_ATLAS_SIZE = 256;
export const SYMBOL_CELL_SIZE = 32;
export const SYMBOLS_PER_ROW = 8;

/** Border line width */
export const BORDER_LINE_WIDTH = 3;

/** Path preview line width */
export const PATH_LINE_WIDTH = 2;

/** Bar (health/org/equip) dimensions */
export const BAR_HEIGHT = 2;
export const BAR_SPACING = 2.5;
export const BAR_WIDTH_MULTIPLIER = 1.6;

/** Camera zoom limits */
export const ZOOM_MIN = 0.3;
export const ZOOM_MAX = 3.0;

/** City hit test radius (in screen units at zoom=1) */
export const CITY_HIT_RADIUS = 10;

/** City dot size in screen pixels (diameter) */
export const CITY_DOT_SIZE = 12;

/** City dot colors by owner index (6 distinct colors for up to 6 players) */
export const CITY_COLORS = [
  { r: 1, g: 0.2, b: 0.2 },   // Red
  { r: 1, g: 1, b: 0.2 },     // Yellow
  { r: 0.2, g: 0.4, b: 1 },   // Blue
  { r: 0.2, g: 0.9, b: 0.3 }, // Green
  { r: 0.7, g: 0.4, b: 0.95 },// Purple
  { r: 1, g: 0.55, b: 0.1 },  // Orange
];

/** Player colors for units (6 distinct colors for up to 6 players) */
export const PLAYER_COLORS = [
  { r: 1, g: 0.2, b: 0.2 },   // Red
  { r: 1, g: 1, b: 0.2 },     // Yellow
  { r: 0.2, g: 0.4, b: 1 },   // Blue
  { r: 0.2, g: 0.9, b: 0.3 }, // Green
  { r: 0.7, g: 0.4, b: 0.95 },// Purple
  { r: 1, g: 0.55, b: 0.1 },  // Orange
];

/** Border color mapping */
export const BORDER_COLORS = [
  { r: 1, g: 0.2, b: 0.2 }, // Red
  { r: 0.2, g: 0.4, b: 1 }, // Blue
];

/** Bar colors */
export const BAR_COLORS = {
  health: { r: 0.2, g: 0.8, b: 0.2 }, // Green
  org: { r: 0.9, g: 0.7, b: 0.1 }, // Yellow
  equip: { r: 0.2, g: 0.4, b: 0.9 }, // Blue
};

/** UI timing constants */
export const CITY_LABEL_UPDATE_INTERVAL = 250; // ms, ~4fps
export const STATS_PANEL_UPDATE_INTERVAL = 100; // ms, ~10fps
export const DEBUG_STATS_POLL_INTERVAL = 500; // ms

/** Connection retry */
export const CONNECTION_INITIAL_RETRY_DELAY = 1000; // ms
export const CONNECTION_MAX_RETRY_DELAY = 30000; // ms (30s)

/** Buffer size estimates (generous, can grow dynamically) */
export const BUFFER_SIZES = {
  terrain: 1024 * 100,
  unit: 1024 * 64,
  symbol: 1024 * 128,
  border: 1024 * 256,
  bar: 1024 * 256,
  city: 1024 * 128,
} as const;

/** City upgrade costs (base_cost * level) */
export const CITY_UPGRADE_BASE_COST = 200;
export const CITY_MAX_LEVEL = 3;

/** Supply bar color thresholds (ordered highest → lowest for iteration) */
export const SUPPLY_THRESHOLDS = [
  { threshold: 80, color: '#4f4' },
  { threshold: 50, color: '#ff4' },
  { threshold: 25, color: '#fa4' },
  { threshold: 0, color: '#f44' },
] as const;

/** Keyboard pan speed */
export const KEYBOARD_PAN_SPEED = 5;

/** Pan speed multiplier for mouse/trackpad panning */
export const PAN_SPEED_MULTIPLIER = 1.0;

/** Zoom increment */
export const ZOOM_INCREMENT = 1.2;

/** Water unit indicator colors */
export const WATER_INDICATOR_COLOR = 'rgba(50, 100, 200, 0.4)';
export const WATER_BORDER_COLOR = '#4af';
export const EMBARK_COLOR = '#ff4';

/** Own-side outline (halo) rendering */
export const OUTLINE_EXPAND = 3.0; // world units, zoom-independent
/** Outline quad fragment alpha */
export const OUTLINE_ALPHA = 0.6;
/** Outline color — bright white so it reads against any terrain */
export const OUTLINE_COLOR = { r: 1, g: 1, b: 1 };
