/**
 * Registration into the T05 shell seams — the keyboard layer's only side effect.
 *
 * Kept out of the component modules for the same reason `terminal/register.ts`
 * and `sidebar/register.ts` are: importing `CommandPalette` must not mutate a
 * global registry, or a test that renders the palette silently changes what the
 * rest of the shell resolves. `App.tsx` imports this module once, after the
 * store exists.
 *
 * No intent is registered here on purpose. The palette *runs* intents that other
 * lanes own (`newSession`, `newWorktree`, `newDispatch`, `saveTranscript`,
 * `reloadSignatures`) and falls back to a store action or a typed command when
 * one is not registered yet; claiming an id here would shadow the real
 * implementation the moment its wave lands.
 */

import { HelpSheet } from '../dialogs/HelpSheet';
import { experimentalLanes } from '../features';
import { WorktreeSwitcher } from '../sidebar/WorktreeSwitcher';
import { registerOverlay } from '../shell/AppShell';
import { CommandPalette } from './CommandPalette';

registerOverlay('palette', CommandPalette);
// ⌘P switches worktrees — an experimental lane (`src/features.ts`).
if (experimentalLanes()) registerOverlay('switcher', WorktreeSwitcher);
registerOverlay('help', HelpSheet);
