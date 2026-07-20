import { describe, it, expect } from 'vitest';
import { Unit } from './units';
import type { UnitState as ServerUnitState } from '../generated/protocol';

function makeServerUnit(overrides: Partial<ServerUnitState> = {}): ServerUnitState {
  return {
    id: 'unit_1',
    owner: 'p1',
    position: [10, 20],
    unit_type: 'infantry',
    symbol: 'I',
    men: 1000,
    max_men: 1000,
    organisation: 100,
    equipment: [{ name: 'rifle', count: 100, cost_per_item: 2.5 }],
    max_equipment: [{ name: 'rifle', count: 100, cost_per_item: 2.5 }],
    path: [],
    healing: true,
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

describe('Unit constructor', () => {
  it('initializes from server state', () => {
    const state = makeServerUnit();
    const unit = new Unit(state);

    expect(unit.id).toBe('unit_1');
    expect(unit.owner).toBe('p1');
    expect(unit.position).toEqual({ x: 10, y: 20 });
    expect(unit.unitType).toBe('infantry');
    expect(unit.symbol).toBe('I');
    expect(unit.men).toBe(1000);
    expect(unit.maxMen).toBe(1000);
    expect(unit.organisation).toBe(100);
    expect(unit.healing).toBe(true);
    expect(unit.resupplyEnabled).toBe(true);
    expect(unit.supplyPercent).toBe(100);
    expect(unit.encircled).toBe(false);
    expect(unit.attacking).toBe(false);
  });

  it('handles optional fields with defaults', () => {
    const state = makeServerUnit({
      resupply_enabled: undefined,
      supply_percent: undefined,
      encircled: undefined,
      attacking: undefined,
      on_water: undefined,
      boat_supply_percent: undefined,
      embarking: undefined,
      disembarking: undefined,
    });
    const unit = new Unit(state);

    expect(unit.resupplyEnabled).toBe(true);
    expect(unit.supplyPercent).toBe(100);
    expect(unit.encircled).toBe(false);
    expect(unit.attacking).toBe(false);
    expect(unit.onWater).toBe(false);
    expect(unit.boatSupplyPercent).toBe(0);
    expect(unit.embarking).toBe(false);
    expect(unit.disembarking).toBe(false);
  });

  it('handles missing max_equipment', () => {
    const state = makeServerUnit({
      max_equipment: [],
    });
    const unit = new Unit(state);

    expect(unit.maxEquipment).toEqual([]);
  });
});

describe('Unit.equipmentPercent', () => {
  it('returns 100 when fully equipped', () => {
    const state = makeServerUnit({
      equipment: [{ name: 'rifle', count: 100, cost_per_item: 2.5 }],
      max_equipment: [{ name: 'rifle', count: 100, cost_per_item: 2.5 }],
    });
    const unit = new Unit(state);

    expect(unit.equipmentPercent()).toBe(100);
  });

  it('returns 50 when half equipped', () => {
    const state = makeServerUnit({
      equipment: [{ name: 'rifle', count: 50, cost_per_item: 2.5 }],
      max_equipment: [{ name: 'rifle', count: 100, cost_per_item: 2.5 }],
    });
    const unit = new Unit(state);

    expect(unit.equipmentPercent()).toBe(50);
  });

  it('returns 0 when no equipment', () => {
    const state = makeServerUnit({
      equipment: [{ name: 'rifle', count: 0, cost_per_item: 2.5 }],
      max_equipment: [{ name: 'rifle', count: 100, cost_per_item: 2.5 }],
    });
    const unit = new Unit(state);

    expect(unit.equipmentPercent()).toBe(0);
  });

  it('returns 100 when max equipment is empty', () => {
    const state = makeServerUnit({
      equipment: [],
      max_equipment: [],
    });
    const unit = new Unit(state);

    expect(unit.equipmentPercent()).toBe(100);
  });

  it('handles multiple equipment types', () => {
    const state = makeServerUnit({
      equipment: [
        { name: 'rifle', count: 50, cost_per_item: 2.5 },
        { name: 'ammo', count: 80, cost_per_item: 0.5 },
      ],
      max_equipment: [
        { name: 'rifle', count: 100, cost_per_item: 2.5 },
        { name: 'ammo', count: 100, cost_per_item: 0.5 },
      ],
    });
    const unit = new Unit(state);

    // (50 + 80) / (100 + 100) * 100 = 65
    expect(unit.equipmentPercent()).toBe(65);
  });
});

describe('Unit.unitRenderData', () => {
  it('returns correct render data', () => {
    const state = makeServerUnit({
      men: 500,
      max_men: 1000,
      organisation: 75,
      supply_percent: 80,
    });
    const unit = new Unit(state);

    const data = unit.unitRenderData();
    expect(data.symbol).toBe('I');
    expect(data.menPercent).toBe(50);
    expect(data.orgPercent).toBe(75);
    expect(data.eqPercent).toBe(80);
  });

  it('handles zero maxMen', () => {
    const state = makeServerUnit({
      men: 0,
      max_men: 0,
    });
    const unit = new Unit(state);

    const data = unit.unitRenderData();
    expect(data.menPercent).toBe(0);
  });
});
