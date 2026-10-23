/**
 * The global keys (scope G, ux-ia §7.2) the shell owns. Every binding goes
 * through T13's `useHotkeys` (capture phase, `event.code`, ⌘ only, never Ctrl,
 * nothing during IME composition, no auto-repeat unless it navigates); while
 * a sheet is open only the sheet's own keys fire.
 *
 * Each binding has a visible control too (ux-ia §7.6): the top bar (⌘K, ⌘J,
 * ⌘N, ⌘B), the sidebar nav rows (⌘0, ⇧⌘F, ⇧⌘A), the palette for the rest.
 */

import { useMemo } from 'react';

import { isScopeActive, useHotkeys } from '../hooks/useHotkeys';
import type { HotkeyBinding } from '../hooks/useHotkeys';
import { clock } from '../lib/clock';
import { appStore } from '../store/useAppStore';
import type { SheetContext } from '../store/uiSlice';
import { sessionOf, visibleQueueItems } from '../store/worldSlice';
import { buildTree, sessionOrder } from '../sidebar/tree';
import { cycleRegion } from './regions';

export interface ShellKeyOptions {
  /** ⌘B: hide/show the pinned sidebar, or open/close the overlay. */
  toggleSidebar: () => void;
  /** Esc while the overlay sidebar is open closes it. */
  overlayOpen: boolean;
  closeOverlay: () => void;
}

/** `⌘N` starts "in the current project" (ux-ia §7.2): the project of the session or team in front. */
export function currentProject(): SheetContext | undefined {
  const s = appStore.getState();
  const view = s.ui.view;
  const uid = view.screen === 'session' ? view.uid : view.screen === 'split' ? s.ui.split.focused : null;
  const projectId = uid ? sessionOf(s.world, uid)?.projectId : view.screen === 'team' ? s.world.teams.get(view.teamKey)?.projectId : null;
  return projectId ? { projectId } : undefined;
}

/** `⌘J` / `⇧⌘J`: the next / previous Needs-you item, remembering where the chain started. */
export function stepQueue(dir: 1 | -1): void {
  const s = appStore.getState();
  s.chainStep(visibleQueueItems(s.world, clock.wall()), dir);
}

/** `⌥⌘↑` / `⌥⌘↓`: the previous / next session in sidebar order. */
export function stepSidebarSession(dir: 1 | -1): void {
  const s = appStore.getState();
  const tree = buildTree(s.world, {
    order: s.ui.sidebarOrder,
    folds: s.ui.sidebarFolds,
    now: clock.wall(),
    sticky: new Set(),
    queued: new Set(),
  });
  const order = sessionOrder(tree);
  if (order.length === 0) return;
  const current = s.ui.view.screen === 'session' ? s.ui.view.uid : null;
  const at = current ? order.indexOf(current) : -1;
  const next = at < 0 ? (dir === 1 ? order[0] : order[order.length - 1]) : order[at + dir];
  if (next) s.openSession(next);
}

/** `⌘W`: close the focused tile, else go back; at Home it does nothing (the window is native). */
function closeInnermost(): void {
  const s = appStore.getState();
  const view = s.ui.view;
  if (view.screen === 'split' && s.ui.split.focused) {
    s.removeFromSplit(s.ui.split.focused);
    return;
  }
  if (view.screen !== 'home' && s.ui.back.length > 0) s.goBack();
}

/** `⇧⌘⌫`: Stop… for the session in front (a sheet; never a direct stop). */
function stopInFront(): void {
  const s = appStore.getState();
  const view = s.ui.view;
  if (view.screen === 'session' || (view.screen === 'split' && s.ui.split.focused)) s.openSheet('stopSession');
}

export function useShellKeys({ toggleSidebar, overlayOpen, closeOverlay }: ShellKeyOptions): void {
  const bindings = useMemo<HotkeyBinding[]>(() => {
    const s = () => appStore.getState();
    return [
      { id: 'palette', combo: 'mod+k', run: () => s().setPaletteOpen(!s().ui.paletteOpen) },
      { id: 'needsYou.next', combo: 'mod+j', run: () => stepQueue(1) },
      { id: 'needsYou.previous', combo: 'mod+shift+j', run: () => stepQueue(-1) },
      { id: 'nav.home', combo: 'mod+0', run: () => s().navigate({ screen: 'home' }) },
      { id: 'nav.history', combo: 'mod+shift+f', run: () => s().navigate({ screen: 'history' }) },
      { id: 'nav.accountsHealth', combo: 'mod+shift+a', run: () => s().navigate({ screen: 'accounts', section: 'accounts' }) },
      { id: 'newSession', combo: 'mod+n', run: () => s().openSheet('newSession', currentProject()) },
      { id: 'sidebar.toggle', combo: 'mod+b', run: () => toggleSidebar() },
      { id: 'nav.back', combo: 'mod+bracketleft', run: () => s().goBack() },
      { id: 'nav.forward', combo: 'mod+bracketright', run: () => s().goForward() },
      {
        id: 'session.previous',
        combo: 'mod+alt+arrowup',
        allowRepeat: true,
        when: () => !isScopeActive('T'),
        run: () => stepSidebarSession(-1),
      },
      {
        id: 'session.next',
        combo: 'mod+alt+arrowdown',
        allowRepeat: true,
        when: () => !isScopeActive('T'),
        run: () => stepSidebarSession(1),
      },
      { id: 'region.next', combo: 'f6', allowInTextEntry: true, run: () => void cycleRegion(1) },
      { id: 'region.previous', combo: 'shift+f6', allowInTextEntry: true, run: () => void cycleRegion(-1) },
      { id: 'close', combo: 'mod+w', run: () => closeInnermost() },
      { id: 'stop', combo: 'mod+shift+backspace', run: () => stopInFront() },
      { id: 'settings', combo: 'mod+comma', run: () => s().navigate({ screen: 'settings' }) },
      { id: 'help', combo: 'mod+slash', run: () => s().openSheet('help') },
      { id: 'zoom.in', combo: ['mod+equal', 'mod+shift+equal'], run: () => s().zoomBy(1) },
      { id: 'zoom.out', combo: 'mod+minus', run: () => s().zoomBy(-1) },
      { id: 'zoom.reset', combo: 'mod+alt+0', run: () => s().setZoom(1) },
      { id: 'quit', combo: 'mod+q', run: () => s().setQuitRequested(true) },
      { id: 'sidebar.closeOverlay', combo: 'escape', when: () => overlayOpen, run: () => closeOverlay() },
    ];
  }, [toggleSidebar, overlayOpen, closeOverlay]);
  useHotkeys(bindings, { scope: 'G' });
}
