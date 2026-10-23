/**
 * The two reporters to the core: persisted UI keys (`ui_state_set`) and the
 * view state (`app_view_state`, F-04); plus toasts.
 */

import type { UiStateSetReq } from '../lib/gen/UiStateSetReq';
import type { ViewState } from '../lib/gen/ViewState';
import { flush, withFakeCore } from '../test/backend';
import { session, snapshot, uid } from '../test/fixtures';
import { assert, assertDeepEqual, assertEqual, sleep, test } from '../test/harness';
import type { Member } from '../lib/gen/Member';
import type { JsonValue } from '../lib/gen/serde_json/JsonValue';
import type { MemberKey } from '../lib/gen/MemberKey';
import type { TeamKey } from '../lib/gen/TeamKey';
import { PERSIST_DEBOUNCE_MS, fitsUiCaps, jsonDepth, startUiPersistence } from './persist';
import { MAX_TOASTS, UNDO_WINDOW_MS } from './toastSlice';
import { createAppStore } from './useAppStore';
import { MAX_VISIBLE_UIDS, startViewStateReporter, viewStateOf } from './viewState';

function member(key: string, teamKey: string, n: number): Member {
  return {
    key: key as MemberKey,
    teamKey: teamKey as TeamKey,
    sessionUid: uid(n),
    name: key,
    agentId: null,
    role: 'member',
    colour: null,
    vendor: 'claude',
    backend: 'pane',
    joinedAt: null,
    presence: { kind: 'present' },
    pane: null,
    planModeRequired: false,
    currentTask: null,
  };
}

test('ui_state_set: debounced per key, only real changes, caps respected, hydration not echoed', async () => {
  await withFakeCore({ ui_state_set: () => ({}) }, async (core) => {
    const store = createAppStore();
    store.getState().hydrateFromBootstrap({
      buildId: 'b',
      version: 'v',
      locale: 'ko',
      theme: 'system',
      launch: { kind: 'restore' },
      uiState: { homeMode: 'classic' },
      platform: { macosMajor: 15, arm64: true },
      features: { orchestrator: false },
      compat: [],
    });
    const stop = startUiPersistence((l) => store.subscribe((s) => s.ui, l), store.getState().ui);
    store.getState().setHomeMode('focus');
    store.getState().setHomeMode('classic');
    store.getState().setHomeMode('focus');
    store.getState().addToSplit(uid(1));
    let deep: JsonValue = 'x';
    for (let i = 0; i < 20; i++) deep = [deep];
    store.getState().setFloorPrefs({ deep });
    assertEqual(core.calls('ui_state_set').length, 0, 'nothing before the debounce');
    await sleep(PERSIST_DEBOUNCE_MS + 50);
    await flush();
    const sent = core.calls('ui_state_set').map((c) => c.req as UiStateSetReq);
    const keys = sent.map((r) => r.key).sort();
    assertDeepEqual(keys, ['homeMode', 'splitSet'], 'one write per changed key; the over-deep value is dropped');
    assertEqual(sent.find((r) => r.key === 'homeMode')?.value, 'focus', 'the latest value');
    stop();
  });
  assertEqual(jsonDepth({ a: [1, { b: 2 }] }), 3, 'depth');
  assert(!fitsUiCaps('x'.repeat(70 * 1024)), '64 KiB cap');
});

test('app_view_state: the open session, Split tiles, team panes; sent on change only', async () => {
  const store = createAppStore();
  const team = 'team-a' as TeamKey;
  store.getState().applyStoreSnapshot(
    snapshot(1, {
      sessions: [session(1), session(2), session(3)],
      members: [member('lead', team, 1), member('m2', team, 2)],
    }),
  );
  const s = () => store.getState();
  s().openSession(uid(3));
  assertDeepEqual(
    viewStateOf(s().ui, s().world),
    { screen: 'session', visibleUids: [uid(3)], focusedUid: uid(3), zoomedUid: null, teamKey: null, documentVisible: true },
    'session screen',
  );
  s().addToSplit(uid(1));
  s().addToSplit(uid(2));
  s().setSplitFocus(uid(2));
  s().navigate({ screen: 'split' });
  let v: ViewState = viewStateOf(s().ui, s().world);
  assertDeepEqual(v.visibleUids, [uid(1), uid(2)], 'every Split tile');
  assertEqual(v.focusedUid, uid(2), 'the focused tile');
  s().toggleSplitZoom(uid(1));
  v = viewStateOf(s().ui, s().world);
  assertDeepEqual(v.visibleUids, [uid(1)], 'only the zoomed tile is visible');
  assertEqual(v.zoomedUid, uid(1), 'zoomed');
  s().navigate({ screen: 'team', teamKey: team, mode: 'panes' });
  s().setTeamLayout(team, { focused: 'm2' as MemberKey });
  v = viewStateOf(s().ui, s().world);
  assertDeepEqual(v.visibleUids, [uid(1), uid(2)], 'team panes');
  assertEqual(v.focusedUid, uid(2), 'focused member');
  assertEqual(v.teamKey, team, 'team key');
  s().navigate({ screen: 'team', teamKey: team, mode: 'board' });
  assertDeepEqual(viewStateOf(s().ui, s().world).visibleUids, [], 'Board shows no session tiles');

  await withFakeCore({ app_view_state: () => ({}) }, async (core) => {
    const stop = startViewStateReporter(
      (l) => store.subscribe((st) => [st.ui, st.world] as const, ([ui, w]) => l(ui, w), { equalityFn: (a, b) => a[0] === b[0] && a[1] === b[1] }),
      { ui: s().ui, world: s().world },
    );
    assertEqual(core.calls('app_view_state').length, 1, 'sent once at start (covers a reload)');
    s().setZoom(1.5);
    assertEqual(core.calls('app_view_state').length, 1, 'an unrelated change sends nothing');
    s().setDocumentVisible(false);
    assertEqual(core.calls('app_view_state').length, 2, 'visibilitychange is sent');
    assertEqual((core.calls('app_view_state')[1]?.req as ViewState).documentVisible, false, 'hidden');
    s().navigate({ screen: 'home' });
    assertEqual(core.calls('app_view_state').length, 3, 'a screen change is sent');
    stop();
  });
  assertEqual(MAX_VISIBLE_UIDS, 32, 'the manifest cap');
});

test('toasts: at most 3, undo calls undo_cancel once, errors keep their detail, expiry', async () => {
  await withFakeCore({ undo_cancel: () => ({ cancelled: true }) }, async (core) => {
    const store = createAppStore();
    const s = () => store.getState();
    for (let i = 0; i < 5; i++) s().pushToastEvent({ key: 'common.copy', params: { i }, undoToken: null });
    assertEqual(s().toasts.length, MAX_TOASTS, 'capped');
    assertEqual(s().toasts[0]?.params.i, 2, 'the oldest went first');
    const id = s().pushToastEvent({ key: 'common.undo', params: {}, undoToken: 'u_00000000000000000000000000000001' as never });
    assertEqual(s().toasts.find((t) => t.id === id)?.ttlMs, UNDO_WINDOW_MS, 'undo window');
    assertEqual(await s().undoToast(id), true, 'cancelled');
    assertEqual(await s().undoToast(id), false, 'a second undo does nothing');
    assertEqual(core.calls('undo_cancel').length, 1, 'one call');
    const err = s().pushErrorToast({ code: 'GIT_FAILED', key: 'error.gitFailed', params: {}, detail: 'fatal: x', retryable: false });
    assertEqual(s().toasts.find((t) => t.id === err)?.detail, 'fatal: x', 'detail kept for the mono line');
    s().expireToasts(Number.MAX_SAFE_INTEGER);
    assertEqual(s().toasts.length, 0, 'all expired');
  });
});
