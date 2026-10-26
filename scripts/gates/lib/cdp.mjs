/**
 * Headless Chrome over the DevTools protocol, with no dependency: Chrome runs
 * with `--remote-debugging-pipe` (commands on fd 3, events on fd 4,
 * NUL-separated JSON), so no port is opened.
 *
 * Every launch uses a fresh temp profile and `--use-mock-keychain` (a foreign
 * profile otherwise pops a keychain dialog on the user's screen), disables
 * extensions, sync, background networking and component updates, and is
 * ended with `Browser.close`, then SIGTERM to the process group it started —
 * never SIGKILL. The profile is removed afterwards.
 */

import { spawn } from 'node:child_process';
import { existsSync, mkdtempSync, rmSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { join } from 'node:path';

export const DEFAULT_CHROME = '/Applications/Google Chrome.app/Contents/MacOS/Google Chrome';

export function chromePath(explicit = null) {
  const p = explicit ?? process.env.CHROME_PATH ?? DEFAULT_CHROME;
  return existsSync(p) ? p : null;
}

export class Cdp {
  constructor(child) {
    this.child = child;
    this.nextId = 1;
    this.pending = new Map();
    this.listeners = new Map();
    let buf = Buffer.alloc(0);
    child.stdio[4].on('data', (chunk) => {
      buf = Buffer.concat([buf, chunk]);
      let at;
      while ((at = buf.indexOf(0)) >= 0) {
        const msg = JSON.parse(buf.subarray(0, at).toString('utf8'));
        buf = buf.subarray(at + 1);
        this.dispatch(msg);
      }
    });
    child.once('exit', () => {
      for (const { reject } of this.pending.values()) reject(new Error('Chrome exited'));
      this.pending.clear();
    });
  }

  dispatch(msg) {
    if (msg.id !== undefined) {
      const p = this.pending.get(msg.id);
      if (!p) return;
      this.pending.delete(msg.id);
      if (msg.error) p.reject(new Error(`${p.method}: ${msg.error.message}`));
      else p.resolve(msg.result);
      return;
    }
    for (const fn of this.listeners.get(msg.method) ?? []) fn(msg.params ?? {}, msg.sessionId ?? null);
  }

  send(method, params = {}, sessionId = null, timeoutMs = 30_000) {
    const id = this.nextId++;
    const message = { id, method, params, ...(sessionId ? { sessionId } : {}) };
    return new Promise((resolve, reject) => {
      const timer = setTimeout(() => {
        this.pending.delete(id);
        reject(new Error(`${method}: no answer in ${timeoutMs} ms`));
      }, timeoutMs);
      this.pending.set(id, {
        method,
        resolve: (v) => {
          clearTimeout(timer);
          resolve(v);
        },
        reject: (e) => {
          clearTimeout(timer);
          reject(e);
        },
      });
      this.child.stdio[3].write(JSON.stringify(message) + '\0');
    });
  }

  on(method, fn) {
    if (!this.listeners.has(method)) this.listeners.set(method, []);
    this.listeners.get(method).push(fn);
  }
}

/** Launch Chrome headless with an isolated profile. Returns `{ cdp, close }`. */
export async function launchChrome({ chrome, width = 1440, height = 900 } = {}) {
  const path = chromePath(chrome);
  if (!path) throw new Error('Chrome not found (pass --chrome or set CHROME_PATH)');
  const profile = mkdtempSync(join(tmpdir(), 'gilde-gates-chrome-'));
  const args = [
    '--headless=new', '--remote-debugging-pipe', `--user-data-dir=${profile}`, '--use-mock-keychain', '--password-store=basic',
    '--no-first-run', '--no-default-browser-check', '--disable-extensions', '--disable-background-networking', '--disable-sync',
    '--disable-component-update', '--disable-default-apps', '--mute-audio', '--hide-scrollbars', '--force-color-profile=srgb',
    `--window-size=${width},${height}`, 'about:blank',
  ];
  const child = spawn(path, args, { stdio: ['ignore', 'ignore', 'pipe', 'pipe', 'pipe'], detached: true });
  child.stdio[2].on('data', () => {});
  const exited = new Promise((r) => child.once('exit', r));
  const cdp = new Cdp(child);
  await cdp.send('Browser.getVersion', {}, null, 60_000);
  const close = async () => {
    try {
      await cdp.send('Browser.close', {}, null, 5000);
    } catch {
      // already gone
    }
    const done = await Promise.race([exited.then(() => true), new Promise((r) => setTimeout(() => r(false), 5000))]);
    if (!done) {
      try {
        process.kill(-child.pid, 'SIGTERM');
      } catch {
        // gone
      }
      await Promise.race([exited, new Promise((r) => setTimeout(r, 5000))]);
    }
    rmSync(profile, { recursive: true, force: true });
  };
  return { cdp, close };
}

/** A page (target) with its session: navigate, emulate, evaluate, key, screenshot. */
export class Page {
  static async open(cdp) {
    const { targetId } = await cdp.send('Target.createTarget', { url: 'about:blank' });
    const { sessionId } = await cdp.send('Target.attachToTarget', { targetId, flatten: true });
    const page = new Page(cdp, targetId, sessionId);
    await page.send('Page.enable');
    await page.send('Runtime.enable');
    // As in the app: Tauri's `freezePrototype` (contract §9.2) runs before any
    // page script, so code that assigns over an `Object.prototype` member
    // (the "override mistake") throws here as it would in the WebView.
    await page.send('Page.addScriptToEvaluateOnNewDocument', { source: 'Object.freeze(Object.prototype);' });
    cdp.on('Runtime.exceptionThrown', (p, s) => {
      if (s === sessionId) page.exceptions.push(p.exceptionDetails?.exception?.description?.split('\n')[0] ?? p.exceptionDetails?.text ?? 'exception');
    });
    cdp.on('Runtime.consoleAPICalled', (p, s) => {
      if (s === sessionId && p.type === 'error') page.consoleErrors += 1;
    });
    return page;
  }

  constructor(cdp, targetId, sessionId) {
    Object.assign(this, { cdp, targetId, sessionId });
    this.exceptions = [];
    this.consoleErrors = 0;
  }

  send(method, params = {}, timeoutMs) {
    return this.cdp.send(method, params, this.sessionId, timeoutMs);
  }

  async viewport(width, height, theme = 'light') {
    await this.send('Emulation.setDeviceMetricsOverride', { width, height, deviceScaleFactor: 1, mobile: false });
    await this.send('Emulation.setEmulatedMedia', { features: [{ name: 'prefers-color-scheme', value: theme }, { name: 'prefers-reduced-motion', value: 'reduce' }] });
  }

  async goto(url) {
    this.exceptions = [];
    this.consoleErrors = 0;
    await this.send('Page.navigate', { url });
  }

  async evaluate(expression, timeoutMs = 30_000) {
    const r = await this.send('Runtime.evaluate', { expression, returnByValue: true, awaitPromise: true }, timeoutMs);
    if (r.exceptionDetails) throw new Error(`page: ${r.exceptionDetails.exception?.description?.split('\n')[0] ?? r.exceptionDetails.text}`);
    return r.result.value;
  }

  /** Poll `expression` until it is truthy; returns its value or null on timeout. */
  async waitFor(expression, timeoutMs = 20_000) {
    const until = Date.now() + timeoutMs;
    while (Date.now() < until) {
      try {
        const v = await this.evaluate(expression, 5000);
        if (v) return v;
      } catch {
        // navigating
      }
      await new Promise((r) => setTimeout(r, 100));
    }
    return null;
  }

  /** One key press: `key` like "Enter", "Escape", "Tab", "1"; `modifiers` bitmask (Alt 1, Ctrl 2, Meta 4, Shift 8). */
  async key(key, { code = null, modifiers = 0, text = null } = {}) {
    const k = { key, code: code ?? (key.length === 1 ? (/[0-9]/.test(key) ? `Digit${key}` : `Key${key.toUpperCase()}`) : key), modifiers, windowsVirtualKeyCode: KEYCODES[key] ?? key.toUpperCase().charCodeAt(0) };
    await this.send('Input.dispatchKeyEvent', { type: 'keyDown', ...k, ...(text ? { text } : {}) });
    await this.send('Input.dispatchKeyEvent', { type: 'keyUp', ...k });
  }

  async screenshot() {
    const { data } = await this.send('Page.captureScreenshot', { format: 'png' }, 60_000);
    return Buffer.from(data, 'base64');
  }

  async close() {
    try {
      await this.cdp.send('Target.closeTarget', { targetId: this.targetId });
    } catch {
      // gone
    }
  }
}

const KEYCODES = { Enter: 13, Escape: 27, Tab: 9, Backspace: 8, ArrowUp: 38, ArrowDown: 40, ArrowLeft: 37, ArrowRight: 39, ' ': 32 };
