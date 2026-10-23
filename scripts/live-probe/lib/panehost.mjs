/**
 * The hosted-team stand-in for probes that need panes before Gilde's own
 * PaneHost (T07) exists: a port of `probes/scripts/host.py`'s tmux surface and
 * of the logging shim `probes/scripts/tmux` (contract §14.4: "their
 * CLI-behaviour parts may run against … a stand-in harness").
 *
 *  - `writeShim(dir, …)` writes `<dir>/tmux`: one JSON line per call to
 *    `GILDE_SHIM_LOG` (argv redacted, env-name presence only, route, exit
 *    code, ms); `-V` → `tmux 3.4`; `-L claude-swarm-*` → the pane host
 *    socket; anything else → exit 127 (no real tmux is ever run).
 *  - `PaneHost` answers the §7.4.4 subcommands for `-L claude-swarm-<pid>`,
 *    logs any other subcommand as a deviation, and runs each `respawn-pane`
 *    payload as `/bin/sh -c <payload>` in a PTY — after the OQ1 guard checked
 *    every `CLAUDE_CONFIG_DIR` the payload sets and the binary it names.
 *  - `watchAccount` samples the registry, team configs and inboxes every
 *    200 ms: key sets and allowlisted fields only (status, waitingFor, kind,
 *    entrypoint, inner frame type, tool name), never message text; `*.key`
 *    files are never matched.
 * Pane processes are ended with SIGHUP on kill-pane (Close pane semantics),
 * SIGTERM after 2 s, never SIGKILL.
 */

import { spawn } from 'node:child_process';
import { chmodSync, existsSync, readFileSync, readdirSync, unlinkSync, writeFileSync } from 'node:fs';
import { createServer } from 'node:net';
import { join } from 'node:path';

import { GuardError, checkBinary, checkConfigDir, checkEnv, cleanEnv, ptyArgv, shQuote } from './guard.mjs';
import { redactArgv, redactCommand, shellWords } from './redact.mjs';

/** Env names whose presence (never value) the shim logs. */
export const SHIM_ENV_NAMES = ['TMUX', 'TMUX_PANE', 'ITERM_SESSION_ID', 'ITERM_PROFILE', 'LC_TERMINAL', 'TERM_PROGRAM', 'GILDE_PANE_HOST', 'CLAUDE_CONFIG_DIR', 'CLAUDECODE', 'CLAUDE_CODE_EXPERIMENTAL_AGENT_TEAMS', 'CLAUDE_CODE_ENTRYPOINT'];

/** The logging shim as a Node script (absolute interpreter, so PATH need not hold node). */
export function shimSource(nodePath) {
  return `#!${nodePath}
// Logging tmux shim of the Gilde 2 live-probe kit (scripts/live-probe/lib/panehost.mjs).
'use strict';
const fs = require('fs'), net = require('net');
const argv = process.argv.slice(2);
const t0 = Date.now();
const NAMES = ${JSON.stringify(SHIM_ENV_NAMES)};
const rec = { ts: t0, argv: argv.length > 3 && argv[2] === 'respawn-pane' ? argv.slice(0, -1).concat(['<payload>']) : argv, cwd: process.cwd(), pid: process.pid, ppid: process.ppid,
  envPresent: Object.fromEntries(NAMES.map((k) => [k, k in process.env])), termProgram: process.env.TERM_PROGRAM || null };
function done(out, err, code) {
  rec.code = code; rec.ms = Date.now() - t0;
  try { if (process.env.GILDE_SHIM_LOG) fs.appendFileSync(process.env.GILDE_SHIM_LOG, JSON.stringify(rec) + '\\n'); } catch (e) {}
  process.stdout.write(out); process.stderr.write(err); process.exitCode = code;
}
if (argv.length === 1 && argv[0] === '-V') { rec.route = 'version'; done('tmux 3.4\\n', '', 0); }
else if (argv[0] === '-L' && /^claude-swarm-/.test(argv[1] || '') && process.env.GILDE_PANE_HOST) {
  rec.route = 'host';
  const c = net.connect(process.env.GILDE_PANE_HOST);
  let buf = '';
  c.on('connect', () => c.write(JSON.stringify({ op: 'tmux', argv, cwd: process.cwd(), env: process.env }) + '\\n'));
  c.on('data', (d) => { buf += d; });
  c.on('end', () => { try { const r = JSON.parse(buf); done(r.stdout || '', r.stderr || '', r.code | 0); } catch (e) { rec.route = 'host-error'; done('', 'gilde-tmux: bad host reply\\n', 1); } });
  c.on('error', () => { rec.route = 'host-error'; done('', 'gilde-tmux: Gilde is not reachable and tmux is not installed\\n', 1); });
} else { rec.route = 'passthrough-unavailable'; done('', 'tmux: not installed (probe shim; real tmux is never run)\\n', 127); }
`;
}

export function writeShim(dir, nodePath = process.execPath) {
  const path = join(dir, 'tmux');
  writeFileSync(path, shimSource(nodePath));
  chmodSync(path, 0o755);
  return path;
}

/** Parse tmux-style flags: `spec` = flags that take a value. */
function flags(rest, spec) {
  const d = { pos: [] };
  let tail = null;
  for (let i = 0; i < rest.length; i++) {
    const x = rest[i];
    if (x === '--') {
      tail = rest.slice(i + 1);
      break;
    }
    if (x.startsWith('-') && x.length > 1) {
      if (spec.has(x)) {
        d[x] = rest[i + 1] ?? null;
        i++;
      } else d[x] = true;
      continue;
    }
    d.pos.push(x);
  }
  return { d, tail };
}

export class PaneHost {
  /**
   * `paneEnv(leadEnv)` builds the env of a pane process from the lead's env
   * (the caller keeps only allowlisted keys and sets DISABLE_AUTOUPDATER=1);
   * the result is checked with `checkEnv` before every pane spawn. `guard`
   * holds the OQ1 options (scratchRoots, dryRun, fakesDir).
   */
  constructor({ socketPath, recorder, guard, paneEnv }) {
    Object.assign(this, { socketPath, recorder, guard, paneEnv });
    this.servers = new Map();
    this.panes = new Map();
    this.deviations = 0;
    this.calls = 0;
    /** Redacted pane commands, in spawn order (§9.7: the command is never kept raw). */
    this.paneCommands = [];
    this.stopped = null;
  }

  start() {
    if (existsSync(this.socketPath)) unlinkSync(this.socketPath);
    this.server = createServer((c) => {
      let buf = '';
      c.on('data', (d) => {
        buf += d;
        if (!buf.endsWith('\n')) return;
        let reply;
        try {
          const req = JSON.parse(buf);
          reply = req.op === 'tmux' ? this.tmux(req.argv, req.env ?? {}, req.cwd) : { stdout: '', stderr: 'unknown op\n', code: 1 };
        } catch (e) {
          reply = { stdout: '', stderr: `gilde-tmux: ${e instanceof GuardError ? e.message : 'bad request'}\n`, code: 1 };
        }
        c.end(JSON.stringify(reply));
      });
    });
    return new Promise((resolve) => this.server.listen(this.socketPath, () => {
      chmodSync(this.socketPath, 0o600);
      resolve(this);
    }));
  }

  log(rec) {
    this.recorder?.write({ dir: 'tmux', ...rec });
  }

  /** One shim call: `argv` = `-L claude-swarm-<pid> <sub> …`. */
  tmux(argv, env, cwd) {
    this.calls++;
    const sock = argv[1];
    const [sub, ...rest] = argv.slice(2);
    const S = this.servers.get(sock) ?? { session: false, windows: [], panes: new Map(), nextPane: 0, nextWindow: 0, env: null, cwd };
    this.servers.set(sock, S);
    const reply = (stdout, stderr, code) => {
      this.log({ sub: sub ?? null, argv: sub === 'respawn-pane' ? argv.slice(0, -1).concat(['<payload>']) : argv, code });
      return { stdout, stderr, code };
    };
    const newPane = (win) => {
      const id = `%${S.nextPane++}`;
      S.panes.set(id, { id, window: win.id, title: null, state: 'placeholder', session: null });
      win.panes.push(id);
      return id;
    };
    const findWindow = (t) => {
      if (!t) return null;
      if (t.startsWith('%')) {
        const p = S.panes.get(t);
        return p ? S.windows.find((w) => w.id === p.window) ?? null : null;
      }
      const [sess, wname] = t.split(':');
      if (sess !== 'claude-swarm' || !S.session) return null;
      return wname ? S.windows.find((w) => w.name === wname || w.id === wname) ?? null : S.windows[0] ?? null;
    };
    const live = () => [...S.panes.values()].filter((p) => p.state !== 'closed').length;
    switch (sub) {
      case 'has-session':
        return S.session ? reply('', '', 0) : reply('', "can't find session: claude-swarm\n", 1);
      case 'new-session': {
        const { d } = flags(rest, new Set(['-s', '-n', '-F', '-c', '-x', '-y', '-t']));
        if (S.session) return reply('', 'duplicate session: claude-swarm\n', 1);
        S.session = true;
        S.env = env;
        S.cwd = cwd;
        const win = { id: `@${S.nextWindow++}`, name: d['-n'] ?? '0', panes: [] };
        S.windows.push(win);
        const p = newPane(win);
        return reply(d['-P'] ? `${p}\n` : '', '', 0);
      }
      case 'list-windows':
        return S.session ? reply(S.windows.map((w) => `${w.name}\n`).join(''), '', 0) : reply('', "can't find session: claude-swarm\n", 1);
      case 'new-window': {
        const { d } = flags(rest, new Set(['-t', '-n', '-F', '-c']));
        if (!S.session) return reply('', "can't find session: claude-swarm\n", 1);
        const win = { id: `@${S.nextWindow++}`, name: d['-n'] ?? String(S.nextWindow), panes: [] };
        S.windows.push(win);
        const p = newPane(win);
        return reply(d['-P'] ? `${p}\n` : '', '', 0);
      }
      case 'list-panes': {
        const { d } = flags(rest, new Set(['-t', '-F']));
        const w = findWindow(d['-t']);
        if (!w) return reply('', `can't find window: ${d['-t']}\n`, 1);
        return reply(w.panes.filter((p) => S.panes.get(p)?.state !== 'closed').map((p) => `${p}\n`).join(''), '', 0);
      }
      case 'split-window': {
        const { d } = flags(rest, new Set(['-t', '-l', '-F', '-c']));
        const w = findWindow(d['-t']);
        if (!w) return reply('', `can't find pane: ${d['-t']}\n`, 1);
        if (live() >= 16) return reply('', 'no space for new pane\n', 1);
        const p = newPane(w);
        return reply(d['-P'] ? `${p}\n` : '', '', 0);
      }
      case 'set-option':
      case 'select-layout':
      case 'resize-pane':
        return reply('', '', 0);
      case 'select-pane': {
        const { d } = flags(rest, new Set(['-t', '-T']));
        const p = S.panes.get(d['-t']);
        if (!p) return reply('', `can't find pane: ${d['-t']}\n`, 1);
        p.title = d['-T'] ?? null;
        return reply('', '', 0);
      }
      case 'respawn-pane': {
        const { d, tail } = flags(rest, new Set(['-t', '-c']));
        const p = S.panes.get(d['-t']);
        if (!p) return reply('', `can't find pane: ${d['-t']}\n`, 1);
        if (!tail || tail.length !== 1) return reply('', 'gilde-tmux: respawn-pane expects exactly one command argument after --\n', 1);
        this.spawnPane(sock, p, tail[0], S);
        return reply('', '', 0);
      }
      case 'kill-pane': {
        const { d } = flags(rest, new Set(['-t']));
        const p = S.panes.get(d['-t']);
        if (!p) return reply('', `can't find pane: ${d['-t']}\n`, 1);
        p.state = 'closed';
        if (p.session) this.closePane(p.session);
        return reply('', '', 0);
      }
      default:
        this.deviations++;
        this.log({ deviation: true, sub: sub ?? null, argv: redactArgv(argv, { claude: false }) });
        return { stdout: '', stderr: `gilde-tmux: unsupported command: ${sub ?? '(none)'}\n`, code: 1 };
    }
  }

  /** Check the payload against the OQ1 guard, then run it as `/bin/sh -c` in a PTY. */
  spawnPane(sock, pane, payload, S) {
    const words = shellWords(payload);
    if (!words) throw new GuardError('payload', 'the pane command cannot be parsed');
    const env = this.paneEnv(S.env ?? {});
    checkEnv(env, { dryRun: Boolean(this.guard.dryRun) });
    const assignments = words.filter((w) => /^CLAUDE_CONFIG_DIR=/.test(w)).map((w) => w.slice('CLAUDE_CONFIG_DIR='.length));
    for (const dir of assignments.length ? assignments : [env.CLAUDE_CONFIG_DIR]) checkConfigDir(dir, { home: env.HOME, scratchRoots: this.guard.scratchRoots });
    const bin = words.find((w) => /(^|\/)claude(\.exe)?$/.test(w) || /fake-claude/.test(w));
    if (!bin) throw new GuardError('payload', 'the pane command names no claude binary');
    checkBinary(bin, this.guard);
    const session = startShellPane(`pane-${sock.split('-').pop()}-${pane.id.slice(1)}`, payload, { env, cwd: S.cwd });
    pane.state = 'running';
    pane.session = session;
    this.panes.set(session.name, session);
    const redacted = redactCommand(payload);
    this.paneCommands.push(redacted);
    this.log({ ev: 'pane-command', pane: pane.id, command: redacted, commandLength: payload.length });
  }

  closePane(session) {
    try {
      process.kill(-session.child.pid, 'SIGHUP');
    } catch {
      // gone
    }
    setTimeout(() => {
      if (session.exitCode === null) {
        try {
          process.kill(-session.child.pid, 'SIGTERM');
        } catch {
          // gone
        }
      }
    }, 2000).unref();
  }

  /** End every pane (SIGHUP, then SIGTERM) and close the socket. Idempotent. */
  stop() {
    this.stopped ??= this.stopOnce();
    return this.stopped;
  }

  async stopOnce() {
    for (const s of this.panes.values()) if (s.exitCode === null) this.closePane(s);
    // SIGHUP now, SIGTERM after 2 s (closePane); wait up to 8 s for every pane.
    for (let i = 0; i < 80 && [...this.panes.values()].some((s) => s.exitCode === null); i++) await new Promise((r) => setTimeout(r, 100));
    const survivors = [...this.panes.values()].filter((s) => s.exitCode === null).map((s) => s.name);
    await new Promise((r) => this.server?.close(() => r()) ?? r());
    if (existsSync(this.socketPath)) unlinkSync(this.socketPath);
    return { survivors, calls: this.calls, deviations: this.deviations };
  }
}

/**
 * Start a pane: `/bin/sh -c <payload>` in a 120×40 PTY (`ptyArgv`), in its
 * own process group. Output is counted, never kept.
 */
function startShellPane(name, payload, { env, cwd, rows = 40, cols = 120 }) {
  const [cmd, args] = ptyArgv(`exec /bin/sh -c ${shQuote(payload)}`, { rows, cols });
  // Own keys only, null prototype: the checked env is the env the pane gets.
  const child = spawn(cmd, args, { env: cleanEnv(env), cwd, detached: true, stdio: ['pipe', 'pipe', 'pipe'] });
  const pane = { name, child, bytes: 0, exitCode: null };
  child.stdout.on('data', (chunk) => {
    pane.bytes += chunk.length;
  });
  child.stderr.on('data', () => {});
  child.stdin.on('error', () => {});
  child.once('exit', (code) => {
    pane.exitCode = code ?? 128;
  });
  return pane;
}

/**
 * Sample the account's registry, team configs and inboxes every 200 ms.
 * Records key sets and allowlisted fields; never the text of a message.
 */
export function watchAccount(configDir, recorder, { pids = new Set(), intervalMs = 200, home, scratchRoots } = {}) {
  // The OQ1 rule before anything under the dir is read.
  checkConfigDir(configDir, { home, scratchRoots });
  const last = new Map();
  const seenInbox = new Set();
  const readJson = (p) => {
    try {
      return JSON.parse(readFileSync(p, 'utf8'));
    } catch {
      return null;
    }
  };
  const list = (dir, re) => {
    try {
      return readdirSync(dir).filter((n) => re.test(n));
    } catch {
      return [];
    }
  };
  const tick = () => {
    for (const name of list(join(configDir, 'sessions'), /^\d+\.json$/)) {
      const pid = Number(name.slice(0, -5));
      if (pids.size && !pids.has(pid)) continue;
      const d = readJson(join(configDir, 'sessions', name));
      if (!d) continue;
      const v = JSON.stringify({ status: d.status ?? null, waitingFor: d.waitingFor ?? null, kind: d.kind ?? null, entrypoint: d.entrypoint ?? null, keys: Object.keys(d).sort() });
      if (last.get(`reg:${pid}`) !== v) {
        last.set(`reg:${pid}`, v);
        recorder?.write({ dir: 'watch', w: 'registry', pid, ...JSON.parse(v) });
      }
    }
    for (const team of list(join(configDir, 'teams'), /^[\w.-]+$/)) {
      const cfg = readJson(join(configDir, 'teams', team, 'config.json'));
      if (cfg && Array.isArray(cfg.members)) {
        const v = JSON.stringify(cfg.members.map((m) => ({ name: m.name ?? null, isActive: m.isActive ?? null, tmuxPaneId: m.tmuxPaneId ?? null, backendType: m.backendType ?? null })));
        if (last.get(`cfg:${team}`) !== v) {
          last.set(`cfg:${team}`, v);
          recorder?.write({ dir: 'watch', w: 'config', team, topKeys: Object.keys(cfg).sort(), members: JSON.parse(v) });
        }
      }
      for (const inbox of list(join(configDir, 'teams', team, 'inboxes'), /\.json$/)) {
        const arr = readJson(join(configDir, 'teams', team, 'inboxes', inbox));
        if (!Array.isArray(arr)) continue;
        arr.forEach((e, i) => {
          const id = `${team}/${inbox}/${e?.msg_id ?? i}`;
          if (seenInbox.has(id)) return;
          seenInbox.add(id);
          let inner = null;
          try {
            const j = JSON.parse(e?.text ?? '');
            if (j && typeof j === 'object') inner = { type: j.type ?? null, keys: Object.keys(j).sort(), toolName: typeof j.tool_name === 'string' ? j.tool_name : null, idleReason: typeof j.idleReason === 'string' ? j.idleReason : null };
          } catch {
            inner = null;
          }
          recorder?.write({ dir: 'watch', w: 'inbox', team, inbox, type: e?.type ?? null, keys: e && typeof e === 'object' ? Object.keys(e).sort() : [], read: e?.read ?? null, inner });
        });
      }
    }
  };
  const timer = setInterval(tick, intervalMs);
  tick();
  return () => clearInterval(timer);
}
