#!/usr/bin/env node
/**
 * stub-host.mjs — the smallest possible stand-in for the Rust host.
 *
 * It speaks the same NDJSON JSON-RPC 2.0 channel the real `SidecarManager` does:
 * it sends `mesh.hello`, answers `host.registerSpawn` / `host.spawned` /
 * `host.exited`, and records everything in arrival order. That recording is the
 * assertion surface — the supervision contract (DESIGN §5-5, §8-3) is *ordering*
 * plus *identity*, and both are only observable from the host side:
 *
 *   A. handshake                 — mesh.hello (protocol 2, contract §4.5) answers
 *                                  `{protocol, buildId, adapters}`: the four
 *                                  contract vendors with capabilities read off
 *                                  the adapter instances (not a hardcoded matrix).
 *   K. build id (T-buildId-rule, the sidecar half, A8.1) — esbuild.mjs's rule
 *                                  rejects every bad class, accepts the version
 *                                  and a release-style id, treats empty as unset;
 *                                  the bundle carries its id as a string literal.
 *   B. exec ordering             — `fake_agent normal 0` produces exactly
 *                                  registerSpawn → spawned → exited, in that
 *                                  order, all three carrying the same spawnId,
 *                                  the registration is exactly the contract's
 *                                  RegisterSpawnParams, and `envKeys` never
 *                                  carries a VALUE.
 *   C. pid identity + timeout    — `fake_agent hang` under a wall-clock cap: the
 *                                  pid the sidecar reported is the pid the OS
 *                                  shows running that argv (`ps -p`), and the cap
 *                                  ends the whole group with SIGTERM (never
 *                                  SIGKILL, C5) and reports timedOut.
 *   D. env marker                — the child really sees `GILDE_SPAWN_ID` = the
 *                                  envMarker this host handed back.
 *   E. garbage tolerance         — an unparseable line, a non-JSON-RPC line, and a
 *                                  9 MiB line (past MAX_LINE_BYTES) are all
 *                                  survivable: the session continues.
 *   F. line stream               — the streaming seam registers the same way and
 *                                  calls `onEnd` exactly once.
 *   J. duplex spawner            — the stdin-open spawner behind the claude quota
 *                                  probe registers as kind 'probe' with no turn.
 *   H. stream hardening          — console.log and a direct process.stdout.write
 *                                  from inside the sidecar land on stderr and
 *                                  never corrupt a frame.
 *   I. backpressure              — with the host not reading, lossy
 *                                  `orch.snapshot` notifications are dropped at
 *                                  the 4096 cap while every response — and every
 *                                  `turn.event` — still arrives.
 *
 * Run:  node test/stub-host.mjs            (from src-tauri/sidecar)
 * Exit: 0 all green, 1 on the first failed assertion.
 *
 * `fake_agent` is T06's fixture binary. When it has not been built yet this
 * script falls back to equivalent one-liners run through `process.execPath`, so
 * the seam is still exercised; the header line says which mode ran.
 */

import { spawn } from 'node:child_process';
import { execFileSync } from 'node:child_process';
import { existsSync, mkdtempSync, readFileSync, writeFileSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { dirname, join } from 'node:path';
import { fileURLToPath } from 'node:url';

const sidecarDir = join(dirname(fileURLToPath(import.meta.url)), '..');
const bundle = join(sidecarDir, 'sidecar.cjs');
const repoRoot = join(sidecarDir, '..', '..');

import { resolveBuildId } from '../esbuild.mjs';

const PROTOCOL = 2;
const APP_INSTANCE = 'stub-host-instance';
const SIDECAR_VERSION = JSON.parse(readFileSync(join(sidecarDir, 'package.json'), 'utf8')).version;
/** The id esbuild.mjs baked into the bundle under test (the same rule, the same env). */
const BUILD_ID = resolveBuildId(process.env.GILDE_BUILD_ID, SIDECAR_VERSION);

// ── fixture resolution ───────────────────────────────────────────────────────
const targetDirs = [process.env.CARGO_TARGET_DIR, join(repoRoot, 'src-tauri', 'target')].filter(Boolean);
const fakeAgent = targetDirs
  .flatMap((dir) => ['debug', 'release'].map((profile) => join(dir, profile, 'fake_agent')))
  .find((p) => existsSync(p));

const NODE = process.execPath;

/** Three lines on disk for the line-stream fixture (`fake_agent emit-jsonl <file>`). */
const linesFile = join(mkdtempSync(join(tmpdir(), 'gilde-stub-host-')), 'lines.jsonl');
writeFileSync(linesFile, 'line 1\nline 2\nline 3\n');

/** `fake_agent <mode> [arg]` when the fixture exists, else an equivalent node one-liner. */
function fixture(mode, arg) {
  if (fakeAgent !== undefined) {
    return { cmd: fakeAgent, args: arg === undefined ? [mode] : [mode, String(arg)] };
  }
  switch (mode) {
    case 'normal':
      return {
        cmd: NODE,
        args: ['-e', `process.stdout.write("fake_agent normal\\n");process.exit(${Number(arg) || 0})`],
      };
    case 'hang':
      return { cmd: NODE, args: ['-e', 'setInterval(() => {}, 1000)'] };
    case 'print-env':
      return { cmd: NODE, args: ['-e', `process.stdout.write(String(process.env[${JSON.stringify(arg)}]))`] };
    case 'emit-jsonl':
      return {
        cmd: NODE,
        args: ['-e', 'process.stdout.write(require("fs").readFileSync(process.argv[1], "utf8"))', String(arg)],
      };
    default:
      throw new Error(`no fallback for fixture mode ${mode}`);
  }
}

// ── tiny assertion harness ───────────────────────────────────────────────────
let failures = 0;
let checks = 0;
function check(name, ok, detail) {
  checks += 1;
  if (ok) {
    process.stdout.write(`  ok   ${name}\n`);
  } else {
    failures += 1;
    process.stdout.write(`  FAIL ${name}${detail === undefined ? '' : ` — ${detail}`}\n`);
  }
}
function eq(name, actual, expected) {
  check(name, Object.is(actual, expected), `expected ${JSON.stringify(expected)}, got ${JSON.stringify(actual)}`);
}

// ── the host channel ─────────────────────────────────────────────────────────
const child = spawn(NODE, [bundle], {
  cwd: sidecarDir,
  env: {
    ...process.env,
    GILDE_SIDECAR_SELFTEST: '1',
    GILDE_APP_INSTANCE: APP_INSTANCE,
    // Canary for the ExecRequest.env contract check below (§5-6).
    GILDE_ENV_LEAK_CANARY: 'leaked',
  },
  stdio: ['pipe', 'pipe', 'pipe'],
});

/** Every `host.*` request the sidecar made, in arrival order. */
const journal = [];
/** spawnId → envMarker we handed out. */
const markers = new Map();
/** Notifications received, by method. */
const noteCounts = new Map();

const pending = new Map();
let nextId = 0;
let buffered = '';
let skipped = 0;

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

const stderrLines = [];
child.stderr.setEncoding('utf8');
child.stderr.on('data', (d) => {
  for (const l of d.split('\n')) if (l.trim().length > 0) stderrLines.push(l);
});

function send(obj) {
  child.stdin.write(`${JSON.stringify(obj)}\n`);
}

function call(method, params, timeoutMs = 30_000) {
  const id = ++nextId;
  return new Promise((resolve, reject) => {
    const timer = setTimeout(() => {
      pending.delete(id);
      reject(new Error(`sidecar did not answer ${method} within ${timeoutMs}ms`));
    }, timeoutMs);
    pending.set(id, { resolve, reject, timer });
    send({ jsonrpc: '2.0', id, method, params });
  });
}

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
    // A request or notification FROM the sidecar.
    if (msg.method.startsWith('host.') && msg.method !== 'host.log') {
      journal.push({ method: msg.method, params: msg.params });
    }
    if (msg.id === undefined || msg.id === null) {
      noteCounts.set(msg.method, (noteCounts.get(msg.method) ?? 0) + 1);
      return; // notification
    }

    let result = { ok: true };
    if (msg.method === 'host.registerSpawn') {
      // The host chooses the marker; the sidecar must use what it is given.
      const envMarker = `marker-${msg.params.spawnId}`;
      markers.set(msg.params.spawnId, envMarker);
      result = { ok: true, envMarker };
    }
    send({ jsonrpc: '2.0', id: msg.id, result });
    return;
  }

  const entry = pending.get(msg.id);
  if (entry === undefined) return;
  pending.delete(msg.id);
  clearTimeout(entry.timer);
  if (msg.error !== undefined) entry.reject(new Error(`${msg.error.code}: ${msg.error.message}`));
  else entry.resolve(msg.result);
}

/** The `host.*` requests recorded for one spawnId, in order. */
const trace = (spawnId) =>
  journal.filter((e) => e.params?.spawnId === spawnId).map((e) => e.method);

function psArgs(pid) {
  try {
    return execFileSync('ps', ['-o', 'args=', '-p', String(pid)], { encoding: 'utf8' }).trim();
  } catch {
    return '';
  }
}

async function main() {
  process.stdout.write(
    `stub-host: ${bundle}\n` +
      `fixture:   ${fakeAgent ?? `(fake_agent not built — node fallbacks; run \`cargo build --bin fake_agent\` for the real one)`}\n\n`,
  );

  // ── A. handshake ───────────────────────────────────────────────────────────
  const hello = await call('mesh.hello', {
    protocol: PROTOCOL,
    buildId: BUILD_ID,
    appInstanceId: APP_INSTANCE,
    providers: { orchestrator: false },
  });
  eq('A1 hello.protocol', hello.protocol, PROTOCOL);
  eq('A2 hello.buildId is the bundle\'s', hello.buildId, BUILD_ID);
  check('A3 hello answers exactly {protocol, buildId, adapters}', Object.keys(hello).sort().join(',') === 'adapters,buildId,protocol', Object.keys(hello).join(','));
  const vendors = (hello.adapters ?? []).map((a) => a.vendor);
  check('A4 four adapters, contract vendor names', vendors.join(',') === 'claude,codex,cursor,agy', vendors.join(','));
  const caps = Object.fromEntries((hello.adapters ?? []).map((a) => [a.vendor, a]));
  check(
    'A5 every adapter row is exactly {vendor, resume, stream, quota, installed}',
    (hello.adapters ?? []).every((a) => Object.keys(a).sort().join(',') === 'installed,quota,resume,stream,vendor'),
    JSON.stringify(hello.adapters),
  );
  // Read off the instances (agent-mesh 0.1.3+: codex streams; cursor and agy have quota).
  check('A6 claude streams and has quota', caps.claude?.stream === true && caps.claude?.quota === true, JSON.stringify(caps.claude));
  check('A7 codex reports stream:true', caps.codex?.stream === true, JSON.stringify(caps.codex));
  check('A8 cursor and agy report quota:true', caps.cursor?.quota === true && caps.agy?.quota === true, JSON.stringify([caps.cursor, caps.agy]));
  check('A9 nothing is installed before mesh.configure', (hello.adapters ?? []).every((a) => a.installed === false), JSON.stringify(hello.adapters));
  check(
    'A10 the five exact agent-mesh pins are logged at start',
    stderrLines.some((l) => l.includes('"@toragonite/agent-mesh":"0.1.4"') && l.includes('agent-mesh-gemini')),
    stderrLines.slice(0, 2).join(' | '),
  );

  const ping = await call('mesh.ping', { nonce: 1 });
  eq('A11 ping echoes the nonce', ping.nonce, 1);

  // ── B. exec ordering ───────────────────────────────────────────────────────
  const normal = fixture('normal', 0);
  const before = journal.length;
  const execResult = await call('dev.exec', {
    cmd: normal.cmd,
    args: normal.args,
    cwd: sidecarDir,
    env: { PATH: process.env.PATH ?? '', GILDE_TEST_SECRET: 'super-secret-value' },
    turnId: 'turn-b',
    vendor: 'codex',
    timeoutMs: 30_000,
  });
  const bEntries = journal.slice(before);
  const bSpawnId = bEntries[0]?.params?.spawnId;
  check(
    'B1 exactly registerSpawn → spawned → exited, in order',
    trace(bSpawnId).join(' → ') === 'host.registerSpawn → host.spawned → host.exited',
    trace(bSpawnId).join(' → '),
  );
  const bRegister = bEntries[0]?.params ?? {};
  const bSpawned = bEntries[1]?.params ?? {};
  const bExited = bEntries[2]?.params ?? {};
  eq('B2 registerSpawn carries the turnId', bRegister.turnId, 'turn-b');
  eq('B3 registerSpawn kind is turn', bRegister.kind, 'turn');
  check(
    'B3b registerSpawn is exactly the contract RegisterSpawnParams',
    Object.keys(bRegister).sort().join(',') === 'args,cmd,cwd,envKeys,kind,spawnId,turnId,vendor',
    Object.keys(bRegister).join(','),
  );
  check('B4 registerSpawn carries argv', bRegister.cmd === normal.cmd, bRegister.cmd);
  check(
    'B5 envKeys are KEYS only — no env value crosses the channel',
    Array.isArray(bRegister.envKeys) &&
      bRegister.envKeys.includes('GILDE_TEST_SECRET') &&
      !JSON.stringify(bRegister).includes('super-secret-value'),
    JSON.stringify(bRegister.envKeys),
  );
  check('B6 pgid === pid (detached: own group leader)', bSpawned.pid === bSpawned.pgid, JSON.stringify(bSpawned));
  eq('B7 exited reports exitCode 0', bExited.exitCode, 0);
  eq('B8 exited reports timedOut false', bExited.timedOut, false);
  eq('B9 ExecResult exitCode 0', execResult.exitCode, 0);
  eq('B10 the same spawnId in all three', new Set([bRegister.spawnId, bSpawned.spawnId, bExited.spawnId]).size, 1);

  // ExecRequest.env semantics (§5-6): when `env` is given it IS the whole child
  // env — it is NOT merged over the sidecar's own environment; when it is omitted
  // the child inherits. Both branches add only the two markers.
  const canary = fixture('print-env', 'GILDE_ENV_LEAK_CANARY');
  const isolated = await call('dev.exec', {
    cmd: canary.cmd,
    args: canary.args,
    cwd: sidecarDir,
    env: { PATH: process.env.PATH ?? '' },
    turnId: 'turn-b2',
    vendor: 'claude',
    timeoutMs: 30_000,
  });
  check(
    'B11 an explicit env is the WHOLE child env (no merge over process.env)',
    !isolated.stdout.includes('leaked'),
    JSON.stringify(isolated.stdout),
  );
  const inherited = await call('dev.exec', {
    cmd: canary.cmd,
    args: canary.args,
    cwd: sidecarDir,
    turnId: 'turn-b3',
    vendor: 'claude',
    timeoutMs: 30_000,
  });
  check(
    'B12 an omitted env inherits the sidecar environment',
    inherited.stdout.includes('leaked'),
    JSON.stringify(inherited.stdout),
  );

  // ── C. pid identity + timeout escalation ───────────────────────────────────
  const hang = fixture('hang');
  const beforeC = journal.length;
  const cPromise = call(
    'dev.exec',
    { cmd: hang.cmd, args: hang.args, cwd: sidecarDir, turnId: 'turn-c', vendor: 'codex', timeoutMs: 1500 },
    30_000,
  );
  // Give the child a moment to exist, then verify the reported pid IS that child.
  await new Promise((r) => setTimeout(r, 700));
  const cSpawned = journal.slice(beforeC).find((e) => e.method === 'host.spawned');
  check('C1 host.spawned arrived while the child was alive', cSpawned !== undefined);
  const cArgs = cSpawned === undefined ? '' : psArgs(cSpawned.params.pid);
  check(
    'C2 the reported pid is the real child (ps -p matches the argv)',
    cArgs.includes(hang.args[hang.args.length - 1]) || cArgs.includes(hang.cmd),
    `pid=${cSpawned?.params?.pid} ps="${cArgs}"`,
  );
  const cResult = await cPromise;
  eq('C3 the wall-clock cap reports timedOut', cResult.timedOut, true);
  const cExited = journal.slice(beforeC).find((e) => e.method === 'host.exited');
  eq('C4 host.exited reports timedOut', cExited?.params?.timedOut, true);
  check(
    'C5 the child is gone after the timeout signal',
    cSpawned === undefined || psArgs(cSpawned.params.pid) === '',
    psArgs(cSpawned?.params?.pid ?? 0),
  );
  check('C6 the timeout signal was SIGTERM, never SIGKILL (C5)', cExited?.params?.signal === 'SIGTERM', JSON.stringify(cExited?.params));

  // ── D. env marker reaches the child ────────────────────────────────────────
  const printEnv = fixture('print-env', 'GILDE_SPAWN_ID');
  const beforeD = journal.length;
  const dResult = await call('dev.exec', {
    cmd: printEnv.cmd,
    args: printEnv.args,
    cwd: sidecarDir,
    turnId: 'turn-d',
    vendor: 'agy',
    timeoutMs: 30_000,
  });
  const dRegister = journal.slice(beforeD).find((e) => e.method === 'host.registerSpawn');
  const expectedMarker = markers.get(dRegister?.params?.spawnId);
  check(
    'D1 the child sees GILDE_SPAWN_ID = the envMarker the HOST chose',
    dResult.stdout.trim() === expectedMarker,
    `stdout=${JSON.stringify(dResult.stdout.trim())} expected=${JSON.stringify(expectedMarker)}`,
  );

  // ── E. garbage tolerance ───────────────────────────────────────────────────
  child.stdin.write('this is not json at all\n');
  child.stdin.write(`${JSON.stringify({ id: 1, method: 'mesh.ping' })}\n`); // no jsonrpc field
  child.stdin.write('\n');
  const afterGarbage = await call('mesh.ping', { nonce: 2 });
  eq('E1 the session survives garbage lines', afterGarbage.nonce, 2);

  // A 9 MiB line is past MAX_LINE_BYTES (8 MiB): the buffer is dropped and the
  // rest of that line discarded, so memory stays bounded and framing recovers.
  child.stdin.write(`${'x'.repeat(9 * 1024 * 1024)}\n`);
  const afterOversize = await call('mesh.ping', { nonce: 3 });
  eq('E2 the session survives a 9 MiB line', afterOversize.nonce, 3);
  check(
    'E3 the oversize line was reported as a protocol violation',
    stderrLines.some((l) => l.includes('MAX_LINE_BYTES')),
    stderrLines.slice(-3).join(' | '),
  );

  // ── F. line stream seam ────────────────────────────────────────────────────
  const emit = fixture('emit-jsonl', linesFile);
  const beforeF = journal.length;
  const fResult = await call('dev.lineStream', {
    cmd: emit.cmd,
    args: emit.args,
    cwd: sidecarDir,
    turnId: 'turn-f',
    vendor: 'cursor',
    timeoutMs: 30_000,
  });
  const fSpawnId = journal.slice(beforeF)[0]?.params?.spawnId;
  check(
    'F1 the streaming seam registers the same way',
    trace(fSpawnId).join(' → ') === 'host.registerSpawn → host.spawned → host.exited',
    trace(fSpawnId).join(' → '),
  );
  check('F2 lines were streamed', fResult.lines.length >= 3, JSON.stringify(fResult.lines));
  check('F3 onEnd fired exactly once with StreamEndMeta', fResult.meta !== null && fResult.meta.exitCode === 0, JSON.stringify(fResult.meta));

  // ── J. duplex spawner (the base of the claude quota probe, §5-7) ───────────
  const beforeJ = journal.length;
  const jResult = await call('dev.duplex', {
    cmd: printEnv.cmd,
    args: printEnv.args,
    cwd: sidecarDir,
    vendor: 'claude',
    timeoutMs: 30_000,
  });
  const jEntries = journal.slice(beforeJ);
  const jSpawnId = jEntries[0]?.params?.spawnId;
  check(
    'J1 the duplex spawner registers like every other child',
    trace(jSpawnId).join(' → ') === 'host.registerSpawn → host.spawned → host.exited',
    trace(jSpawnId).join(' → '),
  );
  eq('J2 the duplex child is registered as kind "probe"', jEntries[0]?.params?.kind, 'probe');
  eq('J3 the duplex child belongs to no turn', jEntries[0]?.params?.turnId, null);
  check(
    'J4 the duplex child carries GILDE_SPAWN_ID too',
    jResult.stdout.trim() === markers.get(jSpawnId),
    `${JSON.stringify(jResult.stdout.trim())} vs ${JSON.stringify(markers.get(jSpawnId))}`,
  );

  // ── H. stream hardening ────────────────────────────────────────────────────
  const stderrBefore = stderrLines.length;
  await call('dev.pollute', { text: 'POLLUTION-MARKER' });
  check(
    'H1 console.log / process.stdout.write are redirected to stderr',
    stderrLines.slice(stderrBefore).filter((l) => l.includes('POLLUTION-MARKER')).length >= 3,
    stderrLines.slice(stderrBefore).join(' | ').slice(0, 200),
  );
  const stillAlive = await call('mesh.ping', { nonce: 4 });
  eq('H2 the channel is intact after the pollution', stillAlive.nonce, 4);

  // ── I. outbound backpressure ───────────────────────────────────────────────
  // Stop reading so the pipe fills, flood orch.snapshot (lossy) and turn.event (not
  // lossy), then resume: the snapshots are discarded at the cap while every
  // response and every turn event still arrives.
  child.stdout.pause();
  const floodPromise = call('dev.floodEvents', { count: 20000, turnEvents: 5000 }, 60_000);
  await new Promise((r) => setTimeout(r, 500));
  child.stdout.resume();
  const flood = await floodPromise;
  eq('I1 the flood response arrived (responses are never dropped)', flood.sent, 20000);
  const stats = await call('dev.stats', {});
  check(
    'I2 orch.snapshot notifications were dropped at the queue cap',
    stats.droppedNotifications > 0,
    JSON.stringify(stats),
  );
  const afterFlood = await call('mesh.ping', { nonce: 5 });
  eq('I3 the channel still works after the flood', afterFlood.nonce, 5);
  eq('I4 every turn.event arrived (never dropped, contract §4.5)', noteCounts.get('turn.event') ?? 0, 5000);

  // ── K. T-buildId-rule, the sidecar half (A8.1) ─────────────────────────────
  const rejects = (env) => {
    try {
      resolveBuildId(env, SIDECAR_VERSION);
      return false;
    } catch (err) {
      return String(err.message).includes('GILDE_BUILD_ID');
    }
  };
  eq('K1 unset → the version', resolveBuildId(undefined, SIDECAR_VERSION), SIDECAR_VERSION);
  eq('K2 empty → unset → the version', resolveBuildId('', SIDECAR_VERSION), SIDECAR_VERSION);
  eq('K3 a release-style id is accepted as is', resolveBuildId('2.0.0+b425b5e.20261022T073000', SIDECAR_VERSION), '2.0.0+b425b5e.20261022T073000');
  for (const [label, bad] of [
    ['whitespace inside', '2.0 .0'],
    ['a leading space (never trimmed)', ' 2.0.0'],
    ['a trailing newline from $(…)', '2.0.0\n'],
    ['a slash', '2.0/0'],
    ['a leading dot', '.2.0.0'],
    ['a leading dash', '-2.0.0'],
    ['65 bytes', 'a'.repeat(65)],
    ['non-ASCII', '2.0.0-é'],
    ['a shell metacharacter', '2.0.0;rm'],
  ]) {
    check(`K4 rejected, naming GILDE_BUILD_ID: ${label}`, rejects(bad));
  }
  check('K5 64 bytes is still accepted', resolveBuildId('a'.repeat(64), SIDECAR_VERSION) === 'a'.repeat(64));
  check('K6 a bad version fallback fails too', (() => { try { resolveBuildId(undefined, 'x y'); return false; } catch { return true; } })());
  check('K7 the bundle carries its build id as a string literal', readFileSync(bundle, 'utf8').includes(JSON.stringify(BUILD_ID)));

  // ── G. no stray stdout ─────────────────────────────────────────────────────
  check(
    'G1 nothing but JSON-RPC ever reached stdout',
    skipped === 0,
    `${skipped} stdout line(s) were not JSON-RPC`,
  );
  check(
    'G2 the sidecar logs on stderr',
    stderrLines.some((l) => l.includes('gilde-code-sidecar')),
    stderrLines.slice(0, 3).join(' | '),
  );

  await call('mesh.shutdown', { graceMs: 0 });
}

const timeoutGuard = setTimeout(() => {
  process.stdout.write('\nstub-host: TIMED OUT\n');
  child.kill('SIGKILL');
  process.exit(1);
}, 120_000);
timeoutGuard.unref();

main()
  .then(() => {
    process.stdout.write(`\nstub-host: ${checks - failures}/${checks} checks passed\n`);
    if (failures > 0) {
      process.stdout.write('sidecar stderr:\n');
      for (const l of stderrLines.slice(-30)) process.stdout.write(`  ${l}\n`);
    }
    setTimeout(() => {
      try {
        child.kill('SIGKILL');
      } catch {
        /* already gone */
      }
      process.exit(failures > 0 ? 1 : 0);
    }, 300);
  })
  .catch((err) => {
    process.stdout.write(`\nstub-host: ERROR ${err.message}\n`);
    process.stdout.write('sidecar stderr:\n');
    for (const l of stderrLines.slice(-30)) process.stdout.write(`  ${l}\n`);
    try {
      child.kill('SIGKILL');
    } catch {
      /* already gone */
    }
    process.exit(1);
  });
