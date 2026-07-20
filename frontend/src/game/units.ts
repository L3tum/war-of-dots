//! Unit representation for client (rendering wrapper around server-generated types)

import {
  UnitState as ServerUnitState,
  EquipmentState as ServerEquipmentState,
} from '../generated/protocol';

export class Unit {
  id: string;
  owner: string;
  position: { x: number; y: number };
  unitType: string;
  symbol: string;
  men: number;
  maxMen: number;
  organisation: number;
  equipment: ServerEquipmentState[];
  maxEquipment: ServerEquipmentState[];
  path: [number, number][];
  healing: boolean;
  resupplyEnabled: boolean;
  supplyPercent: number;
  encircled: boolean;
  attacking: boolean;
  // Water travel fields
  onWater: boolean;
  boatSupplyPercent: number;
  embarking: boolean;
  disembarking: boolean;

  constructor(state: ServerUnitState) {
    this.id = state.id;
    this.owner = state.owner;
    this.position = { x: state.position[0], y: state.position[1] };
    this.unitType = state.unit_type;
    this.symbol = state.symbol;
    this.men = state.men;
    this.maxMen = state.max_men;
    this.organisation = state.organisation;
    this.equipment = state.equipment;
    this.maxEquipment = state.max_equipment || [];
    this.path = state.path;
    this.healing = state.healing;
    this.resupplyEnabled = state.resupply_enabled ?? true;
    this.supplyPercent = state.supply_percent ?? 100;
    this.encircled = state.encircled ?? false;
    this.attacking = state.attacking ?? false;
    // Water travel fields
    this.onWater = state.on_water ?? false;
    this.boatSupplyPercent = state.boat_supply_percent ?? 0;
    this.embarking = state.embarking ?? false;
    this.disembarking = state.disembarking ?? false;
  }

  equipmentPercent(): number {
    if (this.maxEquipment.length === 0) return 100;
    const totalCurrent = this.equipment.reduce((sum, e) => sum + e.count, 0);
    const totalMax = this.maxEquipment.reduce((sum, e) => sum + e.count, 0);
    return totalMax === 0 ? 0 : (totalCurrent / totalMax) * 100;
  }

  unitRenderData() {
    return {
      symbol: this.symbol,
      menPercent: this.maxMen > 0 ? (this.men / this.maxMen) * 100 : 0,
      orgPercent: this.organisation,
      eqPercent: this.supplyPercent, // Supply percent IS the equipment bar color
    };
  }
}
