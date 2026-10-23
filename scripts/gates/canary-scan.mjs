#!/usr/bin/env node
/**
 * canary-scan.mjs — the leak check of T-priv-3 and T-priv-argv (contract
 * §9.7, §14.2): after an integration run that fed the F-03 fixture
 * (`fixtures/hostile-argv.json`: hostile argv, an env-value canary, token and
 * prompt canaries) through spawn, discovery and `respawn-pane`, no canary may
 * appear in `state.db`, `config.json`, logs, Details, `health_process_detail`
 * output, diagnostics exports or crash reports.
 *
 *   node scripts/gates/canary-scan.mjs <file-or-dir>… [--extra <value>]… [--json]
 *
 * Point it at the run's GILDE_HOME, diagnostics export and captured IPC
 * responses. Files are searched byte-wise (UTF-8 and UTF-16LE), so SQLite
 * pages count. Only canary names and file paths are printed.
 *
 * Exit codes: 0 no canary found; 1 a canary was found; 2 bad usage.
 */

import { existsSync } from 'node:fs';
import { resolve } from 'node:path';
import { fileURLToPath } from 'node:url';

import { loadCanaries, scanForCanaries } from './lib/canary.mjs';

function main() {
  const argv = process.argv.slice(2);
  const paths = [];
  const extra = {};
  let json = false;
  for (let i = 0; i < argv.length; i++) {
    if (argv[i] === '--json') json = true;
    else if (argv[i] === '--extra') {
      const v = argv[++i];
      if (!v) {
        console.error('canary-scan: --extra needs a value');
        process.exit(2);
      }
      extra[`EXTRA_${Object.keys(extra).length + 1}`] = v;
    } else paths.push(resolve(argv[i]));
  }
  if (paths.length === 0) {
    console.error('usage: canary-scan.mjs <file-or-dir>… [--extra <value>]… [--json]');
    process.exit(2);
  }
  for (const p of paths) {
    if (!existsSync(p)) {
      console.error(`canary-scan: not found: ${p}`);
      process.exit(2);
    }
  }
  const { canaries } = loadCanaries();
  const hits = scanForCanaries(paths, { ...canaries, ...extra });
  if (json) console.log(JSON.stringify({ ok: hits.length === 0, hits }, null, 2));
  else {
    for (const h of hits) console.log(`FAIL ${h.file}: ${h.names.join(', ')}`);
    console.log(hits.length ? `canary-scan: ${hits.length} file(s) leak a canary` : 'canary-scan: clean');
  }
  process.exitCode = hits.length ? 1 : 0;
}

if (process.argv[1] && resolve(process.argv[1]) === fileURLToPath(import.meta.url)) main();
