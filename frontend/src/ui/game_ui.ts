// DOM helpers for connection/game-flow state rendered in index.html.
//
// Pulled out of net/connection.ts so the connection stays transport +
// dispatch only: the elements touched here are all page-level chrome
// (disconnected overlay, lobby code display, game-over overlay, stats panel,
// city labels) that the connection's message handlers used to poke directly.

/** Show/hide the "disconnected" page overlay. */
export function setDisconnectedOverlay(visible: boolean): void {
  const overlay = document.getElementById('disconnected-overlay');
  if (overlay) {
    overlay.style.display = visible ? 'flex' : 'none';
  }
}

/** Hide the lobby panel and reveal the in-game panels (stats + city labels). */
export function transitionToGameUI(lobbyUI: { hide(): void } | null): void {
  // Hide lobby panel
  lobbyUI?.hide();

  // Show stats panel
  const statsPanel = document.getElementById('stats-panel');
  if (statsPanel) statsPanel.classList.remove('hidden');

  // Show city labels container
  const cityLabels = document.getElementById('city-labels');
  if (cityLabels) cityLabels.classList.remove('hidden');
}

/**
 * Enter the lobby waiting room for a known lobby code: hide any game-over
 * overlay, set the lobby-code display (so the player can read/share the
 * code), and switch the lobby UI to the info tab.
 */
export function showLobbyInfo(
  lobbyCode: string,
  lobbyUI: { showTab(tab: string): void } | null,
): void {
  // Hide game-over overlay if showing
  const overlay = document.getElementById('game-over-overlay');
  if (overlay) overlay.classList.add('hidden');

  // Set lobby code and show waiting room
  const codeDisplay = document.getElementById('lobby-code-display');
  if (codeDisplay) codeDisplay.textContent = lobbyCode;

  // Delegate to LobbyUI for tab/panel management
  lobbyUI?.showTab('info');
}
