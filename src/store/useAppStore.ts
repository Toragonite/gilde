/**
 * The one store: zustand v5 + `subscribeWithSelector`, split into slices.
 *
 *  - world  — the core's World, mirrored through `store_subscribe` (worldSlice)
 *  - ui     — what the window shows (uiSlice)
 *  - toasts — outcomes of the user's own actions (toastSlice)
 *  - health — the health popover (healthSlice; figures derive from world)
 *
 * Rules: PTY bytes and conversation blocks never enter this store (they go
 * channel → view); the world changes only through snapshots and patches; every
 * `gilde://` listener lives in `hooks/useEventBridge.ts`.
 */

import { create } from 'zustand';
import { subscribeWithSelector } from 'zustand/middleware';
import { useShallow } from 'zustand/react/shallow';

import { createHealthSlice } from './healthSlice';
import type { HealthSlice } from './healthSlice';
import { createToastSlice } from './toastSlice';
import type { ToastSlice } from './toastSlice';
import { createUiSlice } from './uiSlice';
import type { UiSlice } from './uiSlice';
import { createWorldSlice } from './worldSlice';
import type { WorldSlice } from './worldSlice';

export type AppStore = WorldSlice & UiSlice & ToastSlice & HealthSlice;

export function createAppStore() {
  return create<AppStore>()(
    subscribeWithSelector<AppStore>((...a) => ({
      ...createWorldSlice(...a),
      ...createUiSlice(...a),
      ...createToastSlice(...a),
      ...createHealthSlice(...a),
    })),
  );
}

export const useAppStore = createAppStore();

/** Imperative access for non-React code (event handlers, the event bridge). */
export const appStore = useAppStore;

export { useShallow };
export type { HealthSlice, ToastSlice, UiSlice, WorldSlice };
