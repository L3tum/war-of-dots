//! Spectator UI tests — reason log rendering, per-player filter, caps,
//! game-over overlay and replay link, clearAll.

import { describe, it, expect, beforeEach, afterEach } from 'vitest';
import { Spectator } from './spectator';

/**
 * Minimal DOM mirroring the ids spectator.ts reaches for in index.html
 * (spectator-panel / spectator-players / spectator-reasons, game-over
 * overlay + title + replay link, stats-panel).
 */
function mountSpectatorDom() {
  const root = document.createElement('div');
  root.innerHTML = `
    <div id="spectator-panel" class="panel hidden">
      <div id="spectator-players"></div>
      <div id="spectator-reasons"></div>
    </div>
    <div id="game-over-overlay" class="hidden">
      <h2 id="game-over-title"></h2>
      <a id="replay-link" href="/replay">Watch Replay</a>
    </div>
    <div id="stats-panel" class="panel hidden"></div>
  `;
  document.body.appendChild(root);
  return root;
}

function spectatorPanel() {
  return document.getElementById('spectator-panel')!;
}

function reasonEntries() {
  return Array.from(document.querySelectorAll<HTMLDivElement>('#spectator-reasons .player-reason'));
}

function filterButtons() {
  return Array.from(document.querySelectorAll<HTMLButtonElement>('#spectator-players button'));
}

describe('Spectator reason log', () => {
  beforeEach(() => {
    mountSpectatorDom();
  });

  afterEach(() => {
    document.body.innerHTML = '';
  });

  it('addReason shows the panel and renders the entry (name, action, reason)', () => {
    const s = new Spectator();
    expect(spectatorPanel().classList.contains('hidden')).toBe(true);

    s.addReason('Alice', 'move', 'flanking through the forest');

    expect(spectatorPanel().classList.contains('hidden')).toBe(false);
    const entries = reasonEntries();
    expect(entries).toHaveLength(1);
    expect(entries[0].innerHTML).toContain('<strong>Alice:</strong>');
    expect(entries[0].textContent).toContain('move');
    expect(entries[0].textContent).toContain('flanking through the forest');
  });

  it('entries render newest-first', () => {
    const s = new Spectator();
    s.addReason('Alice', 'move', 'first');
    s.addReason('Alice', 'attack', 'second');

    const entries = reasonEntries();
    expect(entries).toHaveLength(2);
    expect(entries[0].textContent).toContain('second');
    expect(entries[1].textContent).toContain('first');
  });

  it('caps the log at the last 100 entries per player', () => {
    const s = new Spectator();
    for (let i = 0; i < 105; i++) {
      s.addReason('Alice', 'move', `entry ${i}`);
    }

    const entries = reasonEntries();
    expect(entries).toHaveLength(100);
    // The five oldest entries were trimmed; the newest is first.
    // (Compare the <em> reason text with exact equality — substring checks
    // would confuse 'entry 4' with 'entry 40'.)
    const reasons = entries.map((e) => e.querySelector('em')!.textContent);
    expect(reasons[0]).toBe('entry 104');
    expect(reasons[99]).toBe('entry 5');
    expect(reasons).not.toContain('entry 4');
    expect(reasons).not.toContain('entry 0');
  });

  it('player buttons filter the log; the All button clears the filter', () => {
    const s = new Spectator();
    s.addReason('Alice', 'move', 'alice-reason');
    s.addReason('Bob', 'attack', 'bob-reason');

    // Buttons: All + one per player.
    const btns = filterButtons();
    const names = btns.map((b) => b.textContent);
    expect(names).toContain('All');
    expect(names).toContain('Alice');
    expect(names).toContain('Bob');
    expect(reasonEntries()).toHaveLength(2);

    // Filter to Bob: only his entries remain.
    btns.find((b) => b.textContent === 'Bob')!.click();
    const entries = reasonEntries();
    expect(entries).toHaveLength(1);
    expect(entries[0].textContent).toContain('bob-reason');

    // Back to All: both entries return.
    filterButtons()
      .find((b) => b.textContent === 'All')!
      .click();
    expect(reasonEntries()).toHaveLength(2);
  });

  it('clear() empties the log and hides the panel', () => {
    const s = new Spectator();
    s.addReason('Alice', 'move', 'x');
    s.addReason('Bob', 'attack', 'y');

    s.clear();
    expect(reasonEntries()).toHaveLength(0);
    expect(filterButtons()).toHaveLength(0);
    expect(spectatorPanel().classList.contains('hidden')).toBe(true);
  });
});

describe('Spectator game over + clearAll', () => {
  beforeEach(() => {
    mountSpectatorDom();
  });

  afterEach(() => {
    document.body.innerHTML = '';
  });

  it('gameOver shows the overlay with the server message and a per-game replay link', () => {
    const s = new Spectator();
    s.addReason('Alice', 'move', 'x');

    s.gameOver('Alice', 'game-42', 'Alice crushed the enemy');

    expect(document.getElementById('game-over-overlay')!.classList.contains('hidden')).toBe(false);
    expect(document.getElementById('game-over-title')!.textContent).toBe('Alice crushed the enemy');
    // The replay link targets this specific game (spectator replay route).
    expect(document.getElementById('replay-link')!.getAttribute('href')).toBe(
      '/replay?game_id=game-42',
    );
    // The reason log was cleared and the spectator panel hidden.
    expect(reasonEntries()).toHaveLength(0);
    expect(spectatorPanel().classList.contains('hidden')).toBe(true);
  });

  it('gameOver without a message falls back to the winner title', () => {
    const s = new Spectator();
    s.gameOver('Bob', 'g1');

    expect(document.getElementById('game-over-title')!.textContent).toBe('🏆 Bob Wins!');
  });

  it('clearAll hides the overlay, clears the log, and shows the stats panel', () => {
    const s = new Spectator();
    s.addReason('Alice', 'move', 'x');
    s.gameOver('Alice', 'g9');

    expect(document.getElementById('game-over-overlay')!.classList.contains('hidden')).toBe(false);

    s.clearAll();

    expect(document.getElementById('game-over-overlay')!.classList.contains('hidden')).toBe(true);
    expect(reasonEntries()).toHaveLength(0);
    expect(spectatorPanel().classList.contains('hidden')).toBe(true);
    expect(document.getElementById('stats-panel')!.classList.contains('hidden')).toBe(false);
  });
});
