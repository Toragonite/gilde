#!/usr/bin/env node
/**
 * live-check.mjs — the same channel, driven against the REAL vendor CLIs.
 *
 * `mesh-methods.mjs` proves the protocol with `fake_agent`; nothing there can tell
 * you whether the argv the adapters build is still the argv today's `claude` and
 * `codex` accept, or whether the control-protocol quota probe still gets a
 * `control_response`. Those are exactly the assumptions DESIGN §5 flags as pinned
 * to observed CLI behaviour rather than to a contract (R9), so they need a probe
 * that actually spends a token or two.
 *
 * This is therefore a MANUAL / machine-local check, not part of `npm run build`:
 * it needs logged-in CLIs, it costs real quota, and it takes a minute or two.
 *
 * Run:  node test/live-check.mjs               (claude + codex)
 *       node test/live-check.mjs claude        (one vendor)
 * Exit: 0 all green, 1 on the first failed assertion.
 */

import { execFileSync, spawn } from 'node:child_process';
import { mkdtempSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { dirname, join } from 'node:path';
import { fileURLToPath } from 'node:url';

const sidecarDir = join(dirname(fileURLToPath(import.meta.url)), '..');
const bundle = join(sidecarDir, 'sidecar.cjs');
const NODE = process.execPath;
const PROTOCOL = 1;

const wanted = process.argv.slice(2).filter((a) => !a.startsWith('-'));
const vendors = wanted.length > 0 ? wanted : ['claude', 'codex'];
/**
 * The run cwd. It is a git repo on purpose: in the product every `cwd` is a
 * worktree, and `codex exec` refuses to run outside one ("Not inside a trusted
 * directory and --skip-git-repo-check was not specified"). A bare tmpdir would
 * make this check fail for a reason that cannot happen in the app.
 */
const work = mkdtempSync(join(tmpdir(), 'gilde-live-check-'));
execFileSync('git', ['init', '-q'], { cwd: work, stdio: 'ignore' });

let failures = 0;
let checks = 0;
function check(name, ok, detail) {
  checks += 1;
  if (ok) process.stdout.write(`  ok   ${name}\n`);
  else {
    failures += 1;
    process.stdout.write(`  FAIL ${name}${detail === undefined ? '' : ` — ${detail}`}\n`);
  }
}

const child = spawn(NODE, [bundle], {
  cwd: sidecarDir,
  env: { ...process.env, GILDE_APP_INSTANCE: 'live-check-instance' },
  stdio: ['pipe', 'pipe', 'pipe'],
});

const journal = [];
const events = [];
const stderrLines = [];
const pending = new Map();
let nextId = 0;
let buffered = '';

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

function onLine(line) {
  let msg;
  try {
    msg = JSON.parse(line);
  } catch {
    return;
  }
  if (msg.jsonrpc !== '2.0') return;
  if (typeof msg.method === 'string') {
    if (msg.method === 'mesh.event') events.push(msg.params);
    else if (msg.method.startsWith('host.') && msg.method !== 'host.log') {
      journal.push({ method: msg.method, params: msg.params });
    }
    if (msg.id === undefined || msg.id === null) return;
    const result =
      msg.method === 'host.registerSpawn'
        ? { ok: true, envMarker: `marker-${msg.params.spawnId}` }
        : { ok: true };
    child.stdin.write(`${JSON.stringify({ jsonrpc: '2.0', id: msg.id, result })}\n`);
    return;
  }
  const entry = pending.get(msg.id);
  if (entry === undefined) return;
  pending.delete(msg.id);
  clearTimeout(entry.timer);
  if (msg.error !== undefined) {
    const err = new Error(msg.error.message);
    err.code = msg.error.code;
    err.data = msg.error.data;
    entry.reject(err);
  } else entry.resolve(msg.result);
}

function call(method, params, timeoutMs = 180_000) {
  const id = ++nextId;
  return new Promise((resolve, reject) => {
    const timer = setTimeout(() => {
      pending.delete(id);
      reject(new Error(`sidecar did not answer ${method} within ${timeoutMs}ms`));
    }, timeoutMs);
    pending.set(id, { resolve, reject, timer });
    child.stdin.write(`${JSON.stringify({ jsonrpc: '2.0', id, method, params })}\n`);
  });
}

const sleep = (ms) => new Promise((r) => setTimeout(r, ms));
const eventsFor = (runId) => events.filter((e) => e.runId === runId).map((e) => e.event);

async function waitForTerminal(runId, timeoutMs) {
  const deadline = Date.now() + timeoutMs;
  for (;;) {
    const evs = eventsFor(runId);
    if (evs.some((e) => e.type === 'done' || e.type === 'error')) return evs;
    if (Date.now() > deadline) return evs;
    await sleep(100);
  }
}

async function main() {
  process.stdout.write(`live-check: ${bundle}\nvendors:    ${vendors.join(', ')}\ncwd:        ${work}\n\n`);

  const hello = await call('mesh.hello', {
    protocol: PROTOCOL,
    hostVersion: '0.1.0-live-check',
    appInstanceId: 'live-check-instance',
    bootId: 'live-check-boot',
    binaries: {},
    defaults: { timeoutMs: 120_000 },
  });
  for (const a of hello.adapters ?? []) {
    process.stdout.write(
      `  adapter ${a.vendor.padEnd(7)} installed=${String(a.installed).padEnd(5)} ` +
        `caps=${JSON.stringify(a.capabilities)}\n`,
    );
  }
  process.stdout.write('\n');

  if (vendors.includes('claude')) {
    // ── quota: the registered duplex control-protocol probe (usage.ts) ────────
    const beforeQuota = journal.length;
    const quota = await call('mesh.quota', { vendor: 'claude' });
    process.stdout.write(`  quota(claude) = ${JSON.stringify(quota)}\n`);
    check(
      'live-1 mesh.quota(claude) returned a real snapshot',
      quota !== null && Array.isArray(quota.windows) && quota.windows.length > 0,
      JSON.stringify(quota),
    );
    check(
      'live-2 the quota snapshot is stamped with capturedAt',
      typeof quota?.capturedAt === 'string',
      String(quota?.capturedAt),
    );
    const probeRows = journal.slice(beforeQuota).filter((e) => e.method === 'host.registerSpawn');
    check(
      'live-3 the quota probe went through the registered duplex seam',
      probeRows.some((e) => e.params?.kind === 'probe' && e.params?.runId === 'introspect'),
      JSON.stringify(probeRows.map((e) => e.params?.kind)),
    );

    const auth = await call('mesh.authStatus', { vendor: 'claude' });
    check('live-4 mesh.authStatus(claude) reports a login', auth?.loggedIn === true, JSON.stringify(auth));
    const models = await call('mesh.availableModels', { vendor: 'claude' });
    check('live-5 mesh.availableModels(claude) is non-empty', Array.isArray(models) && models.length > 0, JSON.stringify(models));

    // ── run ───────────────────────────────────────────────────────────────────
    const run = await call('mesh.run', {
      runId: 'live-run-claude',
      vendor: 'claude',
      task: { prompt: 'reply with OK', cwd: work, timeoutMs: 120_000 },
    });
    process.stdout.write(`  run(claude) = ${JSON.stringify({ status: run.status, conversationId: run.conversationId, text: run.text?.slice(0, 60) })}\n`);
    check('live-6 mesh.run(claude) status ok', run.status === 'ok', JSON.stringify(run.status));
    check(
      'live-7 mesh.run(claude) returned a non-empty conversationId',
      typeof run.conversationId === 'string' && run.conversationId.length > 0,
      JSON.stringify(run.conversationId),
    );

    // ── stream ────────────────────────────────────────────────────────────────
    const accepted = await call('mesh.stream', {
      runId: 'live-stream-claude',
      vendor: 'claude',
      task: { prompt: 'reply with OK', cwd: work, timeoutMs: 120_000 },
    });
    check('live-8 mesh.stream(claude) accepted immediately', accepted?.accepted === true, JSON.stringify(accepted));
    const evs = await waitForTerminal('live-stream-claude', 180_000);
    const terminals = evs.filter((e) => e.type === 'done' || e.type === 'error');
    process.stdout.write(`  stream(claude) events = ${JSON.stringify(evs.map((e) => e.type))}\n`);
    check('live-9 the stream produced text events', evs.some((e) => e.type === 'text'), JSON.stringify(evs.map((e) => e.type)));
    check('live-10 exactly one terminal event', terminals.length === 1, `${terminals.length}`);
    check('live-11 the terminal event is a done', terminals[0]?.type === 'done', JSON.stringify(terminals[0]?.type));
  }

  if (vendors.includes('codex')) {
    const models = await call('mesh.availableModels', { vendor: 'codex' });
    check('live-12 mesh.availableModels(codex) is non-empty', Array.isArray(models) && models.length > 0, JSON.stringify(models));

    // A model is named EXPLICITLY here. This machine's codex-cli 0.145.0 defaults
    // to `gpt-6-astra`, which the same CLI is then too old to call ("requires a
    // newer version of Codex"), so an unpinned run reports a truthful
    // `status:'error'` for a reason that has nothing to do with this sidecar.
    // Pinning a model the installed CLI can actually reach keeps this check about
    // the thing it is meant to prove: the argv, the seam, and the result mapping.
    const run = await call('mesh.run', {
      runId: 'live-run-codex',
      vendor: 'codex',
      task: { prompt: 'reply with OK', cwd: work, timeoutMs: 120_000, model: 'gpt-5.6-sol' },
    });
    process.stdout.write(`  run(codex) = ${JSON.stringify({ status: run.status, conversationId: run.conversationId, text: run.text?.slice(0, 60) })}\n`);
    check('live-13 mesh.run(codex) status ok', run.status === 'ok', JSON.stringify(run.status));
    check(
      'live-14 mesh.run(codex) returned a non-empty conversationId',
      typeof run.conversationId === 'string' && run.conversationId.length > 0,
      JSON.stringify(run.conversationId),
    );
  }

  await call('mesh.shutdown', { graceMs: 0 });
}

const guard = setTimeout(() => {
  process.stdout.write('\nlive-check: TIMED OUT\n');
  child.kill('SIGKILL');
  process.exit(1);
}, 600_000);
guard.unref();

main()
  .then(() => {
    process.stdout.write(`\nlive-check: ${checks - failures}/${checks} checks passed\n`);
    if (failures > 0) for (const l of stderrLines.slice(-30)) process.stdout.write(`  ${l}\n`);
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
    process.stdout.write(`\nlive-check: ERROR ${err.message}\n`);
    for (const l of stderrLines.slice(-30)) process.stdout.write(`  ${l}\n`);
    try {
      child.kill('SIGKILL');
    } catch {
      /* already gone */
    }
    process.exit(1);
  });
