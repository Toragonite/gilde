#!/usr/bin/env node
/**
 * ownership-check.mjs — the mechanical ownership check of contract §15.4.
 *
 * Manifest mode (what §15.4 records): parse the §15.2 "Owns" column, test every
 * pair of patterns of different tasks for overlap (exact intersection), test
 * every tracked file at a git ref against all patterns, and list the
 * parent-module relations of F-18.
 *
 *   node scripts/gates/ownership-check.mjs --contract <contract.md> [--repo <dir>] [--ref <rev>] [--json]
 *
 * Task mode (what the lead runs before merging a task, like owncheck.py):
 * every changed path must be owned by the task.
 *
 *   node scripts/gates/ownership-check.mjs --contract <md> --task T19 --diff gilde2/main [--h1]
 *   node scripts/gates/ownership-check.mjs --contract <md> --task T19 --changed <file-with-paths>
 *   node scripts/gates/ownership-check.mjs --contract <md> --task T19 <path>…
 *
 * Owners of paths:
 *
 *   node scripts/gates/ownership-check.mjs --contract <md> --owners <path>…
 *
 * `--diff <base>` = `git diff --name-only <base>` (committed and uncommitted
 * changes, deletions included) plus untracked files. `--h1` accepts paths owned
 * only by T02–T13/T19/T21 (hand-off H1, wave 0 only). The contract path may
 * also come from `GILDE_CONTRACT`.
 *
 * Exit codes: 0 clean; 1 a cross-task overlap, a tracked file with two owners,
 * or a changed path outside the task; 2 the check could not run (bad flags,
 * missing contract, git failure).
 */

import { execFileSync } from 'node:child_process';
import { existsSync, readFileSync } from 'node:fs';
import { resolve } from 'node:path';
import { fileURLToPath } from 'node:url';

import {
  CODE_ROOTS,
  checkChangedPaths,
  classifyFiles,
  findOverlaps,
  ownersOf,
  parentModuleRelations,
  parseOwnership,
} from './lib/ownership.mjs';

function usage(message) {
  if (message) console.error(`ownership-check: ${message}`);
  console.error('usage: ownership-check.mjs --contract <md> [--repo <dir>] [--ref <rev>] [--json]');
  console.error('       ownership-check.mjs --contract <md> --task Txx (--diff <base> | --changed <file> | <path>…) [--h1]');
  console.error('       ownership-check.mjs --contract <md> --owners <path>…');
  process.exit(2);
}

export function parseArgs(argv) {
  const opts = { contract: process.env.GILDE_CONTRACT ?? null, repo: process.cwd(), ref: 'HEAD', json: false, h1: false, task: null, diff: null, changed: null, owners: false, paths: [] };
  const value = (i, flag) => {
    if (i + 1 >= argv.length || argv[i + 1].startsWith('--')) usage(`${flag} needs a value`);
    return argv[i + 1];
  };
  for (let i = 0; i < argv.length; i++) {
    const a = argv[i];
    switch (a) {
      case '--contract': opts.contract = value(i, a); i++; break;
      case '--repo': opts.repo = value(i, a); i++; break;
      case '--ref': opts.ref = value(i, a); i++; break;
      case '--task': opts.task = value(i, a); i++; break;
      case '--diff': opts.diff = value(i, a); i++; break;
      case '--changed': opts.changed = value(i, a); i++; break;
      case '--json': opts.json = true; break;
      case '--h1': opts.h1 = true; break;
      case '--owners': opts.owners = true; break;
      case '-h': case '--help': usage(); break;
      default:
        if (a.startsWith('--')) usage(`unknown flag ${a}`);
        opts.paths.push(a);
    }
  }
  if (!opts.contract) usage('--contract (or GILDE_CONTRACT) is required');
  if (opts.task && !/^T\d\d$/.test(opts.task)) usage(`--task must look like T19, got ${opts.task}`);
  const sources = [opts.diff, opts.changed, opts.paths.length ? 'paths' : null].filter(Boolean).length;
  if (opts.task && sources !== 1) usage('--task needs exactly one of --diff, --changed or paths');
  if (opts.owners && opts.paths.length === 0) usage('--owners needs paths');
  if (!opts.task && !opts.owners && (opts.diff || opts.changed || opts.paths.length)) usage('paths, --diff and --changed need --task');
  return opts;
}

function git(repo, args) {
  try {
    return execFileSync('git', ['-C', repo, ...args], { encoding: 'utf8', maxBuffer: 64 * 1024 * 1024, stdio: ['ignore', 'pipe', 'pipe'] });
  } catch (e) {
    console.error(`ownership-check: git ${args.join(' ')} failed: ${String(e.stderr || e.message).trim()}`);
    process.exit(2);
  }
}

const lines = (text) => text.split('\n').map((l) => l.trim()).filter(Boolean);

function manifestMode(tasks, opts) {
  const files = lines(git(opts.repo, ['ls-tree', '-r', '--name-only', opts.ref]));
  const patternCount = [...tasks.values()].reduce((n, p) => n + p.length, 0);
  const overlaps = findOverlaps(tasks);
  const fileReport = classifyFiles(files, tasks, CODE_ROOTS);
  const relations = parentModuleRelations(tasks, files);
  const failed = overlaps.crossTask.length > 0 || fileReport.multiOwned.length > 0;

  if (opts.json) {
    const pair = ([a, b]) => ({ a: { task: a.task, pattern: a.pattern }, b: { task: b.task, pattern: b.pattern } });
    console.log(JSON.stringify({
      ref: opts.ref,
      tasks: tasks.size,
      patterns: patternCount,
      crossTaskOverlaps: overlaps.crossTask.map(pair),
      intraTaskOverlaps: overlaps.intraTask.map(pair),
      trackedFiles: fileReport.total,
      multiOwned: fileReport.multiOwned,
      unownedInCodeRoots: fileReport.unownedInRoots,
      parentModuleRelations: { fileLevel: relations.fileLevel, dirLevel: relations.dirLevel },
      ok: !failed,
    }, null, 2));
    return failed ? 1 : 0;
  }

  console.log(`ownership-check @ ${opts.ref}`);
  console.log(`  ${tasks.size} tasks, ${patternCount} path patterns`);
  console.log(`  cross-task overlaps: ${overlaps.crossTask.length}`);
  for (const [a, b] of overlaps.crossTask) console.log(`    FAIL ${a.task} ${a.pattern}  <>  ${b.task} ${b.pattern}`);
  console.log(`  intra-task overlaps: ${overlaps.intraTask.length}`);
  for (const [a, b] of overlaps.intraTask) console.log(`    ${a.task}: ${a.pattern}  contains/meets  ${b.pattern}`);
  console.log(`  tracked files: ${fileReport.total} (${fileReport.owned} owned), owned by more than one task: ${fileReport.multiOwned.length}`);
  for (const m of fileReport.multiOwned) console.log(`    FAIL ${m.file}  ${m.owners.join(', ')}`);
  console.log(`  tracked files under ${CODE_ROOTS.join(', ')} that no task owns (must not change): ${fileReport.unownedInRoots.length}`);
  for (const f of fileReport.unownedInRoots) console.log(`    ${f}`);
  console.log(`  parent-module relations (F-18): ${relations.fileLevel.length} file-level, ${relations.dirLevel.length} top-level module directories under lib.rs`);
  for (const r of relations.fileLevel) console.log(`    ${r.file} (${r.owner}) under ${r.parent} (${r.parentOwners.join(', ')})`);
  for (const r of relations.dirLevel) console.log(`    ${r.dir} (${r.owner}) declared in ${r.parent} (${r.parentOwners.join(', ')})`);
  console.log(failed ? 'ownership-check: FAILED' : 'ownership-check: clean');
  return failed ? 1 : 0;
}

function taskMode(tasks, opts) {
  if (!tasks.has(opts.task)) {
    console.error(`ownership-check: ${opts.task} is not in the §15.2 table`);
    return 2;
  }
  let paths;
  if (opts.diff) {
    paths = [
      ...lines(git(opts.repo, ['diff', '--name-only', opts.diff])),
      ...lines(git(opts.repo, ['ls-files', '--others', '--exclude-standard'])),
    ];
  } else if (opts.changed) {
    if (!existsSync(opts.changed)) usage(`--changed file not found: ${opts.changed}`);
    paths = readFileSync(opts.changed, 'utf8').split(/\s+/).filter(Boolean);
  } else {
    paths = opts.paths;
  }
  paths = [...new Set(paths)].sort();
  const { rows, outside } = checkChangedPaths(paths, opts.task, tasks, { h1: opts.h1 });
  if (opts.json) {
    console.log(JSON.stringify({ task: opts.task, h1: opts.h1, rows, outside: outside.length, ok: outside.length === 0 }, null, 2));
    return outside.length ? 1 : 0;
  }
  const counts = {};
  for (const r of rows) {
    counts[r.verdict] = (counts[r.verdict] ?? 0) + 1;
    if (r.verdict !== 'own') console.log(`${(r.verdict + (r.owners.length ? ':' + r.owners.join(',') : '')).padEnd(24)} ${r.path}`);
  }
  console.log(`${opts.task}: ${rows.length} changed path(s)`, counts);
  if (outside.length) {
    console.log(`OUTSIDE OWNERSHIP: ${outside.length}`);
    return 1;
  }
  console.log('ownership-check: every changed path is owned by ' + opts.task);
  return 0;
}

function ownersMode(tasks, opts) {
  for (const path of opts.paths) {
    const owners = ownersOf(path, tasks);
    console.log(`${(owners.join(',') || 'UNOWNED').padEnd(12)} ${path}`);
  }
  return 0;
}

export function main(argv) {
  const opts = parseArgs(argv);
  const contractPath = resolve(opts.contract);
  if (!existsSync(contractPath)) {
    console.error(`ownership-check: contract not found: ${contractPath}`);
    return 2;
  }
  let tasks;
  try {
    tasks = parseOwnership(readFileSync(contractPath, 'utf8'));
  } catch (e) {
    console.error(`ownership-check: ${e.message}`);
    return 2;
  }
  if (opts.owners) return ownersMode(tasks, opts);
  if (opts.task) return taskMode(tasks, opts);
  return manifestMode(tasks, opts);
}

if (process.argv[1] && resolve(process.argv[1]) === fileURLToPath(import.meta.url)) {
  process.exitCode = main(process.argv.slice(2));
}
