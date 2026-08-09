//! Spectator view — manages the spectator panel with player filters and reasoning log.

import { escapeHtml } from './html';

export class Spectator {
  private reasonsContainer: HTMLDivElement;
  private playersContainer: HTMLDivElement;
  private playerReasons: Map<string, Array<{ action: string; reason: string }>> = new Map();
  private currentFilter: string | null = null;

  constructor() {
    this.reasonsContainer = document.getElementById('spectator-reasons') as HTMLDivElement;
    this.playersContainer = document.getElementById('spectator-players') as HTMLDivElement;
  }

  /** Add a reason entry for a player */
  addReason(player: string, action: string, reason: string) {
    if (!this.playerReasons.has(player)) {
      this.playerReasons.set(player, []);
    }
    const list = this.playerReasons.get(player)!;
    list.push({ action, reason });
    // Trim to last 100 per player
    if (list.length > 100) {
      list.splice(0, list.length - 100);
    }
    this.render();
    // Reveal the spectator panel
    const panel = document.getElementById('spectator-panel');
    if (panel) panel.classList.remove('hidden');
  }

  /** Clear all reason logs and hide the panel (game ended) */
  clear() {
    this.playerReasons.clear();
    this.currentFilter = null;
    this.reasonsContainer.innerHTML = '';
    this.playersContainer.innerHTML = '';
    const panel = document.getElementById('spectator-panel');
    if (panel) {
      panel.classList.add('hidden');
    }
  }

  /** Render the spectator log, filtered by the current player (or all) */
  private render() {
    // Build player filter buttons
    const allPlayerNames = Array.from(this.playerReasons.keys());
    this.playersContainer.innerHTML = '';

    // "All" button
    const allBtn = document.createElement('button');
    allBtn.textContent = 'All';
    allBtn.style.padding = '4px 8px';
    allBtn.style.margin = '2px';
    allBtn.style.background = this.currentFilter === null ? '#555' : '#333';
    allBtn.style.border = '1px solid #555';
    allBtn.style.borderRadius = '4px';
    allBtn.style.color = '#fff';
    allBtn.style.cursor = 'pointer';
    allBtn.addEventListener('click', () => {
      this.currentFilter = null;
      this.render();
    });
    this.playersContainer.appendChild(allBtn);

    // Player buttons
    for (const name of allPlayerNames) {
      const btn = document.createElement('button');
      btn.textContent = name;
      btn.style.padding = '4px 8px';
      btn.style.margin = '2px';
      btn.style.background = this.currentFilter === name ? '#555' : '#333';
      btn.style.border = '1px solid #555';
      btn.style.borderRadius = '4px';
      btn.style.color = '#fff';
      btn.style.cursor = 'pointer';
      btn.addEventListener('click', () => {
        this.currentFilter = name;
        this.render();
      });
      this.playersContainer.appendChild(btn);
    }

    // Show reasons
    this.reasonsContainer.innerHTML = '';
    if (this.currentFilter) {
      const list = this.playerReasons.get(this.currentFilter);
      if (list) {
        for (const entry of [...list].reverse()) {
          const div = document.createElement('div');
          div.className = 'player-reason';
          div.innerHTML = `<strong>${escapeHtml(this.currentFilter)}:</strong> ${escapeHtml(entry.action)}<br><em>${escapeHtml(entry.reason)}</em>`;
          this.reasonsContainer.appendChild(div);
        }
      }
    } else {
      // Show all players' entries combined (newest first)
      const allEntries: Array<{ player: string; action: string; reason: string }> = [];
      for (const [player, entries] of this.playerReasons.entries()) {
        for (const entry of entries) {
          allEntries.push({ player, ...entry });
        }
      }
      allEntries.reverse();
      for (const entry of allEntries) {
        const div = document.createElement('div');
        div.className = 'player-reason';
        div.innerHTML = `<strong>${escapeHtml(entry.player)}:</strong> ${escapeHtml(entry.action)}<br><em>${escapeHtml(entry.reason)}</em>`;
        this.reasonsContainer.appendChild(div);
      }
    }
  }

  /** Show the game over overlay */
  gameOver(winner: string, game_id: string, message?: string) {
    // Clear reason logs and hide spectator panel
    this.playerReasons.clear();
    this.currentFilter = null;
    this.reasonsContainer.innerHTML = '';
    this.playersContainer.innerHTML = '';
    const panel = document.getElementById('spectator-panel');
    if (panel) panel.classList.add('hidden');

    // Show game over overlay
    const overlay = document.getElementById('game-over-overlay');
    if (overlay) {
      overlay.classList.remove('hidden');
      const title = document.getElementById('game-over-title');
      if (title) {
        // Use the server message if available, otherwise show winner
        title.textContent = message || `🏆 ${winner} Wins!`;
      }
      const replayLink = document.getElementById('replay-link') as HTMLAnchorElement;
      if (replayLink) {
        replayLink.href = `/replay?game_id=${encodeURIComponent(game_id)}`;
      }
    }
  }

  /** Clear everything including game over overlay (used for ClearSpectator) */
  clearAll() {
    // Hide game over overlay
    const overlay = document.getElementById('game-over-overlay');
    if (overlay) overlay.classList.add('hidden');
    // Clear spectator
    this.clear();
    // Show stats panel
    const statsPanel = document.getElementById('stats-panel');
    if (statsPanel) statsPanel.classList.remove('hidden');
  }
}
