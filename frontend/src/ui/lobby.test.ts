// Lobby UI tests — tab switching, settings collection, leaderboard display.

import { describe, it, expect, beforeEach, afterEach, vi } from 'vitest';
import { LobbyUI } from './lobby';
import { Connection } from '../net/connection';

// Mock fetch to avoid ECONNREFUSED during tests
vi.stubGlobal(
  'fetch',
  vi.fn(() => Promise.resolve({ json: () => Promise.resolve(['default', 'europe']) })),
);

// Minimal mock Connection with typed interface
interface MockConnection {
  send: (msg: unknown) => void;
  getSent: () => unknown[];
}

function createMockConnection(): MockConnection & Connection {
  const sent: unknown[] = [];
  return {
    send: (msg: unknown) => sent.push(msg),
    getSent: () => sent,
  } as unknown as MockConnection & Connection;
}

describe('LobbyUI', () => {
  let container: HTMLDivElement;
  let connection: MockConnection & Connection;

  beforeEach(() => {
    // Create a full DOM structure matching index.html lobby panel
    container = document.createElement('div');
    container.id = 'lobby-panel';
    container.innerHTML = `
      <div id="lobby-tabs">
        <button id="tab-create" class="active">Create</button>
        <button id="tab-join">Join</button>
        <button id="tab-leaderboard">Leaderboard</button>
      </div>
      <div id="lobby-panel-create">
        <select id="map-select"><option value="default">Default</option></select>
        <input type="checkbox" id="use-standard-ruleset" checked>
        <div id="advanced-settings-panel" class="hidden">
          <div class="settings-section">
            <h4>⚙ Economy & Speed</h4>
            <div class="settings-body">
              <input type="number" id="starting-funds" value="1000">
              <input type="number" id="starting-manpower" value="2000">
              <input type="number" id="funds-per-city" value="1.0">
              <input type="number" id="game-speed" value="60">
              <select id="max-players"><option value="2">2</option></select>
              <input type="number" id="recruitment-rate" value="10">
              <input type="number" id="water-speed" value="1.2">
              <input type="checkbox" id="spectator-vision">
            </div>
          </div>
          <div class="settings-section">
            <h4>🏰 City Upgrades</h4>
            <div class="settings-body">
              <input type="checkbox" id="city-upgrades-enabled">
              <input type="number" id="city-max-level" value="100">
              <input type="number" id="city-base-upgrade-cost" value="200">
              <input type="number" id="city-income-growth" value="0.5">
              <input type="number" id="city-base-supply-range" value="5.0">
              <input type="number" id="city-supply-range-growth" value="3.0">
              <input type="number" id="city-secondary-decay" value="0.5">
              <input type="number" id="city-manpower-regen" value="10.0">
            </div>
          </div>
        </div>
        <div id="custom-ruleset-panel" class="hidden">
          <div id="unit-types-editor"></div>
          <button id="add-unit-type-btn">+ Add</button>
        </div>
        <input type="text" id="create-player-name" value="TestPlayer">
        <input type="password" id="lobby-password">
        <button id="create-lobby-btn">Create</button>
        <div id="create-status" class="hidden"></div>
      </div>
      <div id="lobby-panel-join" class="hidden">
        <input type="text" id="lobby-code" value="">
        <input type="text" id="join-player-name" value="">
        <input type="password" id="join-password" value="">
        <button id="join-lobby-btn">Join</button>
        <div id="join-status" class="hidden"></div>
      </div>
      <div id="lobby-info" class="hidden">
        <span id="lobby-code-display"></span>
        <div id="lobby-players"></div>
        <div id="slot-controls" class="hidden"></div>
        <button id="start-game-btn">Start</button>
      </div>
    `;
    document.body.appendChild(container);

    // Leaderboard panel (outside lobby)
    const lbPanel = document.createElement('div');
    lbPanel.id = 'leaderboard-panel';
    lbPanel.className = 'hidden';
    lbPanel.innerHTML = `
      <div style="display:flex;justify-content:space-between;">
        <h4>Leaderboard</h4>
        <button id="leaderboard-close-btn">X</button>
      </div>
      <div id="leaderboard-content"></div>
    `;
    document.body.appendChild(lbPanel);

    connection = createMockConnection();
  });

  afterEach(() => {
    container.remove();
    const lbPanel = document.getElementById('leaderboard-panel');
    lbPanel?.remove();
    // Clean up any other panels that might be in the body
    document.querySelectorAll('.panel').forEach((p) => p.remove());
  });

  it('hides advanced settings when standard ruleset is checked', () => {
    new LobbyUI(connection);
    const checkbox = document.getElementById('use-standard-ruleset') as HTMLInputElement;
    const advanced = document.getElementById('advanced-settings-panel')!;

    // Default: checkbox is checked -> advanced panel hidden
    expect(advanced.classList.contains('hidden')).toBe(true);

    // Uncheck -> show advanced
    checkbox.checked = false;
    checkbox.dispatchEvent(new Event('change'));
    expect(advanced.classList.contains('hidden')).toBe(false);

    // Re-check -> hide again
    checkbox.checked = true;
    checkbox.dispatchEvent(new Event('change'));
    expect(advanced.classList.contains('hidden')).toBe(true);
  });

  it('sends only standard settings when ruleset is standard', () => {
    new LobbyUI(connection);

    // Trigger create lobby
    document.getElementById('create-lobby-btn')!.click();

    const sent = connection.getSent();
    expect(sent).toHaveLength(1);
    const msg = sent[0] as Record<string, unknown>;
    expect(msg).toHaveProperty('LobbyMessage');
    // eslint-disable-next-line @typescript-eslint/no-explicit-any
    const createLobby = (msg.LobbyMessage as any).CreateLobby;
    const settings = createLobby.settings;
    expect(settings.use_standard_ruleset).toBe(true);
    // Advanced fields should NOT be present when standard
    expect(settings).not.toHaveProperty('starting_funds');
    expect(settings).not.toHaveProperty('funds_per_city');
    expect(settings).not.toHaveProperty('game_speed');
    expect(settings).not.toHaveProperty('max_players');
    expect(settings).not.toHaveProperty('custom_unit_types');
    expect(settings).not.toHaveProperty('recruitment_rate');
    expect(settings).not.toHaveProperty('water_speed');
    expect(settings).not.toHaveProperty('city_upgrade_settings');
    // Player name should be included
    expect(createLobby.player_name).toBe('TestPlayer');
  });

  it('requires player name before creating lobby', () => {
    new LobbyUI(connection);

    // Clear the player name
    (document.getElementById('create-player-name') as HTMLInputElement).value = '';

    // Trigger create lobby
    document.getElementById('create-lobby-btn')!.click();

    // Nothing should be sent
    const sent = connection.getSent();
    expect(sent).toHaveLength(0);

    // Status should show error message
    const status = document.getElementById('create-status')!;
    expect(status.textContent).toBe('Please enter your name.');
    expect(status.classList.contains('hidden')).toBe(false);
  });

  it('sends all GameSettings when ruleset is custom', () => {
    new LobbyUI(connection);
    const checkbox = document.getElementById('use-standard-ruleset') as HTMLInputElement;

    // Uncheck standard ruleset
    checkbox.checked = false;
    checkbox.dispatchEvent(new Event('change'));

    document.getElementById('create-lobby-btn')!.click();

    const sent = connection.getSent();
    // eslint-disable-next-line @typescript-eslint/no-explicit-any
    const msg = sent[0] as { LobbyMessage: { CreateLobby: { settings: any } } };
    const settings = msg.LobbyMessage.CreateLobby.settings;

    expect(settings.use_standard_ruleset).toBe(false);
    // Economy & speed
    expect(settings.starting_funds).toBe(1000);
    expect(settings.initial_manpower).toBe(2000);
    expect(settings.funds_per_city).toBe(1.0);
    expect(settings.game_speed).toBe(60);
    expect(settings.max_players).toBe(2);
    // Movement & combat
    expect(settings.recruitment_rate).toBe(10);
    expect(settings.water_speed).toBe(1.2);
    // City upgrades
    expect(settings.city_upgrade_settings).toBeDefined();
    expect(settings.city_upgrade_settings.enabled).toBe(false);
    expect(settings.city_upgrade_settings.max_level).toBe(100);
    expect(settings.city_upgrade_settings.base_upgrade_cost).toBe(200);
    expect(settings.city_upgrade_settings.income_growth_per_level).toBe(0.5);
    expect(settings.city_upgrade_settings.base_supply_range).toBe(5.0);
    expect(settings.city_upgrade_settings.supply_range_growth).toBe(3.0);
    expect(settings.city_upgrade_settings.secondary_city_decay).toBe(0.5);
    expect(settings.city_upgrade_settings.manpower_regen_per_tick).toBe(10.0);
    // Custom unit types
    expect(settings.custom_unit_types).toBeDefined();
    expect(Array.isArray(settings.custom_unit_types)).toBe(true);
  });

  it('switches tabs correctly', () => {
    new LobbyUI(connection);

    // Start on create tab
    expect(document.getElementById('lobby-panel-create')!.classList.contains('hidden')).toBe(false);
    expect(document.getElementById('lobby-panel-join')!.classList.contains('hidden')).toBe(true);

    // Click join tab
    document.getElementById('tab-join')!.click();
    expect(document.getElementById('lobby-panel-create')!.classList.contains('hidden')).toBe(true);
    expect(document.getElementById('lobby-panel-join')!.classList.contains('hidden')).toBe(false);
    expect(document.getElementById('tab-join')!.classList.contains('active')).toBe(true);
    expect(document.getElementById('tab-create')!.classList.contains('active')).toBe(false);

    // Click create tab
    document.getElementById('tab-create')!.click();
    expect(document.getElementById('lobby-panel-create')!.classList.contains('hidden')).toBe(false);
    expect(document.getElementById('tab-create')!.classList.contains('active')).toBe(true);
  });

  it('clicking leaderboard tab sends GetLeaderboard message', () => {
    new LobbyUI(connection);

    document.getElementById('tab-leaderboard')!.click();

    const sent = connection.getSent();
    expect(sent).toHaveLength(1);
    expect(sent[0]).toEqual({ GetLeaderboard: null });
  });

  it('validates join lobby code is not empty', () => {
    new LobbyUI(connection);

    document.getElementById('join-lobby-btn')!.click();

    const status = document.getElementById('join-status')!;
    expect(status.classList.contains('hidden')).toBe(false);
    expect(status.classList.contains('error')).toBe(true);
    expect(status.textContent).toContain('lobby code');

    // No message should have been sent
    expect(connection.getSent()).toHaveLength(0);
  });

  it('sends join lobby message with valid code', () => {
    new LobbyUI(connection);

    (document.getElementById('lobby-code') as HTMLInputElement).value = 'ABC123';
    (document.getElementById('join-player-name') as HTMLInputElement).value = 'Alice';

    document.getElementById('join-lobby-btn')!.click();

    const sent = connection.getSent();
    expect(sent).toHaveLength(1);
    // eslint-disable-next-line @typescript-eslint/no-explicit-any
    const msg = sent[0] as { LobbyMessage: { JoinLobby: any } };
    expect(msg).toHaveProperty('LobbyMessage');
    expect(msg.LobbyMessage.JoinLobby.lobby_code).toBe('ABC123');
    expect(msg.LobbyMessage.JoinLobby.player_name).toBe('Alice');
  });

  it('renders leaderboard entries in a table', () => {
    const ui = new LobbyUI(connection);

    const entries = [
      { player_id: 'p1', name: 'Alice', games_played: 10, games_won: 7, avg_casualties: 150.5 },
      { player_id: 'p2', name: 'Bob', games_played: 5, games_won: 2, avg_casualties: 300.0 },
    ];
    ui.showLeaderboard(entries);

    const panel = document.getElementById('leaderboard-panel')!;
    expect(panel.classList.contains('hidden')).toBe(false);

    const content = document.getElementById('leaderboard-content')!;
    expect(content.innerHTML).toContain('<table>');
    expect(content.innerHTML).toContain('Alice');
    expect(content.innerHTML).toContain('Bob');
    expect(content.innerHTML).toContain('10');
    expect(content.innerHTML).toContain('7');
    expect(content.innerHTML).toContain('150.5');
  });

  it('shows empty message when no leaderboard entries', () => {
    const ui = new LobbyUI(connection);
    ui.showLeaderboard([]);

    const content = document.getElementById('leaderboard-content')!;
    expect(content.innerHTML).toContain('No games recorded');
  });

  it('hides leaderboard on close button click', () => {
    const ui = new LobbyUI(connection);
    ui.showLeaderboard([]);

    expect(document.getElementById('leaderboard-panel')!.classList.contains('hidden')).toBe(false);

    // Activate leaderboard tab first so close switches back
    document.getElementById('tab-leaderboard')!.click();

    document.getElementById('leaderboard-close-btn')!.click();

    expect(document.getElementById('leaderboard-panel')!.classList.contains('hidden')).toBe(true);
  });

  it('updatePlayers renders player list and slot controls', () => {
    const ui = new LobbyUI(connection);

    const players = [
      { id: '1', name: 'Alice', is_human: true, llm_provider: null },
      { id: '2', name: 'Bob', is_human: false, llm_provider: 'openai' },
    ];
    ui.updatePlayers(players);

    const playersDiv = document.getElementById('lobby-players')!;
    expect(playersDiv.innerHTML).toContain('Alice');
    expect(playersDiv.innerHTML).toContain('Human');
    expect(playersDiv.innerHTML).toContain('Bob');
    expect(playersDiv.innerHTML).toContain('openai');
  });

  it('showTab("info") hides tabs and shows waiting room', () => {
    const ui = new LobbyUI(connection);

    // Set lobby code as if server sent LobbyCreated
    (document.getElementById('lobby-code-display') as HTMLSpanElement).textContent = 'XYZ789';

    // Switch to info tab (what LobbyCreated handler does)
    ui.showTab('info');

    // Tabs should be hidden
    expect(document.getElementById('lobby-tabs')!.classList.contains('hidden')).toBe(true);
    // Create/join panels should be hidden
    expect(document.getElementById('lobby-panel-create')!.classList.contains('hidden')).toBe(true);
    expect(document.getElementById('lobby-panel-join')!.classList.contains('hidden')).toBe(true);
    // Info panel should be visible
    expect(document.getElementById('lobby-info')!.classList.contains('hidden')).toBe(false);
    // Tab buttons should be deactivated
    expect(document.getElementById('tab-create')!.classList.contains('active')).toBe(false);
    expect(document.getElementById('tab-join')!.classList.contains('active')).toBe(false);
    // Lobby code should still be set
    expect(document.getElementById('lobby-code-display')!.textContent).toBe('XYZ789');
  });

  it('showTab("create") restores tabs after waiting room', () => {
    const ui = new LobbyUI(connection);

    // Enter info mode (waiting room)
    ui.showTab('info');
    expect(document.getElementById('lobby-tabs')!.classList.contains('hidden')).toBe(true);

    // Switch back to create
    ui.showTab('create');

    // Tabs should be visible again
    expect(document.getElementById('lobby-tabs')!.classList.contains('hidden')).toBe(false);
    expect(document.getElementById('lobby-panel-create')!.classList.contains('hidden')).toBe(false);
    expect(document.getElementById('tab-create')!.classList.contains('active')).toBe(true);
  });

  it('start-game-btn sends StartGame lobby message', () => {
    new LobbyUI(connection);

    document.getElementById('start-game-btn')!.click();

    const sent = connection.getSent();
    expect(sent).toHaveLength(1);
    expect(sent[0]).toEqual({ LobbyMessage: { StartGame: null } });
  });

  it('hide() hides the lobby panel', () => {
    const ui = new LobbyUI(connection);

    // Lobby panel should be visible initially
    const panel = document.getElementById('lobby-panel')!;
    expect(panel.classList.contains('hidden')).toBe(false);

    ui.hide();

    expect(panel.classList.contains('hidden')).toBe(true);
  });

  it('hide() is idempotent', () => {
    const ui = new LobbyUI(connection);

    ui.hide();
    ui.hide(); // calling twice should not crash

    expect(document.getElementById('lobby-panel')!.classList.contains('hidden')).toBe(true);
  });
});
