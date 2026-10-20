/**
 * Registration into the T05 shell seams — the monitor lane's only side effect.
 *
 * Kept out of the component modules for the same reason `terminal/register.ts`
 * is: importing `ProcessMonitorTable` must not mutate a global registry, or a
 * test that renders the panel silently changes what the rest of the shell
 * resolves. `App.tsx` imports this module once, after the store exists.
 *
 * Two seams and two subscriptions — experimental lanes only (`src/features.ts`):
 *
 *   pane view `monitor`   the full panel (⌘⇧M, the rail, the palette)
 *   drawer panel `monitor` the same panel in the bottom drawer
 *
 *   `ui.activeView === 'monitor'`        → open or focus the monitor tab
 *   `worktree.orphanFilterRequestedAt`   → the sidebar's orphan group asked for
 *                                          the our-orphan filter (T21)
 */

import { experimentalLanes } from '../features';
import { translate } from '../i18n';
import { registerPaneView } from '../panes/Pane';
import { registerDrawerPanel } from '../shell/Drawer';
import { useAppStore } from '../store/useAppStore';
import { MonitorPane, ProcessMonitorTable } from './ProcessMonitorTable';

// In the preview nothing is registered and nothing is subscribed: no process
// scan is ever started from the front end.
if (experimentalLanes()) {
  registerPaneView('monitor', MonitorPane);
  registerDrawerPanel('monitor', ProcessMonitorTable);

  /** Open the monitor tab, or focus the one that is already open. */
  function revealMonitorTab(): void {
    const store = useAppStore.getState();
    const existing = store.ui.tabs.find((tab) => tab.descriptor.view === 'monitor');
    if (existing) {
      store.focusTab(existing.id);
      return;
    }
    store.openTab({
      title: translate(store.ui.locale, 'view.monitor'),
      descriptor: { view: 'monitor' },
      closable: true,
    });
  }

  useAppStore.subscribe(
    (s) => s.ui.activeView,
    (view) => {
      if (view === 'monitor') revealMonitorTab();
    },
  );

  useAppStore.subscribe(
    (s) => s.worktree.orphanFilterRequestedAt,
    (at) => {
      if (at === 0) return;
      const store = useAppStore.getState();
      store.setProcGroupFilter(['oursOrphan']);
      store.setProcGroupCollapsed('oursOrphan', false);
      store.clearProcSelection();
      void store.scanProcesses(true);
      revealMonitorTab();
    },
  );
}
