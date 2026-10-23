/**
 * The UI slice: main-view history, Split, zoom, the ⌘J chain, bootstrap
 * hydration of untrusted persisted values.
 */

import type { AppBootstrapRes } from '../lib/gen/AppBootstrapRes';
import { queueItem, uid } from '../test/fixtures';
import { assert, assertDeepEqual, assertEqual, test } from '../test/harness';
import { MAX_SPLIT, MAX_VIEW_HISTORY, chainStep, clampZoom, parseSplit, parseTeamLayout, parseView } from './uiSlice';
import { createAppStore } from './useAppStore';

function bootstrap(extra: Partial<AppBootstrapRes> = {}): AppBootstrapRes {
  return {
    buildId: 'b1',
    version: '2.0.0',
    locale: 'en',
    theme: 'dark',
    launch: { kind: 'restore' },
    uiState: {},
    platform: { macosMajor: 15, arm64: true },
    features: { orchestrator: true },
    compat: [],
    ...extra,
  };
}

test('⌘[ / ⌘] walk the main-view history; a new view clears forward', () => {
  const store = createAppStore();
  const s = () => store.getState();
  s().navigate({ screen: 'history' });
  s().openSession(uid(1));
  s().setSessionSub('changes');
  assertDeepEqual(s().ui.view, { screen: 'session', uid: uid(1), sub: 'changes' }, 'sub-view switch is not a history step');
  s().goBack();
  assertEqual(s().ui.view.screen, 'history', 'back');
  s().goBack();
  assertEqual(s().ui.view.screen, 'home', 'back to home');
  s().goBack();
  assertEqual(s().ui.view.screen, 'home', 'nothing further back');
  s().goForward();
  assertEqual(s().ui.view.screen, 'history', 'forward');
  s().navigate({ screen: 'floor' });
  assertEqual(s().ui.forward.length, 0, 'a new view clears forward');
  s().navigate({ screen: 'floor' });
  assertEqual(s().ui.back[s().ui.back.length - 1]?.screen, 'history', 'the same view twice is one entry');
  for (let i = 0; i < MAX_VIEW_HISTORY + 20; i++) s().navigate(i % 2 ? { screen: 'home' } : { screen: 'history' });
  assertEqual(s().ui.back.length, MAX_VIEW_HISTORY, 'history is bounded');
});

test('Split holds at most 6; closing a focused tile focuses the previous one', () => {
  const store = createAppStore();
  const s = () => store.getState();
  for (let i = 1; i <= MAX_SPLIT; i++) assert(s().addToSplit(uid(i)), `tile ${i} added`);
  assertEqual(s().addToSplit(uid(7)), false, 'the 7th is refused');
  assertEqual(s().addToSplit(uid(3)), true, 'adding a present uid is a no-op success');
  assertEqual(s().ui.split.uids.length, MAX_SPLIT, 'still 6');
  s().setSplitFocus(uid(4));
  s().toggleSplitZoom(uid(4));
  s().removeFromSplit(uid(4));
  assertEqual(s().ui.split.focused, uid(3), 'focus moved to the previous tile in reading order');
  assertEqual(s().ui.split.zoomed, null, 'zoom cleared with its tile');
  s().setSplitFocus(uid(9));
  assertEqual(s().ui.split.focused, uid(3), 'a uid outside the Split cannot take focus');
});

test('zoom stays within 80–200 %', () => {
  assertEqual(clampZoom(0.1), 0.8, 'min');
  assertEqual(clampZoom(5), 2, 'max');
  assertEqual(clampZoom(Number.NaN), 1, 'NaN resets');
  const store = createAppStore();
  store.getState().zoomBy(3);
  assertEqual(store.getState().ui.zoom, 1.3, 'three steps up');
  store.getState().zoomBy(-20);
  assertEqual(store.getState().ui.zoom, 0.8, 'clamped');
});

test('⌘J chain: next item, the handled item\'s slot, and back to the origin at the end', () => {
  const items = [queueItem(1), queueItem(2), queueItem(3)];
  const store = createAppStore();
  const s = () => store.getState();
  s().navigate({ screen: 'history' });
  assertEqual(s().chainStep(items, 1)?.key, items[0]?.key, 'first ⌘J goes to the top item');
  assertDeepEqual(s().ui.view, { screen: 'session', uid: uid(1), sub: 'conversation' }, 'opens its session');
  assertEqual(s().ui.focusCard?.uid, uid(1), 'asks its card to take focus');
  assertEqual(s().chainStep(items, 1)?.key, items[1]?.key, 'next');
  // Item 2 was handled; the queue moved up. Next is the item now in its slot.
  const after = [queueItem(1), queueItem(3)];
  assertEqual(s().chainStep(after, 1)?.key, items[2]?.key, 'the item that took the handled slot');
  assertEqual(s().chainStep(after, 1), null, 'past the end');
  assertEqual(s().ui.view.screen, 'history', 'back where the chain started');
  assertEqual(s().ui.chain, null, 'chain ended');
  assertEqual(s().chainStep([], 1), null, 'an empty queue does nothing');
});

test('⌘J chain: leftovers open Accounts & health; answerIn routes to the lead', () => {
  const lo = queueItem(9, 6, { key: 'q_leftovers', subject: { kind: 'leftovers', count: 2, bytes: 10 } });
  const member = queueItem(5, 1, { subject: { kind: 'session', uid: uid(5), onBehalfOf: null, answerIn: uid(6) } });
  const r1 = chainStep([member, lo], null, 1);
  assertEqual(r1?.item.key, member.key, 'pure step');
  const store = createAppStore();
  store.getState().chainStep([member, lo], 1);
  assertDeepEqual(store.getState().ui.view, { screen: 'session', uid: uid(6), sub: 'conversation' }, 'the lead that shows the card');
  store.getState().chainStep([member, lo], 1);
  assertDeepEqual(store.getState().ui.view, { screen: 'accounts', section: 'health' }, 'leftovers');
  store.getState().chainStep([member, lo], -1);
  assertEqual(store.getState().ui.view.screen, 'session', 'previous');
  store.getState().navigate({ screen: 'floor' });
  assertEqual(store.getState().ui.chain, null, 'any other navigation ends the chain');
});

test('bootstrap hydrates locale, theme and validated persisted values', () => {
  const store = createAppStore();
  store.getState().hydrateFromBootstrap(
    bootstrap({
      uiState: {
        lastView: { screen: 'team', teamKey: 'team-1', mode: 'board' },
        splitSet: { uids: [uid(1), uid(1), 'not-a-uid', uid(2), 7], focused: uid(2) },
        homeMode: 'classic',
        'teamLayout:team-1': { mode: 'floor', focused: 'm-1', zoomed: 42 },
        sidebarFolds: { a: true },
      },
    }),
  );
  const ui = store.getState().ui;
  assertEqual(ui.locale, 'en', 'locale');
  assertEqual(ui.theme, 'dark', 'theme');
  assert(ui.hydrated, 'hydrated');
  assertDeepEqual(ui.view, { screen: 'team', teamKey: 'team-1', mode: 'board' }, 'last view restored on a restore launch');
  assertDeepEqual(ui.split.uids, [uid(1), uid(2)], 'malformed and duplicate uids dropped');
  assertEqual(ui.split.focused, uid(2), 'focus kept');
  assertEqual(ui.homeMode, 'classic', 'home mode');
  assertDeepEqual(ui.teamLayouts['team-1'], { mode: 'floor', focused: 'm-1', zoomed: null, leadRatio: null, tileOrder: null }, 'team layout validated');
  assertDeepEqual(ui.sidebarFolds, { a: true }, 'opaque value kept');

  const other = createAppStore();
  other.getState().hydrateFromBootstrap(bootstrap({ launch: { kind: 'homeAfter8h' }, uiState: { lastView: { screen: 'history' } } }));
  assertEqual(other.getState().ui.view.screen, 'home', 'other launch kinds open Home');
});

test('parsers reject malformed persisted values', () => {
  assertDeepEqual(parseView(null), { screen: 'home' }, 'null');
  assertDeepEqual(parseView({ screen: 'session', uid: '../etc' }), { screen: 'home' }, 'bad uid');
  assertDeepEqual(parseView({ screen: 'session', uid: uid(1), sub: 'bogus' }), { screen: 'session', uid: uid(1), sub: 'conversation' }, 'bad sub');
  assertDeepEqual(parseView({ screen: 'accounts', section: 'x' }), { screen: 'accounts', section: 'accounts' }, 'bad section');
  assertDeepEqual(parseView({ screen: 'nope' }), { screen: 'home' }, 'unknown screen');
  assertEqual(parseSplit(Array.from({ length: 10 }, (_, i) => uid(i + 1))).uids.length, MAX_SPLIT, 'capped at 6');
  assertDeepEqual(parseSplit('x'), { uids: [], focused: null, zoomed: null }, 'not a list');
  assertDeepEqual(parseTeamLayout([]), { mode: 'panes', focused: null, zoomed: null, leadRatio: null, tileOrder: null }, 'not an object');
});

test('drafts are per session and clear', () => {
  const store = createAppStore();
  store.getState().setDraft(uid(1), 'a');
  store.getState().setDraft(uid(2), 'b');
  store.getState().clearDraft(uid(1));
  assertDeepEqual(store.getState().ui.drafts, { [uid(2)]: 'b' }, 'one left');
});
