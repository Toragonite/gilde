/**
 * Test environment shim, imported before anything else in `all.ts`.
 *
 * Node has no `navigator`, so `defaultLocale()` would pick English and the
 * store would be created with an English initial state. The product default on
 * the target machine is Korean (DECISIONS: 사용자에게는 항상 한국어), so the
 * tests run against that.
 */

if (typeof globalThis.navigator === 'undefined') {
  Object.defineProperty(globalThis, 'navigator', {
    // T20: `userAgent` is part of the shim because xterm.js reads it at module
    // scope (`navigator.userAgent.indexOf('Firefox')`) and the terminal module
    // is now in the bundle's import graph. An empty string is the honest value
    // for "this is not a browser".
    value: { language: 'ko-KR', userAgent: '' },
    configurable: true,
    writable: true,
  });
}

export {};
