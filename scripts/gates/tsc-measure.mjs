#!/usr/bin/env node
/**
 * tsc-measure.mjs — G0's `tsc` gate in its wave-1 form (contract A8.2).
 *
 * Until the wave-2 exit, `tsc` passes when files owned by wave-0/1 tasks have
 * 0 errors; errors in files owned by T14–T18 (the old screens they rewrite)
 * are tolerated and counted. Error paths are attributed through the §15.4
 * ownership check (lib/ownership.mjs). `--wave2-exit` applies the later rule:
 * 0 errors in total.
 *
 *   node scripts/gates/tsc-measure.mjs --contract <contract.md> [--from <tsc-output.txt>] [--wave2-exit] [--json]
 *
 * Without `--from` it runs `npx tsc --noEmit -p .` in the repository root
 * (slow on a busy machine; `--from` reads a saved output instead).
 *
 * Exit codes: 0 the rule holds; 1 it does not; 2 the gate could not run.
 */

import { spawnSync } from 'node:child_process';
import { existsSync, readFileSync } from 'node:fs';
import { dirname, join, resolve } from 'node:path';
import { fileURLToPath } from 'node:url';

import { WAVE_01_TASKS, ownersOf, parseOwnership } from './lib/ownership.mjs';

const here = dirname(fileURLToPath(import.meta.url));
const root = resolve(here, '..', '..');

/** `path(line,col): error TSnnnn: …` lines → `{ path, code }` (paths repo-relative). */
export function parseTscOutput(text) {
  const out = [];
  for (const line of text.split('\n')) {
    const m = /^(.+?)\((\d+),(\d+)\): error (TS\d+):/.exec(line.trim());
    if (m) out.push({ path: m[1].replace(/\\/g, '/').replace(/^\.\//, ''), line: Number(m[2]), code: m[4] });
  }
  return out;
}

/** Errors grouped by owner class: `wave01`, `wave2` (T14–T18), `other`, `unowned`; per task and per file. */
export function measure(errors, tasks) {
  const byTask = {};
  const byFile = {};
  const classes = { wave01: 0, wave2: 0, other: 0, unowned: 0 };
  for (const e of errors) {
    const owners = ownersOf(e.path, tasks);
    const key = owners.join(',') || 'UNOWNED';
    byTask[key] = (byTask[key] ?? 0) + 1;
    byFile[e.path] = (byFile[e.path] ?? 0) + 1;
    if (owners.length === 0) classes.unowned++;
    else if (owners.some((o) => WAVE_01_TASKS.has(o))) classes.wave01++;
    else if (owners.every((o) => /^T1[4-8]$/.test(o))) classes.wave2++;
    else classes.other++;
  }
  return { total: errors.length, classes, byTask, byFile };
}

function main() {
  const argv = process.argv.slice(2);
  let contract = process.env.GILDE_CONTRACT ?? null;
  let from = null;
  let json = false;
  let wave2Exit = false;
  for (let i = 0; i < argv.length; i++) {
    const a = argv[i];
    if (a === '--contract') contract = argv[++i];
    else if (a === '--from') from = argv[++i];
    else if (a === '--json') json = true;
    else if (a === '--wave2-exit') wave2Exit = true;
    else {
      console.error(`tsc-measure: unknown argument ${a}`);
      process.exit(2);
    }
  }
  if (!contract || !existsSync(contract)) {
    console.error('tsc-measure: --contract (or GILDE_CONTRACT) must name the contract file');
    process.exit(2);
  }
  let output;
  if (from) {
    if (!existsSync(from)) {
      console.error(`tsc-measure: --from file not found: ${from}`);
      process.exit(2);
    }
    output = readFileSync(from, 'utf8');
  } else {
    const r = spawnSync('npx', ['tsc', '--noEmit', '-p', '.'], { cwd: root, encoding: 'utf8', maxBuffer: 256 * 1024 * 1024 });
    if (r.error) {
      console.error(`tsc-measure: could not run tsc: ${r.error.message}`);
      process.exit(2);
    }
    output = (r.stdout ?? '') + (r.stderr ?? '');
  }
  const tasks = parseOwnership(readFileSync(contract, 'utf8'));
  const m = measure(parseTscOutput(output), tasks);
  const showcase = Object.entries(m.byFile).filter(([p]) => p.startsWith('src/showcase/')).reduce((n, [, c]) => n + c, 0);
  const ok = wave2Exit ? m.total === 0 : m.classes.wave01 === 0 && m.classes.unowned === 0 && m.classes.other === 0;
  if (json) {
    console.log(JSON.stringify({ ok, rule: wave2Exit ? 'wave2-exit' : 'wave1', showcase, ...m }, null, 2));
  } else {
    console.log(`tsc-measure (${wave2Exit ? 'wave-2 exit: 0 errors in total' : 'A8.2 wave 1: 0 errors in wave-0/1 files'})`);
    console.log(`  total ${m.total}: wave-0/1 files ${m.classes.wave01}, T14–T18 files ${m.classes.wave2} (tolerated until the wave-2 exit), other ${m.classes.other}, unowned ${m.classes.unowned}`);
    console.log(`  src/showcase/** (T19): ${showcase}`);
    for (const [task, n] of Object.entries(m.byTask).sort()) console.log(`    ${task.padEnd(12)} ${n}`);
    if (!wave2Exit) {
      for (const [path, n] of Object.entries(m.byFile)) {
        const owners = ownersOf(path, tasks);
        if (owners.length === 0 || owners.some((o) => WAVE_01_TASKS.has(o)) || !owners.every((o) => /^T1[4-8]$/.test(o))) console.log(`  FAIL ${path}: ${n} (${owners.join(',') || 'unowned'})`);
      }
    }
    console.log(ok ? 'tsc-measure: holds' : 'tsc-measure: FAILED');
  }
  process.exitCode = ok ? 0 : 1;
}

if (process.argv[1] && resolve(process.argv[1]) === fileURLToPath(import.meta.url)) main();
