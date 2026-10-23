/**
 * The `app_view_state` report (contract §4.2.1, F-04): what is on screen, sent
 * on every change of screen, visible tiles, focus or zoom, on
 * `visibilitychange`, and after a reload. Rust combines it with its own
 * occlusion state for notification rule 2 and never-spam rule 2. On Home the
 * session of the focus card counts as visible (A26.1). Split and Team publish
 * the sessions they actually render (`ui.visibleUids`, A30.2), which replaces
 * the derivation below while it is set.
 */

import { commands } from '../lib/commands';
import type { Screen } from '../lib/gen/Screen';
import type { SessionUid } from '../lib/gen/SessionUid';
import type { ViewState } from '../lib/gen/ViewState';
import type { UiState } from './uiSlice';
import type { WorldState } from './worldSlice';
import { membersOfTeam, resolveUid } from './worldSlice';

/** `visibleUids` holds at most this many uids (manifest cap). */
export const MAX_VISIBLE_UIDS = 32;

const SCREEN: Readonly<Record<UiState['view']['screen'], Screen>> = {
  home: 'home',
  session: 'session',
  split: 'split',
  team: 'team',
  floor: 'floor',
  history: 'history',
  accounts: 'accounts',
  settings: 'settings',
};

/** The report for a UI state and World mirror. */
export function viewStateOf(ui: UiState, world: WorldState): ViewState {
  const view = ui.view;
  const visible: SessionUid[] = [];
  const add = (uid: SessionUid | null) => {
    if (!uid) return;
    const resolved = resolveUid(world, uid);
    if (!visible.includes(resolved) && visible.length < MAX_VISIBLE_UIDS) visible.push(resolved);
  };
  let focusedUid: SessionUid | null = null;
  let zoomedUid: SessionUid | null = null;
  let teamKey: ViewState['teamKey'] = null;

  switch (view.screen) {
    case 'session':
      add(view.uid);
      focusedUid = resolveUid(world, view.uid);
      break;
    case 'split':
      for (const uid of ui.split.zoomed ? [ui.split.zoomed] : ui.split.uids) add(uid);
      focusedUid = ui.split.focused ? resolveUid(world, ui.split.focused) : null;
      zoomedUid = ui.split.zoomed ? resolveUid(world, ui.split.zoomed) : null;
      break;
    case 'team': {
      teamKey = view.teamKey;
      const layout = ui.teamLayouts[view.teamKey];
      const members = membersOfTeam(world, view.teamKey);
      const sessionOfMember = (key: string | null) =>
        key ? (members.find((m) => m.key === key)?.sessionUid ?? null) : null;
      if (view.mode === 'panes') {
        const zoomed = sessionOfMember(layout?.zoomed ?? null);
        if (zoomed) add(zoomed);
        else for (const m of members) add(m.sessionUid);
        zoomedUid = zoomed ? resolveUid(world, zoomed) : null;
      }
      const focused = sessionOfMember(layout?.focused ?? null);
      focusedUid = focused ? resolveUid(world, focused) : null;
      break;
    }
    case 'home':
      // Home's focus card shows its session (A26.1).
      add(ui.homeFocusUid);
      break;
    default:
      break;
  }
  // A screen that shows several sessions says which ones it renders (A30.2).
  if (ui.visibleUids !== null) {
    visible.length = 0;
    for (const uid of ui.visibleUids) add(uid);
  }
  return {
    screen: SCREEN[view.screen],
    visibleUids: visible,
    focusedUid,
    zoomedUid,
    teamKey,
    documentVisible: ui.visibility.documentVisible,
  };
}

/**
 * Send the report whenever it changes (and once at start, which covers a
 * reload). Returns the stop function.
 */
export function startViewStateReporter(
  subscribe: (listener: (ui: UiState, world: WorldState) => void) => () => void,
  initial: { ui: UiState; world: WorldState },
): () => void {
  let last = '';
  const report = (ui: UiState, world: WorldState) => {
    const state = viewStateOf(ui, world);
    const text = JSON.stringify(state);
    if (text === last) return;
    last = text;
    void commands.app_view_state(state).catch(() => {
      // Dropped report (BUSY or not built yet): send again on the next change.
      if (last === text) last = '';
    });
  };
  report(initial.ui, initial.world);
  return subscribe(report);
}
