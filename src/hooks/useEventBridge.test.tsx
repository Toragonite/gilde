/**
 * The event bridge: bootstrap + build-id rule (§4.7), the World subscription,
 * the four events (§4.3) incl. the answer guard on focus-request and window
 * activation (F-30), and the reporters (view state, input activity, marks).
 */

import { act, render } from '@testing-library/react';

import { manualClock, setClockForTests } from '../lib/clock';
import { emitLocalEvent } from '../lib/events';
import type { AppBootstrapRes } from '../lib/gen/AppBootstrapRes';
import type { AppMarkReq } from '../lib/gen/AppMarkReq';
import type { ViewState } from '../lib/gen/ViewState';
import { createAppStore } from '../store/useAppStore';
import { flush, withFakeCore } from '../test/backend';
import { pending, session, snapshot, uid } from '../test/fixtures';
import { assert, assertDeepEqual, assertEqual, domTest } from '../test/harness';
import { refreshGuards, resetAnswerGuardForTests, useAnswerGuard } from './useAnswerGuard';
import pkg from '../../package.json';
import { resolveBuildId } from '../lib/buildId';
import { embeddedBuildId, startEventBridge } from './useEventBridge';

function bootstrap(buildId = 'build-1'): AppBootstrapRes {
  return {
    buildId,
    version: '2.0.0',
    locale: 'en',
    theme: 'light',
    launch: { kind: 'firstLaunch' },
    uiState: {},
    platform: { macosMajor: 15, arm64: true },
    features: { orchestrator: false },
    compat: [],
  };
}

const HANDLERS = {
  app_bootstrap: () => bootstrap(),
  store_subscribe: () => snapshot(3, { sessions: [session(1, { pending: [pending('r1')] })] }),
  app_mark: () => ({}),
  app_view_state: () => ({}),
  app_input_activity: () => ({}),
  ui_state_set: () => ({}),
};

function GuardProbe({ out }: { out: { guarded: boolean } }) {
  const g = useAnswerGuard('card-1');
  out.guarded = g.guarded;
  return null;
}

domTest('bridge: bootstrap, world, view state and the boot marks', async () => {
  try {
    globalThis.sessionStorage?.clear();
  } catch {
    // no storage in this environment
  }
  await withFakeCore(HANDLERS, async (core) => {
    const store = createAppStore();
    const stop = startEventBridge(store, { buildId: 'build-1' });
    await act(async () => {
      await flush(20);
      await new Promise((r) => setTimeout(r, 20));
    });
    assertEqual(store.getState().ui.locale, 'en', 'locale from bootstrap');
    assertEqual(store.getState().ui.theme, 'light', 'theme from bootstrap');
    assert(store.getState().ui.hydrated, 'hydrated');
    assertEqual(store.getState().world.status, 'live', 'world subscribed');
    assertEqual(store.getState().world.sessions.get(uid(1))?.pending.length, 1, 'pending card mirrored');
    assert(core.calls('app_view_state').length >= 1, 'view state reported at start');
    const marks = core.calls('app_mark').map((c) => (c.req as AppMarkReq).mark).sort();
    assertDeepEqual(marks, ['firstPaint', 'interactive'], 'both boot marks');
    stop();
    assertEqual(store.getState().world.status, 'idle', 'stopped');
  });
});

domTest('bridge: focus-request opens the target and starts the 700 ms guard (F-30)', async () => {
  const clk = manualClock(10_000);
  setClockForTests(clk);
  resetAnswerGuardForTests();
  try {
    await withFakeCore(HANDLERS, async (core) => {
      const store = createAppStore();
      const stop = startEventBridge(store, { buildId: null });
      const probe = { guarded: true };
      render(<GuardProbe out={probe} />);
      await act(async () => flush(20));
      clk.advance(1000);
      act(() => refreshGuards());
      assertEqual(probe.guarded, false, 'the guard ran out');

      act(() => emitLocalEvent('focusRequest', { target: { kind: 'session', uid: uid(1), focusCard: true } }));
      assertDeepEqual(store.getState().ui.view, { screen: 'session', uid: uid(1), sub: 'conversation' }, 'session opened');
      assertEqual(store.getState().ui.focusCard?.uid, uid(1), 'its card asked to take focus');
      assertEqual(probe.guarded, true, 'a focus-request starts the guard');
      clk.advance(701);
      act(() => refreshGuards());
      assertEqual(probe.guarded, false, 'and it ends after 700 ms');

      act(() => emitLocalEvent('visibility', { frontmost: false, occluded: true, asleep: false }));
      act(() => emitLocalEvent('visibility', { frontmost: true, occluded: false, asleep: false }));
      assertEqual(probe.guarded, true, 'window activation starts the guard');
      assertEqual(store.getState().ui.visibility.frontmost, true, 'visibility kept');

      act(() => emitLocalEvent('focusRequest', { target: { kind: 'team', teamKey: 'tk' as never, mode: 'board' } }));
      assertDeepEqual(store.getState().ui.view, { screen: 'team', teamKey: 'tk', mode: 'board' }, 'team target');
      act(() => emitLocalEvent('toast', { key: 'common.copy', params: {}, undoToken: null }));
      assertEqual(store.getState().toasts.length, 1, 'toast event');
      act(() => emitLocalEvent('quitRequested', {} as never));
      assertEqual(store.getState().ui.quitRequested, true, 'quit requested');
      const views = core.calls('app_view_state').map((c) => (c.req as ViewState).screen);
      assert(views.includes('session') && views.includes('team'), 'each screen change was reported');
      stop();
    });
  } finally {
    setClockForTests(null);
    resetAnswerGuardForTests();
  }
});

domTest('bridge: typing reports input activity at most once per 2 s, without content', async () => {
  const clk = manualClock(50_000);
  setClockForTests(clk);
  try {
    await withFakeCore(HANDLERS, async (core) => {
      const store = createAppStore();
      const stop = startEventBridge(store, { buildId: null });
      await act(async () => flush(20));
      const field = document.createElement('textarea');
      field.setAttribute('aria-label', 'composer');
      document.body.append(field);
      const type = () => field.dispatchEvent(new Event('input', { bubbles: true }));
      type();
      type();
      clk.advance(1500);
      type();
      assertEqual(core.calls('app_input_activity').length, 1, 'throttled');
      clk.advance(600);
      type();
      assertEqual(core.calls('app_input_activity').length, 2, 'again after 2 s');
      assertDeepEqual(core.calls('app_input_activity')[0]?.req, {}, 'no content');
      const button = document.createElement('button');
      document.body.append(button);
      clk.advance(5000);
      button.dispatchEvent(new Event('input', { bubbles: true }));
      assertEqual(core.calls('app_input_activity').length, 2, 'only text entries count');
      stop();
    });
  } finally {
    setClockForTests(null);
  }
});

domTest('bridge: a build-id mismatch reloads once, then shows the restart line (§4.7, A8.1)', async () => {
  try {
    globalThis.sessionStorage?.clear();
  } catch {
    // no storage
  }
  await withFakeCore({ ...HANDLERS, app_bootstrap: () => bootstrap('core-build') }, async () => {
    let reloads = 0;
    const first = createAppStore();
    const stop1 = startEventBridge(first, { buildId: 'webview-build', checkBuild: true, reload: () => (reloads += 1) });
    await act(async () => flush(20));
    assertEqual(reloads, 1, 'first mismatch reloads');
    assertEqual(first.getState().ui.buildMismatch, false, 'no line yet');
    stop1();
    const second = createAppStore();
    const stop2 = startEventBridge(second, { buildId: 'webview-build', checkBuild: true, reload: () => (reloads += 1) });
    await act(async () => flush(20));
    assertEqual(reloads, 1, 'no second reload');
    assertEqual(second.getState().ui.buildMismatch, true, 'the restart line shows');
    stop2();
    const third = createAppStore();
    const stop3 = startEventBridge(third, { buildId: 'core-build', checkBuild: true, reload: () => (reloads += 1) });
    await act(async () => flush(20));
    assertEqual(third.getState().ui.buildMismatch, false, 'a matching build clears it');
    stop3();
  });
});

domTest('bridge: the embedded build id is the A8.1 value and is checked against the core on boot', async () => {
  const expected = resolveBuildId(process.env.GILDE_BUILD_ID, pkg.version);
  assertEqual(embeddedBuildId(), expected, 'vite.config.ts defined the id the rule gives');
  try {
    globalThis.sessionStorage?.clear();
  } catch {
    // no storage
  }
  let reloads = 0;
  await withFakeCore({ ...HANDLERS, app_bootstrap: () => bootstrap(expected) }, async () => {
    const store = createAppStore();
    const stop = startEventBridge(store, { checkBuild: true, reload: () => (reloads += 1) });
    await act(async () => flush(20));
    assertEqual(reloads, 0, 'the same build: no reload');
    assertEqual(store.getState().ui.buildMismatch, false, 'no restart line');
    stop();
  });
  await withFakeCore({ ...HANDLERS, app_bootstrap: () => bootstrap(`${expected}.other`) }, async () => {
    const store = createAppStore();
    const stop = startEventBridge(store, { checkBuild: true, reload: () => (reloads += 1) });
    await act(async () => flush(20));
    assertEqual(reloads, 1, 'another core build: one reload');
    stop();
  });
  try {
    globalThis.sessionStorage?.clear();
  } catch {
    // no storage
  }
});

domTest('bridge: the build-id check is skipped only in showcase mode (no core)', async () => {
  try {
    globalThis.sessionStorage?.clear();
  } catch {
    // no storage
  }
  await withFakeCore({ ...HANDLERS, app_bootstrap: () => bootstrap('showcase-core') }, async () => {
    let reloads = 0;
    const store = createAppStore();
    const stop = startEventBridge(store, { buildId: 'webview-build', reload: () => (reloads += 1) });
    await act(async () => flush(20));
    assertEqual(reloads, 0, 'showcase mode: no reload');
    assertEqual(store.getState().ui.buildMismatch, false, 'and no restart line');
    stop();
  });
});
