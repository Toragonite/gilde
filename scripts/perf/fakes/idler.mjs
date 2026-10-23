#!/usr/bin/env node
/**
 * An idle stand-in for a watched vendor session (perf fixture, F-03 fixture):
 * it does nothing, wakes as rarely as a timer allows, and exits on SIGTERM.
 * Its argv may carry a hostile vector (after `--`) and its env the canary;
 * neither is ever read or printed here.
 */

// No `process.title`: on macOS it overwrites the argv area, so `ps` and
// KERN_PROCARGS2 (what discovery reads) would no longer show the hostile vector
// and the F-03 canary fixture would test nothing.
const timer = setInterval(() => {}, 1 << 30);
const stop = () => {
  clearInterval(timer);
  process.exit(0);
};
process.on('SIGTERM', stop);
process.on('SIGHUP', stop);
