#!/usr/bin/env node
/**
 * run.mjs — the G5 perf harness (contract §11.1, §14.3; wakeup counter F-34;
 * the F-03 hostile-argv and env-canary sessions ride in the same fixture).
 *
 * Dry run (no app; proves the harness end to end):
 *   node scripts/perf/run.mjs --dry-run [--seconds 3] [--keep]
 *     materializes the 10 watched sessions of fixtures/idle-20.json in a temp
 *     root (two with hostile argv + the env canary), samples their CPU with
 *     top, measures a stand-in wakeup counter, judges both against the §11.1
 *     budgets, runs the canary scan over the temp root, and tears everything
 *     down (SIGTERM to the groups it started).
 *
 * Fixture only (the lead then starts the release app against it):
 *   node scripts/perf/run.mjs --fixture <dir>
 *     prints HOME / GILDE_HOME to launch the app with, plus the pids of the
 *     stand-ins; Ctrl-C tears it down.
 *
 * Measure a running app (release build, 20-session fixture, §11.1):
 *   node scripts/perf/run.mjs --pids <core,webcontent,sidecar> [--seconds 60]
 *        (--focused | --unfocused) [--wakeup-file <path>] [--scan <dir>…]
 *        [--powermetrics <saved output> --core-pid <pid>]
 *     `--wakeup-file` is the file the core writes with GILDE_WAKEUP_COUNT_FILE
 *     (feature `wakeup-count`); `--scan` runs the canary scan (T-priv-3) on
 *     GILDE_HOME and exports afterwards.
 *
 * Exit codes: 0 every measured budget holds (dry run: the harness ran); 1 a
 * budget failed or a canary leaked; 2 bad usage.
 */

import { spawn } from 'node:child_process';
import { existsSync, mkdtempSync, readFileSync, rmSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { dirname, join, resolve } from 'node:path';
import { fileURLToPath } from 'node:url';

import { loadCanaries, scanForCanaries } from '../gates/lib/canary.mjs';
import { materialize } from './lib/fixture.mjs';
import { judge, measureWakeups, parsePowermetrics, sampleTop, summarizeCpu } from './lib/measure.mjs';

const HERE = dirname(fileURLToPath(import.meta.url));

function usage(msg) {
  if (msg) console.error(`perf: ${msg}`);
  console.error('usage: run.mjs --dry-run [--seconds n] [--keep] | --fixture <dir> | --pids a,b,c (--focused|--unfocused) [--seconds 60] [--wakeup-file f] [--scan dir…] [--powermetrics f --core-pid p]');
  process.exit(2);
}

export function parseArgs(argv) {
  const o = { dryRun: false, fixture: null, pids: null, seconds: null, focus: null, wakeupFile: null, scan: [], keep: false, powermetrics: null, corePid: null };
  for (let i = 0; i < argv.length; i++) {
    const a = argv[i];
    const val = () => (i + 1 < argv.length ? argv[++i] : usage(`${a} needs a value`));
    switch (a) {
      case '--dry-run': o.dryRun = true; break;
      case '--fixture': o.fixture = resolve(val()); break;
      case '--pids': o.pids = val().split(',').map((s) => Number(s.trim())); break;
      case '--seconds': o.seconds = Number(val()); break;
      case '--focused': o.focus = 'focused'; break;
      case '--unfocused': o.focus = 'unfocused'; break;
      case '--wakeup-file': o.wakeupFile = resolve(val()); break;
      case '--scan': o.scan.push(resolve(val())); break;
      case '--keep': o.keep = true; break;
      case '--powermetrics': o.powermetrics = resolve(val()); break;
      case '--core-pid': o.corePid = Number(val()); break;
      default: usage(`unknown argument ${a}`);
    }
  }
  const modes = [o.dryRun, o.fixture !== null, o.pids !== null].filter(Boolean).length;
  if (modes !== 1) usage('pick one of --dry-run, --fixture, --pids');
  if (o.pids && (o.pids.length === 0 || o.pids.some((p) => !Number.isInteger(p) || p <= 1))) usage('--pids takes positive process ids');
  if (o.pids && !o.focus) usage('--pids needs --focused or --unfocused');
  if (o.seconds !== null && (!Number.isInteger(o.seconds) || o.seconds < 1 || o.seconds > 600)) usage('--seconds must be 1–600');
  if (o.powermetrics && !o.corePid) usage('--powermetrics needs --core-pid');
  return o;
}

function report(results) {
  for (const r of results) console.log(`${r.ok ? 'ok  ' : 'FAIL'} ${r.gate}: ${r.value} (budget ${r.budget})`);
  return results.every((r) => r.ok);
}

async function dryRun(o) {
  const root = mkdtempSync(join(tmpdir(), 'gilde-perf-dry-'));
  const seconds = o.seconds ?? 3;
  const fixture = await materialize(root);
  const counterFile = join(root, 'wakeups.json');
  const counter = spawn(process.execPath, [join(HERE, 'fakes', 'wakeup-counter.mjs'), '--per-minute', '20'], { env: { PATH: '/usr/bin:/bin', GILDE_WAKEUP_COUNT_FILE: counterFile }, detached: true, stdio: 'ignore' });
  let ok = true;
  try {
    const pids = fixture.sessions.map((s) => s.pid);
    console.log(`fixture: ${fixture.sessions.length} watched stand-ins (${fixture.sessions.filter((s) => s.hostile).length} with hostile argv + env canary) in ${root}`);
    await new Promise((r) => setTimeout(r, 400));
    const [samples, wake] = await Promise.all([sampleTop(pids, seconds), measureWakeups(counterFile, seconds)]);
    const cpu = summarizeCpu(samples, pids);
    console.log(`top: ${cpu.samples} sample(s), summed CPU of the stand-ins ${cpu.totalPct} %${cpu.missing.length ? `, missing pids ${cpu.missing.join(',')}` : ''}`);
    console.log(`wakeup counter: ${wake.per60s} per 60 s`);
    const verdicts = judge({ cpuPct: cpu.totalPct, wakeupsPer60s: wake.per60s }, 'focused');
    report(verdicts);
    const hits = scanForCanaries([fixture.configDir, fixture.gildeHome], loadCanaries().canaries);
    // The harness itself must never write a canary: registry rows and transcripts carry no argv or env.
    console.log(hits.length ? `FAIL canary in ${hits.map((h) => h.file).join(', ')}` : 'ok   canary scan of the fixture: clean');
    ok = hits.length === 0 && cpu.samples === seconds && wake.per60s !== null;
  } finally {
    try {
      process.kill(-counter.pid, 'SIGTERM');
    } catch {
      // gone
    }
    const survivors = await fixture.stop();
    if (survivors.length) console.log(`note: stand-ins still running after SIGTERM: ${survivors.join(',')} (reported, never SIGKILL)`);
    if (!o.keep) rmSync(root, { recursive: true, force: true });
  }
  console.log(ok ? 'perf dry run: the harness ran end to end' : 'perf dry run: FAILED');
  return ok ? 0 : 1;
}

async function fixtureOnly(o) {
  const fixture = await materialize(o.fixture);
  console.log(`HOME=${fixture.home}`);
  console.log(`GILDE_HOME=${fixture.gildeHome}`);
  console.log(`watched stand-ins: ${fixture.sessions.map((s) => s.pid).join(',')}`);
  console.log('Start the release app with this HOME and GILDE_HOME (and the fake vendor binaries first on PATH), start the 10 Gilde-owned sessions of fixtures/idle-20.json, then run --pids. Ctrl-C tears the stand-ins down.');
  await new Promise((resolveStop) => process.once('SIGINT', resolveStop));
  await fixture.stop();
  return 0;
}

async function measure(o) {
  const seconds = o.seconds ?? 60;
  const [samples, wake] = await Promise.all([sampleTop(o.pids, seconds), o.wakeupFile ? measureWakeups(o.wakeupFile, seconds) : Promise.resolve(null)]);
  const cpu = summarizeCpu(samples, o.pids);
  if (cpu.missing.length) console.log(`note: no samples for pid(s) ${cpu.missing.join(',')}`);
  const results = judge({ cpuPct: cpu.totalPct, wakeupsPer60s: wake?.per60s ?? null }, o.focus);
  if (o.wakeupFile && wake?.per60s === null) results.push({ gate: 'wakeup counter readable', value: 'no reading', budget: o.wakeupFile, ok: false });
  if (o.powermetrics) {
    const row = parsePowermetrics(readFileSync(o.powermetrics, 'utf8'), o.corePid);
    console.log(row ? `powermetrics cross-check: ${row.per60s} interrupt wakeups per 60 s (${row.name})` : 'powermetrics cross-check: core pid not in the saved output');
  }
  let ok = report(results);
  if (o.scan.length) {
    for (const p of o.scan) if (!existsSync(p)) usage(`--scan path not found: ${p}`);
    const hits = scanForCanaries(o.scan, loadCanaries().canaries);
    for (const h of hits) console.log(`FAIL canary ${h.names.join(', ')} in ${h.file}`);
    if (hits.length) ok = false;
    else console.log('ok   canary scan: clean');
  }
  return ok ? 0 : 1;
}

async function main() {
  const o = parseArgs(process.argv.slice(2));
  if (o.dryRun) return dryRun(o);
  if (o.fixture) return fixtureOnly(o);
  return measure(o);
}

if (process.argv[1] && resolve(process.argv[1]) === fileURLToPath(import.meta.url)) {
  main().then((code) => process.exit(code), (e) => {
    console.error(`perf: ${e?.stack ?? e}`);
    process.exit(2);
  });
}
