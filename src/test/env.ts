/**
 * Test environment, imported before anything else in `all.ts`.
 *
 * Installs a happy-dom window as the global DOM so React components, focus,
 * keyboard events and live regions can be tested on Node (contract §14.2
 * T-ui-*, ux-ia §10 Testability; happy-dom and @testing-library/react are the
 * deliberate additions to the dev allowlist).
 *
 * Node's own `navigator` is kept: its `language` follows `LANG`, and the gate
 * runs with `LANG=ko_KR.UTF-8` because Korean is the product default.
 */

import { Window } from 'happy-dom';

const win = new Window({
  url: 'http://localhost/',
  width: 1440,
  height: 900,
  settings: {
    disableJavaScriptEvaluation: true,
    disableJavaScriptFileLoading: true,
    disableCSSFileLoading: true,
    disableIframePageLoading: true,
  },
});

const g = globalThis as unknown as Record<string, unknown>;
const w = win as unknown as Record<string, unknown>;

// DOM classes Node also defines, which must come from the window's realm for
// `instanceof` checks and `dispatchEvent` to work.
const FROM_WINDOW = new Set([
  'Event',
  'EventTarget',
  'CustomEvent',
  'MessageEvent',
  'ErrorEvent',
  'DOMException',
]);

for (const name of Object.getOwnPropertyNames(win)) {
  if (name in g && !FROM_WINDOW.has(name)) continue;
  try {
    Object.defineProperty(g, name, { value: w[name], configurable: true, writable: true });
  } catch {
    // A non-configurable global of Node's own: keep Node's.
  }
}

Object.defineProperty(g, 'window', { value: win, configurable: true, writable: true });
Object.defineProperty(g, 'document', { value: win.document, configurable: true, writable: true });
if (typeof g.navigator === 'undefined') {
  Object.defineProperty(g, 'navigator', {
    value: { language: 'ko-KR', userAgent: '' },
    configurable: true,
    writable: true,
  });
}

// Tell React that tests wrap their updates in act().
g.IS_REACT_ACT_ENVIRONMENT = true;

/** The happy-dom window, for tests that need its control API. */
export const testWindow = win;
