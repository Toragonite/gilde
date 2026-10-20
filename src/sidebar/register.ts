/**
 * Registration into the T05 shell seams — the sidebar lane's only side effect.
 *
 * Kept out of the component modules for the same reason `terminal/register.ts`
 * is: importing `Sidebar` must not mutate a global registry, or a test that
 * renders the tree silently changes what the rest of the shell resolves.
 * `App.tsx` imports this module once, after the store exists.
 */

import { experimentalLanes } from '../features';
import { registerIntent } from '../hooks/useHotkeys';
import { registerOverlay, registerSidebar } from '../shell/AppShell';
import { useAppStore } from '../store/useAppStore';
import { DirtyWorktreeDialog } from '../dialogs/DirtyWorktreeDialog';
import { NewWorktreeDialog } from './NewWorktreeDialog';
import { Sidebar } from './Sidebar';

// The sidebar switches on the view itself (Team gets its own panel), so it is
// registered in every build; the worktree dialogs belong to the experimental lanes.
registerSidebar(Sidebar);
if (experimentalLanes()) {
  registerOverlay('newWorktree', NewWorktreeDialog);
  registerOverlay('dirtyWorktree', DirtyWorktreeDialog);
  /** The palette (T27) and the shell's own affordances run the same intent. */
  registerIntent('newWorktree', () => useAppStore.getState().setNewWorktreeOpen(true));
}
