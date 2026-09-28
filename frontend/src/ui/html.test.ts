// escapeHtml tests — the single XSS defense for LLM + user-controlled data
// embedded in innerHTML across the UI modules (lobby, spectator, advisor,
// unit_info, main's error overlay).

import { describe, it, expect } from 'vitest';
import { escapeHtml } from './html';

describe('escapeHtml', () => {
  it('escapes < to &lt;', () => {
    expect(escapeHtml('<')).toBe('&lt;');
  });

  it('escapes > to &gt;', () => {
    expect(escapeHtml('>')).toBe('&gt;');
  });

  it('escapes & to &amp;', () => {
    expect(escapeHtml('&')).toBe('&amp;');
  });

  it('passes quotes through unchanged — a text node never needs quote escaping', () => {
    // Per the HTML serialization spec, text content escapes only &, <, >.
    // Quotes are safe in a text context (they only matter inside attribute
    // values, which innerHTML-embedded text never becomes once < is escaped).
    expect(escapeHtml('"')).toBe('"');
    expect(escapeHtml("'")).toBe("'");
  });

  it('neutralizes a full <script> tag', () => {
    expect(escapeHtml('<script>alert(1)</script>')).toBe('&lt;script&gt;alert(1)&lt;/script&gt;');
  });

  it('neutralizes an attribute-injection payload', () => {
    // The leading quote survives (text context) but the tag it opens is
    // escaped, so it can never break out into an attribute or a new element.
    expect(escapeHtml('"><img src=x onerror=alert(1)>')).toBe(
      '"&gt;&lt;img src=x onerror=alert(1)&gt;',
    );
  });

  it('passes plain text through unchanged', () => {
    expect(escapeHtml('hello world 100%')).toBe('hello world 100%');
    expect(escapeHtml('')).toBe('');
    expect(escapeHtml('line one\nline two')).toBe('line one\nline two');
  });

  it('keeps the escaped form inert when re-parsed as HTML', () => {
    // The round-trip that matters for XSS: embedding the escaped output in a
    // real container must not create a <script> element.
    const payload = '<script id="xss-probe">alert(1)</script>';
    const div = document.createElement('div');
    div.innerHTML = escapeHtml(payload);
    expect(div.querySelector('script')).toBeNull();
    expect(div.textContent).toBe(payload);
  });
});
