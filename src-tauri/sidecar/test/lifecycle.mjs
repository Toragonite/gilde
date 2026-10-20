#!/usr/bin/env node
/**
 * lifecycle.mjs — the sidecar's three exit paths, pinned against a real process.
 *
 * INT-C. The defect these tests exist to keep dead, observed on a developer
 * machine: after the host was SIGKILLed (a test run cut short, a GUI smoke pass),
 * the orphaned `gilde-node …/sidecar.cjs` processes were reparented to launchd and
 * kept running for 6–7 HOURS at ~100% CPU each, ignoring SIGTERM. Only SIGKILL
 * removed them. That is precisely the zombie population this app exists to
 * prevent, produced by the app itself.
 *
 * The mechanism was a loop, not a leak: with the host gone, `process.stderr.write`
 * failed asynchronously with EPIPE; with no `error` listener that became an
 * `uncaughtException`; the handler logged it, and the log wrote the next EPIPE.
 * The storm starved the event loop, so the SIGTERM handler never ran either.
 *
 * Four cases, each asserting BOTH that the process is gone within its budget and
 * that it was not spinning while it went:
 *
 *   A. stdin closes cleanly            → exit ≤ 2 s
 *   B. SIGTERM                         → exit ≤ 2 s
 *   C. the host dies hard (SIGKILL)    → exit ≤ 2 s, no CPU spin   ← the regression
 *   D. the parent dies with every       → exit ≤ 6 s (2 s watchdog poll + budget)
 *      stream still open                 ← nothing but the watchdog can notice
 *
 * Case D is the one that needs care: its sidecar reads a FIFO the *test* process
 * holds open and writes to files, so no stream ever ends. The only evidence the
 * host is gone is `process.ppid`.
 *
 * Run:  node test/lifecycle.mjs        (from src-tauri/sidecar)
 * Exit: 0 all green, 1 on the first failed assertion.
 *
 * Every process this file starts is killed in a `finally`, including on failure:
 * a test for orphan processes must not leave orphan processes.
 */

import { execFileSync, spawn } from 'node:child_process';
import { closeSync, existsSync, mkdtempSync, openSync, readFileSync, writeFileSync, writeSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { dirname, join } from 'node:path';
import { fileURLToPath } from 'node:url';

const sidecarDir = join(dirname(fileURLToPath(import.meta.url)), '..');
const bundle = join(sidecarDir, 'sidecar.cjs');

/** The vendored runtime when the tree has one — the binary that actually ships. */
const vendored = join(sidecarDir, '..', 'binaries', 'gilde-node-aarch64-apple-darwin');
const NODE = existsSync(vendored) ? vendored : process.execPath;

const HELLO =
  JSON.stringify({
    jsonrpc: '2.0',
    id: 1,
    method: 'mesh.hello',
    params: {
      protocol: 1,
      hostVersion: '0.0.0-lifecycle-test',
      appInstanceId: 'lifecycle-test',
      bootId: 'lifecycle',
      binaries: {},
      defaults: { timeoutMs: 60_000 },
    },
  }) + '\n';

/** Anything started here, so the `finally` can be exhaustive. */
const started = new Set();

// ── tiny assertion harness (same shape as stub-host.mjs) ─────────────────────
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

const sleep = (ms) => new Promise((r) => setTimeout(r, ms));

function alive(pid) {
  try {
    process.kill(pid, 0);
    return true;
  } catch (err) {
    return err.code === 'EPERM';
  }
}

/** Recent CPU share of `pid`, or null once it is gone. */
function cpuOf(pid) {
  try {
    const out = execFileSync('ps', ['-o', '%cpu=', '-p', String(pid)], { stdio: ['ignore', 'pipe', 'ignore'] })
      .toString()
      .trim();
    return out.length === 0 ? null : Number.parseFloat(out);
  } catch {
    return null;
  }
}

function hardKill(pid) {
  if (pid === undefined || pid === null) return;
  try {
    process.kill(pid, 'SIGKILL');
  } catch {
    /* already gone */
  }
}

/**
 * Wait for `pid` to disappear, sampling CPU the whole time.
 *
 * Returns `{ exitedMs, peakCpu, samples }`. `exitedMs` is null when it never
 * exited — the failure shape the original defect produced.
 */
async function awaitExit(pid, budgetMs) {
  const t0 = Date.now();
  let peakCpu = 0;
  let samples = 0;
  while (Date.now() - t0 < budgetMs) {
    if (!alive(pid)) return { exitedMs: Date.now() - t0, peakCpu, samples };
    const cpu = cpuOf(pid);
    if (cpu !== null) {
      peakCpu = Math.max(peakCpu, cpu);
      samples += 1;
    }
    await sleep(150);
  }
  return { exitedMs: alive(pid) ? null : Date.now() - t0, peakCpu, samples };
}

/**
 * The CPU assertion.
 *
 * The bug pegged a full core (~100%, sustained). Idle shutdown work is a few
 * percent. 50% is comfortably between the two and does not turn a loaded CI box
 * into a false failure. `samples === 0` means it exited before the first sample —
 * which is the best possible outcome, not a missing assertion.
 */
function checkNotSpinning(name, result) {
  check(
    `${name}: CPU never pegged (peak ${result.peakCpu}% over ${result.samples} sample(s))`,
    result.peakCpu < 50,
    `peak ${result.peakCpu}%`,
  );
}

// ── A. stdin closes ──────────────────────────────────────────────────────────
async function caseStdinClose() {
  const child = spawn(NODE, [bundle], { cwd: sidecarDir, stdio: ['pipe', 'pipe', 'pipe'] });
  started.add(child.pid);
  child.stderr.resume();

  let out = '';
  child.stdout.setEncoding('utf8');
  child.stdout.on('data', (d) => {
    out += d;
  });

  child.stdin.write(HELLO);
  for (let i = 0; i < 40 && !out.includes('\n'); i += 1) await sleep(100);
  check('A stdin close: the handshake answered before the test', out.includes('"protocol":1'), out.slice(0, 120));

  child.stdin.end();
  const r = await awaitExit(child.pid, 2_000);
  check(`A stdin close: exits within 2 s (${r.exitedMs}ms)`, r.exitedMs !== null && r.exitedMs < 2_000);
  checkNotSpinning('A stdin close', r);
}

// ── B. SIGTERM ───────────────────────────────────────────────────────────────
async function caseSigterm() {
  const child = spawn(NODE, [bundle], { cwd: sidecarDir, stdio: ['pipe', 'pipe', 'pipe'] });
  started.add(child.pid);
  child.stderr.resume();
  child.stdout.resume();

  child.stdin.write(HELLO);
  await sleep(600);
  check('B SIGTERM: the sidecar is up', alive(child.pid));

  process.kill(child.pid, 'SIGTERM');
  const r = await awaitExit(child.pid, 2_000);
  check(`B SIGTERM: exits within 2 s (${r.exitedMs}ms)`, r.exitedMs !== null && r.exitedMs < 2_000);
  checkNotSpinning('B SIGTERM', r);
}

/**
 * A stand-in host: spawns the sidecar the way `SidecarManager` does, prints its
 * pid, then waits to be killed. `mode` picks the sidecar's stdio.
 */
function writeStandInHost(dir, mode, fifo, outFile, errFile) {
  const path = join(dir, `stand-in-host-${mode}.mjs`);
  const stdio =
    mode === 'pipes'
      ? `['pipe', 'pipe', 'pipe']`
      : `[openSync(${JSON.stringify(fifo)}, 'r'), openSync(${JSON.stringify(outFile)}, 'w'), openSync(${JSON.stringify(errFile)}, 'w')]`;
  writeFileSync(
    path,
    `import { spawn } from 'node:child_process';
import { openSync } from 'node:fs';
const child = spawn(${JSON.stringify(NODE)}, [${JSON.stringify(bundle)}], {
  cwd: ${JSON.stringify(sidecarDir)},
  env: { ...process.env, GILDE_HOST_PID: String(process.pid) },
  stdio: ${stdio},
});
if (child.stdout) child.stdout.resume();
if (child.stderr) child.stderr.resume();
if (child.stdin) child.stdin.write(${JSON.stringify(HELLO)});
process.stdout.write('SIDECAR_PID ' + child.pid + '\\n');
setInterval(() => {}, 1000);
`,
  );
  return path;
}

/** Start a stand-in host and return `{ hostPid, sidecarPid }`. */
async function startStandIn(script) {
  const host = spawn(NODE, [script], { stdio: ['ignore', 'pipe', 'ignore'] });
  started.add(host.pid);
  let buf = '';
  host.stdout.setEncoding('utf8');
  host.stdout.on('data', (d) => {
    buf += d;
  });
  for (let i = 0; i < 60 && !buf.includes('SIDECAR_PID'); i += 1) await sleep(100);
  const m = /SIDECAR_PID (\d+)/.exec(buf);
  const sidecarPid = m === null ? undefined : Number.parseInt(m[1], 10);
  if (sidecarPid !== undefined) started.add(sidecarPid);
  return { hostPid: host.pid, sidecarPid };
}

// ── C. the host dies hard — THE regression ───────────────────────────────────
async function caseHostSigkilled(dir) {
  const script = writeStandInHost(dir, 'pipes');
  const { hostPid, sidecarPid } = await startStandIn(script);
  check('C host SIGKILL: the stand-in host started a sidecar', sidecarPid !== undefined);
  if (sidecarPid === undefined) return;

  // Let the handshake land, so the run surface is fully built when the host dies.
  await sleep(800);
  process.kill(hostPid, 'SIGKILL');

  const r = await awaitExit(sidecarPid, 2_000);
  check(
    `C host SIGKILL: the orphan exits within 2 s (${r.exitedMs}ms)`,
    r.exitedMs !== null && r.exitedMs < 2_000,
    'this is the 6-hour, 100%-CPU orphan of INT-C',
  );
  checkNotSpinning('C host SIGKILL', r);
}

// ── D. the parent dies with every stream still open ──────────────────────────
async function caseParentDeathOnly(dir) {
  const fifo = join(dir, 'in.fifo');
  execFileSync('mkfifo', [fifo]);
  const outFile = join(dir, 'sidecar.out');
  const errFile = join(dir, 'sidecar.err');

  // O_RDWR: opening a FIFO both ways never blocks and keeps a writer on it, so
  // the sidecar's stdin never reaches EOF even after its parent is gone. That is
  // the whole point — nothing but `process.ppid` can reveal the death.
  const fifoFd = openSync(fifo, 'r+');
  try {
    const script = writeStandInHost(dir, 'files', fifo, outFile, errFile);
    const { hostPid, sidecarPid } = await startStandIn(script);
    check('D parent death: the stand-in host started a sidecar', sidecarPid !== undefined);
    if (sidecarPid === undefined) return;

    // The handshake comes from HERE, not from the stand-in host: with fd-based
    // stdio the host has no `child.stdin` stream, and the writer we need to
    // survive its death is this process's FIFO fd anyway.
    writeSync(fifoFd, HELLO);
    await sleep(800);
    check(
      'D parent death: the sidecar answered the handshake over the FIFO',
      readFileSync(outFile, 'utf8').includes('"protocol":1'),
      readFileSync(errFile, 'utf8').slice(0, 200),
    );

    process.kill(hostPid, 'SIGKILL');
    // 2 s watchdog poll + a 1 s exit budget, with slack for a loaded machine.
    const r = await awaitExit(sidecarPid, 6_000);
    check(
      `D parent death: the watchdog exits it within 6 s (${r.exitedMs}ms)`,
      r.exitedMs !== null && r.exitedMs < 6_000,
      'no stream ever ended; only the ppid check can see this',
    );
    checkNotSpinning('D parent death', r);
  } finally {
    try {
      closeSync(fifoFd);
    } catch {
      /* already closed */
    }
  }
}

// ── run ──────────────────────────────────────────────────────────────────────
const dir = mkdtempSync(join(tmpdir(), 'gilde-code-sidecar-lifecycle-'));
process.stdout.write(`gilde-code-sidecar lifecycle (INT-C) — runtime ${NODE}\n`);

try {
  await caseStdinClose();
  await caseSigterm();
  await caseHostSigkilled(dir);
  await caseParentDeathOnly(dir);
} catch (err) {
  failures += 1;
  process.stdout.write(`  FAIL harness threw — ${err && err.stack ? err.stack : err}\n`);
} finally {
  // Exhaustive, and it runs on the failure path too: a test about orphan
  // processes that leaks one has proved the opposite of its point.
  for (const pid of started) hardKill(pid);
  await sleep(200);
  const survivors = [...started].filter((pid) => alive(pid));
  check('cleanup: nothing this file started is still running', survivors.length === 0, survivors.join(','));
}

process.stdout.write(`\n${checks - failures}/${checks} checks passed\n`);
process.exit(failures === 0 ? 0 : 1);
