/**
 * App — the root. It mounts T13's event bridge exactly once (bootstrap, the
 * World subscription, the four global events and the reporters: `app_view_state`
 * on every view change [F-04], `ui_state_set`, `app_input_activity`) and
 * renders the two-zone shell [M1].
 *
 * Nothing here fetches data: every screen reads the World mirror and the UI
 * slice of the one store (`store/useAppStore.ts`). Screens owned by other
 * tasks are found by the shell (`shell/slots.ts`); App imports only the shell.
 */

import { useEventBridge } from './hooks/useEventBridge';
import { Shell } from './shell/Shell';

export default function App() {
  // The one and only place `gilde://` events are subscribed (UI-GUIDELINES §2-10).
  useEventBridge();
  return <Shell />;
}
