/**
 * Registration into the T05 shell seams — the terminal lane's only side effect.
 *
 * Keeping it out of the component modules is deliberate: importing
 * `TerminalPane` must not mutate a global registry, or a test that renders the
 * pane silently changes what the rest of the shell resolves. `App.tsx` imports
 * this module once, after the store exists, which is also what keeps the
 * registries out of the import cycle that runs through `useHotkeys`.
 */

import { experimentalLanes } from '../features';
import { registerIntent } from '../hooks/useHotkeys';
import { registerPaneView } from '../panes/Pane';
import { registerOverlay } from '../shell/AppShell';
import { registerInspectorActions } from '../shell/Inspector';
import { useAppStore } from '../store/useAppStore';
import { NewSessionDialog } from './NewSessionDialog';
import { SessionInspectorActions, TerminalPane } from './TerminalPane';

// An experimental lane (`src/features.ts`): in the preview nothing is registered.
if (experimentalLanes()) {
  registerPaneView('terminal', TerminalPane);
  registerOverlay('newSession', NewSessionDialog);
  registerInspectorActions('session', SessionInspectorActions);
  /** ⌘T (DESIGN §9-8). The palette (T27) can run the same intent. */
  registerIntent('newSession', () => useAppStore.getState().setNewSessionOpen(true));
}
