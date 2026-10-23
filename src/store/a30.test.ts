/**
 * A30.2–A30.4 (one-time grant to T16b in T13's store): the published visible
 * set replaces viewStateOf's derivation while it is set; TeamLayout's
 * leadRatio and tileOrder are validated and persisted; setTeamMode switches
 * team modes without a history entry.
 */

import type { MemberKey } from '../lib/gen/MemberKey';
import type { SessionUid } from '../lib/gen/SessionUid';
import type { TeamKey } from '../lib/gen/TeamKey';
import { session, snapshot, uid } from '../test/fixtures';
import { assert, assertDeepEqual, assertEqual, test } from '../test/harness';
import { persistedValues } from './persist';
import { createAppStore } from './useAppStore';
import { defaultTeamLayout, initialUiState, parseLeadRatio, parseTeamLayout, parseTileOrder, remapUi } from './uiSlice';
import { viewStateOf } from './viewState';
import { emptyWorld, worldFromSnapshot } from './worldSlice';

const TK = 't_00000000000000b1' as TeamKey;
const m = (n: number) => `m_${n.toString(16).padStart(16, '0')}` as MemberKey;

test('A30.3 parseTeamLayout: leadRatio 0.2–0.8 and a well-formed tileOrder survive; anything else becomes null', () => {
  const ok = parseTeamLayout({ mode: 'board', focused: m(2), zoomed: null, leadRatio: 0.55, tileOrder: [m(3), m(2)] });
  assertDeepEqual(ok, { mode: 'board', focused: m(2), zoomed: null, leadRatio: 0.55, tileOrder: [m(3), m(2)] }, 'kept');
  for (const bad of [0.1, 0.81, Number.NaN, Number.POSITIVE_INFINITY, '0.5', null, true, {}]) {
    assertEqual(parseLeadRatio(bad), null, `leadRatio ${String(bad)} → null`);
  }
  assertEqual(parseLeadRatio(0.2), 0.2, 'lower bound');
  assertEqual(parseLeadRatio(0.8), 0.8, 'upper bound');
  const tooMany = Array.from({ length: 65 }, (_, i) => m(i + 1));
  for (const bad of ['m_1', [m(1), 'x'], [m(1), m(1)], [m(1), 7], tooMany, { 0: m(1) }, ['M_00000000000000AA']]) {
    assertEqual(parseTileOrder(bad), null, `tileOrder ${JSON.stringify(bad).slice(0, 40)} → null (dropped whole)`);
  }
  assertEqual(parseTileOrder(tooMany.slice(0, 64))?.length, 64, '64 keys are allowed');
  const partial = parseTeamLayout({ mode: 'panes', leadRatio: 2, tileOrder: 'nope' });
  assertDeepEqual([partial.leadRatio, partial.tileOrder, partial.mode], [null, null, 'panes'], 'bad fields only are dropped');
  assertDeepEqual(parseTeamLayout(undefined), defaultTeamLayout(), 'garbage → defaults');
});

test('A30.3 leadRatio and tileOrder persist with the layout and hydrate back', () => {
  const store = createAppStore();
  store.getState().setTeamLayout(TK, { leadRatio: 0.3, tileOrder: [m(4), m(2)] });
  const saved = persistedValues(store.getState().ui).get(`teamLayout:${TK}`);
  assertDeepEqual(saved, { mode: 'panes', focused: null, zoomed: null, leadRatio: 0.3, tileOrder: [m(4), m(2)] }, 'persisted value');
  const fresh = createAppStore();
  fresh.getState().hydrateFromBootstrap({
    buildId: 'b',
    version: '0',
    locale: 'en',
    theme: 'system',
    launch: { kind: 'restore' },
    uiState: { [`teamLayout:${TK}`]: saved as never },
    platform: { macosMajor: 15, arm64: true },
    features: { orchestrator: false },
    compat: [],
  } as never);
  const back = fresh.getState().ui.teamLayouts[TK];
  assertEqual(back?.leadRatio, 0.3, 'ratio survives a reload');
  assertDeepEqual(back?.tileOrder, [m(4), m(2)], 'order survives a reload');
});

test('A30.4 setTeamMode: replaces the team view (no history entry) and keeps the mode in the layout', () => {
  const store = createAppStore();
  const s = () => store.getState();
  s().navigate({ screen: 'home' });
  s().navigate({ screen: 'team', teamKey: TK, mode: 'panes' });
  const backBefore = s().ui.back.length;
  s().setTeamMode(TK, 'board');
  assertDeepEqual(s().ui.view, { screen: 'team', teamKey: TK, mode: 'board' }, 'view replaced');
  assertEqual(s().ui.back.length, backBefore, 'no history entry');
  assertEqual(s().ui.forward.length, 0, 'no forward entry');
  assertEqual(s().ui.teamLayouts[TK]?.mode, 'board', 'kept in the layout');
  s().goBack();
  assertEqual(s().ui.view.screen, 'home', '⌘[ leaves the team, not the previous mode');
  s().setTeamMode(TK, 'floor');
  assertEqual(s().ui.view.screen, 'home', 'on another screen only the layout changes');
  assertEqual(s().ui.teamLayouts[TK]?.mode, 'floor', 'layout updated');
  const before = s().ui;
  s().setTeamMode(TK, 'floor');
  assertEqual(s().ui, before, 'same mode: no change');
});

test('A30.2 visibleUids: viewStateOf reports the published set while it is set, else its own derivation', () => {
  const world = worldFromSnapshot(snapshot(1, { sessions: [session(1), session(2), session(3)] }), emptyWorld()).world;
  const split = { ...initialUiState(), view: { screen: 'split' as const }, split: { uids: [uid(1), uid(2), uid(3)], focused: uid(1), zoomed: null } };
  assertDeepEqual(viewStateOf(split, world).visibleUids, [uid(1), uid(2), uid(3)], 'derivation: the whole set');
  const published = { ...split, visibleUids: [uid(1)] };
  const v = viewStateOf(published, world);
  assertDeepEqual(v.visibleUids, [uid(1)], 'only what the screen renders');
  assertEqual(v.focusedUid, uid(1), 'focus still derived');
  assertDeepEqual(viewStateOf({ ...split, visibleUids: [] }, world).visibleUids, [], 'an empty set means nothing visible');
});

test('A30.2 setVisibleUids: dedupes, no-ops on equal sets, clears with null, remaps tombstones', () => {
  const store = createAppStore();
  const s = () => store.getState();
  s().setVisibleUids([uid(1), uid(2), uid(1)]);
  assertDeepEqual(s().ui.visibleUids, [uid(1), uid(2)], 'deduped');
  const before = s().ui;
  s().setVisibleUids([uid(1), uid(2)]);
  assertEqual(s().ui, before, 'equal set: no store change (no report churn)');
  s().setVisibleUids(null);
  assertEqual(s().ui.visibleUids, null, 'cleared');
  const remapped = remapUi({ ...initialUiState(), visibleUids: [uid(5), uid(6)] }, new Map<SessionUid, SessionUid>([[uid(6), uid(5)]]));
  assertDeepEqual(remapped.visibleUids, [uid(5)], 'merged uid remapped and deduped');
  assert(remapUi(initialUiState(), new Map([[uid(1), uid(2)]])).visibleUids === null, 'null stays null');
});
