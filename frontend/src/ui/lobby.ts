//! Lobby UI — lobby creation, joining, and leaderboard display.

import { Connection } from '../net/connection';
import { LeaderboardEntry } from '../generated/protocol';
import { escapeHtml } from './html';

interface UnitTypeConfig {
  name: string;
  symbol: string;
  men: number;
  equipment: [string, number][];
  soft_attack: number;
  soft_defense: number;
  hard_attack: number;
  hard_defense: number;
  speed: number;
  max_organisation: number;
  reinforcement_rate: number;
  recruitment_cost: number;
}

type LobbyTab = 'create' | 'join' | 'leaderboard' | 'info';

export class LobbyUI {
  private connection: Connection;
  private playersDiv: HTMLDivElement;
  private leaderboardPanel: HTMLDivElement;
  private leaderboardContent: HTMLDivElement;
  private startButton!: HTMLButtonElement;

  constructor(connection: Connection) {
    this.connection = connection;
    this.playersDiv = document.getElementById('lobby-players') as HTMLDivElement;
    this.leaderboardPanel = document.getElementById('leaderboard-panel') as HTMLDivElement;
    this.leaderboardContent = document.getElementById('leaderboard-content') as HTMLDivElement;
    this.startButton = document.getElementById('start-game-btn') as HTMLButtonElement;
    this.setupEventListeners();
    this.setupCustomRuleset();
    this.populateMaps();
  }

  /** Disable the Start Game button (e.g. immediately after clicking it) */
  disableStartButton(): void {
    this.startButton.disabled = true;
  }

  /** Re-enable the Start Game button (e.g. after GameOver or an Error) */
  enableStartButton(): void {
    this.startButton.disabled = false;
  }

  private setupEventListeners() {
    // ── Tab switching ──────────────────────────────
    const tabs: Record<string, LobbyTab> = {
      'tab-create': 'create',
      'tab-join': 'join',
      'tab-leaderboard': 'leaderboard',
    };
    for (const [id, tab] of Object.entries(tabs)) {
      document.getElementById(id)!.addEventListener('click', () => {
        this.showTab(tab);
      });
    }

    // ── Create lobby ───────────────────────────────
    document.getElementById('create-lobby-btn')!.addEventListener('click', () => {
      this.createLobby();
    });

    // ── Join lobby ─────────────────────────────────
    document.getElementById('join-lobby-btn')!.addEventListener('click', () => {
      this.joinLobby();
    });

    // ── Start game ─────────────────────────────────
    document.getElementById('start-game-btn')!.addEventListener('click', () => {
      // Disable immediately to prevent a second click/rapid double-start.
      this.disableStartButton();
      this.connection.send({ LobbyMessage: { StartGame: null } });
    });

    // ── Leaderboard close ──────────────────────────
    const closeBtn = document.getElementById('leaderboard-close-btn');
    if (closeBtn) {
      closeBtn.addEventListener('click', () => {
        this.hideLeaderboard();
        // Switch back to create tab if leaderboard tab was active
        const lbTab = document.getElementById('tab-leaderboard');
        if (lbTab?.classList.contains('active')) {
          this.showTab('create');
        }
      });
    }
  }

  /** Switch between lobby tabs */
  showTab(tab: LobbyTab): void {
    // Hide all lobby content panels
    ['lobby-panel-create', 'lobby-panel-join', 'lobby-info'].forEach((id) => {
      document.getElementById(id)!.classList.add('hidden');
    });

    // Deactivate all tab buttons
    ['tab-create', 'tab-join', 'tab-leaderboard'].forEach((id) => {
      document.getElementById(id)!.classList.remove('active');
    });

    // Handle waiting room — hide tabs, show info
    if (tab === 'info') {
      document.getElementById('lobby-tabs')!.classList.add('hidden');
      document.getElementById('lobby-info')!.classList.remove('hidden');
      return;
    }

    // Show tabs again (they may have been hidden by info view)
    document.getElementById('lobby-tabs')!.classList.remove('hidden');

    if (tab === 'leaderboard') {
      document.getElementById('tab-leaderboard')!.classList.add('active');
      // Fetch leaderboard data
      this.connection.send({ GetLeaderboard: null });
      return;
    }

    // Activate correct tab button
    const tabBtnId = tab === 'create' ? 'tab-create' : `tab-${tab}`;
    document.getElementById(tabBtnId)!.classList.add('active');

    // Show correct panel
    const panelId = tab === 'create' ? 'lobby-panel-create' : `lobby-panel-${tab}`;
    document.getElementById(panelId)!.classList.remove('hidden');
  }

  private setupCustomRuleset() {
    const checkbox = document.getElementById('use-standard-ruleset') as HTMLInputElement;
    const advancedPanel = document.getElementById('advanced-settings-panel');
    const customPanel = document.getElementById('custom-ruleset-panel');

    checkbox.addEventListener('change', () => {
      const isStandard = checkbox.checked;
      // Hide advanced settings AND custom ruleset when standard is checked
      if (advancedPanel) advancedPanel.classList.toggle('hidden', isStandard);
      if (customPanel) customPanel.classList.toggle('hidden', isStandard);
    });

    const addBtn = document.getElementById('add-unit-type-btn');
    if (addBtn) {
      addBtn.addEventListener('click', () => this.addUnitTypeEditor());
    }

    // Add one default unit type on start
    this.addUnitTypeEditor();
  }

  private createLobby() {
    const useStandard = (document.getElementById('use-standard-ruleset') as HTMLInputElement)
      .checked;

    // eslint-disable-next-line @typescript-eslint/no-explicit-any
    const settings: any = {
      map: (document.getElementById('map-select') as HTMLSelectElement).value,
      use_standard_ruleset: useStandard,
      llm_thinking_delay: 3.0,
    };

    if (!useStandard) {
      // Custom ruleset: send all game settings + custom unit types
      settings.starting_funds = Number(
        (document.getElementById('starting-funds') as HTMLInputElement).value,
      );
      settings.initial_manpower = Number(
        (document.getElementById('starting-manpower') as HTMLInputElement).value,
      );
      settings.funds_per_city = Number(
        (document.getElementById('funds-per-city') as HTMLInputElement).value,
      );
      settings.game_speed = Number(
        (document.getElementById('game-speed') as HTMLInputElement).value,
      );
      settings.max_players = Number(
        (document.getElementById('max-players') as HTMLSelectElement).value,
      );
      settings.spectator_vision = (
        document.getElementById('spectator-vision') as HTMLInputElement
      ).checked;
      settings.custom_unit_types = this.collectUnitTypes();

      // Movement & combat settings
      settings.recruitment_rate = Number(
        (document.getElementById('recruitment-rate') as HTMLInputElement).value,
      );
      settings.water_speed = Number(
        (document.getElementById('water-speed') as HTMLInputElement).value,
      );

      // City upgrade settings
      settings.city_upgrade_settings = {
        enabled: (document.getElementById('city-upgrades-enabled') as HTMLInputElement).checked,
        max_level: Number((document.getElementById('city-max-level') as HTMLInputElement).value),
        base_upgrade_cost: Number(
          (document.getElementById('city-base-upgrade-cost') as HTMLInputElement).value,
        ),
        income_growth_per_level: Number(
          (document.getElementById('city-income-growth') as HTMLInputElement).value,
        ),
        base_supply_range: Number(
          (document.getElementById('city-base-supply-range') as HTMLInputElement).value,
        ),
        supply_range_growth: Number(
          (document.getElementById('city-supply-range-growth') as HTMLInputElement).value,
        ),
        secondary_city_decay: Number(
          (document.getElementById('city-secondary-decay') as HTMLInputElement).value,
        ),
        manpower_regen_per_tick: Number(
          (document.getElementById('city-manpower-regen') as HTMLInputElement).value,
        ),
      };
    }

    const playerName = (
      (document.getElementById('create-player-name') as HTMLInputElement).value || ''
    ).trim();

    if (!playerName) {
      this.showCreateStatus('Please enter your name.', true);
      return;
    }

    const password =
      (document.getElementById('lobby-password') as HTMLInputElement).value || undefined;

    this.showCreateStatus('Creating lobby...', false);
    this.connection.send({
      LobbyMessage: {
        CreateLobby: { settings, password, player_name: playerName },
      },
    });
  }

  private showCreateStatus(message: string, isError: boolean): void {
    const el = document.getElementById('create-status')!;
    el.textContent = message;
    el.classList.remove('hidden', 'error');
    if (isError) el.classList.add('error');
  }

  private showJoinStatus(message: string, isError: boolean): void {
    const el = document.getElementById('join-status')!;
    el.textContent = message;
    el.classList.remove('hidden', 'error');
    if (isError) el.classList.add('error');
  }

  private addUnitTypeEditor() {
    const editor = document.getElementById('unit-types-editor')!;
    const div = document.createElement('div');
    div.className = 'unit-type-row';
    div.innerHTML = `
      <div class="form-row">
        <div class="form-group">
          <label>Name</label>
          <input type="text" class="ut-name" value="Unit ${editor.children.length + 1}" placeholder="Unit name">
        </div>
        <div class="form-group">
          <label>Symbol</label>
          <input type="text" class="ut-symbol" value="U" maxlength="1" style="width:40px;">
        </div>
      </div>
      <div class="form-row">
        <div class="form-group">
          <label>Men</label>
          <input type="number" class="ut-men" value="1000" min="100" max="10000">
        </div>
        <div class="form-group">
          <label>Recruitment Cost</label>
          <input type="number" class="ut-cost" value="20">
        </div>
      </div>
      <div class="form-group">
        <label>Equipment (name:count, comma-separated)</label>
        <input type="text" class="ut-equipment" placeholder="e.g. rifles:100">
      </div>
      <div class="form-row" style="grid-template-columns:1fr 1fr 1fr 1fr;">
        <div class="form-group">
          <label>Soft Atk</label>
          <input type="number" class="ut-soft-atk" value="10" step="1">
        </div>
        <div class="form-group">
          <label>Soft Def</label>
          <input type="number" class="ut-soft-def" value="10" step="1">
        </div>
        <div class="form-group">
          <label>Hard Atk</label>
          <input type="number" class="ut-hard-atk" value="0" step="1">
        </div>
        <div class="form-group">
          <label>Hard Def</label>
          <input type="number" class="ut-hard-def" value="0" step="1">
        </div>
      </div>
      <div class="form-row">
        <div class="form-group">
          <label>Speed</label>
          <input type="number" class="ut-speed" value="1.0" step="0.1">
        </div>
        <div class="form-group">
          <label>Reinforcement Rate</label>
          <input type="number" class="ut-reinforce" value="1.0" step="0.1">
        </div>
      </div>
      <button class="btn-danger remove-unit-btn" style="margin-top:8px;">Remove</button>
    `;
    div.querySelector('.remove-unit-btn')!.addEventListener('click', () => div.remove());
    editor.appendChild(div);
  }

  private collectUnitTypes(): UnitTypeConfig[] {
    const rows = document.querySelectorAll('.unit-type-row');
    return Array.from(rows).map((row) => ({
      name: (row.querySelector('.ut-name') as HTMLInputElement).value,
      symbol: (row.querySelector('.ut-symbol') as HTMLInputElement).value,
      men: Number((row.querySelector('.ut-men') as HTMLInputElement).value),
      equipment: this.parseEquipment(
        (row.querySelector('.ut-equipment') as HTMLInputElement).value,
      ),
      soft_attack: Number((row.querySelector('.ut-soft-atk') as HTMLInputElement).value),
      soft_defense: Number((row.querySelector('.ut-soft-def') as HTMLInputElement).value),
      hard_attack: Number((row.querySelector('.ut-hard-atk') as HTMLInputElement).value),
      hard_defense: Number((row.querySelector('.ut-hard-def') as HTMLInputElement).value),
      speed: Number((row.querySelector('.ut-speed') as HTMLInputElement).value),
      max_organisation: 100,
      reinforcement_rate: Number((row.querySelector('.ut-reinforce') as HTMLInputElement).value),
      recruitment_cost: Number((row.querySelector('.ut-cost') as HTMLInputElement).value),
    }));
  }

  private parseEquipment(equipStr: string): [string, number][] {
    if (!equipStr.trim()) return [['rifles', 100]];
    const parts = equipStr
      .split(/[,;]/)
      .map((s) => s.trim())
      .filter(Boolean);
    return parts.map((part) => {
      const [name, countStr] = part.split(':');
      return [name?.trim() || '', Number(countStr) || 0];
    });
  }

  private joinLobby() {
    const code = (document.getElementById('lobby-code') as HTMLInputElement).value.trim();
    if (!code) {
      this.showJoinStatus('Please enter a lobby code.', true);
      return;
    }

    const playerNameInput = document.getElementById('join-player-name') as HTMLInputElement | null;
    const password =
      (document.getElementById('join-password') as HTMLInputElement).value || undefined;
    const playerName = playerNameInput ? playerNameInput.value || undefined : undefined;

    this.showJoinStatus('Joining lobby...', false);
    this.connection.send({
      LobbyMessage: {
        JoinLobby: { lobby_code: code, password, player_name: playerName },
      },
    });
  }

  /** Update the lobby players list */
  updatePlayers(
    players: Array<{ id: string; name: string; is_human: boolean; llm_provider: string | null }>,
  ) {
    this.playersDiv.innerHTML = players
      .map((p) => {
        const name = document.createTextNode(p.name);
        const type = p.is_human ? 'Human' : p.llm_provider || 'LLM';
        const div = document.createElement('div');
        div.style.padding = '2px 0';
        div.appendChild(name);
        div.appendChild(document.createTextNode(` — ${type}`));
        return div.outerHTML;
      })
      .join('');

    // Render slot type selectors for the host (shown in lobby-info)
    const slotControls = document.getElementById('slot-controls');
    if (slotControls) {
      slotControls.classList.remove('hidden');

      const providerOptions = ['human', 'openai', 'anthropic', 'ollama'];
      const providerLabels: Record<string, string> = {
        human: 'Human',
        openai: 'OpenAI',
        anthropic: 'Anthropic',
        ollama: 'Ollama',
      };

      slotControls.innerHTML =
        players
          .map(
            (p, i) => `
        <div class="slot-row">
          <span>${p.name}</span>
          <select class="slot-type-select" data-slot="${i}">
            ${providerOptions
              .map(
                (opt) =>
                  `<option value="${opt}" ${p.is_human && opt === 'human' ? 'selected' : ''} ${p.llm_provider === opt ? 'selected' : ''}>${providerLabels[opt]}</option>`,
              )
              .join('')}
          </select>
        </div>
      `,
          )
          .join('') +
        `
        <div style="margin-top:10px;padding-top:8px;border-top:1px solid #333;">
          <div style="display:flex;gap:6px;align-items:center;">
            <input type="text" id="add-llm-name" placeholder="LLM player name" class="form-input" style="flex:1;margin:0;"/>
            <select id="add-llm-provider" class="form-input" style="flex:0 0 auto;">
              <option value="openai">OpenAI</option>
              <option value="anthropic">Anthropic</option>
              <option value="ollama">Ollama</option>
            </select>
            <button id="add-llm-btn" class="btn-primary" style="white-space:nowrap;">Add</button>
          </div>
        </div>
      `;

      // Bind change events for existing slot selectors
      slotControls.querySelectorAll('.slot-type-select').forEach((select) => {
        select.addEventListener('change', (e) => {
          const target = e.target as HTMLSelectElement;
          const slot = Number(target.dataset.slot);
          const provider = target.value;
          this.connection.send({
            LobbyMessage: {
              SetSlotLlm: { slot, llm_provider: provider === 'human' ? '' : provider },
            },
          });
        });
      });

      // Bind add LLM player button
      const addBtn = document.getElementById('add-llm-btn');
      if (addBtn) {
        addBtn.addEventListener('click', () => {
          const nameInput = document.getElementById('add-llm-name') as HTMLInputElement;
          const providerSelect = document.getElementById('add-llm-provider') as HTMLSelectElement;
          const name = nameInput?.value.trim() || 'LLM Player';
          const provider = providerSelect?.value || 'openai';
          this.connection.send({
            LobbyMessage: {
              AddLlmPlayer: { name, llm_provider: provider },
            },
          });
          if (nameInput) nameInput.value = '';
        });
      }
    }
  }

  /** Show the leaderboard entries */
  showLeaderboard(entries: LeaderboardEntry[]): void {
    this.leaderboardPanel.classList.remove('hidden');

    if (entries.length === 0) {
      this.leaderboardContent.innerHTML =
        '<p style="color:#888;text-align:center;padding:16px 0;">No games recorded yet.</p>';
      return;
    }

    const table = `
      <table>
        <thead>
          <tr>
            <th>#</th>
            <th>Player</th>
            <th style="text-align:center;">Games</th>
            <th style="text-align:center;">Wins</th>
            <th style="text-align:center;">Avg Casualties</th>
          </tr>
        </thead>
        <tbody>
          ${entries
            .map(
              (e, i) => `
            <tr>
              <td style="text-align:center;">${i + 1}</td>
              <td>${escapeHtml(e.name)}</td>
              <td style="text-align:center;">${e.games_played}</td>
              <td style="text-align:center;">${e.games_won}</td>
              <td style="text-align:center;">${e.avg_casualties.toFixed(1)}</td>
            </tr>
          `,
            )
            .join('')}
        </tbody>
      </table>
    `;
    this.leaderboardContent.innerHTML = table;
  }

  /** Hide the leaderboard panel */
  hideLeaderboard(): void {
    this.leaderboardPanel.classList.add('hidden');
  }

  /** Hide the lobby panel (called when game starts) */
  hide(): void {
    const panel = document.getElementById('lobby-panel');
    if (panel) {
      panel.classList.add('hidden');
    }
  }

  /** Populate the map select dropdown from the server */
  async populateMaps() {
    try {
      const resp = await fetch('/maps');
      const maps: string[] = await resp.json();
      const select = document.getElementById('map-select') as HTMLSelectElement;
      select.innerHTML = '';
      for (const name of maps) {
        const opt = document.createElement('option');
        opt.value = name;
        opt.textContent = name.charAt(0).toUpperCase() + name.slice(1);
        select.appendChild(opt);
      }
    } catch (e) {
      console.warn('Failed to load maps:', e);
      // Fallback to default
      const select = document.getElementById('map-select') as HTMLSelectElement;
      const opt = document.createElement('option');
      opt.value = 'default';
      opt.textContent = 'Default';
      select.appendChild(opt);
    }
  }
}
