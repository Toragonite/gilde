/**
 * A26.1 / A26.2 (T14's one-time grants in the store):
 *  - Home's focus card counts as visible: `ui.homeFocusUid` is reported in
 *    `app_view_state.visibleUids` while the view is Home, so Rust sends no
 *    notification for the item on Home's focus card (rule 2, F-04);
 *  - sheets carry a context: `openSheet(id, { projectId })` → `ui.sheetContext`,
 *    cleared by `closeSheet()`.
 */

import type { ProjectId } from '../lib/gen/ProjectId';
import type { SessionUid } from '../lib/gen/SessionUid';
import { uid } from '../test/fixtures';
import { assertDeepEqual, assertEqual, test } from '../test/harness';
import { initialUiState, remapUi } from './uiSlice';
import { createAppStore } from './useAppStore';
import { viewStateOf } from './viewState';
import { emptyWorld } from './worldSlice';

test('A26.1: on Home the focus card session is visible; elsewhere it is not', () => {
  const world = emptyWorld();
  const home = { ...initialUiState(), homeFocusUid: uid(3) };
  const v = viewStateOf(home, world);
  assertEqual(v.screen, 'home', 'Home');
  assertDeepEqual(v.visibleUids, [uid(3)], 'the focus card session is visible');
  assertEqual(v.focusedUid, null, 'it is not a focused tile');
  assertDeepEqual(viewStateOf(initialUiState(), world).visibleUids, [], 'nothing without a focus card');
  const history = { ...home, view: { screen: 'history' as const } };
  assertDeepEqual(viewStateOf(history, world).visibleUids, [], 'not on another screen');
});

test('A26.1: setHomeFocusUid sets and clears; a tombstone remaps it to the survivor', () => {
  const store = createAppStore();
  store.getState().setHomeFocusUid(uid(1));
  assertEqual(store.getState().ui.homeFocusUid, uid(1), 'set');
  store.getState().setHomeFocusUid(null);
  assertEqual(store.getState().ui.homeFocusUid, null, 'cleared');
  const remapped = remapUi({ ...initialUiState(), homeFocusUid: uid(1) }, new Map<SessionUid, SessionUid>([[uid(1), uid(2)]]));
  assertEqual(remapped.homeFocusUid, uid(2), 'remapped to the survivor');
});

test('A26.2: openSheet carries a context; closeSheet clears it; no context is null', () => {
  const store = createAppStore();
  const projectId = 'p_checkout' as ProjectId;
  store.getState().openSheet('newSession', { projectId });
  assertEqual(store.getState().ui.sheet, 'newSession', 'opened');
  assertDeepEqual(store.getState().ui.sheetContext, { projectId }, 'with its project');
  store.getState().closeSheet();
  assertEqual(store.getState().ui.sheet, null, 'closed');
  assertEqual(store.getState().ui.sheetContext, null, 'context cleared');
  store.getState().openSheet('help');
  assertEqual(store.getState().ui.sheetContext, null, 'a sheet opened without context has none');
});
