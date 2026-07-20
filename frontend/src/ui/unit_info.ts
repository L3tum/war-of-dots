//! Unit and City info panels with controls for healing, resupply, and upgrades.

import { GameState } from '../game/game_state';
import { Connection } from '../net/connection';
import { Unit } from '../game/units';
import { CityState } from '../generated/protocol';
import { CITY_UPGRADE_BASE_COST, CITY_MAX_LEVEL, SUPPLY_THRESHOLDS } from '../game/config';

export class UnitInfoUI {
  private unitPanel: HTMLDivElement;
  private unitDetails: HTMLDivElement;
  private unitActions: HTMLDivElement;
  private unitTitle: HTMLHeadingElement;

  private cityPanel: HTMLDivElement;
  private cityDetails: HTMLDivElement;
  private cityActions: HTMLDivElement;
  private cityTitle: HTMLHeadingElement;

  private gameState: GameState;
  private connection: Connection;

  private selectedUnitId: string | null = null;
  private selectedCityId: string | null = null;

  constructor(gameState: GameState, connection: Connection) {
    this.gameState = gameState;
    this.connection = connection;

    this.unitPanel = document.getElementById('unit-info-panel') as HTMLDivElement;
    this.unitDetails = document.getElementById('unit-info-details') as HTMLDivElement;
    this.unitActions = document.getElementById('unit-info-actions') as HTMLDivElement;
    this.unitTitle = document.getElementById('unit-info-title') as HTMLHeadingElement;

    this.cityPanel = document.getElementById('city-info-panel') as HTMLDivElement;
    this.cityDetails = document.getElementById('city-info-details') as HTMLDivElement;
    this.cityActions = document.getElementById('city-info-actions') as HTMLDivElement;
    this.cityTitle = document.getElementById('city-info-title') as HTMLHeadingElement;
  }

  /** Set the currently selected unit and update the panel */
  selectUnit(unitId: string | null) {
    this.selectedUnitId = unitId;
    this.selectedCityId = null;
    this.hideCityPanel();

    if (!unitId) {
      this.hideUnitPanel();
      return;
    }

    const unit = this.gameState.units.get(unitId);
    if (!unit) {
      this.hideUnitPanel();
      return;
    }

    this.showUnitPanel(unit);
  }

  /** Set the currently selected city and update the panel */
  selectCity(cityId: string | null) {
    this.selectedCityId = cityId;
    this.selectedUnitId = null;
    this.hideUnitPanel();

    if (!cityId) {
      this.hideCityPanel();
      return;
    }

    const city = this.gameState.cities.get(cityId);
    if (!city) {
      this.hideCityPanel();
      return;
    }

    this.showCityPanel(city);
  }

  /** Called each frame to refresh the panel if a unit/city is selected.
   * This ensures the panel stays in sync when delta updates modify state. */
  refresh(): void {
    if (this.selectedUnitId) {
      const unit = this.gameState.units.get(this.selectedUnitId);
      if (unit) {
        this.showUnitPanel(unit);
      } else {
        // Unit was removed (destroyed/surrendered)
        this.selectedUnitId = null;
        this.hideUnitPanel();
      }
    } else if (this.selectedCityId) {
      const city = this.gameState.cities.get(this.selectedCityId);
      if (city) {
        this.showCityPanel(city);
      } else {
        // City was removed
        this.selectedCityId = null;
        this.hideCityPanel();
      }
    }
  }

  private showUnitPanel(unit: Unit) {
    this.unitPanel.classList.remove('hidden');
    this.unitTitle.textContent = `${unit.symbol} ${unit.unitType}`;

    const supplyColor = this.supplyBarColor(unit.supplyPercent);
    const boatColor = this.supplyBarColor(unit.boatSupplyPercent);

    // Water status indicator
    let waterStatus = '';
    if (unit.embarking) {
      waterStatus = '<div style="color:#4af">⛴ Embarking...</div>';
    } else if (unit.disembarking) {
      waterStatus = '<div style="color:#4af">⛴ Disembarking...</div>';
    } else if (unit.onWater) {
      waterStatus = '<div style="color:#4af">⚓ On Boat</div>';
    }

    // Boat supply display
    let boatSupplyHtml = '';
    if (unit.boatSupplyPercent > 0 || unit.onWater || unit.embarking) {
      boatSupplyHtml = `<div>Boat Supply: ${Math.round(unit.boatSupplyPercent)}% 
        <span style="color:${boatColor}">■</span></div>`;
    }

    this.unitDetails.innerHTML = `
      <div>Position: ${Math.round(unit.position.x)}, ${Math.round(unit.position.y)}</div>
      <div>Men: ${Math.round(unit.men)}/${Math.round(unit.maxMen)}</div>
      <div>Org: ${Math.round(unit.organisation)}%</div>
      <div>Supply: ${Math.round(unit.supplyPercent)}% 
        <span style="color:${supplyColor}">■</span></div>
      ${boatSupplyHtml}
      ${waterStatus}
      ${unit.encircled ? '<div style="color:#f44">⚠ ENCIRCLED</div>' : ''}
      ${unit.attacking ? '<div style="color:#fa0">⚔ IN COMBAT</div>' : ''}
    `;

    // Build action buttons
    let actionsHtml = `
      <button id="toggle-healing-btn" style="margin-right:4px;">
        ${unit.healing ? '⏸ Stop Healing' : '🩹 Heal'}
      </button>
      <button id="toggle-resupply-btn">
        ${unit.resupplyEnabled ? '📦 Resupply ON' : '📦 Resupply OFF'}
      </button>
    `;

    // Add disembark button if on water
    if (unit.onWater && !unit.disembarking) {
      actionsHtml += `
        <button id="disembark-btn" style="margin-left:4px;">
          ⛴ Disembark
        </button>
      `;
    }

    this.unitActions.innerHTML = actionsHtml;

    // Setup click handlers
    const healBtn = document.getElementById('toggle-healing-btn')!;
    healBtn.addEventListener('click', () => {
      this.connection.send({
        ToggleHealing: {
          unit_id: unit.id,
          enabled: !unit.healing,
        },
      });
      this.selectUnit(unit.id); // Refresh panel
    });

    const resupplyBtn = document.getElementById('toggle-resupply-btn')!;
    resupplyBtn.addEventListener('click', () => {
      this.connection.send({
        ToggleResupply: {
          unitId: unit.id,
          enabled: !unit.resupplyEnabled,
        },
      });
      this.selectUnit(unit.id); // Refresh panel
    });

    // Disembark button handler
    const disembarkBtn = document.getElementById('disembark-btn');
    if (disembarkBtn) {
      disembarkBtn.addEventListener('click', () => {
        this.connection.send({
          DisembarkUnit: {
            unitId: unit.id,
          },
        });
        this.selectUnit(unit.id); // Refresh panel
      });
    }
  }

  private showCityPanel(city: CityState) {
    this.cityPanel.classList.remove('hidden');
    this.cityTitle.textContent = `City ${city.id}`;

    const level = city.level ?? 1;
    const canUpgrade = level < CITY_MAX_LEVEL && city.owner;

    this.cityDetails.innerHTML = `
      <div>Position: ${Math.round(city.position[0])}, ${Math.round(city.position[1])}</div>
      <div>Manpower: ${city.manpower}</div>
      <div>Level: ${level}/${CITY_MAX_LEVEL}</div>
      ${city.owner ? `<div>Owner: ${city.owner}</div>` : '<div style="color:#888">Unowned</div>'}
    `;

    if (canUpgrade) {
      // Formula matches server: base_cost * next_level (linear scaling)
      const nextLevel = level + 1;
      const cost = CITY_UPGRADE_BASE_COST * nextLevel;
      const funds = this.gameState.player.funds;
      const canAfford = funds >= cost;

      this.cityActions.innerHTML = `
        <button id="upgrade-city-btn" ${canAfford ? '' : 'disabled style="opacity:0.5"'}>
          ⬆ Upgrade to Lv.${nextLevel} (${cost.toLocaleString()} funds)
        </button>
      `;

      const upgradeBtn = document.getElementById('upgrade-city-btn')!;
      upgradeBtn.addEventListener('click', () => {
        this.connection.send({
          UpgradeCity: {
            cityId: city.id,
          },
        });
        this.selectCity(city.id); // Refresh panel
      });
    } else {
      this.cityActions.innerHTML =
        level >= CITY_MAX_LEVEL
          ? '<div style="color:#8f8">Max Level</div>'
          : '<div style="color:#888">Not owned</div>';
    }
  }

  private hideUnitPanel() {
    this.unitPanel.classList.add('hidden');
  }

  private hideCityPanel() {
    this.cityPanel.classList.add('hidden');
  }

  private supplyBarColor(percent: number): string {
    // SUPPLY_THRESHOLDS is ordered from highest to lowest threshold
    for (const entry of SUPPLY_THRESHOLDS) {
      if (percent >= entry.threshold) {
        return entry.color;
      }
    }
    return SUPPLY_THRESHOLDS[SUPPLY_THRESHOLDS.length - 1].color;
  }
}
