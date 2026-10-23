/**
 * A coarse shared clock for "waiting 2 min" style ages: one timer for every
 * mounted card and composer line, ticking every 15 s (ages are shown in whole
 * minutes after the first one, so a finer tick only costs wakeups).
 */

import { useSyncExternalStore } from 'react';

const TICK_MS = 15_000;
const listeners = new Set<() => void>();
let timer: ReturnType<typeof setInterval> | null = null;
let now = Date.now();

function subscribe(fn: () => void): () => void {
  listeners.add(fn);
  if (timer === null) {
    now = Date.now();
    timer = setInterval(() => {
      now = Date.now();
      for (const l of [...listeners]) l();
    }, TICK_MS);
  }
  return () => {
    listeners.delete(fn);
    if (listeners.size === 0 && timer !== null) {
      clearInterval(timer);
      timer = null;
    }
  };
}

function snapshot(): number {
  return now;
}

/**
 * Wall-clock milliseconds, refreshed every 15 s while anything is mounted.
 * The first subscriber refreshes the value, and React re-reads the snapshot
 * after subscribing, so a value left over from an idle period never sticks.
 */
export function useNow(): number {
  return useSyncExternalStore(subscribe, snapshot, snapshot);
}
