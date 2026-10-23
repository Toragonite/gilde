/**
 * Toasts — outcomes of the user's own actions only (ux-ia §6.2, contract §4.3
 * `gilde://toast`). Never used for things that happened elsewhere: those go to
 * the queue, notices or the live regions.
 *
 * A toast is `{ key, params }` rendered with the i18n tables. One with an
 * `undoToken` stays for the 10 s undo window, and Undo calls `undo_cancel`.
 */

import type { StateCreator } from 'zustand';

import { clock } from '../lib/clock';
import { commands } from '../lib/commands';
import type { ToastEvent } from '../lib/gen/ToastEvent';
import type { UndoToken } from '../lib/gen/UndoToken';
import type { IpcError } from '../lib/ipcError';

export type ToastLevel = 'info' | 'error';

export interface Toast {
  id: number;
  level: ToastLevel;
  /** i18n key (a plain string: it may come from Rust). */
  key: string;
  params: Readonly<Record<string, string | number | boolean>>;
  /** Mono detail of an error (`IpcError.detail`: stderr or a correlation id). */
  detail: string | null;
  undoToken: UndoToken | null;
  createdAt: number;
  ttlMs: number;
}

export const UNDO_WINDOW_MS = 10_000;
export const TOAST_TTL_MS: Readonly<Record<ToastLevel, number>> = { info: 5_000, error: 8_000 };
/** At most this many toasts show; the oldest goes first. */
export const MAX_TOASTS = 3;

export interface ToastSlice {
  toasts: readonly Toast[];
  /** A `gilde://toast` event. */
  pushToastEvent: (event: ToastEvent) => number;
  /** The failure of one of the user's own commands. */
  pushErrorToast: (error: IpcError) => number;
  dismissToast: (id: number) => void;
  /** Undo the action a toast reports; resolves true when the core cancelled it. */
  undoToast: (id: number) => Promise<boolean>;
  /** Drop toasts whose time is up (the shell calls this on a timer). */
  expireToasts: (now?: number) => void;
}

let nextId = 1;

export const createToastSlice: StateCreator<ToastSlice, [['zustand/subscribeWithSelector', never]], [], ToastSlice> = (
  set,
  get,
) => {
  const push = (toast: Omit<Toast, 'id' | 'createdAt'>): number => {
    const id = nextId++;
    const full: Toast = { ...toast, id, createdAt: clock.now() };
    set((s) => ({ toasts: [...s.toasts, full].slice(-MAX_TOASTS) }));
    return id;
  };

  return {
    toasts: [],

    pushToastEvent: (event) =>
      push({
        level: 'info',
        key: event.key,
        params: { ...event.params },
        detail: null,
        undoToken: event.undoToken,
        ttlMs: event.undoToken ? UNDO_WINDOW_MS : TOAST_TTL_MS.info,
      }),

    pushErrorToast: (error) =>
      push({
        level: 'error',
        key: error.key,
        params: { ...error.params },
        detail: error.detail,
        undoToken: null,
        ttlMs: TOAST_TTL_MS.error,
      }),

    dismissToast: (id) => set((s) => ({ toasts: s.toasts.filter((t) => t.id !== id) })),

    undoToast: async (id) => {
      const toast = get().toasts.find((t) => t.id === id);
      if (!toast || !toast.undoToken) return false;
      get().dismissToast(id);
      try {
        const res = await commands.undo_cancel({ undoToken: toast.undoToken });
        return res.cancelled;
      } catch (error) {
        get().pushErrorToast(error as IpcError);
        return false;
      }
    },

    expireToasts: (now = clock.now()) => {
      const keep = get().toasts.filter((t) => now - t.createdAt < t.ttlMs);
      if (keep.length !== get().toasts.length) set({ toasts: keep });
    },
  };
};
