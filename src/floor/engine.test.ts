/**
 * The Floor's scheduler and its test hooks (pixel-office §8, contract §11.2
 * rule 6, A9.2): 0 frames per second when nothing is due, nothing while
 * hidden, occluded, asleep, paused or under reduced motion; a store change
 * asks for exactly one frame; `__flSetTime` freezes the clock or throws.
 */

import type { StoreSnapshot } from '../lib/gen/StoreSnapshot';
import { assert, assertEqual, sleep, test } from '../test/harness';
import { checkSeconds, installFloorTestHooks, originFromQueue } from './testHooks';
import { T0, mountEngine, showcaseSnapshot } from './testing';

type Timers = { timer: unknown; minuteTimer: unknown };
const timers = (e: unknown) => e as Timers;

function withStates(snap: StoreSnapshot, kind: 'ready' | 'working'): StoreSnapshot {
  return { ...snap, sessions: snap.sessions.map((s) => ({ ...s, activity: { ...s.activity, state: { kind } } })) };
}

function withReducedMotion<T>(on: boolean, run: () => T): T {
  const g = globalThis as unknown as { matchMedia?: unknown };
  const was = g.matchMedia;
  g.matchMedia = (q: string) => ({ matches: on && q.includes('reduce'), media: q, addEventListener() {}, removeEventListener() {} });
  let out: T;
  try {
    out = run();
  } catch (e) {
    g.matchMedia = was;
    throw e;
  }
  if (out instanceof Promise) return out.finally(() => (g.matchMedia = was)) as T;
  g.matchMedia = was;
  return out;
}

test('T-floor-fps: idle means no frame is scheduled (at most the elapsed labels’ minute frame)', () => {
  withReducedMotion(false, () => {
    const m = mountEngine(withStates(showcaseSnapshot(8, [], 0), 'ready'));
    try {
      m.engine.frame();
      assertEqual(m.engine.rate(), 0, 'nothing animates: 0 fps');
      assertEqual(timers(m.engine).timer, null, 'no animation timer');
    } finally {
      m.unmount();
    }
  });
});

test('T-floor-fps: working agents type at 4 fps, 1 fps when the window is not frontmost', () => {
  withReducedMotion(false, () => {
    const m = mountEngine(withStates(showcaseSnapshot(8, ['native'], 0), 'working'));
    try {
      m.engine.frame();
      assertEqual(m.engine.rate(), 4, 'typing rate');
      m.store.getState().setVisibility({ frontmost: false, occluded: false, asleep: false });
      assertEqual(m.engine.rate(), 1, 'capped while not frontmost');
    } finally {
      m.unmount();
    }
  });
});

test('T-floor-fps: reduced motion, Pause, occlusion, sleep and a hidden document stop every frame', () => {
  const snap = withStates(showcaseSnapshot(8, ['native'], 0), 'working');
  withReducedMotion(true, () => {
    const m = mountEngine(snap);
    try {
      m.engine.frame();
      assertEqual(m.engine.rate(), 0, 'reduced motion: still poses');
      assertEqual(timers(m.engine).timer, null, 'no timer');
    } finally {
      m.unmount();
    }
  });
  withReducedMotion(false, () => {
    const m = mountEngine(snap);
    try {
      m.engine.setPaused(true);
      m.engine.frame();
      assertEqual(m.engine.rate(), 0, 'Pause');
      m.engine.setPaused(false);
      m.store.getState().setVisibility({ frontmost: true, occluded: true, asleep: false });
      assertEqual(m.engine.rate(), 0, 'occluded');
      m.engine.frame();
      assertEqual(timers(m.engine).timer, null, 'no timer while occluded');
      assertEqual(timers(m.engine).minuteTimer, null, 'not even the minute frame');
      m.store.getState().setVisibility({ frontmost: true, occluded: false, asleep: true });
      assertEqual(m.engine.rate(), 0, 'asleep');
      m.store.getState().setVisibility({ frontmost: true, occluded: false, asleep: false });
      Object.defineProperty(document, 'hidden', { value: true, configurable: true });
      try {
        assertEqual(m.engine.rate(), 0, 'document hidden');
        m.engine.request();
        assertEqual(timers(m.engine).timer, null, 'a hidden document asks for no frame');
      } finally {
        delete (document as unknown as Record<string, unknown>).hidden;
      }
    } finally {
      m.unmount();
    }
  });
});

test('T-floor-fps: a store change requests exactly one frame', async () => {
  await withReducedMotion(true, async () => {
    const m = mountEngine(showcaseSnapshot(8, ['native'], 0));
    try {
      await sleep(40);
      const before = m.engine.frames;
      m.store.getState().applyStoreSnapshot(showcaseSnapshot(8, ['native'], 5_000));
      m.store.getState().applyStoreSnapshot(showcaseSnapshot(8, ['native'], 15_000));
      await sleep(60);
      assertEqual(m.engine.frames - before, 1, 'two changes in one frame, coalesced');
      await sleep(120);
      assertEqual(m.engine.frames - before, 1, 'and nothing after it');
    } finally {
      m.unmount();
    }
  });
});

test('T-floor-keys: arrows move to the nearest character in that direction; Tab walks reading order', () => {
  const m = mountEngine(showcaseSnapshot(12, ['native'], 0));
  try {
    m.engine.freeze(T0);
    const f = m.engine.last;
    assert(f, 'a frame');
    const first = f.order[0] as string;
    m.engine.select(first);
    const right = m.engine.nearest('right');
    assertEqual(right, f.order[1], 'the next desk of the line is to the right');
    assertEqual(m.engine.nearest('left'), null, 'nothing left of the first desk');
    m.engine.select(f.order[f.order.length - 1] as string);
    assertEqual(m.engine.step(1), null, 'Tab leaves the Floor after the last character');
    m.engine.select(null);
    assertEqual(m.engine.step(1), first, 'Tab enters at the first character');
  } finally {
    m.unmount();
  }
});

test('T-floor-hooks: __flSetTime throws RangeError unless seconds is a finite number ≥ 0', () => {
  for (const bad of [Number.NaN, -1, -0.001, Infinity, -Infinity, '5', undefined, null]) {
    let threw: unknown = null;
    try {
      checkSeconds(bad);
    } catch (e) {
      threw = e;
    }
    assert(threw instanceof RangeError, `${String(bad)} throws RangeError`);
  }
  assertEqual(checkSeconds(0), 0, '0 is fine');
  assertEqual(checkSeconds(14.5), 14.5, 'fractions are fine');
});

test('T-floor-hooks: __flSetTime freezes the Floor at origin + t; __flOverlaps checks the frame', () => {
  const snap = showcaseSnapshot(8, ['native'], 0);
  const m = mountEngine(snap);
  const target: Record<string, unknown> = {};
  const uninstall = installFloorTestHooks(target, m.engine, m.store);
  try {
    assertEqual(originFromQueue(m.store.getState().world), T0, 'the showcase world’s now, from the queue');
    (target.__flSetTime as (s: number) => void)(5);
    assertEqual(m.engine.frozenAt, T0 + 5000, 'frozen at origin + 5 s');
    assertEqual(m.engine.rate(), 0, 'a frozen clock animates nothing');
    assertEqual((target.__flOverlaps as () => unknown[])().length, 0, 'the gate fixture passes');
    target.__gildeShowcase = { origin: T0 - 1000 };
    (target.__flSetTime as (s: number) => void)(0);
    assertEqual(m.engine.frozenAt, T0 - 1000, 'the page hook’s origin wins when present');
    let threw = false;
    try {
      (target.__flSetTime as (s: unknown) => void)(Number.NaN);
    } catch (e) {
      threw = e instanceof RangeError;
    }
    assert(threw, 'NaN throws');
  } finally {
    uninstall();
    m.unmount();
  }
  assertEqual(target.__flOverlaps, undefined, 'uninstalled');
});

test('T-floor-hooks: the app build (no showcase switch) installs no hooks', async () => {
  await import('./index');
  const w = window as unknown as Record<string, unknown>;
  assertEqual(typeof w.__flOverlaps, 'undefined', '__flOverlaps absent');
  assertEqual(typeof w.__flSetTime, 'undefined', '__flSetTime absent');
});
