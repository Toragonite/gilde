#!/usr/bin/env node
/**
 * A stand-in for the core's wakeup counter (feature `wakeup-count`, F-34) for
 * the harness dry run: writes `{"v":1,"wakeups":n,"monotonicMs":t}` to the
 * file named by GILDE_WAKEUP_COUNT_FILE every 200 ms (atomic rename), with n
 * growing at `--per-minute` wakeups per minute. Exits on SIGTERM.
 */

import { renameSync, writeFileSync } from 'node:fs';
import { performance } from 'node:perf_hooks';

const path = process.env.GILDE_WAKEUP_COUNT_FILE;
if (!path) process.exit(2);
const i = process.argv.indexOf('--per-minute');
const perMinute = i >= 0 ? Number(process.argv[i + 1]) : 20;
const t0 = performance.now();
const write = () => {
  const ms = performance.now() - t0;
  writeFileSync(`${path}.tmp`, JSON.stringify({ v: 1, wakeups: Math.floor((ms / 60_000) * perMinute), monotonicMs: Math.round(ms) }));
  renameSync(`${path}.tmp`, path);
};
write();
const timer = setInterval(write, 200);
process.on('SIGTERM', () => {
  clearInterval(timer);
  process.exit(0);
});
