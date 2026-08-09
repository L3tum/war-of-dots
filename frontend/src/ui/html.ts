//! HTML helpers shared across UI modules.

/** Escape HTML entities to prevent XSS when embedding user/LM-controlled data in innerHTML */
export function escapeHtml(text: string): string {
  const div = document.createElement('div');
  div.textContent = text;
  return div.innerHTML;
}
