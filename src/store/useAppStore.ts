/**
 * The one store (DESIGN §9-3): zustand v5 + `subscribeWithSelector`, split into
 * slices.
 *
 * Why not Context or Redux: four live terminals, a 3 s process scan and a 10 Hz
 * graph patch stream mean *many small* updates. Selector-level subscriptions
 * keep a terminal repaint from touching the sidebar, and `subscribeWithSelector`
 * lets the imperative islands (xterm, React Flow nodes) subscribe from outside
 * React.
 *
 * Hard rules enforced in review:
 *  1. PTY bytes never enter this store — they go Channel → base64 decode →
 *     `term.write(Uint8Array)`.
 *  2. The graph lives in `Map`s and only changes through `GraphOp`s.
 *  3. Every `listen()` subscription lives in `useEventBridge()`.
 *
 * ── extension point ────────────────────────────────────────────────────────
 * Later waves add their own slices next to these three:
 *   sessionSlice   (T20)   worktreeSlice (T21)
 *   procSlice      (T23)   dispatchSlice (T24)
 * Each is one import plus one spread in `combined` below, and one member in
 * `AppStore`. Adding a slice is the only reason to edit this file.
 */

import { create } from 'zustand';
import { subscribeWithSelector } from 'zustand/middleware';
import { useShallow } from 'zustand/react/shallow';

import { createDispatchSlice } from './dispatchSlice'; // T24
import type { DispatchSlice } from './dispatchSlice'; // T24
import { createGraphSlice } from './graphSlice';
import { createHealthSlice } from './healthSlice'; // health surface (gilde://health)
import type { HealthSlice } from './healthSlice';
import type { GraphSlice } from './graphSlice';
import { createProcSlice } from './procSlice'; // T23
import type { ProcSlice } from './procSlice'; // T23
import { createSessionSlice } from './sessionSlice'; // T20
import type { SessionSlice } from './sessionSlice'; // T20
import { createTeamSlice } from './teamSlice'; // T8 team preview (read-only)
import type { TeamSlice } from './teamSlice'; // T8
import { createWorkersSlice } from './workersSlice'; // plan #3c worker accounts (read-only)
import type { WorkersSlice } from './workersSlice'; // plan #3c
import { createToastSlice } from './toastSlice';
import type { ToastSlice } from './toastSlice';
import { createUiSlice } from './uiSlice';
import type { UiSlice } from './uiSlice';
import { createWorktreeSlice } from './worktreeSlice'; // T21
import type { WorktreeSlice } from './worktreeSlice'; // T21

export type AppStore = GraphSlice &
  UiSlice &
  ToastSlice &
  SessionSlice & // T20
  WorktreeSlice & // T21
  ProcSlice & // T23
  DispatchSlice & // T24
  TeamSlice & // T8 team preview (read-only)
  WorkersSlice & // plan #3c worker accounts (read-only)
  HealthSlice; // health surface (gilde://health)

export const useAppStore = create<AppStore>()(
  subscribeWithSelector<AppStore>((...a) => ({
    ...createGraphSlice(...a),
    ...createUiSlice(...a),
    ...createToastSlice(...a),
    ...createSessionSlice(...a), // T20
    ...createWorktreeSlice(...a), // T21
    ...createProcSlice(...a), // T23
    ...createDispatchSlice(...a), // T24
    ...createTeamSlice(...a), // T8 team preview (read-only)
    ...createWorkersSlice(...a), // plan #3c worker accounts (read-only)
    ...createHealthSlice(...a), // health surface (gilde://health)
  })),
);

/** Imperative access for non-React islands and for event handlers. */
export const appStore = useAppStore;

export { useShallow };

export type {
  DispatchSlice, // T24
  GraphSlice,
  HealthSlice,
  ProcSlice,
  SessionSlice,
  TeamSlice, // T8
  WorkersSlice, // plan #3c
  ToastSlice,
  UiSlice,
  WorktreeSlice,
};
