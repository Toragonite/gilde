// Tests for scripts/gates/lib/ownership.mjs and ownership-check.mjs (contract §15.4).
// The real-contract tests run when GILDE_CONTRACT names the contract file.

import { test } from 'node:test';
import assert from 'node:assert/strict';
import { spawnSync, execFileSync } from 'node:child_process';
import { existsSync, mkdtempSync, writeFileSync, readFileSync, rmSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { dirname, join, resolve } from 'node:path';
import { fileURLToPath } from 'node:url';

import {
  checkChangedPaths,
  classifyFiles,
  expandBraces,
  findOverlaps,
  matchPattern,
  ownersOf,
  parentModuleRelations,
  parseOwnership,
  patternsOverlap,
} from '../lib/ownership.mjs';

const here = dirname(fileURLToPath(import.meta.url));
const repo = resolve(here, '..', '..', '..');
const cli = join(repo, 'scripts', 'gates', 'ownership-check.mjs');

const MINI = `
### 15.2 Task table

| Task | Wave | Goal | Owns (exclusive) | Provides |
|---|---|---|---|---|
| **T01** Contract | 0 | goal | \`src-tauri/src/contract/**\`, \`src-tauri/src/error.rs\`, \`src-tauri/capabilities/**\` (\`main.json\`, \`debug.json\`), \`src-tauri/src/model/ids.rs\` (delete) | x |
| **T04** Render | 1 | goal | \`src-tauri/src/render/{mod,blocks}.rs\`, \`src-tauri/tests/render_*.rs\` | x |
| **T05** Convo | 1 | goal | \`src-tauri/src/convo/{mod,driver}.rs\`, \`src-tauri/src/render/claude_stream.rs\`, \`src-tauri/tests/convo_*.rs\` | x |
| **T09** Life | 1 | goal | \`src-tauri/src/lib.rs\`, \`src-tauri/src/commands/mod.rs\`, \`src-tauri/src/model/mod.rs\`, \`src/lib/*\` | x |
| **T13** FE | 1 | goal | \`src/lib/gen/**\`, \`src/store/**\` | x |
| **T18** Screens | 2 | goal | \`src/accounts/**\`, \`src/monitor/**\` (delete; the stop sheet becomes \`src/accounts/StopSheet.tsx\`) | x |

### 15.3 Waves
`;

test('expandBraces expands every alternative of the first group, recursively', () => {
  assert.deepEqual(expandBraces('a/{b,c}.rs'), ['a/b.rs', 'a/c.rs']);
  assert.deepEqual(expandBraces('{x,y}/{1,2}'), ['x/1', 'x/2', 'y/1', 'y/2']);
  assert.deepEqual(expandBraces('plain'), ['plain']);
});

test('parseOwnership reads the Owns column like owncheck.py (braces, bare file names, delete marks)', () => {
  const tasks = parseOwnership(MINI);
  assert.deepEqual([...tasks.keys()], ['T01', 'T04', 'T05', 'T09', 'T13', 'T18']);
  assert.deepEqual(tasks.get('T01').map((p) => p.pattern), [
    'src-tauri/src/contract/**', 'src-tauri/src/error.rs', 'src-tauri/capabilities/**', 'main.json', 'debug.json', 'src-tauri/src/model/ids.rs',
  ]);
  assert.equal(tasks.get('T01').find((p) => p.pattern.endsWith('ids.rs')).deleted, true);
  assert.equal(tasks.get('T01').find((p) => p.pattern.endsWith('error.rs')).deleted, false);
  assert.deepEqual(tasks.get('T04').map((p) => p.pattern), ['src-tauri/src/render/mod.rs', 'src-tauri/src/render/blocks.rs', 'src-tauri/tests/render_*.rs']);
  assert.throws(() => parseOwnership('# no table here'), /15\.2/);
});

test('matchPattern: /** recursive, /* direct files only, globs cross "/", exact otherwise', () => {
  assert.equal(matchPattern('src/store/a/b.ts', 'src/store/**'), true);
  assert.equal(matchPattern('src/storefront.ts', 'src/store/**'), false);
  assert.equal(matchPattern('src/lib/a.ts', 'src/lib/*'), true);
  assert.equal(matchPattern('src/lib/gen/a.ts', 'src/lib/*'), false);
  assert.equal(matchPattern('src-tauri/tests/world_a.rs', 'src-tauri/tests/world_*.rs'), true);
  assert.equal(matchPattern('src-tauri/tests/world_a/b.rs', 'src-tauri/tests/world_*.rs'), true, 'fnmatch * crosses /');
  assert.equal(matchPattern('src-tauri/tests/worlds.rs', 'src-tauri/tests/world_*.rs'), false);
  assert.equal(matchPattern('a/b', 'a/b'), true);
  assert.equal(matchPattern('a/b/c', 'a/b'), false);
});

test('patternsOverlap is exact on the contract grammar', () => {
  const yes = [
    ['src/accounts/**', 'src/accounts/StopSheet.tsx'],
    ['src/lib/*', 'src/lib/x.ts'],
    ['src/**', 'src/lib/*'],
    ['a/*.rs', 'a/b/**'],
    ['src-tauri/tests/*_x.rs', 'src-tauri/tests/a_*.rs'],
    ['x/[ab].rs', 'x/a.rs'],
  ];
  const no = [
    ['src/lib/*', 'src/lib/gen/**'],
    ['src-tauri/tests/world_*.rs', 'src-tauri/tests/turns_*.rs'],
    ['src/store/**', 'src/storefront/**'],
    ['src-tauri/tests/sidecar_faults.rs', 'src-tauri/tests/sessions_*.rs'],
    ['x/[ab].rs', 'x/c.rs'],
    ['a/*', 'a/b/c'],
  ];
  for (const [p, q] of yes) {
    assert.equal(patternsOverlap(p, q), true, `${p} ∩ ${q}`);
    assert.equal(patternsOverlap(q, p), true, `${q} ∩ ${p}`);
  }
  for (const [p, q] of no) {
    assert.equal(patternsOverlap(p, q), false, `${p} ∩ ${q}`);
    assert.equal(patternsOverlap(q, p), false, `${q} ∩ ${p}`);
  }
});

test('patternsOverlap agrees with brute force: every pattern pair, every string up to length 6', () => {
  // Patterns: every string of up to 3 pieces over {a, b, /, *, ?} (so exact
  // paths, globs, and literal-directory `x/*`, `x/**` forms), plus deeper
  // dir/** and dir/* forms. Witness strings use the same five characters, so a
  // literal "*" or "?" in a path is covered too. A minimal witness of two such
  // patterns is at most 6 long (e.g. `*aa` ∩ `a/b/**` = "a/b/aa"), so brute
  // force over length ≤ 6 decides each pair exactly.
  const alphabet = ['a', 'b', '/', '*', '?'];
  const patterns = new Set();
  const grow = (s) => {
    if (s.length > 0) patterns.add(s);
    if (s.length >= 3) return;
    for (const p of alphabet) grow(s + p);
  };
  grow('');
  for (const d of ['a', 'a/b', 'b']) {
    patterns.add(`${d}/**`);
    patterns.add(`${d}/*`);
  }
  const pats = [...patterns];
  const strings = [];
  const gen = (s) => {
    strings.push(s);
    if (s.length >= 6) return;
    for (const c of alphabet) gen(s + c);
  };
  gen('');
  const words = Math.ceil(strings.length / 32);
  const sets = pats.map((p) => {
    const bits = new Uint32Array(words);
    strings.forEach((s, k) => {
      if (matchPattern(s, p)) bits[k >>> 5] |= 1 << (k & 31);
    });
    return bits;
  });
  let pairs = 0;
  for (let i = 0; i < pats.length; i++) {
    for (let j = i; j < pats.length; j++) {
      let brute = false;
      for (let w = 0; w < words && !brute; w++) brute = (sets[i][w] & sets[j][w]) !== 0;
      assert.equal(patternsOverlap(pats[i], pats[j]), brute, `${pats[i]} ∩ ${pats[j]}`);
      pairs++;
    }
  }
  assert.ok(pairs > 10000, `checked ${pairs} pairs`);
});

test('findOverlaps separates cross-task from intra-task overlaps', () => {
  const tasks = parseOwnership(MINI);
  const { crossTask, intraTask } = findOverlaps(tasks);
  assert.equal(crossTask.length, 0);
  assert.deepEqual(intraTask.map(([a, b]) => [a.task, a.pattern, b.pattern]), [['T18', 'src/accounts/**', 'src/accounts/StopSheet.tsx']]);
  const clash = new Map(tasks);
  clash.set('T99', [{ task: 'T99', pattern: 'src/lib/x.ts', token: 'src/lib/x.ts', deleted: false }]);
  const r = findOverlaps(clash);
  assert.deepEqual(r.crossTask.map(([a, b]) => [a.task, b.task]), [['T09', 'T99']]);
});

test('classifyFiles reports multi-owned files and unowned files under the code roots', () => {
  const tasks = parseOwnership(MINI);
  const r = classifyFiles(['src/lib/a.ts', 'src/main.tsx', 'docs/x.md', 'scripts/y.sh', 'src/store/s.ts'], tasks);
  assert.equal(r.multiOwned.length, 0);
  assert.deepEqual(r.unownedInRoots, ['src/main.tsx', 'scripts/y.sh']);
  assert.deepEqual(ownersOf('src/lib/a.ts', tasks), ['T09']);
});

test('parentModuleRelations finds Rust files under a parent module file of another owner', () => {
  const tasks = parseOwnership(MINI);
  const { fileLevel, dirLevel } = parentModuleRelations(tasks, []);
  assert.deepEqual(fileLevel.map((r) => `${r.file}<${r.parent}`), [
    'src-tauri/src/error.rs<src-tauri/src/lib.rs',
    'src-tauri/src/model/ids.rs<src-tauri/src/model/mod.rs',
    'src-tauri/src/render/claude_stream.rs<src-tauri/src/render/mod.rs',
  ]);
  assert.deepEqual(dirLevel.map((r) => `${r.dir}:${r.owner}`), [
    'src-tauri/src/contract/:T01', 'src-tauri/src/convo/:T05', 'src-tauri/src/render/:T04',
  ]);
});

test('checkChangedPaths: own, H1, unowned and foreign paths', () => {
  const tasks = parseOwnership(MINI);
  const r = checkChangedPaths(['src/store/a.ts', 'src/lib/b.ts', 'README.md', 'src/accounts/x.tsx'], 'T13', tasks, { h1: true });
  assert.deepEqual(r.rows.map((x) => x.verdict), ['own', 'h1', 'unowned', 'other']);
  assert.equal(r.outside.length, 2);
  const strict = checkChangedPaths(['src/lib/b.ts'], 'T13', tasks);
  assert.equal(strict.rows[0].verdict, 'other');
});

test('CLI task mode: exit 0 inside, 1 outside, 2 on bad usage', () => {
  const dir = mkdtempSync(join(tmpdir(), 'owncheck-'));
  try {
    const contract = join(dir, 'contract.md');
    writeFileSync(contract, MINI);
    const ok = spawnSync(process.execPath, [cli, '--contract', contract, '--task', 'T13', 'src/store/a.ts'], { encoding: 'utf8' });
    assert.equal(ok.status, 0, ok.stdout + ok.stderr);
    const bad = spawnSync(process.execPath, [cli, '--contract', contract, '--task', 'T13', 'src/store/a.ts', 'src/main.tsx'], { encoding: 'utf8' });
    assert.equal(bad.status, 1);
    assert.match(bad.stdout, /UNOWNED|unowned/);
    assert.match(bad.stdout, /OUTSIDE OWNERSHIP: 1/);
    const list = join(dir, 'changed.txt');
    writeFileSync(list, 'src/store/a.ts\nsrc/lib/x.ts\n');
    const h1 = spawnSync(process.execPath, [cli, '--contract', contract, '--task', 'T13', '--changed', list, '--h1'], { encoding: 'utf8' });
    assert.equal(h1.status, 0, h1.stdout);
    const usage = spawnSync(process.execPath, [cli, '--contract', contract, '--task', 'X1', 'a'], { encoding: 'utf8' });
    assert.equal(usage.status, 2);
    const missing = spawnSync(process.execPath, [cli, '--contract', join(dir, 'nope.md')], { encoding: 'utf8' });
    assert.equal(missing.status, 2);
  } finally {
    rmSync(dir, { recursive: true, force: true });
  }
});

const CONTRACT = process.env.GILDE_CONTRACT;
const haveContract = CONTRACT && existsSync(CONTRACT);
let haveFc414 = false;
try {
  execFileSync('git', ['-C', repo, 'cat-file', '-e', 'fc414a1^{commit}'], { stdio: 'ignore' });
  haveFc414 = true;
} catch {
  haveFc414 = false;
}

test('the real contract reproduces the §15.4 result', { skip: haveContract ? false : 'GILDE_CONTRACT not set' }, () => {
  const tasks = parseOwnership(readFileSync(CONTRACT, 'utf8'));
  assert.equal(tasks.size, 22);
  const patterns = [...tasks.values()].reduce((n, p) => n + p.length, 0);
  // §15.4 recorded 258; A7.1 added `src-tauri/permissions/**` to T01 (and amended §15.4).
  assert.equal(patterns, 259);
  assert.ok(tasks.get('T01').some((p) => p.pattern === 'src-tauri/permissions/**'));
  const { crossTask, intraTask } = findOverlaps(tasks);
  assert.equal(crossTask.length, 0);
  assert.deepEqual(intraTask.map(([a, b]) => `${a.task}:${a.pattern}:${b.pattern}`), ['T18:src/accounts/**:src/accounts/StopSheet.tsx']);
  const { fileLevel } = parentModuleRelations(tasks, []);
  assert.equal(fileLevel.length, 28);
  assert.equal(fileLevel.filter((r) => r.parent === 'src-tauri/src/commands/mod.rs').length, 20);
});

test('the real contract at fc414a1: no file with two owners, the six recorded unowned files', { skip: haveContract && haveFc414 ? false : 'GILDE_CONTRACT not set or fc414a1 missing' }, () => {
  const tasks = parseOwnership(readFileSync(CONTRACT, 'utf8'));
  const files = execFileSync('git', ['-C', repo, 'ls-tree', '-r', '--name-only', 'fc414a1'], { encoding: 'utf8' }).split('\n').filter(Boolean);
  const r = classifyFiles(files, tasks);
  assert.equal(r.multiOwned.length, 0);
  assert.deepEqual(r.unownedInRoots, [
    'scripts/check-version.mjs', 'scripts/export-public.sh', 'scripts/fetch-node.sh', 'scripts/hygiene.sh', 'src-tauri/src/main.rs', 'src/vite-env.d.ts',
  ]);
});
