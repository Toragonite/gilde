/**
 * Toast slice.
 *
 * Rust never sends prose (DESIGN §9-9): a toast crosses IPC as
 * `{ level, key, params, actionId?, undoToken? }` and is rendered by `t()`.
 * Toasts with an `undoToken` hold a 10 s window that `undo_cancel(token)` can
 * still cancel, so those are never auto-dismissed early.
 *
 * `shell/ToastHost.tsx` (T26) renders this list.
 */

import type { StateCreator } from 'zustand';

import type { GildeErrorPayload, HealthAction, ToastEvent, ToastLevel } from '../lib/ipc';

export interface Toast {
  id: number;
  level: ToastLevel;
  /** i18n key; `key` is a plain string because it may come from Rust. */
  key: string;
  params: Record<string, unknown>;
  /** Untranslated technical detail (raw stderr) shown verbatim, collapsed. */
  detail: string | null;
  actionId: string | null;
  undoToken: string | null;
  at: number;
  /** Milliseconds until auto-dismiss; 0 means it stays until dismissed. */
  ttlMs: number;
  /**
   * A health alert's toast (severity `action`): rendered from the alert in the
   * health slice, with its actions as buttons; closing it dismisses the alert.
   */
  healthAlertId?: string;
  actions?: HealthAction[];
}

export interface ToastSlice {
  toasts: Toast[];
  pushToast: (toast: Omit<Toast, 'id' | 'at'> & { at?: number }) => number;
  pushToastEvent: (event: ToastEvent) => number;
  pushErrorToast: (error: GildeErrorPayload) => number;
  dismissToast: (id: number) => void;
  clearToasts: () => void;
}

const DEFAULT_TTL: Record<ToastLevel, number> = {
  info: 5000,
  success: 4000,
  warn: 8000,
  error: 0,
};

/** Undo windows are exactly 10 s (DESIGN §8-6). */
export const UNDO_WINDOW_MS = 10_000;

const MAX_TOASTS = 6;

let counter = 0;

export const createToastSlice: StateCreator<
  ToastSlice,
  [['zustand/subscribeWithSelector', never]],
  [],
  ToastSlice
> = (set, get) => ({
  toasts: [],

  pushToast: (input) => {
    counter += 1;
    const toast: Toast = { ...input, id: counter, at: input.at ?? Date.now() };
    const toasts = [...get().toasts, toast];
    set({ toasts: toasts.length > MAX_TOASTS ? toasts.slice(toasts.length - MAX_TOASTS) : toasts });
    return toast.id;
  },

  pushToastEvent: (event) =>
    get().pushToast({
      level: event.level,
      key: event.key,
      params: event.params,
      detail: null,
      actionId: event.actionId,
      undoToken: event.undoToken,
      ttlMs: event.undoToken ? UNDO_WINDOW_MS : DEFAULT_TTL[event.level],
    }),

  pushErrorToast: (error) =>
    get().pushToast({
      level: 'error',
      key: error.key,
      params: error.params,
      detail: error.detail,
      actionId: null,
      undoToken: null,
      ttlMs: DEFAULT_TTL.error,
    }),

  dismissToast: (id) => set({ toasts: get().toasts.filter((t) => t.id !== id) }),

  clearToasts: () => set({ toasts: [] }),
});
