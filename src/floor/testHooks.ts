/**
 * The Floor's test hooks (contract A9.2), installed in showcase builds only —
 * `index.ts` calls `installFloorTestHooks` behind the build-time showcase
 * switch, so an app build drops this module (the showcase tree-shaking gate).
 *
 *  - `window.__flOverlaps(): Array<{a, b, kind}>` — draws one frame with every
 *    collidable box recorded and returns what breaks the M16 rules (`[]` =
 *    pass; `geometry.ts`).
 *  - `window.__flSetTime(seconds)` — stops the Floor's real-time clock and
 *    renders deterministically at `t = seconds` of the fixture's patch script:
 *    animations settle, and the clock reads the script's origin + t. Throws a
 *    `RangeError` unless `seconds` is a finite number ≥ 0.
 *
 * The script's origin is the showcase world's `now`: read from the page hook
 * (`__gildeShowcase.origin`) if a page ever provides one, otherwise from the first
 * live attention queue (the showcase publishes it with `waitedMs = now −
 * since`, so `since + waitedMs` is the world's `now`), otherwise the wall
 * clock when the world first went live.
 */

import { clock } from '../lib/clock';
import type { WorldState } from '../store/worldSlice';
import type { FloorEngine } from './engine';
import type { Overlap } from './geometry';

export interface FloorTestHooks {
  __flOverlaps: () => Overlap[];
  __flSetTime: (seconds: number) => void;
  /** Diagnostics: scale, bays, seats, frames drawn, the current frame rate. */
  __flInfo: () => Record<string, unknown>;
}

interface HookStore {
  getState(): { world: WorldState };
  subscribe(listener: (state: { world: WorldState }) => void): () => void;
}

/** The world's `now` as the showcase published it, from the queue (`null` when the queue has no session item). */
export function originFromQueue(world: WorldState): number | null {
  let best: number | null = null;
  for (const i of world.queue.items) {
    if (i.subject.kind !== 'session') continue;
    const t = i.since + i.waitedMs;
    if (Number.isFinite(t) && (best === null || t > best)) best = t;
  }
  return best;
}

export function checkSeconds(seconds: unknown): number {
  if (typeof seconds !== 'number' || !Number.isFinite(seconds) || seconds < 0) {
    throw new RangeError('__flSetTime(seconds): seconds must be a finite number ≥ 0');
  }
  return seconds;
}

/** Install the hooks on `target` (the window). Returns the uninstall function. */
export function installFloorTestHooks(target: Record<string, unknown>, engine: FloorEngine, store: HookStore): () => void {
  const prior = target.__flUninstall as (() => void) | undefined;
  if (typeof prior === 'function') prior(); // idempotent: a second install replaces the first
  let anchor: number | null = null;
  const capture = (world: WorldState) => {
    if (anchor !== null || world.status !== 'live') return;
    anchor = originFromQueue(world) ?? clock.wall();
  };
  capture(store.getState().world);
  const unsub = store.subscribe((s) => capture(s.world));
  const origin = (): number => {
    const page = target.__gildeShowcase as { origin?: unknown } | undefined;
    if (page && typeof page.origin === 'number' && Number.isFinite(page.origin)) return page.origin;
    capture(store.getState().world);
    return anchor ?? clock.wall();
  };
  const hooks: FloorTestHooks = {
    __flOverlaps: () => engine.overlaps(),
    __flSetTime: (seconds: number) => {
      const s = checkSeconds(seconds);
      engine.freeze(origin() + Math.round(s * 1000));
    },
    __flInfo: () => ({ ...engine.info(), origin: origin() }),
  };
  const uninstall = () => {
    unsub();
    if (target.__flUninstall === uninstall) delete target.__flUninstall;
    if (target.__flOverlaps === hooks.__flOverlaps) delete target.__flOverlaps;
    if (target.__flSetTime === hooks.__flSetTime) delete target.__flSetTime;
    if (target.__flInfo === hooks.__flInfo) delete target.__flInfo;
  };
  Object.assign(target, hooks, { __flUninstall: uninstall });
  return uninstall;
}
