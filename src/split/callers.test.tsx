/**
 * A30.5: every add-to-Split caller asks on a full Split — the Session screen
 * (⌘D and its button), the palette's "Add to Split" (no longer disabled when
 * six tiles are open) and the Floor's ⌘D all reach `addToSplitOrAsk`, so the
 * 7th-tile choice appears in the Split instead of the add being dropped.
 */

import { act, cleanup, fireEvent, render } from '@testing-library/react';

import { floorEngine } from '../floor/engine';
import { FloorScreen } from '../floor';
import { showcaseSnapshot } from '../floor/testing';
import { resetHotkeysForTests } from '../hooks/useHotkeys';
import { translate } from '../i18n';
import type { SessionUid } from '../lib/gen/SessionUid';
import { buildEntries } from '../palette/entries';
import { runPaletteAction } from '../palette/Palette';
import { SessionScreen } from '../session/SessionScreen';
import { appStore, useAppStore } from '../store/useAppStore';
import { initialUiState } from '../store/uiSlice';
import { emptyWorld } from '../store/worldSlice';
import { flush, withFakeCore } from '../test/backend';
import { session, snapshot, uid } from '../test/fixtures';
import { assert, assertEqual, domTest, sleep, test } from '../test/harness';
import { currentPendingAdd, resetReplaceForTests } from './replace';

const SIX: SessionUid[] = [1, 2, 3, 4, 5, 6].map((n) => uid(n));

function reset() {
  resetHotkeysForTests();
  resetReplaceForTests();
  appStore.setState({ world: emptyWorld(), ui: { ...initialUiState(), locale: 'en', hydrated: true } });
}

function fillSplit() {
  const ui = appStore.getState().ui;
  appStore.setState({ ui: { ...ui, split: { uids: [...SIX], focused: SIX[0] as SessionUid, zoomed: null } } });
}

async function settle(ms = 30) {
  await act(async () => {
    await flush(10);
    await sleep(ms);
  });
}

const handlers = {
  convo_subscribe: () => ({ subId: 'v_00000000000000c1', source: 'stream', note: null }),
  convo_unsubscribe: () => ({}),
  changes_get: () => ({ location: 'l', base: null, files: [], commits: [], sharedFolder: null, actions: { commit: { kind: 'hidden' }, merge: { kind: 'hidden' }, discard: { kind: 'hidden' } } }),
  session_mark_seen: () => ({}),
};

domTest('A30.5 Session screen: ⌘D and "Add to split" on a full Split ask which tile to replace', async () => {
  reset();
  try {
    await withFakeCore(handlers, async () => {
      try {
        act(() => appStore.getState().applyStoreSnapshot(snapshot(1, { sessions: [1, 2, 3, 4, 5, 6, 7].map((n) => session(n)) })));
        act(() => appStore.getState().openSession(uid(7)));
        fillSplit();
        const view = render(<SessionScreen uid={uid(7)} sub="conversation" />);
        await settle();
        fireEvent.click(view.getByText(translate('en', 'session.addToSplit')));
        assertEqual(currentPendingAdd(), uid(7), 'the button asks');
        assertEqual(appStore.getState().ui.view.screen, 'split', 'and opens the Split to choose');
        assertEqual(appStore.getState().ui.split.uids.length, 6, 'nothing replaced yet');
        resetReplaceForTests();
        act(() => appStore.getState().openSession(uid(7)));
        fireEvent.keyDown(document.body, { key: 'd', code: 'KeyD', metaKey: true });
        assertEqual(currentPendingAdd(), uid(7), '⌘D asks too');
      } finally {
        cleanup();
      }
    });
  } finally {
    reset();
  }
});

test('A30.5 palette: "Add to Split" stays enabled on a full Split and its run asks', () => {
  reset();
  try {
    appStore.getState().applyStoreSnapshot(snapshot(2, { sessions: [1, 2, 3, 4, 5, 6, 7].map((n) => session(n)) }));
    appStore.getState().openSession(uid(7));
    fillSplit();
    const s = appStore.getState();
    const entry = buildEntries({ world: s.world, ui: s.ui, t: (k, p) => translate('en', k, p), locale: 'en' }).find((e) => e.id === 'addToSplit');
    assert(entry !== undefined, 'the command is listed');
    assertEqual(entry?.disabledReason ?? null, null, 'not disabled: the Split asks instead');
    runPaletteAction({ kind: 'run', id: 'addToSplit' });
    assertEqual(currentPendingAdd(), uid(7), 'asks which tile to replace');
    assertEqual(appStore.getState().ui.view.screen, 'split', 'the Split shows the choice');
  } finally {
    reset();
  }
});

domTest('A30.5 Floor: ⌘D on a character with a full Split asks which tile to replace', async () => {
  reset();
  floorEngine.resetForTests();
  try {
    const s = useAppStore.getState();
    s.navigate({ screen: 'floor' });
    s.applyStoreSnapshot(showcaseSnapshot(8, [], 0, 0));
    fillSplit();
    const view = render(<FloorScreen />);
    await settle();
    const seat = view.container.querySelector<HTMLElement>('[role="treeitem"][data-flsel]');
    assert(seat, 'a session item in the twin');
    act(() => seat.focus());
    act(() => {
      fireEvent.keyDown(seat, { key: 'd', code: 'KeyD', metaKey: true });
    });
    assertEqual(currentPendingAdd(), seat.getAttribute('data-flsel') as SessionUid, 'the selected session waits for a tile');
    assertEqual(useAppStore.getState().ui.view.screen, 'split', 'the Split shows the choice');
  } finally {
    cleanup();
    floorEngine.resetForTests();
    reset();
  }
});
