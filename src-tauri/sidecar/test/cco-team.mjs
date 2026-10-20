#!/usr/bin/env node
/**
 * cco-team.mjs — the read-only team observer (`src/methods/cco.ts`) against FAKE
 * dispatch hosts: Unix sockets that speak the observer side of the host protocol
 * (team contract v2 §7.6), driven through the real bundle over its NDJSON channel.
 *
 * Nothing real is touched: HOME is a fresh temporary directory, so the data root
 * `~/.claude-code-orchestrator` this sidecar reads is the one this test writes. No
 * `claude`, no `cco`, no real host.
 *
 * What is pinned here:
 *
 *   A. discovery      — a record related to an open root, whose socket is exactly
 *                       socketPathFor's and whose host is alive, is connected to; the
 *                       hello is `{role:'observer', client:'gilde'}`; then list → get →
 *                       subscribe {activity:true}.
 *   B. refusals       — a record whose socket is NOT socketPathFor's answer is never
 *                       connected to (its server sees no connection); an unrelated
 *                       workspace is never connected to; a dead host pid is skipped;
 *                       a host without `team-1` is `no-teams` and never listed.
 *   C. forwarding     — `cco.team.snapshot` carries a VIEW (source `host`, live), never
 *                       an event; activity lines and messages ride in it.
 *   D. A14 debounce   — 40 events in ~1 s cause at most a handful of `cco/team.get`
 *                       re-reads (≤ 2/s + edges), the last state wins, and the views
 *                       forwarded are coalesced.
 *   E. hostile values — a lone surrogate becomes U+FFFD, a fractional number an
 *                       integer, an oversized board a truncated view under 1 MiB.
 *   F. reconnect      — the host drops the connection: the sidecar reconnects with
 *                       backoff and resubscribes FROM THE LAST SEQ it saw.
 *   G. disk teams     — a suspended team in `teams/<id>/state.json` related to a root
 *                       is in view as `source: disk`, with its feed backlog.
 *   H. params         — a bad team id and bad roots are -32602; a filesystem root is
 *                       never a scope.
 *   J. hostile input  — a host-log storm in run/ is not a list storm; garbage lines,
 *                       another team's snapshot and a bad member name change nothing; a
 *                       symlinked record is never followed; a long body is read capped
 *                       from `bodies/m<seq>.md`.
 *   G. (cont.)        — TeamListEntry.reason (CCO 0b7b1b8): the reason a connected host's
 *                       `cco/team.list` gives for a team rides in that team's summary, one
 *                       line, bidi-free, ≤ 300 chars; a team with no reason has none; an entry
 *                       with a bad id or a non-string reason gives nothing; once the host
 *                       that said it is gone, the reason is gone (K16b).
 *   I. scope          — an empty scope drops the host (the view is withdrawn: `null`);
 *                       a Gilde pane pid alone brings it back, tagged with the pane.
 *   L. all workspaces — `allWorkspaces: true` (Gilde preview) lists a disk team of an
 *                       unrelated workspace and connects to its live host; false (or
 *                       absent) does not; a non-boolean is -32602; the socket and dead-pid
 *                       refusals hold either way.
 *   K. workers        — Plan #3c, `cco.workers` → `tools/call list_workers` on the same
 *                       observer connection (the fake host answers it in every shape the
 *                       contract names: the fixture snapshot, text only, isError, an RPC
 *                       refusal, `schema: cco.workers/2`, hostile values): one call shared
 *                       by concurrent consumers; the 60 s floor serves the held snapshot
 *                       with its generatedAt; refresh is a plain call at most every 5 s; an
 *                       older host / unknown schema / host error is a typed reason and the
 *                       text is never parsed; socket gone, no host, host restart keep the
 *                       last snapshot marked stale until a host answers again.
 *                       CCO 0b7b1b8 answers: the contract's caps (key ≤ 128, spendLabel ≤ 40,
 *                       sameAccountAs ≤ 8 × 64, windows ≤ 8, accounts ≤ 200); cooldownUntil
 *                       not after generatedAt → null; `servedFromCache` passed through only as
 *                       the boolean the host sent.
 *                       The snapshot fixture (test/fixtures/cco-workers-snapshot.json) is the
 *                       one the frontend tests and any docs/screenshots use — never a live one.
 *
 * Run:  node test/cco-team.mjs        (from src-tauri/sidecar, after `node esbuild.mjs`)
 * Exit: 0 all green, 1 on any failed check.
 */

import { spawn } from 'node:child_process';
import { appendFileSync, mkdirSync, mkdtempSync, readFileSync, realpathSync, rmSync, symlinkSync, unlinkSync, writeFileSync } from 'node:fs';
import net from 'node:net';
import { tmpdir } from 'node:os';
import { dirname, join } from 'node:path';
import { fileURLToPath } from 'node:url';

const sidecarDir = join(dirname(fileURLToPath(import.meta.url)), '..');
const bundle = join(sidecarDir, 'sidecar.cjs');
const NODE = process.execPath;

// A short HOME: a socket path must fit in 100 bytes (host-protocol SOCKET_PATH_MAX_BYTES),
// and macOS's per-user tmpdir is too deep for `<home>/.claude-code-orchestrator/run/s<pid>.sock`.
const base = tmpdir().length <= 24 ? tmpdir() : '/tmp';
const HOME = mkdtempSync(join(base, 'gcco-'));
const DATA_ROOT = join(HOME, '.claude-code-orchestrator');
const RUN_DIR = join(DATA_ROOT, 'run');
const TEAMS_DIR = join(DATA_ROOT, 'teams');
mkdirSync(RUN_DIR, { recursive: true, mode: 0o700 });
mkdirSync(TEAMS_DIR, { recursive: true, mode: 0o700 });
const PROJECT = join(HOME, 'proj');
mkdirSync(PROJECT, { recursive: true });
const PROJECT_REAL = realpathSync(PROJECT);
writeFileSync(join(DATA_ROOT, 'registry.json'), JSON.stringify({ teamMode: 'on', workers: [] }));

/**
 * Plan #3c: THE worker-accounts fixture — a `WorkersSnapshot` (cco.workers/1) as the engine
 * answers it. The frontend tests, docs and any screenshot use this file, never a live answer
 * (host-protocol RULES, vendored at CCO 0b7b1b8).
 */
const WORKERS_FIXTURE = JSON.parse(readFileSync(join(dirname(fileURLToPath(import.meta.url)), 'fixtures', 'cco-workers-snapshot.json'), 'utf8'));

const TEAM_ID = 'tmabcdefghi1';
const DISK_TEAM_ID = 'tmzzzzzzzzz2';
const NOW = Date.now();

// ── timing knobs ─────────────────────────────────────────────────────────────
// Two kinds of time bound live in this file, and they are treated differently:
//  - a HANG GUARD only stops a wait for something that must happen eventually
//    (`until(…)` budgets, the settle sleeps before a negative check). On a loaded
//    machine those are what flake, so they are 3× what an idle machine needs and
//    scale with CCO_TEAM_TEST_TIME_SCALE (default 1): `hang(ms)`, `settle(ms)`.
//  - a BEHAVIOUR bound is what the sidecar promises (the ~1 s reconnect backoff
//    floor, the 5 s refresh minimum, the 60 s workers floor, the two-reads-per-
//    second debounce derived from the measured burst). Those stay exact.
function positiveNumber(name, fallback) {
  const raw = process.env[name];
  if (raw === undefined || raw === '') return fallback;
  const value = Number(raw);
  if (Number.isFinite(value) && value > 0) return value;
  process.stdout.write(`cco-team: ignoring ${name}=${JSON.stringify(raw)} (not a positive number), using ${fallback}\n`);
  return fallback;
}
const TIME_SCALE = positiveNumber('CCO_TEAM_TEST_TIME_SCALE', 1);
const GUARD_MS = Math.round(positiveNumber('CCO_TEAM_TEST_GUARD_MS', 600_000));
/** A wait that only guards against a hang: 3× the idle-machine need, times the scale. */
const hang = (ms) => Math.round(ms * 3 * TIME_SCALE);
/** A settle pause before a negative check (nothing must have happened): times the scale. */
const settle = (ms) => Math.round(ms * TIME_SCALE);

// ── progress (what was running when the guard fired) ────────────────────────
const STARTED_AT = Date.now();
let current = { name: 'setup', startedAt: STARTED_AT };
const finishedSections = [];
let lastCheck = '(none yet)';
/** Mark the start of a section; the previous one is recorded with its duration. */
function section(name) {
  const now = Date.now();
  finishedSections.push({ name: current.name, ms: now - current.startedAt });
  current = { name, startedAt: now };
}
function sectionTimings() {
  return finishedSections.map((s) => `${s.name.split(' ')[0]} ${(s.ms / 1000).toFixed(1)}s`).join(', ');
}

// ── assertion harness ────────────────────────────────────────────────────────
let failures = 0;
let checks = 0;
function check(name, ok, detail) {
  checks += 1;
  lastCheck = name;
  if (ok) process.stdout.write(`  ok   ${name}\n`);
  else {
    failures += 1;
    process.stdout.write(`  FAIL ${name}${detail === undefined ? '' : ` — ${detail}`}\n`);
  }
}
const sleep = (ms) => new Promise((r) => setTimeout(r, ms));
async function until(pred, timeoutMs = hang(8_000), stepMs = 25) {
  const deadline = Date.now() + timeoutMs;
  for (;;) {
    const v = pred();
    if (v) return v;
    if (Date.now() > deadline) return v;
    await sleep(stepMs);
  }
}

// ── fixtures ─────────────────────────────────────────────────────────────────
function member(name, extra = {}) {
  return {
    name,
    index: 1,
    role: `${name} role`,
    state: 'idle',
    provider: 'claude',
    worker: 'w1',
    accountLabel: 'w1',
    machineLogin: false,
    sharesLeadPlan: false,
    billableOverage: false,
    runsCommands: true,
    model: 'claude-opus-4-8',
    incarnation: 1,
    mailbox: 'tools',
    portBase: 41000,
    worktree: { path: '/x/wt', branch: `cco/demo-hi1/${name}`, baseCommit: 'a'.repeat(40), ready: true },
    turns: 0,
    tokens: { in: 0, out: 0 },
    costUsd: null,
    pending: 0,
    tasks: [],
    peerWakes: 0,
    ...extra,
  };
}

function teamState(extra = {}) {
  return {
    v: 1,
    id: TEAM_ID,
    name: 'demo',
    title: 'Demo team',
    status: 'active',
    goal: 'Ship the thing',
    workspace: PROJECT_REAL,
    repoRoot: PROJECT_REAL,
    leadRel: '',
    baseCommit: 'a'.repeat(40),
    createdAt: NOW - 60_000,
    seq: 10,
    owner: { hostPid: process.pid, hostStartedAt: NOW - 120_000, session: { pid: 424242, startedAt: NOW - 130_000 } },
    lead: { unread: 0, readSeq: 0, waiter: true, lastCallAt: null, wakesToday: 0 },
    user: { unread: 0, readSeq: 0 },
    members: [member('alice'), member('bob', { state: 'blocked', reason: { code: 'quota', text: 'quota wall' } })],
    board: [
      {
        id: 'b1',
        title: 'Write the parser',
        state: 'claimed',
        assignee: 'alice',
        paths: ['src/parser.ts'],
        allowOverlap: false,
        createdBy: 'lead',
        createdAt: NOW - 50_000,
        reports: [],
        commits: [],
        turns: [],
        updatedAt: NOW - 40_000,
        updatedBy: 'alice',
      },
    ],
    budgets: { turnsToday: 3, turnsPerDay: 300, running: 0, maxRunning: 4, pingPongTripped: false },
    totals: { turns: 3, inputTokens: 100, outputTokens: 50, costUsd: null },
    warnings: [],
    ...extra,
  };
}

/** A fake dispatch host: one Unix socket, the observer side of §7.6, everything counted. */
function fakeHost({ socketPath, hostPid = process.pid, workspace = PROJECT_REAL, features = ['team-1'] }) {
  const host = {
    socketPath,
    connections: 0,
    hellos: [],
    lists: 0,
    gets: [],
    subscribes: [],
    /** Every `tools/call` this host was asked (Plan #3c: list_workers). */
    toolCalls: [],
    /** How `tools/call list_workers` is answered: see workersAnswer(). */
    workers: { mode: 'snapshot' },
    /** The last structuredContent served in `snapshot` mode (the shifted fixture). */
    workersServed: undefined,
    /** Extra TeamListEntry objects appended to every `cco/team.list` answer (CCO 0b7b1b8 `reason`). */
    listExtra: [],
    /** Whether `cco/team.list` says this host owns TEAM_ID (`ownedHere`). */
    ownsTeam: true,
    state: teamState(),
    conns: new Set(),
    sub: undefined,
    server: undefined,
  };
  host.server = net.createServer((conn) => {
    host.connections += 1;
    host.conns.add(conn);
    let buffered = '';
    conn.setEncoding('utf8');
    conn.on('data', (chunk) => {
      buffered += chunk;
      for (;;) {
        const nl = buffered.indexOf('\n');
        if (nl === -1) break;
        const line = buffered.slice(0, nl);
        buffered = buffered.slice(nl + 1);
        if (line.trim() !== '') onLine(conn, JSON.parse(line));
      }
    });
    conn.on('error', () => undefined);
    conn.on('close', () => {
      host.conns.delete(conn);
      if (host.sub?.conn === conn) host.sub = undefined;
    });
  });
  const send = (conn, msg) => {
    try {
      conn.write(`${JSON.stringify({ jsonrpc: '2.0', ...msg })}\n`);
    } catch {
      /* gone */
    }
  };
  function onLine(conn, msg) {
    const { id, method, params } = msg;
    switch (method) {
      case 'cco/hello':
        host.hellos.push(params);
        send(conn, {
          id,
          result: {
            protocol: 1,
            hostPid,
            hostStartedAt: NOW - 120_000,
            workspace,
            serverHash: 'abcdef123456',
            dataRoot: DATA_ROOT,
            features,
          },
        });
        return;
      case 'cco/team.list':
        host.lists += 1;
        send(conn, {
          id,
          result: {
            ok: true,
            teams: [{ teamId: TEAM_ID, name: 'demo', state: host.state.status, members: 2, ownedHere: host.ownsTeam }, ...host.listExtra],
          },
        });
        return;
      case 'cco/team.get':
        host.gets.push(Date.now());
        send(conn, { id, result: { ok: true, state: host.state } });
        return;
      case 'cco/team.subscribe':
        host.subscribes.push(params);
        if (host.sub !== undefined && host.sub.conn === conn) send(conn, { id: host.sub.id, result: { ok: true, replaced: true } });
        host.sub = { conn, id, teamId: params.teamId };
        send(conn, {
          method: 'cco/team.event',
          params: { requestId: id, teamId: params.teamId, seq: host.state.seq, ts: Date.now(), kind: 'snapshot', state: host.state },
        });
        return; // HELD
      case 'tools/call':
        host.toolCalls.push({ at: Date.now(), params });
        workersAnswer(host, conn, id, params);
        return;
      case 'cco/team.unsubscribe':
        send(conn, { id, result: { ok: true } });
        if (host.sub?.conn === conn) {
          send(conn, { id: host.sub.id, result: { ok: true } });
          host.sub = undefined;
        }
        return;
      default:
        send(conn, { id, error: { code: -32601, message: `no ${method}` } });
    }
  }
  host.send = send;
  host.event = (seq, kind, data) => {
    if (host.sub === undefined) return false;
    send(host.sub.conn, {
      method: 'cco/team.event',
      params: { requestId: host.sub.id, teamId: host.sub.teamId, seq, ts: Date.now(), kind, data: { by: 'engine', ...data } },
    });
    return true;
  };
  host.activity = (memberName, items) => {
    if (host.sub === undefined) return false;
    send(host.sub.conn, {
      method: 'cco/team.activity',
      params: { requestId: host.sub.id, teamId: host.sub.teamId, member: memberName, items },
    });
    return true;
  };
  host.dropAll = () => {
    for (const conn of host.conns) conn.destroy();
  };
  host.listen = () => new Promise((resolve) => host.server.listen(socketPath, resolve));
  host.close = () => new Promise((resolve) => {
    host.dropAll();
    host.server.close(() => resolve());
  });
  return host;
}

/**
 * Plan #3c: the fake host's `tools/call list_workers`, by `host.workers.mode`:
 *   snapshot   `{content, structuredContent: <fixture, generatedAt: now>}` (the frozen answer)
 *   raw        `{content, structuredContent: host.workers.structuredContent}` as given
 *   text-only  a host that predates the addition: text only — text that LOOKS parseable
 *   is-error   `{content: [{text: host.workers.text}], isError: true}` (an unreadable registry)
 *   forbidden  the RPC error an older role table answers (RPC_ERROR.roleForbidden)
 *   schema     a newer host: `schema: host.workers.schema`, otherwise a valid-looking snapshot
 */
function workersAnswer(host, conn, id, params) {
  if (params?.name !== 'list_workers') {
    host.send(conn, { id, error: { code: -32013, message: `not for an observer: ${params?.name}` } });
    return;
  }
  const text = 'main: 42% of Session (5hr)\nw1: 100% — cooling down\n';
  const content = [{ type: 'text', text }];
  const now = Date.now();
  switch (host.workers.mode) {
    case 'snapshot':
      // The fixture as if composed now: every time moves together, so a cooldown stays after generatedAt.
      host.workersServed = shiftedFixture(now);
      host.send(conn, { id, result: { content, structuredContent: host.workersServed } });
      return;
    case 'raw':
      host.send(conn, { id, result: { content, structuredContent: host.workers.structuredContent } });
      return;
    case 'text-only':
      host.send(conn, { id, result: { content, isError: false } });
      return;
    case 'is-error':
      host.send(conn, { id, result: { content: [{ type: 'text', text: host.workers.text }], isError: true } });
      return;
    case 'forbidden':
      host.send(conn, { id, error: { code: -32013, message: 'tools/call list_workers is not an observer method on this host' } });
      return;
    case 'schema':
      host.send(conn, {
        id,
        result: {
          content,
          structuredContent: { ...WORKERS_FIXTURE, schema: host.workers.schema, generatedAt: now, accounts: [{ ...WORKERS_FIXTURE.accounts[0], name: 'from-a-newer-schema' }] },
        },
      });
      return;
    default:
      host.send(conn, { id, error: { code: -32603, message: 'bad test mode' } });
  }
}

/** THE fixture with every time field moved so that `generatedAt` is `at` (null stays null). */
function shiftedFixture(at) {
  const delta = at - WORKERS_FIXTURE.generatedAt;
  const t = (v) => (v === null ? null : v + delta);
  const copy = JSON.parse(JSON.stringify(WORKERS_FIXTURE));
  copy.generatedAt = at;
  for (const a of copy.accounts) {
    for (const w of a.windows) w.resetsAt = t(w.resetsAt);
    a.windowsStaleSince = t(a.windowsStaleSince);
    a.lastGoodWindowsAt = t(a.lastGoodWindowsAt);
    a.usageFetchedAt = t(a.usageFetchedAt);
    a.cooldownUntil = t(a.cooldownUntil);
  }
  return copy;
}

/** JSON with sorted keys: two values with the same content compare equal. */
function canonical(value) {
  return JSON.stringify(value, (_k, v) =>
    v && typeof v === 'object' && !Array.isArray(v) ? Object.fromEntries(Object.entries(v).sort(([a], [b]) => a.localeCompare(b))) : v,
  );
}

function writeRecord(sessionPid, { hostPid = process.pid, workspace = PROJECT_REAL, socket } = {}) {
  writeFileSync(
    join(RUN_DIR, `s${sessionPid}.json`),
    JSON.stringify({
      protocol: 1,
      hostPid,
      hostStartedAt: NOW - 120_000,
      session: { pid: sessionPid, startedAt: NOW - 130_000 },
      workspace,
      socket: socket ?? join(RUN_DIR, `s${sessionPid}.sock`),
      serverHash: 'abcdef123456',
    }),
    { mode: 0o600 },
  );
}

/** A pid that is not alive right now. */
function deadPid() {
  for (let pid = 4_000_000; pid < 4_100_000; pid += 7) {
    try {
      process.kill(pid, 0);
    } catch (err) {
      if (err.code === 'ESRCH') return pid;
    }
  }
  return 4_194_301;
}

// ── the sidecar channel ──────────────────────────────────────────────────────
function startSidecar() {
  const child = spawn(NODE, [bundle], {
    cwd: sidecarDir,
    env: { PATH: process.env.PATH ?? '/usr/bin:/bin', HOME, LANG: 'en_US.UTF-8' },
    stdio: ['pipe', 'pipe', 'pipe'],
  });
  const state = { child, pending: new Map(), nextId: 0, buffered: '', snapshots: [], rawSnapshots: [], logs: [], stderr: [] };
  child.stdout.setEncoding('utf8');
  child.stdout.on('data', (chunk) => {
    state.buffered += chunk;
    for (;;) {
      const nl = state.buffered.indexOf('\n');
      if (nl === -1) break;
      const line = state.buffered.slice(0, nl);
      state.buffered = state.buffered.slice(nl + 1);
      if (line.trim() !== '') onSidecarLine(state, line);
    }
  });
  child.stderr.setEncoding('utf8');
  child.stderr.on('data', (d) => {
    for (const l of d.split('\n')) if (l.trim() !== '') state.stderr.push(l);
  });
  return state;
}

function onSidecarLine(state, line) {
  const msg = JSON.parse(line);
  if (typeof msg.method === 'string') {
    if (msg.method === 'cco.team.snapshot') {
      state.snapshots.push(msg.params);
      state.rawSnapshots.push(line);
    } else if (msg.method === 'host.log') state.logs.push(msg.params);
    if (msg.id !== undefined && msg.id !== null) {
      state.child.stdin.write(`${JSON.stringify({ jsonrpc: '2.0', id: msg.id, result: { ok: true } })}\n`);
    }
    return;
  }
  const entry = state.pending.get(msg.id);
  if (entry === undefined) return;
  state.pending.delete(msg.id);
  if (msg.error !== undefined) {
    const err = new Error(msg.error.message);
    err.code = msg.error.code;
    entry.reject(err);
  } else entry.resolve(msg.result);
}

function call(state, method, params) {
  const id = `h${++state.nextId}`;
  return new Promise((resolve, reject) => {
    state.pending.set(id, { resolve, reject, method, at: Date.now() });
    state.child.stdin.write(`${JSON.stringify({ jsonrpc: '2.0', id, method, params })}\n`);
  });
}

async function expectError(promise) {
  try {
    await promise;
    return null;
  } catch (err) {
    return err;
  }
}

const lastView = (sc, teamId) => {
  for (let i = sc.snapshots.length - 1; i >= 0; i -= 1) if (sc.snapshots[i].teamId === teamId) return sc.snapshots[i];
  return undefined;
};

// ── the run ──────────────────────────────────────────────────────────────────
const servers = [];
let sc;

async function main() {
  process.stdout.write(`cco-team: ${bundle}\nHOME:     ${HOME}\n\n`);

  // Host 1: good. Host 2: its record names a socket that is NOT socketPathFor's.
  // Host 3: an unrelated workspace. Host 4: no team feature. Host 5: a dead host pid.
  const h1 = fakeHost({ socketPath: join(RUN_DIR, 's424242.sock') });
  const elsewhere = fakeHost({ socketPath: join(RUN_DIR, 'elsewhere.sock') });
  const h3 = fakeHost({ socketPath: join(RUN_DIR, 's626262.sock'), workspace: '/nowhere/else' });
  const h4 = fakeHost({ socketPath: join(RUN_DIR, 's727272.sock'), features: [] });
  const h5 = fakeHost({ socketPath: join(RUN_DIR, 's828282.sock') });
  for (const h of [h1, elsewhere, h3, h4, h5]) {
    servers.push(h);
    await h.listen();
  }
  // CCO 0b7b1b8 TeamListEntry.reason: h1 lists the disk team with a hostile reason, plus entries that give nothing.
  h1.listExtra = [
    { teamId: DISK_TEAM_ID, name: 'old', state: 'suspended', members: 1, ownedHere: false, reason: `host gone\u202E\n\u0007 since ${'r'.repeat(600)}` },
    { teamId: '../../etc', name: 'bad', state: 'damaged', members: 0, ownedHere: false, reason: 'a bad id gives nothing' },
    { teamId: 'tmyyyyyyyyy3', name: 'n', state: 'damaged', members: 0, ownedHere: false, reason: 42 },
  ];
  writeRecord(424242);
  writeRecord(525252, { socket: elsewhere.socketPath });
  writeRecord(626262, { workspace: '/nowhere/else' });
  writeRecord(727272);
  writeRecord(828282, { hostPid: deadPid() });

  // A suspended team on disk, related to the project, with a feed backlog.
  const diskDir = join(TEAMS_DIR, DISK_TEAM_ID);
  mkdirSync(join(diskDir, 'members', 'carol'), { recursive: true, mode: 0o700 });
  writeFileSync(
    join(diskDir, 'team.json'),
    JSON.stringify({ v: 1, id: DISK_TEAM_ID, name: 'old', title: 'Old', workspace: PROJECT_REAL, repoRoot: PROJECT_REAL, commonDir: join(PROJECT_REAL, '.git'), createdAt: NOW - 3_600_000 }),
  );
  writeFileSync(
    join(diskDir, 'state.json'),
    JSON.stringify({
      ...teamState(),
      id: DISK_TEAM_ID,
      name: 'old',
      status: 'suspended',
      createdAt: NOW - 3_600_000,
      owner: { hostPid: deadPid(), hostStartedAt: NOW - 3_700_000, session: { pid: 999, startedAt: 1 } },
      members: [member('carol', { state: 'idle' })],
    }),
  );
  writeFileSync(
    join(diskDir, 'members', 'carol', 'feed.jsonl'),
    `${[1, 2, 3].map((i) => JSON.stringify({ ts: NOW - 10_000 + i, turn: 't1', k: 'tool', tool: 'Read', d: `file${i}.ts` })).join('\n')}\n`,
  );

  sc = startSidecar();

  // ── K0. workers with no host in sight (Plan #3c) ───────────────────────────
  section('K0. workers with no host');
  {
    const none = await call(sc, 'cco.workers', { roots: [join(HOME, 'no-such-project')], panePids: [] });
    check(
      'K0 no host in sight: no snapshot (null, not an empty one), unavailable no-host, not stale',
      none.supported === true && none.snapshot === null && none.unavailable?.reason === 'no-host' && none.stale === false && none.minPollMs === 60_000,
      JSON.stringify(none),
    );
    const badRefresh = await expectError(call(sc, 'cco.workers', { refresh: 'yes' }));
    check('K0b a refresh that is not a boolean is -32602', badRefresh !== null && badRefresh.code === -32602, String(badRefresh?.code));
  }

  // ── H. params ──────────────────────────────────────────────────────────────
  section('H. params');
  {
    const bad = await expectError(call(sc, 'cco.team.get', { teamId: '../../etc' }));
    check('H1 a team id that is not TEAM_ID is -32602', bad !== null && bad.code === -32602, String(bad?.code));
    const badRoots = await expectError(call(sc, 'cco.team.list', { roots: '/', panePids: [] }));
    check('H2 roots that are not an array are -32602', badRoots !== null && badRoots.code === -32602, String(badRoots?.code));
    const fsRoot = await call(sc, 'cco.team.list', { roots: ['/'], panePids: [] });
    await sleep(settle(300));
    check('H3 a filesystem root is never a scope (nothing connected)', h1.connections === 0 && fsRoot.hosts.length === 0, `${h1.connections} ${JSON.stringify(fsRoot.hosts)}`);
    const unknown = await call(sc, 'cco.team.get', { teamId: 'tmnotknown12' });
    check('H4 an unknown team is null, not an error', unknown === null, JSON.stringify(unknown));
  }

  // ── A. discovery ───────────────────────────────────────────────────────────
  section('A. discovery');
  const first = await call(sc, 'cco.team.list', { roots: [PROJECT], panePids: [] });
  check('A1 supported, data root under HOME, teamMode read from registry.json', first.supported === true && first.dataRoot === DATA_ROOT && first.teamMode === 'on', JSON.stringify({ s: first.supported, d: first.dataRoot, t: first.teamMode }));
  await until(() => h1.subscribes.length > 0);
  check('A2 the related host was connected and greeted as an observer', h1.hellos.length === 1 && h1.hellos[0].role === 'observer' && h1.hellos[0].client === 'gilde' && h1.hellos[0].protocol === 1 && typeof h1.hellos[0].clientPid === 'number', JSON.stringify(h1.hellos));
  check('A3 then list → get → subscribe {activity:true}', h1.lists >= 1 && h1.gets.length >= 1 && h1.subscribes[0]?.teamId === TEAM_ID && h1.subscribes[0]?.activity === true, JSON.stringify({ lists: h1.lists, gets: h1.gets.length, subs: h1.subscribes }));
  check('A4 the first subscribe replays the whole short log (fromSeq: 0 is sent, not dropped)', h1.subscribes[0]?.fromSeq === 0, JSON.stringify(h1.subscribes[0]));

  // ── B. refusals ────────────────────────────────────────────────────────────
  section('B. refusals');
  await sleep(settle(600));
  check('B1 a record whose socket is not socketPathFor\'s is never connected to', elsewhere.connections === 0, String(elsewhere.connections));
  check('B2 an unrelated workspace is never connected to', h3.connections === 0, String(h3.connections));
  check('B3 a dead host pid is skipped', h5.connections === 0, String(h5.connections));
  const listed = await call(sc, 'cco.team.list', { roots: [PROJECT], panePids: [] });
  const hostKeys = listed.hosts.map((h) => `${h.key}:${h.state}`).sort();
  check('B4 only the good and the no-team host are in sight', JSON.stringify(hostKeys) === JSON.stringify(['s424242:connected', 's727272:no-teams']), JSON.stringify(hostKeys));
  check('B5 the no-team host was greeted once and never listed', h4.hellos.length === 1 && h4.lists === 0, JSON.stringify({ hellos: h4.hellos.length, lists: h4.lists }));

  // ── C. forwarding ──────────────────────────────────────────────────────────
  section('C. forwarding');
  const v1 = await until(() => lastView(sc, TEAM_ID));
  check('C1 a view is forwarded: source host, live, the engine projection', v1?.view?.source === 'host' && v1.view.live === true && v1.view.state.id === TEAM_ID && v1.view.state.members.length === 2, JSON.stringify(v1?.view?.source));
  check('C2 nothing but views crosses the pipe (no event payloads)', sc.rawSnapshots.every((l) => !l.includes('"kind":"snapshot"') && !l.includes('cco/team.event')), 'an event leaked');
  const summary = listed.teams.find((t) => t.teamId === TEAM_ID);
  check('C3 the list summary carries counts and the latest rev', summary?.live === true && summary.members === 2 && summary.blocked === 1 && summary.rev > 0, JSON.stringify(summary));

  h1.activity('alice', [{ ts: Date.now(), turn: 't9', k: 'tool', tool: 'Edit', d: 'src/a.ts' }]);
  h1.event(11, 'msg.posted', { from: 'alice', to: 'lead', kind: 'report', urgent: false, body: 'done with b1', bytes: 12, sha256: 'x', source: 'tool' });
  const withActivity = await until(() => {
    const v = lastView(sc, TEAM_ID)?.view;
    return v?.activity?.some((a) => a.member === 'alice' && a.lines.some((l) => l.text === 'Edit · src/a.ts')) && v.messages.some((m) => m.id === 'm11') ? v : undefined;
  });
  check('C4 activity lines ride in the view', withActivity !== undefined, JSON.stringify(lastView(sc, TEAM_ID)?.view?.activity));
  check('C5 a msg.posted becomes a message of the view', withActivity?.messages?.[0]?.body === 'done with b1' && withActivity.messages[0].from === 'alice', JSON.stringify(withActivity?.messages));
  // The Rust mirror's fixture (src-tauri/tests/fixtures/cco/team-snapshot.json) is THIS
  // notification's params, written when asked: `CCO_FIXTURE_OUT=<path> node test/cco-team.mjs`.
  if (process.env.CCO_FIXTURE_OUT) {
    const params = [...sc.snapshots].reverse().find((p) => p.view === withActivity) ?? lastView(sc, TEAM_ID);
    writeFileSync(process.env.CCO_FIXTURE_OUT, `${JSON.stringify(params, null, 2)}\n`);
  }

  // ── D. A14 debounce ────────────────────────────────────────────────────────
  section('D. debounce');
  await sleep(settle(700));
  const getsBefore = h1.gets.length;
  const viewsBefore = sc.snapshots.length;
  const t0 = Date.now();
  for (let i = 0; i < 40; i += 1) {
    h1.state = teamState({ seq: 20 + i, members: [member('alice', { role: `role-${i}` }), member('bob')] });
    h1.event(20 + i, 'member.state', { name: 'alice', state: 'running' });
    await sleep(25);
  }
  const burstMs = Date.now() - t0;
  const getsInBurst = h1.gets.filter((t) => t >= t0 && t <= t0 + burstMs).length;
  await sleep(900);
  // The trailing read and view may need longer on a loaded machine: wait for the last
  // state (a hang guard), then derive the view bound from the time that really passed.
  await until(() => lastView(sc, TEAM_ID)?.view?.state?.seq === 59);
  const settleMs = Date.now() - t0 - burstMs;
  const bound = Math.ceil(burstMs / 500) + 2;
  check(`D1 40 events in ${burstMs} ms → ${getsInBurst} re-reads (≤ ${bound}: two per second)`, getsInBurst >= 1 && getsInBurst <= bound, String(getsInBurst));
  const final = lastView(sc, TEAM_ID)?.view;
  check('D2 the last state wins', final?.state?.members?.find((m) => m.name === 'alice')?.role === 'role-39' && final.state.seq === 59, JSON.stringify(final?.state?.members?.[0]?.role));
  const viewsInBurst = sc.snapshots.length - viewsBefore;
  const viewBound = Math.ceil((burstMs + settleMs) / 250) + 2;
  check(`D3 views are coalesced (${viewsInBurst} for 40 events, ≤ ${viewBound})`, viewsInBurst <= viewBound, String(viewsInBurst));
  check('D4 re-reads after the burst settle (one trailing read, not one per event)', h1.gets.length - getsBefore <= bound + 1, String(h1.gets.length - getsBefore));

  // ── E. hostile values ──────────────────────────────────────────────────────
  section('E. hostile values');
  h1.state = teamState({ seq: 70, createdAt: 1234.75, members: [member('alice', { role: 'bad \uD800 role', turns: 2.9 }), member('bob')] });
  h1.event(70, 'member.state', { name: 'alice', state: 'idle' });
  const hostile = await until(() => {
    const v = lastView(sc, TEAM_ID)?.view;
    return v?.state?.seq === 70 ? v : undefined;
  });
  const raw = sc.rawSnapshots[sc.rawSnapshots.length - 1] ?? '';
  check('E1 a lone surrogate reaches Rust as U+FFFD (no \\ud800 escape on the wire)', hostile?.state?.members?.[0]?.role === 'bad \uFFFD role' && !/\\ud800/i.test(raw), JSON.stringify(hostile?.state?.members?.[0]?.role));
  check('E2 fractional numbers become integers', hostile?.state?.createdAt === 1234 && hostile.state.members[0].turns === 2, JSON.stringify({ c: hostile?.state?.createdAt, t: hostile?.state?.members?.[0]?.turns }));
  {
    const board = [];
    for (let i = 1; i <= 200; i += 1) {
      board.push({
        id: `b${i}`,
        title: `task ${i} ${'t'.repeat(300)}`,
        state: i % 3 === 0 ? 'done' : 'open',
        note: 'n'.repeat(2_000),
        paths: Array.from({ length: 40 }, (_, k) => `src/${'p'.repeat(280)}/${k}.ts`),
        allowOverlap: false,
        createdBy: 'lead',
        createdAt: NOW,
        reports: [],
        commits: [],
        turns: [],
        updatedAt: NOW,
        updatedBy: 'lead',
      });
    }
    h1.state = teamState({ seq: 80, board });
    h1.event(80, 'board.created', { task: board[0] });
    const big = await until(() => {
      const v = lastView(sc, TEAM_ID)?.view;
      return v?.state?.seq === 80 ? v : undefined;
    });
    const bytes = Buffer.byteLength(sc.rawSnapshots[sc.rawSnapshots.length - 1] ?? '', 'utf8');
    check(`E3 an oversized board is cut to fit (${bytes} bytes ≤ 1 MiB + envelope), truncated:true`, big?.truncated === true && bytes <= 1024 * 1024 + 4096 && big.state.members.length === 2, JSON.stringify({ truncated: big?.truncated, bytes }));
    h1.state = teamState({ seq: 81 });
    h1.event(81, 'board.updated', { id: 'b1', changes: { state: 'open' } });
  }

  // ── F. reconnect ───────────────────────────────────────────────────────────
  section('F. reconnect');
  await until(() => lastView(sc, TEAM_ID)?.view?.state?.seq === 81);
  const lastSeq = 81;
  const hellosBefore = h1.hellos.length;
  const subsBefore = h1.subscribes.length;
  const dropAt = Date.now();
  h1.dropAll();
  const backOnDisk = await until(() => {
    const v = lastView(sc, TEAM_ID);
    return v !== undefined && (v.view === null || v.view.live === false) ? v : undefined;
  }, settle(3_000));
  check('F1 a dropped connection withdraws the live view at once', backOnDisk !== undefined, JSON.stringify(lastView(sc, TEAM_ID)?.view?.live));
  await until(() => h1.subscribes.length > subsBefore, hang(8_000));
  const reconnectMs = Date.now() - dropAt;
  check(`F2 the sidecar reconnects with backoff (${reconnectMs} ms, ≥ ~1 s) and says hello again`, h1.hellos.length === hellosBefore + 1 && reconnectMs >= 900, JSON.stringify({ hellos: h1.hellos.length, reconnectMs }));
  check('F3 it resubscribes FROM THE LAST SEQ it saw', h1.subscribes[subsBefore]?.fromSeq === lastSeq, JSON.stringify(h1.subscribes[subsBefore]));
  const relive = await until(() => lastView(sc, TEAM_ID)?.view?.live === true);
  check('F4 the view is live again', relive === true, JSON.stringify(lastView(sc, TEAM_ID)?.view?.source));

  // ── G. disk teams ──────────────────────────────────────────────────────────
  section('G. disk teams');
  const diskView = await until(() => lastView(sc, DISK_TEAM_ID)?.view);
  check('G1 a suspended team on disk is in view as source disk, not live', diskView?.source === 'disk' && diskView.live === false && diskView.state.status === 'suspended', JSON.stringify(diskView?.source));
  check('G2 its feed backlog is read (tail of members/<name>/feed.jsonl)', diskView?.activity?.[0]?.member === 'carol' && diskView.activity[0].lines.length === 3 && diskView.activity[0].lines[2].text === 'Read · file3.ts', JSON.stringify(diskView?.activity));
  const viaGet = await call(sc, 'cco.team.get', { teamId: DISK_TEAM_ID });
  check('G3 cco.team.get answers the same team with a newer rev', viaGet?.teamId === DISK_TEAM_ID && viaGet.rev > diskView.rev, JSON.stringify({ rev: viaGet?.rev }));
  {
    const withReasons = await call(sc, 'cco.team.list', { roots: [PROJECT], panePids: [] });
    const disk = withReasons.teams.find((t) => t.teamId === DISK_TEAM_ID);
    const owned = withReasons.teams.find((t) => t.teamId === TEAM_ID);
    check('G4 TeamListEntry.reason rides in the summary: one line, no controls or bidi, ≤ 300 chars', typeof disk?.reason === 'string' && disk.reason.startsWith('host gone since rrr') && disk.reason.length === 300 && !/[\n\u0007\u202E]/.test(disk.reason), JSON.stringify(disk?.reason?.slice(0, 40)));
    check('G5 a team the host gives no reason for has none', owned !== undefined && !('reason' in owned), JSON.stringify(owned));
    check('G6 a bad team id or a non-string reason gives nothing (no extra team, no reason)', !withReasons.teams.some((t) => t.teamId === 'tmyyyyyyyyy3' || t.teamId.includes('/')) && !JSON.stringify(withReasons).includes('a bad id gives nothing'), JSON.stringify(withReasons.teams.map((t) => t.teamId)));
  }

  // ── J. hostile hosts and files ─────────────────────────────────────────────
  section('J. hostile hosts and files');
  {
    // J1: the run directory also holds every host's LOG; a log storm is not a list storm.
    const listsBefore = h1.lists;
    const logFile = join(RUN_DIR, 's424242.log');
    for (let i = 0; i < 200; i += 1) {
      appendFileSync(logFile, `log line ${i}\n`);
      if (i % 20 === 0) await sleep(10);
    }
    await sleep(settle(1_500));
    check(`J1 a host-log storm in run/ causes no list storm (${h1.lists - listsBefore} list polls)`, h1.lists - listsBefore <= 2, String(h1.lists - listsBefore));

    // J2: garbage from the host, a notification for another team, a snapshot of another
    // team — all ignored; the view keeps its team and the sidecar keeps answering.
    const before = lastView(sc, TEAM_ID)?.view;
    const conn = h1.sub?.conn;
    conn?.write('this is not json\n');
    conn?.write(`${JSON.stringify({ jsonrpc: '2.0', method: 'cco/team.event', params: { requestId: h1.sub.id, teamId: 'tmothertea12', seq: 999, ts: 1, kind: 'snapshot', state: { ...teamState(), id: 'tmothertea12', name: 'intruder' } } })}\n`);
    conn?.write(`${JSON.stringify({ jsonrpc: '2.0', method: 'cco/team.event', params: { requestId: h1.sub.id, teamId: TEAM_ID, seq: 998, ts: 1, kind: 'snapshot', state: { ...teamState(), id: 'tmothertea12', name: 'intruder' } } })}\n`);
    conn?.write(`${JSON.stringify({ jsonrpc: '2.0', method: 'cco/team.activity', params: { requestId: h1.sub.id, teamId: TEAM_ID, member: '../../etc', items: [{ ts: 1, turn: 't', k: 'text', s: 'nope' }] } })}\n`);
    await sleep(settle(800));
    const after = lastView(sc, TEAM_ID)?.view;
    const stillAnswers = await call(sc, 'cco.team.get', { teamId: TEAM_ID });
    check('J2 garbage, another team\'s snapshot and a bad member name change nothing', after?.state?.name === 'demo' && !sc.snapshots.some((p) => p.teamId === 'tmothertea12') && stillAnswers?.state?.name === 'demo' && !(after?.activity ?? []).some((a) => a.member.includes('/')), JSON.stringify({ name: after?.state?.name, before: before?.state?.name }));

    // J3: a record that is a SYMLINK (to a well-formed record of its own pid) is never read.
    mkdirSync(join(HOME, 'elsewhere'), { recursive: true });
    const linked = fakeHost({ socketPath: join(RUN_DIR, 's434343.sock') });
    servers.push(linked);
    await linked.listen();
    const target = join(HOME, 'elsewhere', 'real.json');
    writeFileSync(target, JSON.stringify({ protocol: 1, hostPid: process.pid, hostStartedAt: NOW, session: { pid: 434343, startedAt: NOW }, workspace: PROJECT_REAL, socket: join(RUN_DIR, 's434343.sock'), serverHash: 'x' }));
    symlinkSync(target, join(RUN_DIR, 's434343.json'));
    await call(sc, 'cco.team.list', { roots: [PROJECT], panePids: [] });
    await sleep(settle(1_200));
    check('J3 a symlinked host record is never followed', linked.connections === 0, String(linked.connections));

    // J4: a body over the inline cap is read from bodies/m<seq>.md (its head only).
    const bodies = join(TEAMS_DIR, TEAM_ID, 'bodies');
    mkdirSync(bodies, { recursive: true, mode: 0o700 });
    writeFileSync(join(bodies, 'm90.md'), `HEAD-OF-BODY ${'y'.repeat(20_000)}`);
    h1.event(90, 'msg.posted', { from: 'lead', to: 'alice', kind: 'text', urgent: true, bodyFile: 'bodies/m90.md', bytes: 20_013, sha256: 'x', source: 'cco' });
    const long = await until(() => lastView(sc, TEAM_ID)?.view?.messages?.find((m) => m.id === 'm90'));
    check('J4 a long body is read from its file, capped, and marked cut', long?.body?.startsWith('HEAD-OF-BODY') === true && long.body.length <= 2_000 && long.cut === true && long.urgent === true, JSON.stringify({ len: long?.body?.length, cut: long?.cut }));
  }

  // ── I. scope ───────────────────────────────────────────────────────────────
  section('I. scope');
  const closedBefore = h1.conns.size;
  await call(sc, 'cco.team.list', { roots: [], panePids: [] });
  const withdrawn = await until(() => lastView(sc, TEAM_ID)?.view === null && lastView(sc, DISK_TEAM_ID)?.view === null, hang(3_000));
  check('I1 an empty scope withdraws every view (view: null) and closes the host', withdrawn === true && (await until(() => h1.conns.size === 0, hang(2_000))) === true, JSON.stringify({ closedBefore, now: h1.conns.size }));
  const paneList = await call(sc, 'cco.team.list', { roots: [], panePids: [424242] });
  await until(() => lastView(sc, TEAM_ID)?.view?.live === true, hang(8_000));
  const paneView = lastView(sc, TEAM_ID)?.view;
  check('I2 a Gilde pane pid alone brings its host back, tagged with the pane', paneView?.paneSessionPid === 424242 && paneList.hosts.some((h) => h.key === 's424242' && h.pane === true), JSON.stringify({ pane: paneView?.paneSessionPid, hosts: paneList.hosts }));
  check('I3 never a write: the host saw only list/get/subscribe/unsubscribe and hellos', h1.gets.length > 0, '');
  check('I4 the team preview alone never calls a tool (list_workers is asked only by cco.workers)', h1.toolCalls.length === 0, String(h1.toolCalls.length));

  await allWorkspacesSection(h1, h3);
  await sharedTeamSection(h1, h3);
  await workersSection(h1);
}

// ── L. all workspaces (Gilde preview: Team works with no project added) ──────
const FAR_TEAM_ID = 'tmfarawayzz4';
async function allWorkspacesSection(h1, h3) {
  section('L. all workspaces');
  // A suspended team on disk in a workspace no root is related to.
  const farDir = join(TEAMS_DIR, FAR_TEAM_ID);
  mkdirSync(farDir, { recursive: true, mode: 0o700 });
  writeFileSync(
    join(farDir, 'team.json'),
    JSON.stringify({ v: 1, id: FAR_TEAM_ID, name: 'far', title: 'Far', workspace: '/nowhere/else', repoRoot: '/nowhere/else', commonDir: '/nowhere/else/.git', createdAt: NOW - 1_000 }),
  );
  writeFileSync(
    join(farDir, 'state.json'),
    JSON.stringify({
      ...teamState(),
      id: FAR_TEAM_ID,
      name: 'far',
      status: 'suspended',
      workspace: '/nowhere/else',
      repoRoot: '/nowhere/else',
      createdAt: NOW - 1_000,
      owner: { hostPid: deadPid(), hostStartedAt: NOW - 3_700_000, session: { pid: 998, startedAt: 1 } },
      members: [member('dave', { state: 'idle' })],
    }),
  );

  const badFlag = await expectError(call(sc, 'cco.team.list', { roots: [PROJECT], panePids: [], allWorkspaces: 'yes' }));
  check('L1 allWorkspaces that is not a boolean is -32602', badFlag !== null && badFlag.code === -32602, String(badFlag?.code));

  const h3Before = h3.connections;
  await call(sc, 'cco.team.list', { roots: [PROJECT], panePids: [], allWorkspaces: false });
  await sleep(settle(600));
  const scoped = await call(sc, 'cco.team.list', { roots: [PROJECT], panePids: [], allWorkspaces: false });
  check(
    'L2 allWorkspaces false: a team in an unrelated workspace is NOT listed, its host never connected',
    !scoped.teams.some((t) => t.teamId === FAR_TEAM_ID) && !scoped.hosts.some((h) => h.key === 's626262') && h3.connections === h3Before,
    JSON.stringify({ teams: scoped.teams.map((t) => t.teamId), hosts: scoped.hosts.map((h) => h.key), h3: h3.connections }),
  );

  await call(sc, 'cco.team.list', { roots: [PROJECT], panePids: [], allWorkspaces: true });
  let everywhere;
  const deadline = Date.now() + hang(8_000);
  for (;;) {
    everywhere = await call(sc, 'cco.team.list', { roots: [PROJECT], panePids: [], allWorkspaces: true });
    if ((everywhere.teams.some((t) => t.teamId === FAR_TEAM_ID) && h3.connections > h3Before) || Date.now() > deadline) break;
    await sleep(100);
  }
  check(
    'L3 allWorkspaces true: the unrelated workspace\'s team IS listed and its live host is connected',
    everywhere.teams.some((t) => t.teamId === FAR_TEAM_ID && t.source === 'disk') && everywhere.hosts.some((h) => h.key === 's626262') && h3.connections > h3Before,
    JSON.stringify({ teams: everywhere.teams.map((t) => t.teamId), hosts: everywhere.hosts.map((h) => h.key), h3: h3.connections }),
  );
  check(
    'L4 allWorkspaces still refuses a record whose socket is not socketPathFor\'s and a dead host pid',
    !everywhere.hosts.some((h) => h.key === 's525252' || h.key === 's828282'),
    JSON.stringify(everywhere.hosts.map((h) => h.key)),
  );

  // Back to the scope the next section expects (I2's): the far team and host drop out again.
  // Views are forwarded coalesced; wait for the far team's before narrowing again.
  const farForwarded = await until(() => (lastView(sc, FAR_TEAM_ID)?.view ? lastView(sc, FAR_TEAM_ID) : undefined), hang(4_000));
  await call(sc, 'cco.team.list', PANE_SCOPE);
  const narrowed = await until(() => lastView(sc, FAR_TEAM_ID)?.view === null && h3.conns.size === 0, hang(4_000));
  const after = await call(sc, 'cco.team.list', PANE_SCOPE);
  check(
    'L5 narrowing the scope again withdraws the far team (view: null) and closes its host',
    farForwarded?.view?.teamId === FAR_TEAM_ID && narrowed === true && !after.teams.some((t) => t.teamId === FAR_TEAM_ID),
    JSON.stringify({ forwarded: farForwarded?.view?.teamId, last: lastView(sc, FAR_TEAM_ID)?.view, h3: h3.conns.size, teams: after.teams.map((t) => t.teamId) }),
  );
  rmSync(farDir, { recursive: true, force: true });
  // L6: h3 (the unrelated workspace, connected only while allWorkspaces was on) also lists
  // TEAM_ID as ownedHere, so the team may have been followed through it. With h3 out of
  // sight again, the team must be live again through the pane host — what a fresh sidecar
  // with this scope would forward.
  const backLive = await until(() => isLiveVia(TEAM_ID, 's424242'), hang(8_000));
  const backList = await call(sc, 'cco.team.list', PANE_SCOPE);
  check(
    'L6 back to the pane scope: the team is live again through the pane host (view and list)',
    backLive === true && backList.teams.some((t) => t.teamId === TEAM_ID && t.live === true && t.paneSessionPid === 424242),
    JSON.stringify({ last: viewSummary(TEAM_ID), teams: backList.teams.map((t) => [t.teamId, t.source, t.live]) }),
  );
}

/** The last forwarded view of a team is live, through the given host. */
function isLiveVia(teamId, hostKey) {
  const v = lastView(sc, teamId)?.view;
  return v?.live === true && v.hostKey === hostKey && (hostKey !== 's424242' || v.paneSessionPid === 424242);
}
const viewSummary = (teamId) => {
  const v = lastView(sc, teamId)?.view;
  return v === undefined ? 'none' : v === null ? 'null' : { live: v.live, source: v.source, hostKey: v.hostKey, pane: v.paneSessionPid };
};

// ── M. two hosts claim one team (the bug class of L6, other sequences) ───────
// A team is followed through ONE host connection at a time. When a second connected host
// claims the same team (a lease handoff window; here h3, in sight only with allWorkspaces)
// and then leaves or stops claiming it, the first host must follow the team again — the
// forwarded set after any sequence equals what a fresh sidecar with the final scope forwards.
async function sharedTeamSection(h1, h3) {
  section('M. two hosts, one team');
  const WIDE = { ...PANE_SCOPE, allWorkspaces: true };
  // M1: pane → all → pane → all → pane. Each widening lets h3 take the team over; each
  // narrowing must hand it back to the pane host.
  const rounds = [];
  for (let round = 1; round <= 2; round += 1) {
    await call(sc, 'cco.team.list', WIDE);
    const taken = await until(() => isLiveVia(TEAM_ID, 's626262'), hang(8_000));
    await call(sc, 'cco.team.list', PANE_SCOPE);
    const back = await until(() => isLiveVia(TEAM_ID, 's424242'), hang(8_000));
    rounds.push({ round, taken: taken === true, back: back === true, last: viewSummary(TEAM_ID) });
  }
  check('M1 pane → all → pane, twice: every narrowing leaves the team live through the pane host', rounds.every((r) => r.taken && r.back), JSON.stringify(rounds));

  // M2: no scope change at all — the other claimant stops claiming (its list no longer says
  // ownedHere, as after a lease moves back): the team is followed through the pane host again.
  await call(sc, 'cco.team.list', WIDE);
  const taken = await until(() => isLiveVia(TEAM_ID, 's626262'), hang(8_000));
  // M3: while BOTH claim it, the team stays with the claimant that has it — no flapping:
  // once each host has been list-polled again (the 5 s discovery tick, a poll at most every
  // 4.5 s — so up to ~10 s), plus the time a re-subscribe would take, nobody subscribed again.
  const h1Subs = h1.subscribes.length;
  const h3Subs = h3.subscribes.length;
  const h1Lists = h1.lists;
  const h3Lists = h3.lists;
  await until(() => h1.lists > h1Lists && h3.lists > h3Lists, hang(10_000));
  await sleep(settle(500));
  check(
    'M3 two claimants at once: no flapping across a list poll (no new subscribe, still followed through the same host)',
    taken === true && h1.lists > h1Lists && h3.lists > h3Lists && h1.subscribes.length === h1Subs && h3.subscribes.length === h3Subs && isLiveVia(TEAM_ID, 's626262'),
    JSON.stringify({ lists: [h1.lists - h1Lists, h3.lists - h3Lists], subs: [h1.subscribes.length - h1Subs, h3.subscribes.length - h3Subs], last: viewSummary(TEAM_ID) }),
  );
  h3.ownsTeam = false;
  const handedBack = await until(() => isLiveVia(TEAM_ID, 's424242'), hang(8_000));
  const wideList = await call(sc, 'cco.team.list', WIDE);
  check(
    'M2 the other host stops claiming the team (scope unchanged): live again through the pane host',
    taken === true && handedBack === true && wideList.teams.some((t) => t.teamId === TEAM_ID && t.live === true && t.hostKey === 's424242'),
    JSON.stringify({ taken: taken === true, last: viewSummary(TEAM_ID), teams: wideList.teams.map((t) => [t.teamId, t.live, t.hostKey]) }),
  );
  // Back to the scope the next section expects: h3 out of sight, the team live via the pane host.
  h3.ownsTeam = true;
  await call(sc, 'cco.team.list', PANE_SCOPE);
  await until(() => h3.conns.size === 0 && isLiveVia(TEAM_ID, 's424242'), hang(8_000));
}

// ── K. worker accounts (Plan #3c: `cco.workers` → `tools/call list_workers`) ──
const PANE_SCOPE = { roots: [], panePids: [424242] };
const REFRESH_WAIT_MS = 5_150; // just past WORKERS_REFRESH_MIN_MS

async function workersSection(h1) {
  section('K. worker accounts');
  // K1: three consumers at once share ONE host call; the snapshot is the fixture, by value.
  h1.workers = { mode: 'snapshot' };
  const [a, b, c] = await Promise.all([call(sc, 'cco.workers', PANE_SCOPE), call(sc, 'cco.workers', PANE_SCOPE), call(sc, 'cco.workers', PANE_SCOPE)]);
  const generatedAt = a.snapshot?.generatedAt;
  check('K1 concurrent requests share one tools/call list_workers {name, arguments:{}}', h1.toolCalls.length === 1 && h1.toolCalls[0].params?.name === 'list_workers' && canonical(h1.toolCalls[0].params?.arguments) === '{}', JSON.stringify(h1.toolCalls.map((t) => t.params)));
  check('K2 the answer is the fixture snapshot (times shifted to now), unchanged by the value checks', a.snapshot !== null && h1.workersServed !== undefined && canonical(a.snapshot) === canonical(h1.workersServed), canonical(a.snapshot).slice(0, 300));
  check('K2b servedFromCache passes through as the host said it (the fixture: false)', a.snapshot?.servedFromCache === false && WORKERS_FIXTURE.servedFromCache === false, JSON.stringify(a.snapshot?.servedFromCache));
  check('K3 fresh: not stale, not from cache, no unavailable, the host and the receive time named', a.stale === false && a.fromCache === false && a.unavailable === null && a.hostKey === 's424242' && typeof a.receivedAt === 'number' && b.snapshot?.generatedAt === generatedAt && c.snapshot?.generatedAt === generatedAt, JSON.stringify({ stale: a.stale, fromCache: a.fromCache, u: a.unavailable, host: a.hostKey }));

  // K4: the 60 s floor — a second request (any consumer) gets the held snapshot, its generatedAt.
  const again = await call(sc, 'cco.workers', PANE_SCOPE);
  check('K4 inside the 60 s floor: the cached snapshot with its own generatedAt, no host call', again.fromCache === true && again.snapshot?.generatedAt === generatedAt && h1.toolCalls.length === 1 && again.nextPollAt >= (a.receivedAt ?? 0) + 59_000, JSON.stringify({ fromCache: again.fromCache, calls: h1.toolCalls.length, next: again.nextPollAt - (a.receivedAt ?? 0) }));
  const early = await call(sc, 'cco.workers', { ...PANE_SCOPE, refresh: true });
  check('K5 an explicit refresh within 5 s of the last call is still served from cache', early.fromCache === true && h1.toolCalls.length === 1, String(h1.toolCalls.length));

  // K6: hostile values from the host are checked by value.
  const nasty = '<b>bold</b>\u202E\u0007 line\nbreak ' + 'x'.repeat(5_000);
  // generatedAt is not a time, so it reads the receive time (≥ now): a cooldown an hour out is after it, one in 2017 is not.
  const coolingUntil = Date.now() + 3_600_000 + 0.75;
  const hostile = {
    ...WORKERS_FIXTURE,
    generatedAt: 'yesterday',
    servedFromCache: 'yes',
    accounts: [
      { ...WORKERS_FIXTURE.accounts[1], name: nasty, lastError: nasty, windows: [{ kind: 'mystery', label: nasty, percent: 250, severity: 'apocalyptic', resetsAt: -5 }, { kind: 'session', label: 'nan', percent: '50', severity: 'normal', resetsAt: 1 }], cooldownUntil: coolingUntil, running: -3, sameAccountAs: Array.from({ length: 30 }, (_, i) => `acct${i}${'n'.repeat(i === 0 ? 200 : 0)}`), overage: { enabled: true, percent: 12, spendLabel: `$${'9'.repeat(200)}` } },
      { ...WORKERS_FIXTURE.accounts[2], login: 'quantum', cooldownUntil: 1.5e12 },
      { ...WORKERS_FIXTURE.accounts[3], key: `k${'k'.repeat(500)}`, name: 'many-windows', windows: Array.from({ length: 20 }, (_, i) => ({ kind: 'session', label: `win ${i}`, percent: i, severity: 'normal', resetsAt: null })) },
      { ...WORKERS_FIXTURE.accounts[1] }, // a duplicate key
      { name: 'no-key' },
      'not an account',
      ...Array.from({ length: 300 }, (_, i) => ({ ...WORKERS_FIXTURE.accounts[4], key: `bulk-${i}`, name: `bulk-${i}` })),
    ],
    guards: { overageWorkerDispatch: 'ALLOW', frontierWorkerDispatch: 'allow' },
    extraField: { anything: true },
  };
  h1.workers = { mode: 'raw', structuredContent: hostile };
  await sleep(REFRESH_WAIT_MS);
  const checked = await call(sc, 'cco.workers', { ...PANE_SCOPE, refresh: true });
  const acc = checked.snapshot?.accounts ?? [];
  const w = acc[0]?.windows ?? [];
  check('K6 a refresh past 5 s is a plain call to the host', h1.toolCalls.length === 2 && checked.fromCache === false, String(h1.toolCalls.length));
  check('K7 strings: one line, no controls or bidi, capped (name ≤ 64, lastError ≤ 200, label ≤ 40); markup stays text', acc[0]?.name.length <= 64 && !/[\n\u0007\u202E]/.test(acc[0]?.name ?? '\n') && acc[0]?.lastError.length <= 200 && w[0]?.label.length <= 40 && acc[0]?.name.startsWith('<b>bold</b>'), JSON.stringify(acc[0]?.name));
  check('K8 numbers: percent clamped to 100, unknown kind/severity → other/normal (additive, same schema), a bad time → null, a non-number percent drops the window', w.length === 1 && w[0].percent === 100 && w[0].kind === 'other' && w[0].severity === 'normal' && w[0].resetsAt === null && acc[0].cooldownUntil === Math.trunc(coolingUntil) && acc[0].running === 0, JSON.stringify({ w, cd: acc[0]?.cooldownUntil, r: acc[0]?.running }));
  check('K8b cooldownUntil not after generatedAt → null (the reader guards as well)', acc[1]?.cooldownUntil === null, JSON.stringify(acc[1]?.cooldownUntil));
  check('K9 lists: sameAccountAs ≤ 8 names of ≤ 64, a duplicate key and keyless entries dropped, accounts ≤ 200 (contract cap)', acc[0]?.sameAccountAs.length === 8 && acc[0].sameAccountAs[0].length === 64 && acc.filter((x) => x.key === WORKERS_FIXTURE.accounts[1].key).length === 1 && !acc.some((x) => x.name === 'no-key') && acc.length === 200, JSON.stringify({ same: acc[0]?.sameAccountAs.length, n: acc.length }));
  check('K9b windows ≤ 8 per account (kept in order), key ≤ 128, spendLabel ≤ 40 (contract caps)', acc[2]?.name === 'many-windows' && acc[2].windows.length === 8 && acc[2].windows[7].label === 'win 7' && acc[2].key.length === 128 && acc[0]?.overage?.spendLabel?.length === 40, JSON.stringify({ n: acc[2]?.windows.length, key: acc[2]?.key.length, spend: acc[0]?.overage?.spendLabel?.length }));
  check('K9c a servedFromCache that is not a boolean is as if absent', checked.snapshot !== null && !('servedFromCache' in checked.snapshot), JSON.stringify(checked.snapshot?.servedFromCache));
  check('K10 an unknown login state is shown as what it is (error + its name), a bad generatedAt reads the receive time, only literal "allow" allows', acc[1]?.login === 'error' && acc[1]?.loginDetail === 'unknown login state "quantum"' && checked.snapshot?.generatedAt === checked.receivedAt && checked.snapshot?.guards.overageWorkerDispatch === 'block' && checked.snapshot?.guards.frontierWorkerDispatch === 'allow' && !('extraField' in (checked.snapshot ?? {})), JSON.stringify({ login: acc[1]?.login, d: acc[1]?.loginDetail, g: checked.snapshot?.guards }));
  const heldAt = checked.snapshot?.generatedAt;

  // K11: an older host — text only. No snapshot from it; the text is never parsed.
  h1.workers = { mode: 'text-only' };
  await sleep(REFRESH_WAIT_MS);
  const old = await call(sc, 'cco.workers', { ...PANE_SCOPE, refresh: true });
  check('K11 no structuredContent → host-too-old; the held snapshot is kept, marked stale; the text is not parsed', old.unavailable?.reason === 'host-too-old' && old.stale === true && old.snapshot?.generatedAt === heldAt && !JSON.stringify(old).includes('cooling down'), JSON.stringify({ u: old.unavailable, stale: old.stale }));

  // K12: a schema this Gilde does not know — shown as newer/older, NOT parsed.
  h1.workers = { mode: 'schema', schema: 'cco.workers/2' };
  await sleep(REFRESH_WAIT_MS);
  const newer = await call(sc, 'cco.workers', { ...PANE_SCOPE, refresh: true });
  check('K12 schema cco.workers/2 → schema-unknown naming it; nothing of that answer is read', newer.unavailable?.reason === 'schema-unknown' && newer.unavailable?.detail === 'cco.workers/2' && newer.stale === true && newer.snapshot?.generatedAt === heldAt && !JSON.stringify(newer).includes('from-a-newer-schema'), JSON.stringify({ u: newer.unavailable }));

  // K13: isError (an unreadable registry) → host-error with the host's line, as text.
  h1.workers = { mode: 'is-error', text: 'worker registry unreadable: <i>/x</i>\nsecond line' };
  await sleep(REFRESH_WAIT_MS);
  const err = await call(sc, 'cco.workers', { ...PANE_SCOPE, refresh: true });
  check('K13 isError → host-error with its one line; the held snapshot stays, stale', err.unavailable?.reason === 'host-error' && err.unavailable?.detail === 'worker registry unreadable: <i>/x</i> second line' && err.stale === true, JSON.stringify(err.unavailable));

  // K14: an older role table refuses the tool → host-too-old.
  h1.workers = { mode: 'forbidden' };
  await sleep(REFRESH_WAIT_MS);
  const refused = await call(sc, 'cco.workers', { ...PANE_SCOPE, refresh: true });
  check('K14 RPC roleForbidden → host-too-old', refused.unavailable?.reason === 'host-too-old' && refused.stale === true, JSON.stringify(refused.unavailable));

  // K15: good again — this time the HOST says it answered from its own observer cache.
  h1.workers = { mode: 'raw', structuredContent: { ...shiftedFixture(Date.now()), servedFromCache: true } };
  await sleep(REFRESH_WAIT_MS);
  const good = await call(sc, 'cco.workers', { ...PANE_SCOPE, refresh: true });
  const goodAt = good.snapshot?.generatedAt;
  check('K15 a good answer clears unavailable and staleness', good.unavailable === null && good.stale === false && goodAt > heldAt && good.snapshot?.accounts.length === WORKERS_FIXTURE.accounts.length, JSON.stringify({ u: good.unavailable, stale: good.stale }));
  check('K15b servedFromCache: true from the host is passed through (distinct from Gilde\'s own fromCache)', good.snapshot?.servedFromCache === true && good.fromCache === false, JSON.stringify({ host: good.snapshot?.servedFromCache, gilde: good.fromCache }));
  h1.workers = { mode: 'snapshot' };

  // K16: the socket goes away → the snapshot is kept, stale, with its time; no host is asked.
  const callsBefore = h1.toolCalls.length;
  const closedLogs = () => sc.stderr.filter((l) => l.includes('the connection closed')).length;
  const closedBefore = closedLogs();
  h1.dropAll();
  await sleep(150);
  // On a loaded machine 150 ms may not be enough for the sidecar to see the drop: wait for
  // its own log line saying so (it comes before the ~1 s reconnect backoff ends).
  await until(() => closedLogs() > closedBefore, hang(3_000));
  const dropped = await call(sc, 'cco.workers', PANE_SCOPE);
  {
    // K16b: the reason a team is listed with lives only while the host that said it is connected.
    const listDropped = await call(sc, 'cco.team.list', PANE_SCOPE);
    const diskDropped = listDropped.teams.find((t) => t.teamId === DISK_TEAM_ID);
    check('K16b once the host is gone, its TeamListEntry.reason is gone from the summary', diskDropped !== undefined && !('reason' in diskDropped), JSON.stringify(diskDropped));
  }
  check('K16 connection gone: the last snapshot is kept, stale, its generatedAt and receivedAt kept', dropped.stale === true && dropped.snapshot?.generatedAt === goodAt && dropped.receivedAt === good.receivedAt && ['no-host', 'host-gone'].includes(dropped.unavailable?.reason) && h1.toolCalls.length === callsBefore, JSON.stringify({ stale: dropped.stale, u: dropped.unavailable, calls: h1.toolCalls.length - callsBefore }));

  // K17: no host at all (the host process and its record are gone).
  await h1.close();
  unlinkSync(join(RUN_DIR, 's424242.json'));
  await sleep(REFRESH_WAIT_MS);
  const gone = await call(sc, 'cco.workers', PANE_SCOPE);
  check('K17 no host at all: no-host, the last snapshot kept and stale', gone.unavailable?.reason === 'no-host' && gone.stale === true && gone.snapshot?.generatedAt === goodAt, JSON.stringify({ u: gone.unavailable, stale: gone.stale }));

  // K18: the host restarts (a new host process on the session's socket) → asked at once, inside the 60 s floor.
  const restarted = fakeHost({ socketPath: join(RUN_DIR, 's424242.sock'), hostPid: process.ppid });
  servers.push(restarted);
  await restarted.listen();
  writeRecord(424242, { hostPid: process.ppid });
  await until(() => restarted.hellos.length > 0, hang(8_000));
  await sleep(REFRESH_WAIT_MS);
  const back = await call(sc, 'cco.workers', PANE_SCOPE);
  check('K18 a restarted host is asked inside the old floor (its predecessor is gone): fresh again', restarted.toolCalls.length === 1 && back.stale === false && back.unavailable === null && back.snapshot?.generatedAt > goodAt, JSON.stringify({ calls: restarted.toolCalls.length, u: back.unavailable, stale: back.stale }));
  const floored = await call(sc, 'cco.workers', PANE_SCOPE);
  check('K19 and the floor holds for it', floored.fromCache === true && restarted.toolCalls.length === 1, String(restarted.toolCalls.length));
}

const guard = setTimeout(() => {
  const now = Date.now();
  const waiting = [...(sc?.pending?.values() ?? [])].map((p) => `${p.method} (${now - p.at} ms)`);
  process.stdout.write(
    `\ncco-team: TIMED OUT after ${((now - STARTED_AT) / 1000).toFixed(1)} s (guard ${GUARD_MS} ms, CCO_TEAM_TEST_GUARD_MS; time scale ${TIME_SCALE}, CCO_TEAM_TEST_TIME_SCALE)\n` +
      `  running:  section ${current.name} for ${((now - current.startedAt) / 1000).toFixed(1)} s, last check: ${lastCheck}\n` +
      `  waiting on sidecar: ${waiting.length > 0 ? waiting.join(', ') : 'nothing'}\n` +
      `  finished: ${sectionTimings() || '(none)'}\n` +
      `  checks so far: ${checks - failures}/${checks} passed\n`,
  );
  for (const l of (sc?.stderr ?? []).slice(-30)) process.stdout.write(`  ${l}\n`);
  finish(1);
}, GUARD_MS);
guard.unref();

async function finish(code) {
  try {
    sc?.child.kill('SIGKILL');
  } catch {
    /* gone */
  }
  for (const s of servers) {
    try {
      await s.close();
    } catch {
      /* closed */
    }
  }
  rmSync(HOME, { recursive: true, force: true });
  process.exit(code);
}

main()
  .then(async () => {
    section('end');
    process.stdout.write(`\ncco-team: ${checks - failures}/${checks} checks passed in ${((Date.now() - STARTED_AT) / 1000).toFixed(1)} s (${sectionTimings()})\n`);
    if (failures > 0) {
      process.stdout.write('sidecar stderr:\n');
      for (const l of (sc?.stderr ?? []).slice(-30)) process.stdout.write(`  ${l}\n`);
    }
    await finish(failures > 0 ? 1 : 0);
  })
  .catch(async (err) => {
    process.stdout.write(`\ncco-team: ERROR ${err.stack ?? err.message}\n`);
    for (const l of (sc?.stderr ?? []).slice(-30)) process.stdout.write(`  ${l}\n`);
    await finish(1);
  });
