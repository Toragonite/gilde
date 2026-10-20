#!/usr/bin/env node
/**
 * mesh-methods.mjs — stub-host integration test for the `mesh.*` run surface.
 *
 * `test/stub-host.mjs` proves the TRANSPORT and the supervision seam. This script
 * proves the layer above it: the methods of DESIGN §5-4, driven end to end over the
 * same NDJSON channel, with `fake_agent` standing in for the `claude` binary so the
 * assertions are deterministic and cost nothing.
 *
 * What is pinned here:
 *
 *   A. handshake        — `capabilities` come off the adapter INSTANCES, and a
 *                         `protocol` mismatch is a hard error, not a guess.
 *   B. mesh.run         — one run produces exactly registerSpawn → spawned →
 *                         exited, and the `RunResult` comes back verbatim.
 *   C. mesh.stream      — `{accepted:true}` first, then events; `text` merged over a
 *                         50ms window; EXACTLY ONE terminal `done`.
 *   D. terminal synth   — a stream that ends with no result event still yields
 *                         exactly one terminal event, an `error`.
 *   E. 16 KiB cap       — no merged `text` message exceeds the cap, and no
 *                         character is lost or duplicated across the split.
 *   F. errors           — an adapter failure is a JSON-RPC error carrying the
 *                         AgentMesh error NAME, and the process survives it.
 *   G. task validation  — a relative or non-existent `cwd` is -32602.
 *   H. mesh.steer       — not registered: -32601 method not found (it was removed).
 *   I. mesh.cancel      — a live run's process group is signalled and the child
 *                         dies; an unknown run is an honest `{stopped:false}`.
 *   J. fleet.route      — the only method that picks a vendor; NoRouteError maps.
 *   K. robustness       — a malformed line and a 9 MiB line are both survivable
 *                         (the host-side halves of these live in the Rust tests).
 *   L. defaults         — `mesh.hello`'s `defaults.permissionMode` reaches argv,
 *                         and the prompt stays the final argv element.
 *   M. terminal synth   — driven through a SCRIPTED event source, because no real
 *                         adapter can be talked into throwing without a terminal
 *                         event: an iterator that throws, one that just ends, and
 *                         one that emits a second terminal all resolve to exactly
 *                         one terminal event.
 *
 * Run:  node test/mesh-methods.mjs        (from src-tauri/sidecar)
 * Exit: 0 all green, 1 on the first failed assertion.
 *
 * Requires `fake_agent` (T06). Build it with:
 *   cargo build --manifest-path ../Cargo.toml --bin fake_agent
 */

import { spawn, execFileSync } from 'node:child_process';
import { existsSync, mkdtempSync, writeFileSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { basename, dirname, join } from 'node:path';
import { fileURLToPath } from 'node:url';

const sidecarDir = join(dirname(fileURLToPath(import.meta.url)), '..');
const bundle = join(sidecarDir, 'sidecar.cjs');
const repoRoot = join(sidecarDir, '..', '..');
const NODE = process.execPath;
/** Real tools a registered spawn must never launch from this file (see the host handler). */
const REAL_TOOLS = new Set(['claude', 'codex', 'cursor-agent', 'agy', 'gemini', 'security']);
const PROTOCOL = 1;
const APP_INSTANCE = 'mesh-methods-instance';

const fakeAgent = ['debug', 'release']
  .map((profile) => join(repoRoot, 'src-tauri', 'target', profile, 'fake_agent'))
  .find((p) => existsSync(p));

if (fakeAgent === undefined) {
  process.stdout.write(
    'mesh-methods: fake_agent is not built.\n' +
      '  cargo build --manifest-path ../Cargo.toml --bin fake_agent\n',
  );
  process.exit(1);
}

const work = mkdtempSync(join(tmpdir(), 'gilde-mesh-methods-'));

/** One JSON object: what `claude -p --output-format json` prints. */
const runResultFile = join(work, 'run-result.json');
writeFileSync(
  runResultFile,
  `${JSON.stringify({
    type: 'result',
    session_id: 'conv-run-1',
    result: 'OK',
    is_error: false,
    model: 'claude-sonnet-5',
    usage: { input_tokens: 11, output_tokens: 3, cache_read_input_tokens: 2 },
  })}\n`,
);

/** A stream-json transcript: init, many tiny text blocks, a tool, then a result. */
const TEXT_BLOCKS = 40;
const streamFile = join(work, 'stream.jsonl');
{
  const lines = [JSON.stringify({ type: 'system', subtype: 'init', session_id: 'conv-stream-1' })];
  for (let i = 0; i < TEXT_BLOCKS; i += 1) {
    lines.push(
      JSON.stringify({ type: 'assistant', message: { content: [{ type: 'text', text: `t${i};` }] } }),
    );
  }
  lines.push(
    JSON.stringify({
      type: 'assistant',
      message: { content: [{ type: 'tool_use', name: 'Read', input: { file: 'a.txt' } }] },
    }),
  );
  lines.push(
    JSON.stringify({
      type: 'result',
      session_id: 'conv-stream-1',
      result: 'done',
      is_error: false,
      usage: { input_tokens: 5, output_tokens: 7 },
    }),
  );
  writeFileSync(streamFile, `${lines.join('\n')}\n`);
}
const EXPECTED_STREAM_TEXT = Array.from({ length: TEXT_BLOCKS }, (_, i) => `t${i};`).join('');

/** A stream that ends WITHOUT a result event — the terminal-synthesis fixture. */
const truncatedFile = join(work, 'truncated.jsonl');
writeFileSync(
  truncatedFile,
  `${JSON.stringify({ type: 'system', subtype: 'init', session_id: 'conv-trunc' })}\n` +
    `${JSON.stringify({ type: 'assistant', message: { content: [{ type: 'text', text: 'partial' }] } })}\n`,
);

/** One 40000-character text block: forces the 16 KiB per-message cap to split. */
const BIG_TEXT_LEN = 40_000;
const bigTextFile = join(work, 'big-text.jsonl');
writeFileSync(
  bigTextFile,
  `${JSON.stringify({
    type: 'assistant',
    message: { content: [{ type: 'text', text: 'a'.repeat(BIG_TEXT_LEN) }] },
  })}\n` +
    `${JSON.stringify({ type: 'result', session_id: 'conv-big', result: 'ok', is_error: false })}\n`,
);

// ── assertion harness ────────────────────────────────────────────────────────
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
function eq(name, actual, expected) {
  check(
    name,
    Object.is(actual, expected),
    `expected ${JSON.stringify(expected)}, got ${JSON.stringify(actual)}`,
  );
}

// ── channel ──────────────────────────────────────────────────────────────────
function startSidecar() {
  const child = spawn(NODE, [bundle], {
    cwd: sidecarDir,
    // GILDE_SIDECAR_SELFTEST unlocks `dev.streamScript` (section M): the scripted
    // driver for the terminal-event invariant. It is off in production.
    //
    // AGENT_CLI_CREDENTIAL_STORE=memory: since agent-mesh 0.1.3 cursor has a real
    // quota(), which reads the developer's macOS keychain (`security
    // find-generic-password`) or ~/.cursor/auth.json and calls Cursor's usage
    // endpoint. `fleet.route` asks every candidate for its quota, so without this the
    // J section would reach the machine's real Cursor login. The "memory" store has no
    // keychain and no login file, so cursor's quota() is `null` without any I/O.
    env: {
      ...process.env,
      GILDE_APP_INSTANCE: APP_INSTANCE,
      GILDE_SIDECAR_SELFTEST: '1',
      AGENT_CLI_CREDENTIAL_STORE: 'memory',
    },
    stdio: ['pipe', 'pipe', 'pipe'],
  });

  const state = {
    child,
    journal: [],
    events: [],
    stderrLines: [],
    skipped: 0,
    pending: new Map(),
    nextId: 0,
    buffered: '',
    /** Launches this host refused because they would have run a real tool. */
    refusedSpawns: [],
  };

  child.stdout.setEncoding('utf8');
  child.stdout.on('data', (chunk) => {
    state.buffered += chunk;
    for (;;) {
      const nl = state.buffered.indexOf('\n');
      if (nl === -1) break;
      const line = state.buffered.slice(0, nl);
      state.buffered = state.buffered.slice(nl + 1);
      if (line.trim().length > 0) onLine(state, line);
    }
  });
  child.stderr.setEncoding('utf8');
  child.stderr.on('data', (d) => {
    for (const l of d.split('\n')) if (l.trim().length > 0) state.stderrLines.push(l);
  });
  return state;
}

function onLine(state, line) {
  let msg;
  try {
    msg = JSON.parse(line);
  } catch {
    state.skipped += 1;
    return;
  }
  if (msg.jsonrpc !== '2.0') {
    state.skipped += 1;
    return;
  }

  if (typeof msg.method === 'string') {
    if (msg.method === 'mesh.event') state.events.push(msg.params);
    else if (msg.method.startsWith('host.') && msg.method !== 'host.log') {
      state.journal.push({ method: msg.method, params: msg.params });
    }
    if (msg.id === undefined || msg.id === null) return;
    let result = { ok: true };
    if (msg.method === 'host.registerSpawn') {
      // Hermeticity backstop: this file never runs a real vendor CLI or the keychain
      // tool. Refusing the plan row makes the sidecar's registered seam reject
      // BEFORE spawning, and the refusal is recorded so a check can fail on it.
      const cmd = String(msg.params?.cmd ?? '');
      if (REAL_TOOLS.has(basename(cmd))) {
        state.refusedSpawns.push(cmd);
        state.child.stdin.write(
          `${JSON.stringify({ jsonrpc: '2.0', id: msg.id, error: { code: -32000, message: `test host refuses to launch ${cmd}` } })}\n`,
        );
        return;
      }
      result = { ok: true, envMarker: `marker-${msg.params.spawnId}` };
    }
    state.child.stdin.write(`${JSON.stringify({ jsonrpc: '2.0', id: msg.id, result })}\n`);
    return;
  }

  const entry = state.pending.get(msg.id);
  if (entry === undefined) return;
  state.pending.delete(msg.id);
  clearTimeout(entry.timer);
  if (msg.error !== undefined) {
    const err = new Error(msg.error.message);
    err.code = msg.error.code;
    err.data = msg.error.data;
    entry.reject(err);
  } else entry.resolve(msg.result);
}

function call(state, method, params, timeoutMs = 60_000) {
  const id = ++state.nextId;
  return new Promise((resolve, reject) => {
    const timer = setTimeout(() => {
      state.pending.delete(id);
      reject(new Error(`sidecar did not answer ${method} within ${timeoutMs}ms`));
    }, timeoutMs);
    state.pending.set(id, { resolve, reject, timer });
    state.child.stdin.write(`${JSON.stringify({ jsonrpc: '2.0', id, method, params })}\n`);
  });
}

/** Await a rejection and return the JSON-RPC error, or `null` when it resolved. */
async function expectError(promise) {
  try {
    await promise;
    return null;
  } catch (err) {
    return err;
  }
}

const sleep = (ms) => new Promise((r) => setTimeout(r, ms));

const trace = (state, spawnId) =>
  state.journal.filter((e) => e.params?.spawnId === spawnId).map((e) => e.method);

function psArgs(pid) {
  try {
    return execFileSync('ps', ['-o', 'args=', '-p', String(pid)], { encoding: 'utf8' }).trim();
  } catch {
    return '';
  }
}

const eventsFor = (state, runId) => state.events.filter((e) => e.runId === runId).map((e) => e.event);

/** Wait until `runId` has emitted a terminal event, or the deadline passes. */
async function waitForTerminal(state, runId, timeoutMs = 30_000) {
  const deadline = Date.now() + timeoutMs;
  for (;;) {
    const evs = eventsFor(state, runId);
    if (evs.some((e) => e.type === 'done' || e.type === 'error')) return evs;
    if (Date.now() > deadline) return evs;
    await sleep(25);
  }
}

// ── the run ──────────────────────────────────────────────────────────────────
const host = startSidecar();

async function main() {
  process.stdout.write(`mesh-methods: ${bundle}\nfixture:      ${fakeAgent}\n\n`);

  // ── A. handshake ───────────────────────────────────────────────────────────
  const hello = await call(host, 'mesh.hello', {
    protocol: PROTOCOL,
    hostVersion: '0.1.0-mesh-methods',
    appInstanceId: APP_INSTANCE,
    bootId: 'mesh-methods-boot',
    // fake_agent stands in for `claude`; the other three keep their real names.
    binaries: { claude: fakeAgent },
    // No default permissionMode on THIS channel: the adapter would put
    // `--permission-mode <mode>` ahead of `extra.args`, and fake_agent reads its
    // mode from argv[0]. The default is covered on its own channel in L below.
    defaults: { timeoutMs: 45 * 60_000 },
  });
  const caps = Object.fromEntries((hello.adapters ?? []).map((a) => [a.vendor, a.capabilities]));
  check(
    'A1 capabilities are read off the adapter instances',
    caps.claude?.stream === true &&
      caps.claude?.steer === 'mid-run' &&
      caps.claude?.quota === true &&
      // agent-mesh 0.1.3: codex streams (new `lineStream` seam); cursor and gemini
      // report quota.
      caps.codex?.stream === true &&
      caps.cursor?.quota === true &&
      caps.gemini?.quota === true,
    JSON.stringify(caps),
  );
  eq('A2 protocol echoed', hello.protocol, PROTOCOL);

  // A mismatched protocol must be a HARD error on a fresh channel.
  {
    const other = startSidecar();
    const err = await expectError(call(other, 'mesh.hello', { protocol: 99 }));
    check('A3 a protocol mismatch is a hard error', err !== null && err.code === -32002, String(err));
    other.child.kill('SIGKILL');
  }

  // ── B. mesh.run ────────────────────────────────────────────────────────────
  const beforeB = host.journal.length;
  const runResult = await call(host, 'mesh.run', {
    runId: 'run-b',
    vendor: 'claude',
    task: {
      prompt: 'reply with OK',
      cwd: work,
      timeoutMs: 30_000,
      extra: { args: ['emit-jsonl', runResultFile] },
    },
  });
  const bSpawnId = host.journal.slice(beforeB)[0]?.params?.spawnId;
  check(
    'B1 one run → registerSpawn → spawned → exited, in order',
    trace(host, bSpawnId).join(' → ') === 'host.registerSpawn → host.spawned → host.exited',
    trace(host, bSpawnId).join(' → '),
  );
  eq('B2 the spawn is attributed to the runId', host.journal.slice(beforeB)[0]?.params?.runId, 'run-b');
  eq('B3 RunResult.vendor', runResult.vendor, 'claude');
  eq('B4 RunResult.conversationId is verbatim', runResult.conversationId, 'conv-run-1');
  eq('B5 RunResult.status', runResult.status, 'ok');
  eq('B6 RunResult.text', runResult.text, 'OK');
  check(
    'B7 usage is mapped through verbatim',
    runResult.usage?.inputTokens === 11 && runResult.usage?.cachedInputTokens === 2,
    JSON.stringify(runResult.usage),
  );
  check('B8 raw vendor payload survives', runResult.raw?.session_id === 'conv-run-1', JSON.stringify(runResult.raw));

  // ── C. mesh.stream ─────────────────────────────────────────────────────────
  const accepted = await call(host, 'mesh.stream', {
    runId: 'run-c',
    vendor: 'claude',
    task: {
      prompt: 'stream please',
      cwd: work,
      timeoutMs: 30_000,
      extra: { args: ['emit-jsonl', streamFile] },
    },
  });
  eq('C1 mesh.stream returns {accepted:true} immediately', accepted.accepted, true);
  const cEvents = await waitForTerminal(host, 'run-c');
  const cTerminals = cEvents.filter((e) => e.type === 'done' || e.type === 'error');
  eq('C2 exactly one terminal event', cTerminals.length, 1);
  eq('C3 the terminal event is a done', cTerminals[0]?.type, 'done');
  eq('C4 done carries the conversationId', cTerminals[0]?.result?.conversationId, 'conv-stream-1');
  const cText = cEvents.filter((e) => e.type === 'text');
  eq('C5 no text is lost by merging', cText.map((e) => e.text).join(''), EXPECTED_STREAM_TEXT);
  check(
    `C6 ${TEXT_BLOCKS} source text blocks were merged into far fewer messages`,
    cText.length > 0 && cText.length <= TEXT_BLOCKS / 4,
    `${cText.length} text event(s)`,
  );
  check(
    'C7 tool and usage events pass through unmerged',
    cEvents.some((e) => e.type === 'tool' && e.name === 'Read') &&
      cEvents.some((e) => e.type === 'usage'),
    JSON.stringify(cEvents.map((e) => e.type)),
  );
  check(
    'C8 the terminal event is last',
    cEvents.indexOf(cTerminals[0]) === cEvents.length - 1,
    JSON.stringify(cEvents.map((e) => e.type)),
  );

  // ── D. a stream with no result event still terminates exactly once ─────────
  await call(host, 'mesh.stream', {
    runId: 'run-d',
    vendor: 'claude',
    task: {
      prompt: 'truncated',
      cwd: work,
      timeoutMs: 30_000,
      extra: { args: ['emit-jsonl', truncatedFile] },
    },
  });
  const dEvents = await waitForTerminal(host, 'run-d');
  const dTerminals = dEvents.filter((e) => e.type === 'done' || e.type === 'error');
  eq('D1 exactly one terminal event when the stream is truncated', dTerminals.length, 1);
  eq('D2 that terminal event is an error', dTerminals[0]?.type, 'error');
  check(
    'D3 the partial text still reached the host',
    dEvents.some((e) => e.type === 'text' && e.text === 'partial'),
    JSON.stringify(dEvents.map((e) => e.type)),
  );

  // ── E. the 16 KiB per-message cap ──────────────────────────────────────────
  await call(host, 'mesh.stream', {
    runId: 'run-e',
    vendor: 'claude',
    task: {
      prompt: 'big',
      cwd: work,
      timeoutMs: 30_000,
      extra: { args: ['emit-jsonl', bigTextFile] },
    },
  });
  const eEvents = await waitForTerminal(host, 'run-e');
  const eText = eEvents.filter((e) => e.type === 'text');
  check('E1 a 40000-char block was split', eText.length >= 2, `${eText.length} text event(s)`);
  check(
    'E2 no merged text message exceeds 16 KiB',
    eText.every((e) => Buffer.byteLength(e.text, 'utf8') <= 16 * 1024),
    JSON.stringify(eText.map((e) => Buffer.byteLength(e.text, 'utf8'))),
  );
  eq('E3 nothing was lost or duplicated in the split', eText.map((e) => e.text).join('').length, BIG_TEXT_LEN);
  eq(
    'E4 still exactly one terminal event',
    eEvents.filter((e) => e.type === 'done' || e.type === 'error').length,
    1,
  );

  // ── M. the terminal-event invariant, forced ────────────────────────────────
  // All four agent-mesh adapters catch their own failures and always yield a
  // terminal event, so these two branches are unreachable through them — and they
  // are exactly the ones that would strand a run as `running` forever. The
  // scripted source reaches them directly.
  await call(host, 'dev.streamScript', {
    runId: 'run-m1',
    events: [{ type: 'text', text: 'partial' }],
    end: 'throw',
    throwMessage: 'iterator blew up',
  });
  const m1 = await waitForTerminal(host, 'run-m1');
  const m1Terminals = m1.filter((e) => e.type === 'done' || e.type === 'error');
  eq('M1 an iterator that throws still yields exactly one terminal', m1Terminals.length, 1);
  check(
    'M2 the synthesized terminal is an error carrying the cause',
    m1Terminals[0]?.type === 'error' && m1Terminals[0]?.message.includes('iterator blew up'),
    JSON.stringify(m1Terminals[0]),
  );
  check(
    'M3 buffered text is flushed before the terminal',
    m1[0]?.type === 'text' && m1[0]?.text === 'partial',
    JSON.stringify(m1.map((e) => e.type)),
  );

  await call(host, 'dev.streamScript', {
    runId: 'run-m2',
    events: [{ type: 'text', text: 'x' }],
    end: 'natural',
  });
  const m2 = await waitForTerminal(host, 'run-m2');
  const m2Terminals = m2.filter((e) => e.type === 'done' || e.type === 'error');
  eq('M4 an iterator that just ends is given a synthesized terminal', m2Terminals.length, 1);
  eq('M5 that terminal is an error', m2Terminals[0]?.type, 'error');

  const doneResult = { vendor: 'claude', conversationId: 'c-m3', text: 'ok', status: 'ok' };
  await call(host, 'dev.streamScript', {
    runId: 'run-m3',
    events: [
      { type: 'done', result: doneResult },
      { type: 'text', text: 'after the end' },
      { type: 'error', message: 'a second terminal' },
    ],
    end: 'natural',
  });
  await waitForTerminal(host, 'run-m3');
  await sleep(150); // let anything that would wrongly follow the terminal arrive
  const m3All = eventsFor(host, 'run-m3');
  eq(
    'M6 a second terminal event is suppressed',
    m3All.filter((e) => e.type === 'done' || e.type === 'error').length,
    1,
  );
  eq('M7 the surviving terminal is the first one', m3All[0]?.type, 'done');
  eq('M8 nothing is emitted after the terminal', m3All.length, 1);

  // ── F. adapter errors become typed JSON-RPC errors ─────────────────────────
  const fErr = await expectError(
    call(host, 'mesh.run', {
      runId: 'run-f',
      vendor: 'claude',
      task: {
        prompt: 'fail please',
        cwd: work,
        timeoutMs: 30_000,
        extra: { args: ['normal', '3'] },
      },
    }),
  );
  check('F1 a failing agent rejects the call', fErr !== null, String(fErr));
  eq('F2 the AgentMesh error name is in data', fErr?.data?.name, 'AdapterExecError');
  eq('F3 the vendor is in data', fErr?.data?.vendor, 'claude');
  eq('F4 the exit code is in data', fErr?.data?.exitCode, 3);
  const aliveAfterF = await call(host, 'mesh.ping', { nonce: 'f' });
  eq('F5 a thrown adapter error never kills the process', aliveAfterF.nonce, 'f');

  const gone = await expectError(
    call(host, 'mesh.run', {
      runId: 'run-f2',
      vendor: 'not-a-vendor',
      task: { prompt: 'x', cwd: work, timeoutMs: 1000 },
    }),
  );
  eq('F6 an unknown vendor is -32602', gone?.code, -32602);

  // ── G. task validation (§5-4) ──────────────────────────────────────────────
  const relative = await expectError(
    call(host, 'mesh.run', {
      runId: 'run-g1',
      vendor: 'claude',
      task: { prompt: 'x', cwd: 'relative/path', timeoutMs: 1000 },
    }),
  );
  eq('G1 a relative cwd is -32602', relative?.code, -32602);
  const missing = await expectError(
    call(host, 'mesh.run', {
      runId: 'run-g2',
      vendor: 'claude',
      task: { prompt: 'x', cwd: join(work, 'no-such-dir'), timeoutMs: 1000 },
    }),
  );
  eq('G2 a non-existent cwd is -32602', missing?.code, -32602);

  // ── H. mesh.steer was removed (it only ever refused) ───────────────────────
  const steer = await expectError(
    call(host, 'mesh.steer', { vendor: 'claude', conversationId: 'c', message: 'hi' }),
  );
  eq('H1 mesh.steer is not a method (-32601)', steer?.code, -32601);

  // ── I. mesh.cancel ─────────────────────────────────────────────────────────
  eq(
    'I1 cancelling an unknown run is an honest {stopped:false, via:"none"}',
    JSON.stringify(await call(host, 'mesh.cancel', { runId: 'never-started' })),
    JSON.stringify({ stopped: false, via: 'none' }),
  );

  const beforeI = host.journal.length;
  const hangingRun = expectError(
    call(
      host,
      'mesh.run',
      {
        runId: 'run-i',
        vendor: 'claude',
        task: { prompt: 'hang', cwd: work, timeoutMs: 120_000, extra: { args: ['hang'] } },
      },
      90_000,
    ),
  );
  // Let the child exist and its pid reach us before cancelling.
  await sleep(900);
  const iSpawned = host.journal.slice(beforeI).find((e) => e.method === 'host.spawned');
  check('I2 the hanging child registered a pid', iSpawned !== undefined);
  const cancel = await call(host, 'mesh.cancel', { runId: 'run-i' });
  eq('I3 cancel reports stopped', cancel.stopped, true);
  eq('I4 cancel went out as killpg', cancel.via, 'killpg');
  const iErr = await hangingRun;
  check('I5 the cancelled run settles instead of hanging', iErr !== null, 'the run resolved');
  await sleep(300);
  check(
    'I6 the child is gone',
    iSpawned === undefined || psArgs(iSpawned.params.pid) === '',
    psArgs(iSpawned?.params?.pid ?? 0),
  );
  const secondCancel = await call(host, 'mesh.cancel', { runId: 'run-i' });
  eq('I7 the run entry was released when it settled', secondCancel.via, 'none');

  // ── J. fleet.route — the only router ───────────────────────────────────────
  const route = await call(host, 'fleet.route', {
    // cursor's quota() is `null` here without any I/O: the sidecar runs with
    // AGENT_CLI_CREDENTIAL_STORE=memory (no keychain, no login file — see
    // startSidecar), so this routes without touching a real CLI or credential.
    task: { prompt: 'anything', cwd: work },
    policy: { prefer: ['cursor'] },
  });
  eq('J1 fleet.route returns the chosen vendor', route.vendor, 'cursor');
  check(
    'J0 routing launched no real CLI and read no keychain',
    host.refusedSpawns.length === 0 &&
      !host.journal.some((e) => e.method === 'host.registerSpawn' && basename(String(e.params?.cmd ?? '')) === 'security'),
    JSON.stringify(host.refusedSpawns),
  );
  const noRoute = await expectError(
    call(host, 'fleet.route', { task: { prompt: 'x' }, policy: { prefer: ['nobody'] } }),
  );
  eq('J2 NoRouteError keeps its AgentMesh name', noRoute?.data?.name, 'NoRouteError');
  const badPolicy = await expectError(
    call(host, 'fleet.route', { task: { prompt: 'x' }, policy: { strategy: 'vibes' } }),
  );
  eq('J3 an unknown strategy is -32602, never silently defaulted', badPolicy?.code, -32602);

  // ── K. robustness of the channel under this surface ────────────────────────
  host.child.stdin.write('not json at all\n');
  host.child.stdin.write(`${JSON.stringify({ id: 1, method: 'mesh.ping' })}\n`);
  eq('K1 a malformed line is skipped, the session continues', (await call(host, 'mesh.ping', { nonce: 'k1' })).nonce, 'k1');
  host.child.stdin.write(`${'x'.repeat(9 * 1024 * 1024)}\n`);
  eq('K2 a 9 MiB line is a protocol violation, not a crash', (await call(host, 'mesh.ping', { nonce: 'k2' })).nonce, 'k2');
  eq('K3 nothing but JSON-RPC ever reached stdout', host.skipped, 0);

  // ── L. the handshake defaults reach the child's argv ───────────────────────
  // On its own channel because `defaults` are fixed at `mesh.hello`, and a default
  // permissionMode changes argv in a way the fake_agent fixture cannot survive.
  {
    const other = startSidecar();
    await call(other, 'mesh.hello', {
      protocol: PROTOCOL,
      appInstanceId: `${APP_INSTANCE}-defaults`,
      binaries: { claude: fakeAgent },
      defaults: { timeoutMs: 7_000, permissionMode: 'acceptEdits' },
    });
    // The run fails (fake_agent sees `--permission-mode` as its mode) — the point
    // is the argv the host was told about before it ever ran.
    await expectError(
      call(other, 'mesh.run', {
        runId: 'run-l',
        vendor: 'claude',
        task: { prompt: 'x', cwd: work, extra: { args: ['normal', '0'] } },
      }),
    );
    const registered = other.journal.find((e) => e.method === 'host.registerSpawn')?.params ?? {};
    const args = registered.args ?? [];
    check(
      'L1 defaults.permissionMode is applied when the task names none',
      args[0] === '--permission-mode' && args[1] === 'acceptEdits',
      JSON.stringify(args),
    );
    check(
      'L2 the prompt is still the final argv element',
      args[args.length - 1] === 'x' && args[args.length - 2] === '--',
      JSON.stringify(args.slice(-3)),
    );
    other.child.kill('SIGKILL');
  }

  await call(host, 'mesh.shutdown', { graceMs: 0 });
}

const guard = setTimeout(() => {
  process.stdout.write('\nmesh-methods: TIMED OUT\n');
  host.child.kill('SIGKILL');
  process.exit(1);
}, 240_000);
guard.unref();

main()
  .then(() => {
    process.stdout.write(`\nmesh-methods: ${checks - failures}/${checks} checks passed\n`);
    if (failures > 0) {
      process.stdout.write('sidecar stderr:\n');
      for (const l of host.stderrLines.slice(-30)) process.stdout.write(`  ${l}\n`);
    }
    setTimeout(() => {
      try {
        host.child.kill('SIGKILL');
      } catch {
        /* already gone */
      }
      process.exit(failures > 0 ? 1 : 0);
    }, 300);
  })
  .catch((err) => {
    process.stdout.write(`\nmesh-methods: ERROR ${err.message}\n`);
    for (const l of host.stderrLines.slice(-30)) process.stdout.write(`  ${l}\n`);
    try {
      host.child.kill('SIGKILL');
    } catch {
      /* already gone */
    }
    process.exit(1);
  });
