/**
 * Registration into the T05 shell seams — the recovery/settings/toast lane's
 * only side effect (T26).
 *
 * Kept out of the component modules for the same reason `sidebar/register.ts`
 * and `terminal/register.ts` are: importing `SettingsSheet` must not mutate a
 * global registry, or a test that renders the form silently changes what the
 * rest of the shell resolves. `App.tsx` imports this module once, after the
 * store exists.
 *
 * All three mount as overlays, which is what puts the Recovered Sessions sheet
 * *in front of* the main UI (DESIGN §8-5) and the toast stack above everything
 * except a modal sheet.
 */

import { registerOverlay } from '../shell/AppShell';
import { ToastHost } from '../shell/ToastHost';
import { RecoveredSessionsSheet } from './RecoveredSessionsSheet';
import { SettingsSheet } from './SettingsSheet';

registerOverlay('toasts', ToastHost);
registerOverlay('settings', SettingsSheet);
registerOverlay('recoveredSessions', RecoveredSessionsSheet);
