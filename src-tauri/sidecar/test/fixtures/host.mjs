/**
 * test/fixtures/host.mjs — a stand-in for the Rust host, shared by the protocol-2
 * suites (`mesh-methods.mjs`, `turns.mjs`, `probes.mjs`).
 *
 * It speaks the sidecar channel exactly as `SidecarManager` does — NDJSON JSON-RPC
 * 2.0, string ids from the host, numeric ids from the sidecar — and answers the
 * supervision seam (`host.registerSpawn` / `host.spawned` / `host.exited` /
 * `host.log`). `host.registerSpawn` applies the same ALLOWLIST the real host does
 * (contract §4.5, §9.3): `cmd` must be one of the configured binaries, else the
 * request is refused and the sidecar never spawns.
 *
 * Every child a test starts is a fake vendor CLI (`fixtures/fake-*.cjs`) — no real
 * vendor CLI is ever run (contract §14.1).
 */

import { spawn } from 'node:child_process';
import { chmodSync, existsSync, mkdtempSync, readFileSync, writeFileSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { dirname, join } from 'node:path';
import { fileURLToPath } from 'node:url';

import { resolveBuildId } from '../../esbuild.mjs';

export const fixturesDir = dirname(fileURLToPath(import.meta.url));
export const sidecarDir = join(fixturesDir, '..', '..');
export const bundle = join(sidecarDir, 'sidecar.cjs');

/** The build id baked into the bundle under test (the same rule as esbuild.mjs). */
export function bundleBuildId() {
  const pkg = JSON.parse(readFileSync(join(sidecarDir, 'package.json'), 'utf8'));
  return resolveBuildId(process.env.GILDE_BUILD_ID, pkg.version);
}

/** `a_` + 16 hex, deterministic per label (an account id needs no real config dir here). */
export function accountId(label) {
  let h = 0n;
  for (const c of `acct:${label}`) h = (h * 131n + BigInt(c.codePointAt(0))) % (1n << 64n);
  return `a_${h.toString(16).padStart(16, '0').slice(-16)}`;
}

/** A session uid of the contract's shape. */
export function sessionUid(n = 1) {
  const tail = String(n).padStart(12, '0');
  return `s_01890a5d-ac96-7a3b-8c4d-${tail}`;
}

/**
 * The fake vendor CLIs, copied into a fresh temp dir with a `#!` line naming THIS
 * node (so they run under a hermetic PATH) and made executable. Returns their
 * absolute paths by contract vendor name.
 */
export function installFakeVendors() {
  const dir = mkdtempSync(join(tmpdir(), 'gilde-fake-vendors-'));
  const out = {};
  for (const [vendor, file] of [
    ['claude', 'fake-claude.cjs'],
    ['codex', 'fake-codex.cjs'],
    ['cursor', 'fake-cursor.cjs'],
    ['agy', 'fake-agy.cjs'],
  ]) {
    const body = readFileSync(join(fixturesDir, file), 'utf8').replace(/^#!.*\n/, '');
    const target = join(dir, vendor === 'cursor' ? 'cursor-agent' : vendor);
    writeFileSync(target, `#!${process.execPath}\n${body}`);
    chmodSync(target, 0o755);
    out[vendor] = target;
  }
  return { dir, binaries: out };
}

// ── tiny assertion harness ───────────────────────────────────────────────────
export function harness(name) {
  let failures = 0;
  let checks = 0;
  const check = (label, ok, detail) => {
    checks += 1;
    if (ok) process.stdout.write(`  ok   ${label}\n`);
    else {
      failures += 1;
      process.stdout.write(`  FAIL ${label}${detail === undefined ? '' : ` — ${detail}`}\n`);
    }
  };
  const eq = (label, actual, expected) =>
    check(label, Object.is(actual, expected), `expected ${JSON.stringify(expected)}, got ${JSON.stringify(actual)}`);
  const deepEq = (label, actual, expected) =>
    check(label, JSON.stringify(actual) === JSON.stringify(expected), `expected ${JSON.stringify(expected)}, got ${JSON.stringify(actual)}`);
  const summary = () => {
    process.stdout.write(`\n${name}: ${checks - failures}/${checks} checks passed\n`);
    return failures;
  };
  return { check, eq, deepEq, summary, get failures() { return failures; } };
}

/**
 * Start the bundle and wire a host to it.
 *
 *   opts.env        extra env for the sidecar process
 *   opts.selftest   expose the `dev.*` hooks (default true)
 */
export function startSidecar(opts = {}) {
  const child = spawn(process.execPath, [bundle], {
    cwd: sidecarDir,
    env: {
      ...process.env,
      AGENT_CLI_CREDENTIAL_STORE: 'memory',
      DISABLE_AUTOUPDATER: '1',
      ...(opts.selftest === false ? {} : { GILDE_SIDECAR_SELFTEST: '1' }),
      ...(opts.env ?? {}),
    },
    stdio: ['pipe', 'pipe', 'pipe'],
  });

  /** Binaries the host "configured": the allowlist of `host.registerSpawn`. */
  const allowed = new Set();
  /** Every `host.*` request, in arrival order. */
  const journal = [];
  /** Every notification (except `host.log`), in arrival order. */
  const notes = [];
  const logs = [];
  const stderrLines = [];
  const pending = new Map();
  let nextId = 0;
  let buffered = '';
  let skipped = 0;
  let exit = null;
  const exitWaiters = [];
  const markers = new Map();

  child.on('exit', (code, signal) => {
    exit = { code, signal };
    for (const w of exitWaiters.splice(0)) w(exit);
    for (const [, p] of pending) p.reject(new Error(`sidecar exited (${code ?? signal})`));
    pending.clear();
  });

  child.stdout.setEncoding('utf8');
  child.stdout.on('data', (chunk) => {
    buffered += chunk;
    for (;;) {
      const nl = buffered.indexOf('\n');
      if (nl === -1) break;
      const line = buffered.slice(0, nl);
      buffered = buffered.slice(nl + 1);
      if (line.trim().length > 0) onLine(line);
    }
  });
  child.stderr.setEncoding('utf8');
  child.stderr.on('data', (d) => {
    for (const l of d.split('\n')) if (l.trim().length > 0) stderrLines.push(l);
  });

  const send = (obj) => {
    if (exit === null) child.stdin.write(`${JSON.stringify(obj)}\n`);
  };

  function onLine(line) {
    let msg;
    try {
      msg = JSON.parse(line);
    } catch {
      skipped += 1;
      return;
    }
    if (msg.jsonrpc !== '2.0') {
      skipped += 1;
      return;
    }
    if (typeof msg.method === 'string') {
      if (msg.id === undefined || msg.id === null) {
        if (msg.method === 'host.log') logs.push(msg.params);
        else notes.push({ method: msg.method, params: msg.params, at: Date.now() });
        return;
      }
      journal.push({ method: msg.method, params: msg.params });
      if (msg.method === 'host.registerSpawn') {
        const p = msg.params ?? {};
        if (!allowed.has(p.cmd)) {
          send({ jsonrpc: '2.0', id: msg.id, error: { code: -32010, message: 'notAllowlisted', data: { reason: 'notAllowlisted' } } });
          return;
        }
        const envMarker = `marker-${p.spawnId}`;
        markers.set(p.spawnId, envMarker);
        send({ jsonrpc: '2.0', id: msg.id, result: { ok: true, envMarker } });
        return;
      }
      send({ jsonrpc: '2.0', id: msg.id, result: { ok: true } });
      return;
    }
    const entry = pending.get(msg.id);
    if (entry === undefined) return;
    pending.delete(msg.id);
    clearTimeout(entry.timer);
    if (msg.error !== undefined) {
      const err = new Error(`${msg.error.code}: ${msg.error.message}`);
      err.code = msg.error.code;
      err.data = msg.error.data;
      entry.reject(err);
    } else entry.resolve(msg.result);
  }

  function call(method, params, timeoutMs = 30_000) {
    const id = `h${++nextId}`;
    return new Promise((resolve, reject) => {
      if (exit !== null) {
        reject(new Error(`sidecar already exited (${exit.code ?? exit.signal})`));
        return;
      }
      const timer = setTimeout(() => {
        pending.delete(id);
        reject(new Error(`sidecar did not answer ${method} within ${timeoutMs}ms`));
      }, timeoutMs);
      pending.set(id, { resolve, reject, timer });
      send({ jsonrpc: '2.0', id, method, params });
    });
  }

  /** The error a call ends with (`null` when it succeeded). */
  async function callError(method, params, timeoutMs) {
    try {
      await call(method, params, timeoutMs);
      return null;
    } catch (err) {
      return err;
    }
  }

  function waitExit(timeoutMs = 5_000) {
    if (exit !== null) return Promise.resolve(exit);
    return new Promise((resolve) => {
      const t = setTimeout(() => resolve(null), timeoutMs);
      exitWaiters.push((e) => {
        clearTimeout(t);
        resolve(e);
      });
    });
  }

  async function waitFor(predicate, timeoutMs = 10_000) {
    const deadline = Date.now() + timeoutMs;
    while (Date.now() < deadline) {
      if (predicate()) return true;
      await new Promise((r) => setTimeout(r, 20));
    }
    return predicate();
  }

  /** The `turn.event`s of one turn, in arrival order. */
  const turnEvents = (turnId) =>
    notes.filter((n) => n.method === 'turn.event' && n.params?.turnId === turnId).map((n) => n.params);

  async function hello(extra = {}) {
    return call('mesh.hello', {
      protocol: 2,
      buildId: bundleBuildId(),
      appInstanceId: 'stub-host-instance',
      providers: { orchestrator: false },
      ...extra,
    });
  }

  async function configure({ binaries = {}, accounts = [], orchestrator = false } = {}) {
    allowed.clear();
    for (const p of Object.values(binaries)) allowed.add(p);
    return call('mesh.configure', { binaries, accounts, providers: { orchestrator } });
  }

  async function close() {
    if (exit === null) {
      try {
        await call('mesh.shutdown', { graceMs: 0 }, 3_000);
      } catch {
        /* exiting anyway */
      }
      const e = await waitExit(4_000);
      if (e === null) child.kill('SIGKILL');
    }
  }

  return {
    child,
    call,
    callError,
    hello,
    configure,
    allowed,
    journal,
    notes,
    logs,
    markers,
    stderrLines,
    turnEvents,
    waitFor,
    waitExit,
    close,
    get skipped() {
      return skipped;
    },
    get exit() {
      return exit;
    },
  };
}

/** True when `pid` names a live process (signal 0). */
export function alive(pid) {
  try {
    process.kill(pid, 0);
    return true;
  } catch (err) {
    return err.code === 'EPERM';
  }
}

export { existsSync };
