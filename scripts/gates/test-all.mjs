#!/usr/bin/env node
/**
 * test-all.mjs — runs every test of the T19 scripts (gates, perf harness,
 * live-probe kit) with Node's built-in test runner; no dependency.
 *
 *   node scripts/gates/test-all.mjs                 all script tests
 *   GILDE_CONTRACT=<contract.md> node …             + the real-contract ownership tests
 *   GILDE_GATES_CHROME=1 node …                     + the real-Chrome browser-gate self-test
 *
 * The front-end tests of src/showcase run with the app's own runner
 * (`npm run test:fe`).
 */

import { spawnSync } from 'node:child_process';
import { readdirSync } from 'node:fs';
import { dirname, join, resolve } from 'node:path';
import { fileURLToPath } from 'node:url';

const scripts = resolve(dirname(fileURLToPath(import.meta.url)), '..');
const files = [];
for (const area of ['gates', 'perf', 'live-probe']) {
  const dir = join(scripts, area, 'test');
  for (const name of readdirSync(dir).sort()) if (name.endsWith('.test.mjs')) files.push(join(dir, name));
}
const r = spawnSync(process.execPath, ['--test', '--test-concurrency=1', ...files], { stdio: 'inherit' });
process.exit(r.status ?? 1);
