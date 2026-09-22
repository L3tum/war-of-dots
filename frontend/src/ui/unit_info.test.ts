//! UnitInfoUI tests — unit/city info panels, action buttons and their wire
//! messages, upgrade gating (level/funds), and per-frame refresh sync.

import { describe, it, expect, beforeEach, afterEach, vi } from 'vitest';
import { GameState } from '../game/game_state';
import { Unit } from '../game/units';
import { UnitInfoUI } from './unit_info';
import type { CityState, UnitState } from '../generated/protocol';
import type { Connection } from '../net/connection';

/** Minimal DOM mirroring the ids unit_info.ts reaches for in index.html. */
function mountUnitInfoDom() {
  const root = document.createElement('div');
  root.innerHTML = `
    <div id="unit-info-panel" class="panel hidden">
      <h3 id="unit-info-title"></h3>
      <div id="unit-info-details"></div>
      <div id="unit-info-actions"></div>
    </div>
    <div id="city-info-panel" class="panel hidden">
      <h3 id="city-info-title"></h3>
      <div id="city-info-details"></div>
      <div id="city-info-actions"></div>
    </div>
  `;
  document.body.appendChild(root);
  return root;
}

function makeUnitState(overrides: Partial<UnitState> = {}): UnitState {
  const equipment = [{ name: 'rifle', cost_per_item: 2, count: 10 }];
  return {
    id: 'u1',
    owner: 'player_0',
    position: [5, 7],
    unit_type: 'infantry',
    symbol: 'S',
    men: 80,
    max_men: 100,
    organisation: 60,
    equipment,
    max_equipment: equipment,
    path: [],
    healing: false,
    resupply_enabled: true,
    supply_percent: 100,
    encircled: false,
    attacking: false,
    on_water: false,
    boat_supply_percent: 0,
    embarking: false,
    disembarking: false,
    ...overrides,
  };
}

function makeCityState(overrides: Partial<CityState> = {}): CityState {
  return {
    id: 'c1',
    position: [10, 12],
    owner: 'player_0',
    manpower: 1000,
    level: 1,
    ...overrides,
  };
}

interface Harness {
  ui: UnitInfoUI;
  gameState: GameState;
  send: ReturnType<typeof vi.fn>;
}

function makeHarness(): Harness {
  const gameState = new GameState();
  const send = vi.fn();
  const connection = { send } as unknown as Connection;
  const ui = new UnitInfoUI(gameState, connection);
  return { ui, gameState, send };
}

function unitPanel() {
  return document.getElementById('unit-info-panel')!;
}

function cityPanel() {
  return document.getElementById('city-info-panel')!;
}

function clickUnitAction(id: string) {
  const btn = document.getElementById(id) as HTMLButtonElement | null;
  expect(btn, `button #${id} should exist`).toBeTruthy();
  btn!.click();
}

describe('UnitInfoUI — unit panel', () => {
  beforeEach(() => {
    mountUnitInfoDom();
  });

  afterEach(() => {
    document.body.innerHTML = '';
    vi.restoreAllMocks();
  });

  it('selectUnit shows the panel with men/org/supply and action buttons', () => {
    const h = makeHarness();
    const state = makeUnitState();
    h.gameState.units.set(state.id, new Unit(state));

    h.ui.selectUnit('u1');

    expect(unitPanel().classList.contains('hidden')).toBe(false);
    expect(document.getElementById('unit-info-title')!.textContent).toBe('S infantry');
    const details = document.getElementById('unit-info-details')!.textContent!;
    expect(details).toContain('Men: 80/100');
    expect(details).toContain('Org: 60%');
    expect(details).toContain('Supply: 100%');
    // Default action buttons are present.
    expect(document.getElementById('toggle-healing-btn')).toBeTruthy();
    expect(document.getElementById('toggle-resupply-btn')).toBeTruthy();
    // Not on water → no disembark button.
    expect(document.getElementById('disembark-btn')).toBeNull();
  });

  it('heal button sends ToggleHealing with the inverted flag', () => {
    const h = makeHarness();
    const unit = new Unit(makeUnitState({ healing: false }));
    h.gameState.units.set('u1', unit);
    h.ui.selectUnit('u1');

    clickUnitAction('toggle-healing-btn');
    expect(h.send).toHaveBeenCalledWith({
      ToggleHealing: { unit_id: 'u1', enabled: true },
    });
    // The click re-renders from the (still unchanged) local state; the label
    // flips once the server's delta confirms the toggle. Simulate that delta,
    // then the per-frame refresh must pick it up.
    unit.healing = true;
    h.ui.refresh();
    expect(document.getElementById('toggle-healing-btn')!.textContent).toContain('Stop Healing');
  });

  it('resupply button sends ToggleResupply with the inverted flag', () => {
    const h = makeHarness();
    h.gameState.units.set('u1', new Unit(makeUnitState({ resupply_enabled: true })));
    h.ui.selectUnit('u1');

    clickUnitAction('toggle-resupply-btn');
    expect(h.send).toHaveBeenCalledWith({
      ToggleResupply: { unitId: 'u1', enabled: false },
    });
  });

  it('on-water unit shows a disembark button that sends DisembarkUnit', () => {
    const h = makeHarness();
    h.gameState.units.set(
      'u1',
      new Unit(makeUnitState({ on_water: true, boat_supply_percent: 55, disembarking: false })),
    );
    h.ui.selectUnit('u1');

    // On-water state is surfaced in the details.
    expect(document.getElementById('unit-info-details')!.textContent).toContain('On Boat');
    clickUnitAction('disembark-btn');
    expect(h.send).toHaveBeenCalledWith({ DisembarkUnit: { unitId: 'u1' } });
  });

  it('a unit that is already disembarking does not offer the button again', () => {
    const h = makeHarness();
    h.gameState.units.set(
      'u1',
      new Unit(makeUnitState({ on_water: true, disembarking: true })),
    );
    h.ui.selectUnit('u1');

    expect(document.getElementById('disembark-btn')).toBeNull();
    expect(document.getElementById('unit-info-details')!.textContent).toContain('Disembarking');
  });

  it('selectUnit(null) hides the panel', () => {
    const h = makeHarness();
    h.gameState.units.set('u1', new Unit(makeUnitState()));
    h.ui.selectUnit('u1');
    expect(unitPanel().classList.contains('hidden')).toBe(false);

    h.ui.selectUnit(null);
    expect(unitPanel().classList.contains('hidden')).toBe(true);
  });

  it('refresh() re-renders live state and hides the panel when the unit is removed', () => {
    const h = makeHarness();
    const state = makeUnitState({ men: 80 });
    h.gameState.units.set('u1', new Unit(state));
    h.ui.selectUnit('u1');

    // Delta changes the unit in place; refresh must pick it up.
    const unit = h.gameState.units.get('u1')!;
    unit.men = 42;
    unit.supplyPercent = 17;
    h.ui.refresh();
    const details = document.getElementById('unit-info-details')!.textContent!;
    expect(details).toContain('Men: 42/100');
    expect(details).toContain('Supply: 17%');

    // Unit destroyed → panel hides itself.
    h.gameState.units.delete('u1');
    h.ui.refresh();
    expect(unitPanel().classList.contains('hidden')).toBe(true);
  });
});

describe('UnitInfoUI — city panel', () => {
  beforeEach(() => {
    mountUnitInfoDom();
  });

  afterEach(() => {
    document.body.innerHTML = '';
    vi.restoreAllMocks();
  });

  it('selectCity shows the panel with position/manpower/level and an upgrade button', () => {
    const h = makeHarness();
    h.gameState.player.funds = 1000;
    h.gameState.cities.set('c1', makeCityState({ level: 1 }));

    h.ui.selectCity('c1');

    expect(cityPanel().classList.contains('hidden')).toBe(false);
    expect(document.getElementById('city-info-title')!.textContent).toBe('City c1');
    const details = document.getElementById('city-info-details')!.textContent!;
    expect(details).toContain('Manpower: 1000');
    expect(details).toContain('Level: 1/3');
    // base_cost(200) * next_level(2) = 400; funds 1000 → enabled.
    const btn = document.getElementById('upgrade-city-btn') as HTMLButtonElement;
    expect(btn).toBeTruthy();
    expect(btn.disabled).toBe(false);
    expect(btn.textContent).toContain('Upgrade to Lv.2');
    expect(btn.textContent).toContain('400');
  });

  it('upgrade click sends UpgradeCity', () => {
    const h = makeHarness();
    h.gameState.player.funds = 1000;
    h.gameState.cities.set('c1', makeCityState({ level: 1 }));
    h.ui.selectCity('c1');

    (document.getElementById('upgrade-city-btn') as HTMLButtonElement).click();
    expect(h.send).toHaveBeenCalledWith({ UpgradeCity: { cityId: 'c1' } });
  });

  it('upgrade button is disabled when the player cannot afford it', () => {
    const h = makeHarness();
    h.gameState.player.funds = 100; // < 200 * 2 = 400
    h.gameState.cities.set('c1', makeCityState({ level: 1 }));
    h.ui.selectCity('c1');

    const btn = document.getElementById('upgrade-city-btn') as HTMLButtonElement;
    expect(btn.disabled).toBe(true);
  });

  it('max-level city shows the max-level note instead of a button', () => {
    const h = makeHarness();
    h.gameState.player.funds = 1_000_000;
    h.gameState.cities.set('c1', makeCityState({ level: 3 }));
    h.ui.selectCity('c1');

    expect(document.getElementById('upgrade-city-btn')).toBeNull();
    expect(document.getElementById('city-info-actions')!.textContent).toContain('Max Level');
  });

  it('unowned city shows the not-owned note instead of a button', () => {
    const h = makeHarness();
    h.gameState.player.funds = 1_000_000;
    h.gameState.cities.set('c1', makeCityState({ level: 1, owner: null }));
    h.ui.selectCity('c1');

    expect(document.getElementById('upgrade-city-btn')).toBeNull();
    expect(document.getElementById('city-info-details')!.textContent).toContain('Unowned');
    expect(document.getElementById('city-info-actions')!.textContent).toContain('Not owned');
  });

  it('unit and city selections are mutually exclusive', () => {
    const h = makeHarness();
    h.gameState.units.set('u1', new Unit(makeUnitState()));
    h.gameState.cities.set('c1', makeCityState());
    h.gameState.player.funds = 1000;

    h.ui.selectUnit('u1');
    expect(unitPanel().classList.contains('hidden')).toBe(false);
    h.ui.selectCity('c1');
    expect(unitPanel().classList.contains('hidden')).toBe(true);
    expect(cityPanel().classList.contains('hidden')).toBe(false);

    h.ui.selectUnit('u1');
    expect(cityPanel().classList.contains('hidden')).toBe(true);
    expect(unitPanel().classList.contains('hidden')).toBe(false);
  });

  it('refresh() hides the city panel when the city is removed', () => {
    const h = makeHarness();
    h.gameState.player.funds = 1000;
    h.gameState.cities.set('c1', makeCityState());
    h.ui.selectCity('c1');
    expect(cityPanel().classList.contains('hidden')).toBe(false);

    h.gameState.cities.delete('c1');
    h.ui.refresh();
    expect(cityPanel().classList.contains('hidden')).toBe(true);
  });
});
