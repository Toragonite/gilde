/**
 * Registration into the T05 shell seams — the graph lane's only side effect.
 *
 * Kept out of the component modules for the same reason `terminal/register.ts`
 * is: importing `GraphDashboard` must not mutate a global registry, or a test
 * that renders the canvas silently changes what the rest of the shell resolves.
 * `App.tsx` imports this module once, after the store exists.
 */

import { experimentalLanes } from '../features';
import { translate } from '../i18n';
import { registerPaneView } from '../panes/Pane';
import { useAppStore } from '../store/useAppStore';
import { GraphDashboard } from './GraphDashboard';

/** One graph tab, reused: the canvas is a place, not a document. */
export const GRAPH_TAB_ID = 'tab-graph';

/** Focus the graph tab, opening it if this is the first time. */
export function revealGraph(): void {
  const store = useAppStore.getState();
  const existing = store.ui.tabs.find((tab) => tab.descriptor.view === 'graph');
  if (existing) {
    store.focusTab(existing.id);
    return;
  }
  store.openTab({
    id: GRAPH_TAB_ID,
    title: translate(store.ui.locale, 'graph.title'),
    descriptor: { view: 'graph' },
    closable: true,
  });
}

// An experimental lane (`src/features.ts`): in the preview nothing is registered.
if (experimentalLanes()) {
  registerPaneView('graph', GraphDashboard);

  // `⌘⇧G` and the rail both switch the active view; that has to actually put the
  // canvas in front of the user rather than only change a highlight.
  useAppStore.subscribe(
    (s) => s.ui.activeView,
    (view) => {
      if (view === 'graph') revealGraph();
    },
  );

  // The tab title is a translated string that was frozen when the tab opened;
  // switching locale has to move it too (DESIGN §9-9).
  useAppStore.subscribe(
    (s) => s.ui.locale,
    (locale) => {
      const store = useAppStore.getState();
      for (const tab of store.ui.tabs) {
        if (tab.descriptor.view !== 'graph') continue;
        store.updateTab(tab.id, { title: translate(locale, 'graph.title') });
      }
    },
  );
}
