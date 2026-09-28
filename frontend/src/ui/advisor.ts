// Advisor panel — displays advisor suggestions and manages the enable/disable toggle.

import { escapeHtml } from './html';

export class Advisor {
  private panel: HTMLDivElement;
  private statsPanel: HTMLDivElement | null;

  constructor() {
    this.panel = document.getElementById('advisor-panel') as HTMLDivElement;
    this.statsPanel = document.getElementById('stats-panel') as HTMLDivElement | null;
  }

  /** Show an advisor suggestion */
  show(text: string) {
    this.panel.classList.remove('hidden');
    // The advisor text is LM-generated → escape it before injecting into innerHTML.
    this.panel.innerHTML = `<h4>Advisor</h4><p>${escapeHtml(text)}</p>`;
  }

  /** Hide the advisor panel */
  hide() {
    this.panel.classList.add('hidden');
  }

  /** Add the advisor toggle button to the stats panel (if it exists) */
  setupToggle() {
    if (!this.statsPanel || document.getElementById('advisor-toggle-btn')) return;

    const toggleDiv = document.createElement('div');
    toggleDiv.innerHTML = `
      <button id="advisor-toggle-btn" style="margin-top: 8px; padding: 6px 12px; cursor: pointer; background: #444; color: #fff; border: 1px solid #555; border-radius: 4px;">
        Enable Advisor
      </button>
    `;
    this.statsPanel.appendChild(toggleDiv);

    const btn = document.getElementById('advisor-toggle-btn')!;
    btn.addEventListener('click', () => {
      const enabled = btn.textContent === 'Enable Advisor';
      btn.textContent = enabled ? 'Disable Advisor' : 'Enable Advisor';
      // Dispatch a custom event that the connection will listen to
      window.dispatchEvent(new CustomEvent('advisor-toggle', { detail: { enabled } }));
    });
  }
}
