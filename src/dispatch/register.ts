/**
 * Registration into the T05 shell seams — the dispatch lane's only side effect.
 *
 * Kept out of the component modules for the same reason `terminal/register.ts`
 * is: importing `ResultCardGrid` must not mutate a global registry, or a test
 * that renders the board silently changes what the rest of the shell resolves.
 * `App.tsx` imports this module once, after the store exists, which is also
 * what keeps the registries out of the import cycle through `useHotkeys`.
 */

import { experimentalLanes } from '../features';
import { registerIntent } from '../hooks/useHotkeys';
import { translate } from '../i18n';
import { registerPaneView } from '../panes/Pane';
import { registerOverlay } from '../shell/AppShell';
import { useAppStore } from '../store/useAppStore';
import { DispatchComposer } from './DispatchComposer';
import { ResultCardGrid } from './ResultCardGrid';

// An experimental lane (`src/features.ts`): in the preview nothing is registered.
if (experimentalLanes()) {
  registerPaneView('dispatch', ResultCardGrid);
  registerOverlay('dispatchComposer', DispatchComposer);
  /**
   * ⌘⇧D (DESIGN §9-8). `useShellHotkeys` already switches the rail to the
   * dispatch view and then runs this intent, so one chord both reveals the board
   * and opens the composer. The palette (T27) can run the same intent.
   */
  registerIntent('newDispatch', () => {
    const store = useAppStore.getState();
    // The board is a tile: make sure there is one before the composer's result
    // has somewhere to land.
    const existing = store.ui.tabs.find((tab) => tab.descriptor.view === 'dispatch');
    if (existing) store.focusTab(existing.id);
    else {
      store.openTab({
        title: translate(store.ui.locale, 'dispatch.title'),
        descriptor: { view: 'dispatch' },
        closable: true,
      });
    }
    store.setComposerOpen(true);
  });
}
