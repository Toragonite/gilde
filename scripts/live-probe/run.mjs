#!/usr/bin/env node
/**
 * run.mjs — the live-probe runner of contract §14.4 (Phase P), T19.
 *
 *   node scripts/live-probe/run.mjs --list
 *   node scripts/live-probe/run.mjs --dry-run [--probe LP-01,LP-40] [--out <dir>] [--keep]
 *   node scripts/live-probe/run.mjs --live --quota-file <quota.json> [--probe …] [--owner-present]
 *        [--claude <path>] [--config-dir <dir>] [--scratch-root /tmp/gilde-lp] [--out <dir>] [--keep]
 *   node scripts/live-probe/run.mjs --watch <config dir>
 *
 * `--dry-run` runs every kit script against the fakes (`fakes/claude.mjs`) in
 * a temp root with its own HOME, account dir and scratch root: no real CLI can
 * start (the guard only accepts the fake binary) and nothing outside the temp
 * root is touched. It validates the kit, so its exit code is 0 when every
 * selected probe completed (whatever the verdict), 1 when one threw.
 *
 * `--live` is for the lead under OQ1, never for a build worker: account g
 * (`$HOME/.claude-g`) or a fresh scratch config dir only; pre-flight (stat of
 * `.claude.json`, `--version`, realpath + fingerprint, re-checked before every
 * probe); the LP-01(b) handshake first (a signed-out account stops the phase);
 * a fresh quota reading before every probe and the pressure monitor during it
 * (rule 3); scratch projects under `/tmp/gilde-lp/<LP>` deleted afterwards
 * (rule 4); process groups ended without SIGKILL (rule 5); shapes only in the
 * records (rule 6); `pty`/`hosted` probes only with `--owner-present`
 * (rule 7); DISABLE_AUTOUPDATER=1 for every process (rule 8).
 *
 * Exit codes: 0 done; 1 a probe threw (dry run) or the phase stopped (live);
 * 2 bad usage or a guard refusal before anything ran.
 */

import { execFileSync } from 'node:child_process';
import { chmodSync, existsSync, mkdirSync, mkdtempSync, rmSync, writeFileSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { dirname, join, resolve } from 'node:path';
import { fileURLToPath } from 'node:url';
import { performance } from 'node:perf_hooks';

import { DEFAULT_SCRATCH_ROOT, GuardError, allowedEnvKey, buildEnv, checkConfigDir, checkScratchRoot, realHome } from './lib/guard.mjs';
import { PaneHost, watchAccount, writeShim } from './lib/panehost.mjs';
import { PhaseStop, checkQuotaReading, makeScratchProject, preflight, pressureMonitor, recheck, removeScratch } from './lib/phase.mjs';
import { PtySession } from './lib/pty.mjs';
import { Recorder } from './lib/record.mjs';
import { StreamSession, initialize, l1Argv } from './lib/stream.mjs';
import { PROBES, PROBE_IDS, probeById, runnable, statusOf } from './probes/index.mjs';

const KIT = dirname(fileURLToPath(import.meta.url));
const BASE_PATH = '/usr/bin:/bin:/usr/sbin:/sbin:/opt/homebrew/bin';

function usage(message) {
  if (message) console.error(`live-probe: ${message}`);
  console.error('usage: run.mjs --list | --dry-run [--probe …] | --live --quota-file <f> [--probe …] [--owner-present] | --watch <config dir>');
  process.exit(2);
}

export function parseArgs(argv) {
  const o = { list: false, dryRun: false, live: false, watch: null, probes: null, out: null, keep: false, quotaFile: null, ownerPresent: false, claude: null, configDir: null, scratchRoot: DEFAULT_SCRATCH_ROOT, dryRoot: null };
  for (let i = 0; i < argv.length; i++) {
    const a = argv[i];
    const val = () => {
      if (i + 1 >= argv.length) usage(`${a} needs a value`);
      return argv[++i];
    };
    switch (a) {
      case '--list': o.list = true; break;
      case '--dry-run': o.dryRun = true; break;
      case '--live': o.live = true; break;
      case '--watch': o.watch = val(); break;
      case '--probe': o.probes = val().split(',').map((s) => s.trim()).filter(Boolean); break;
      case '--out': o.out = resolve(val()); break;
      case '--keep': o.keep = true; break;
      case '--quota-file': o.quotaFile = resolve(val()); break;
      case '--owner-present': o.ownerPresent = true; break;
      case '--claude': o.claude = resolve(val()); break;
      case '--config-dir': o.configDir = val(); break;
      case '--scratch-root': o.scratchRoot = val(); break;
      case '--dry-root': o.dryRoot = resolve(val()); break;
      default: usage(`unknown argument ${a}`);
    }
  }
  const modes = [o.list, o.dryRun, o.live, o.watch !== null].filter(Boolean).length;
  if (modes !== 1) usage('pick exactly one of --list, --dry-run, --live, --watch');
  if (o.probes) for (const id of o.probes) if (!PROBE_IDS.includes(id)) usage(`unknown probe ${id}`);
  if (o.live && !o.quotaFile) usage('--live needs --quota-file (rule 3: a fresh quota reading before each probe)');
  if (o.dryRun && (o.claude || o.configDir)) usage('--claude and --config-dir are not accepted with --dry-run (the fake and a temp account are used)');
  return o;
}

/** The dry-run world: temp HOME with an account-g dir, a scratch root, the fake binary. */
export function dryWorld(root) {
  const home = join(root, 'home');
  const configDir = join(home, '.claude-g');
  mkdirSync(configDir, { recursive: true });
  writeFileSync(join(configDir, '.claude.json'), '');
  const scratchRoot = join(root, 'lp');
  mkdirSync(scratchRoot, { recursive: true });
  const fakesDir = join(root, 'fakes-bin');
  mkdirSync(fakesDir, { recursive: true });
  const binary = join(fakesDir, 'claude');
  writeFileSync(binary, `#!/bin/sh\nexec '${process.execPath}' '${join(KIT, 'fakes', 'claude.mjs')}' "$@"\n`);
  chmodSync(binary, 0o755);
  const quotaFile = join(root, 'quota.json');
  writeFileSync(quotaFile, JSON.stringify({ windows: [{ kind: 'fiveHour', usedPct: 15 }, { kind: 'week', usedPct: 40 }], fetchedAt: Date.now() }));
  return { home, configDir, scratchRoot, fakesDir, binary, quotaFile };
}

/** The per-probe context the probe scripts use. */
export function makeContext(probe, run) {
  const projects = new Map();
  const sessions = [];
  const hosts = [];
  const guard = { dryRun: run.dryRun, fakesDir: run.fakesDir, scratchRoots: [run.scratchRoot], configKind: 'accountG' };
  const recorder = (name) => new Recorder(join(run.out, probe.id, `${name}.frames.jsonl`), { home: run.home });
  const fakeExtra = run.dryRun ? { FAKE_CLAUDE_SELF: run.binary } : {};
  const env = ({ configDir = run.configDir, l1 = false, extra = {}, path = BASE_PATH } = {}) => buildEnv({ configDir, home: run.home, path, l1, extra: { ...fakeExtra, ...extra }, dryRun: run.dryRun });
  const ctx = {
    lp: probe.id,
    dryRun: run.dryRun,
    home: run.home,
    configDir: run.configDir,
    scratchRoot: run.scratchRoot,
    // Hosted probes wait on many shim round trips (one process per call).
    timeout: probe.kind === 'hosted' ? (run.dryRun ? 90_000 : 180_000) : run.dryRun ? 10_000 : 120_000,
    calls: run.dryRun ? 3 : 20,
    monitor: run.monitor,
    project(name = 'proj') {
      if (!projects.has(name)) projects.set(name, makeScratchProject(run.scratchRoot, probe.id, { name }));
      return projects.get(name);
    },
    env,
    stream(name, args, { cwd, configDir, extraEnv = {}, env: given } = {}) {
      const e = given ?? env({ configDir, l1: true, extra: extraEnv });
      const kind = checkConfigDir(e.CLAUDE_CONFIG_DIR, { home: e.HOME, scratchRoots: [run.scratchRoot] }).kind;
      const s = new StreamSession({ name, binary: run.binary, args, cwd, env: e, recorder: recorder(name), guard: { ...guard, configKind: kind }, monitor: run.monitor });
      sessions.push(s);
      return s.start();
    },
    pty(name, args, { cwd, env: given } = {}) {
      const e = given ?? env({});
      const p = new PtySession({ name, binary: run.binary, args, cwd, env: e, recorder: recorder(name), guard });
      sessions.push(p);
      return p.start();
    },
    async hosted() {
      const dir = join(run.out, probe.id, 'team');
      const shimDir = join(dir, 'shim');
      mkdirSync(shimDir, { recursive: true });
      writeShim(shimDir);
      const socket = join(tmpdir(), `gilde-lp-${process.pid}-${hosts.length}.sock`);
      const log = join(dir, 'shim.jsonl');
      const teamEnv = env({ path: `${shimDir}:${BASE_PATH}`, extra: { CLAUDE_CODE_EXPERIMENTAL_AGENT_TEAMS: '1', GILDE_PANE_HOST: socket, GILDE_SHIM_LOG: log } });
      const host = new PaneHost({
        socketPath: socket,
        recorder: recorder('panehost'),
        guard,
        // The pane env is rebuilt from the lead's env: allowlisted keys only (the CLI's own
        // markers such as CLAUDECODE are dropped; the pane command sets what it needs).
        paneEnv: (lead) => {
          const out = {};
          for (const [k, v] of Object.entries(lead)) if (allowedEnvKey(k, { dryRun: run.dryRun }) && typeof v === 'string') out[k] = v;
          out.DISABLE_AUTOUPDATER = '1';
          return out;
        },
      });
      await host.start();
      hosts.push(host);
      return { env: teamEnv, log, host, stop: () => host.stop(), paneCommands: () => host.paneCommands };
    },
    /** End every session and host this probe started. */
    async cleanup() {
      for (const s of sessions) await s.finish?.({ exitCommand: false }).catch?.(() => {});
      for (const h of hosts) await h.stop();
    },
  };
  return ctx;
}

function binaryFromPath() {
  try {
    return execFileSync('/usr/bin/which', ['claude'], { encoding: 'utf8' }).trim() || null;
  } catch {
    return null;
  }
}

async function runPhase(o) {
  let run;
  if (o.dryRun) {
    const root = o.dryRoot ?? mkdtempSync(join(tmpdir(), 'gilde-lp-dry-'));
    mkdirSync(root, { recursive: true });
    run = { dryRun: true, root, ...dryWorld(root), out: o.out ?? join(root, 'records') };
  } else {
    const home = realHome();
    const binary = o.claude ?? binaryFromPath();
    if (!binary) usage('no claude binary: pass --claude <path>');
    run = { dryRun: false, root: null, home, configDir: o.configDir ?? join(home, '.claude-g'), scratchRoot: o.scratchRoot, fakesDir: null, binary, quotaFile: o.quotaFile, out: o.out ?? join(o.scratchRoot, 'records', new Date().toISOString().replace(/[:.]/g, '-')) };
  }
  try {
    checkScratchRoot(run.scratchRoot);
    const kind = checkConfigDir(run.configDir, { home: run.home, scratchRoots: [run.scratchRoot] });
    if (kind.kind !== 'accountG') throw new GuardError('account', 'the phase runs on account g; scratch config dirs are only for LP-09(b)');
  } catch (e) {
    console.error(`live-probe: ${e.message}`);
    return 2;
  }
  mkdirSync(run.out, { recursive: true });
  const phase = { dryRun: run.dryRun, startedAt: new Date().toISOString(), configDir: run.dryRun ? '<temp account g>' : '~/.claude-g', probes: [], stopped: null };
  let stopReason = null;
  run.monitor = pressureMonitor((reason) => {
    stopReason = reason;
  });

  const env0 = buildEnv({ configDir: run.configDir, home: run.home, extra: run.dryRun ? { FAKE_CLAUDE_SELF: run.binary } : {}, dryRun: run.dryRun });
  const guard0 = { dryRun: run.dryRun, fakesDir: run.fakesDir, scratchRoots: [run.scratchRoot] };
  const cwd0 = run.dryRun ? run.root : run.scratchRoot;
  mkdirSync(cwd0, { recursive: true });
  const first = await preflight({ binary: run.binary, configDir: run.configDir, env: env0, cwd: cwd0, guard: guard0 });
  phase.preflight = { version: first.version, fingerprint: first.fingerprint.slice(0, 16), claudeJsonExists: first.claudeJsonExists };
  console.log(`pre-flight: claude ${first.version ?? '?'} (${first.fingerprint.slice(0, 12)}…), account dir ${first.claudeJsonExists ? 'configured' : 'NOT configured'}`);
  if (!first.claudeJsonExists) {
    phase.stopped = 'account g has no .claude.json — signing in is the owner\'s act';
    writeFileSync(join(run.out, 'phase.json'), JSON.stringify(phase, null, 2));
    console.error(`live-probe: ${phase.stopped}`);
    return 1;
  }
  // LP-01(b)'s handshake first: a signed-out account stops the phase.
  {
    const hs = new StreamSession({ name: 'handshake', binary: run.binary, args: l1Argv({}), cwd: cwd0, env: { ...env0, CLAUDE_CODE_EMIT_SESSION_STATE_EVENTS: '1' }, recorder: new Recorder(join(run.out, 'handshake.frames.jsonl'), { home: run.home }), guard: guard0, monitor: run.monitor }).start();
    const { response } = await initialize(hs, {}, run.dryRun ? 10_000 : 60_000);
    await hs.finish();
    const tokenSource = response?.response?.response?.account?.tokenSource ?? null;
    if (!response || tokenSource === 'none') {
      phase.stopped = response ? 'the CLI reports it is not signed in (tokenSource none)' : 'no initialize response';
      writeFileSync(join(run.out, 'phase.json'), JSON.stringify(phase, null, 2));
      console.error(`live-probe: phase stopped: ${phase.stopped}`);
      return 1;
    }
  }

  const selected = runnable(o.probes);
  const skipped = (o.probes ?? []).filter((id) => !selected.some((p) => p.id === id || p.sharedWith === id));
  for (const id of skipped) {
    const p = probeById(id);
    phase.probes.push({ id, status: statusOf(p), reason: p.manual ?? p.notRun ?? p.blocked ?? p.conditional ?? null });
  }
  let threw = 0;
  for (const p of selected) {
    if (stopReason) break;
    if (!run.dryRun && (p.kind === 'pty' || p.kind === 'hosted') && !o.ownerPresent) {
      phase.probes.push({ id: p.id, status: 'skipped', reason: 'needs the owner present (rule 7): pass --owner-present' });
      continue;
    }
    try {
      checkQuotaReading(run.quotaFile, { maxAgeMs: run.dryRun ? Infinity : 15 * 60_000 });
      const now = await recheck(first, { binary: run.binary, configDir: run.configDir, env: env0, cwd: cwd0, guard: guard0 });
      if (now.changed) console.log(`  note: the binary changed before ${p.id}: ${first.version} → ${now.version} (verdicts are attributed per version)`);
      const ctx = makeContext(p, run);
      const started = performance.now();
      let result;
      try {
        result = await p.run(ctx);
      } finally {
        await ctx.cleanup();
        if (!o.keep) removeScratch(run.scratchRoot, p.id);
      }
      const entry = { id: p.id, ...(p.sharedWith ? { alsoCovers: p.sharedWith } : {}), version: now.version, verdict: result.verdict, freezes: p.freezes, seconds: Math.round((performance.now() - started) / 100) / 10, observations: result.observations, ...(result.note ? { note: result.note } : {}) };
      phase.probes.push(entry);
      writeFileSync(join(run.out, p.id, 'summary.json'), JSON.stringify(entry, null, 2));
      console.log(`  ${p.id.padEnd(7)} ${result.verdict.padEnd(12)} ${entry.seconds}s  ${p.freezes}`);
    } catch (e) {
      if (e instanceof PhaseStop) {
        stopReason = e.reason;
        break;
      }
      threw++;
      phase.probes.push({ id: p.id, status: 'error', error: e instanceof GuardError ? e.message : String(e?.stack ?? e).split('\n').slice(0, 3).join(' | ') });
      console.log(`  ${p.id.padEnd(7)} ERROR        ${e?.message ?? e}`);
    }
  }
  phase.stopped = stopReason;
  phase.endedAt = new Date().toISOString();
  writeFileSync(join(run.out, 'phase.json'), JSON.stringify(phase, null, 2));
  if (stopReason) console.log(`phase stopped: ${stopReason} — resume only after the window resets and the lead says go`);
  console.log(`records: ${run.out}`);
  if (run.dryRun && !o.keep && !o.out && run.root) rmSync(run.root, { recursive: true, force: true });
  if (run.dryRun) return threw ? 1 : 0;
  return stopReason || threw ? 1 : 0;
}

function list() {
  console.log('LP       kind     account        status        freezes');
  for (const p of PROBES) {
    console.log(`${p.id.padEnd(8)} ${p.kind.padEnd(8)} ${p.account.padEnd(14)} ${statusOf(p).padEnd(13)} ${p.freezes}`);
    const why = p.manual ?? p.notRun ?? p.blocked ?? p.conditional ?? p.note;
    if (why) console.log(`${' '.repeat(9)}${why}`);
  }
  return 0;
}

function watch(dir) {
  const home = realHome();
  try {
    checkConfigDir(resolve(dir), { home, scratchRoots: [DEFAULT_SCRATCH_ROOT] });
  } catch (e) {
    console.error(`live-probe: ${e.message}`);
    return 2;
  }
  const out = { write: (r) => console.log(JSON.stringify(r)) };
  const stop = watchAccount(resolve(dir), out, { home, scratchRoots: [DEFAULT_SCRATCH_ROOT] });
  process.on('SIGINT', () => {
    stop();
    process.exit(0);
  });
  return null;
}

async function main() {
  const o = parseArgs(process.argv.slice(2));
  if (o.list) return list();
  if (o.watch) return watch(o.watch);
  if (o.live && !existsSync(o.quotaFile)) usage(`quota file not found: ${o.quotaFile}`);
  return runPhase(o);
}

if (process.argv[1] && resolve(process.argv[1]) === fileURLToPath(import.meta.url)) {
  main().then((code) => {
    if (code !== null) process.exit(code);
  }, (e) => {
    console.error(`live-probe: ${e instanceof GuardError ? e.message : e?.stack ?? e}`);
    process.exit(2);
  });
}
