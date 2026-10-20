#!/usr/bin/env node
/**
 * check-showcase-tree-shaking.mjs — proves the showcase (src/showcase/, behind
 * `VITE_GILDE_SHOWCASE=1`, see src/features.ts) is NOT in a normal build and IS
 * in a showcase build.
 *
 *   node scripts/check-showcase-tree-shaking.mjs            build both ways into temp dirs, scan
 *   node scripts/check-showcase-tree-shaking.mjs --dist     also scan ./dist (after `npm run build`)
 *
 * The marker is `SHOWCASE_MARKER` of src/showcase/backend.ts, read from the
 * source so the two can never drift. Too slow for the unit runner (two
 * production builds), so it is a script of its own.
 *
 * Exit codes: 0 both checks hold; 1 a check failed; 2 the builds could not run.
 */

import { spawnSync } from 'node:child_process';
import { existsSync, mkdtempSync, readFileSync, readdirSync, rmSync, statSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { dirname, join, relative, resolve } from 'node:path';
import { fileURLToPath } from 'node:url';

const root = resolve(dirname(fileURLToPath(import.meta.url)), '..');
const source = readFileSync(join(root, 'src/showcase/backend.ts'), 'utf8');
const marker = /export const SHOWCASE_MARKER = '([^']+)'/.exec(source)?.[1];
if (!marker) {
  console.error('check-showcase: SHOWCASE_MARKER not found in src/showcase/backend.ts');
  process.exit(2);
}

/** Every file under `dir` whose text holds the marker. */
function filesWithMarker(dir) {
  const hits = [];
  const walk = (d) => {
    for (const name of readdirSync(d)) {
      const p = join(d, name);
      if (statSync(p).isDirectory()) walk(p);
      else if (readFileSync(p, 'utf8').includes(marker)) hits.push(relative(dir, p));
    }
  };
  walk(dir);
  return hits;
}

/** A production build into a fresh temp dir, with the flag set or not. */
function build(showcase) {
  const outDir = mkdtempSync(join(tmpdir(), `gilde-showcase-${showcase ? 'on' : 'off'}-`));
  const env = { ...process.env };
  delete env.VITE_GILDE_SHOWCASE;
  delete env.VITE_GILDE_EXPERIMENTAL_LANES;
  if (showcase) env.VITE_GILDE_SHOWCASE = '1';
  const run = spawnSync('npx', ['vite', 'build', '--outDir', outDir, '--emptyOutDir', '--logLevel', 'error'], {
    cwd: root,
    env,
    stdio: ['ignore', 'inherit', 'inherit'],
  });
  if (run.status !== 0) {
    console.error(`check-showcase: the ${showcase ? 'showcase' : 'normal'} build failed (exit ${run.status})`);
    rmSync(outDir, { recursive: true, force: true });
    process.exit(2);
  }
  return outDir;
}

let failed = false;
const report = (ok, line) => {
  console.log(`${ok ? 'ok  ' : 'FAIL'} ${line}`);
  if (!ok) failed = true;
};

const off = build(false);
const on = build(true);
try {
  const offHits = filesWithMarker(off);
  const offChunks = readdirSync(join(off, 'assets')).filter((n) => n.startsWith('showcase'));
  report(offHits.length === 0, `normal build: marker "${marker}" absent${offHits.length ? ` — found in ${offHits.join(', ')}` : ''}`);
  report(offChunks.length === 0, `normal build: no showcase chunk${offChunks.length ? ` — ${offChunks.join(', ')}` : ''}`);
  const onHits = filesWithMarker(on);
  report(onHits.length > 0, `showcase build: marker present${onHits.length ? ` (${onHits.join(', ')})` : ''}`);
} finally {
  rmSync(off, { recursive: true, force: true });
  rmSync(on, { recursive: true, force: true });
}

if (process.argv.includes('--dist')) {
  const dist = join(root, 'dist');
  if (!existsSync(dist)) report(false, './dist does not exist — run `npm run build` first');
  else {
    const hits = filesWithMarker(dist);
    report(hits.length === 0, `./dist: marker absent${hits.length ? ` — found in ${hits.join(', ')}` : ''}`);
  }
}

console.log(failed ? 'check-showcase: FAILED' : 'check-showcase: clean');
process.exit(failed ? 1 : 0);
