/**
 * Terminal-mode probe sessions (the PTY part of `probes/scripts/host.py`).
 *
 * A `PtySession` runs claude in a pseudo-terminal through `guardedSpawn`
 * (`pty: {rows, cols}`; macOS script(1) behind a pipe). The screen output is
 * kept in memory as ANSI-stripped text for `waitText` and dropped with the
 * session; the record holds byte counts, the named patterns waited for and
 * whether they matched, and the keys sent by name (digit, Enter, Esc, arrows,
 * `text(n)`), never screen or prompt text.
 */

import { performance } from 'node:perf_hooks';

import { endGroup, guardedSpawn } from './guard.mjs';

const KEEP = 256 * 1024;

/** Strip CSI/OSC/DCS sequences and other escapes, keep printable text and newlines. */
export function stripAnsi(text) {
  return text
    .replace(/\u001b\][^\u0007\u001b]*(\u0007|\u001b\\)/g, '')
    .replace(/\u001bP[^\u001b]*\u001b\\/g, '')
    .replace(/\u001b\[[0-9;?<>=!]*[ -/]*[@-~]/g, ' ')
    .replace(/\u001b[()][A-Za-z0-9]/g, '')
    .replace(/\u001b[=>78DEHMNOZc]/g, '')
    .replace(/[\u0000-\u0008\u000b\u000c\u000e-\u001f\u007f]/g, '')
    .replace(/\r\n?/g, '\n');
}

/** A key description for the record (never typed text). */
export function describeKeys(data) {
  const names = { '\r': 'Enter', '\u001b': 'Esc', '\u001b[A': 'Up', '\u001b[B': 'Down', '\u001b[C': 'Right', '\u001b[D': 'Left', '\t': 'Tab', '\u0003': 'Ctrl-C', '\u001a': 'Ctrl-Z' };
  if (names[data]) return names[data];
  if (/^[0-9]$/.test(data)) return `digit:${data}`;
  if (data === '/exit\r') return 'slash-exit';
  if (data.startsWith('\u001b[200~')) return `paste(${data.length})`;
  return `text(${data.length})`;
}

export class PtySession {
  constructor({ name, binary, args, cwd, env, recorder, guard, rows = 40, cols = 120 }) {
    Object.assign(this, { name, binary, args, cwd, env, recorder, guard, rows, cols });
    this.text = '';
    this.bytes = 0;
    this.exitCode = null;
    this.waiters = new Set();
  }

  t() {
    return Math.round(performance.now() - this.t0) / 1000;
  }

  start() {
    const { child, binary } = guardedSpawn(this.binary, this.args, { env: this.env, cwd: this.cwd, ...this.guard, pty: { rows: this.rows, cols: this.cols } });
    this.child = child;
    this.t0 = performance.now();
    this.exited = new Promise((resolve) => child.once('exit', resolve));
    this.recorder?.spawn(0, { name: this.name, argv: [binary, ...this.args], envKeys: Object.keys(this.env), configKind: this.guard?.configKind ?? null, cwd: this.cwd, pid: child.pid, pgid: child.pid });
    child.stdout.on('data', (chunk) => {
      this.bytes += chunk.length;
      this.text = (this.text + stripAnsi(chunk.toString('utf8'))).slice(-KEEP);
      for (const w of [...this.waiters]) w();
    });
    child.stderr.on('data', () => {});
    child.stdin.on('error', () => {});
    child.once('exit', (code, signal) => {
      this.exitCode = code ?? (signal ? 128 : null);
      this.recorder?.write({ t: this.t(), dir: 'exit', code, signal, bytes: this.bytes });
      for (const w of [...this.waiters]) w();
    });
    return this;
  }

  write(data) {
    this.recorder?.write({ t: this.t(), dir: 'pty-in', key: describeKeys(data) });
    try {
      this.child.stdin.write(data);
    } catch {
      // the session ended
    }
  }

  /** Bracketed paste followed by a separate CR (R6 §4). */
  paste(text) {
    this.write(`\u001b[200~${text}\u001b[201~`);
    setTimeout(() => this.write('\r'), 150);
  }

  /** Wait until the screen text since `from` matches `re`; records the pattern name and the result. */
  waitText(name, re, timeoutMs = 60_000, { from = 0 } = {}) {
    return new Promise((resolve) => {
      const finish = (hit) => {
        this.waiters.delete(check);
        clearTimeout(timer);
        this.recorder?.write({ t: this.t(), dir: 'wait', pattern: name, hit });
        resolve(hit);
      };
      const check = () => {
        if (re.test(this.text.slice(from))) finish(true);
        else if (this.exitCode !== null) finish(false);
      };
      const timer = setTimeout(() => finish(false), timeoutMs);
      this.waiters.add(check);
      check();
    });
  }

  /** The current text length, to wait only for what comes after it. */
  mark() {
    return this.text.length;
  }

  async finish({ exitCommand = true } = {}) {
    if (exitCommand && this.exitCode === null) this.write('/exit\r');
    const r = await endGroup(this.child, this.exited, { graceMs: 5000 });
    if (r.survivor) this.recorder?.note(this.t(), 'PTY process group survived SIGTERM; reported, never SIGKILL', { pid: this.child.pid });
    return r;
  }
}
