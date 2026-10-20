/**
 * Health slice — "is everything running well" (gilde://health).
 *
 * Rust builds a `HealthSnapshot` on every watchdog tick (5 s) from the scan it
 * already takes, and decides every alert. This slice only keeps the newest
 * snapshot and turns alerts into the two things the user sees besides the
 * status-bar indicator and the popover:
 *
 *  1. **One sticky toast per `action` alert raise.** A toast is keyed by
 *     `(id, raisedAt)`: an alert that holds is updated in place by Rust and is
 *     not toasted again; one that is re-raised (after a dismissal cooldown, or
 *     because its severity rose) carries a new `raisedAt` and is toasted once
 *     more. `attention` alerts never toast — they only colour the indicator.
 *     When an alert ends, is dismissed, or is no longer `action`, its toast goes.
 *  2. **Actions.** Copy command (clipboard), Details / Show agents (the
 *     popover), Clean up (the existing kill sheet, restricted by Rust to the
 *     confirmed leftovers via `onlyHealthOrphans`).
 *
 * Dismissing goes to Rust (`health_dismiss`), which owns the 10-minute quiet
 * period; the answer is applied as the new snapshot.
 */

import type { StateCreator } from 'zustand';

import { cmd, isUnavailable } from '../lib/commands';
import type { HealthAction, HealthAlert, HealthSnapshot } from '../lib/ipc';
import { cleanUpAgents } from '../shell/healthText';
import type { ProcSlice } from './procSlice';
import type { ToastSlice } from './toastSlice';
import type { UiSlice } from './uiSlice';

export interface HealthState {
  /** The newest snapshot; null before the first `health_get` / event. */
  snapshot: HealthSnapshot | null;
  popoverOpen: boolean;
  /** `${id}@${raisedAt}` of every alert raise already toasted. */
  toasted: Set<string>;
  /** alert id → the toast showing it now. */
  toastOf: Map<string, number>;
}

export interface HealthSlice {
  health: HealthState;
  /** Fold one snapshot in (the event, `health_get`, `health_dismiss`). */
  applyHealth: (snapshot: HealthSnapshot) => void;
  /** `health_get` once (boot). Never throws. */
  loadHealth: () => Promise<void>;
  setHealthPopover: (open: boolean) => void;
  /** Dismiss in Rust (10 min quiet unless it escalates) and drop its toast. Never throws. */
  dismissHealthAlert: (id: string) => Promise<void>;
  /** Run one action of one alert. Never throws. */
  runHealthAction: (alert: HealthAlert, action: HealthAction) => Promise<void>;
}

export function initialHealthState(): HealthState {
  return { snapshot: null, popoverOpen: false, toasted: new Set(), toastOf: new Map() };
}

export function raiseKey(alert: HealthAlert): string {
  return `${alert.id}@${alert.raisedAt}`;
}

/** Whether an alert should be on screen as a toast now. */
export function wantsToast(alert: HealthAlert): boolean {
  return alert.severity === 'action' && !alert.dismissed;
}

/**
 * Copy text to the clipboard: the async Clipboard API first, then the old
 * hidden-textarea path. Resolves `false` when neither worked.
 */
export async function copyText(text: string): Promise<boolean> {
  try {
    const clipboard = (globalThis.navigator as Navigator | undefined)?.clipboard;
    if (clipboard && typeof clipboard.writeText === 'function') {
      await clipboard.writeText(text);
      return true;
    }
  } catch {
    // fall through to the textarea path
  }
  try {
    if (typeof document === 'undefined') return false;
    const area = document.createElement('textarea');
    area.value = text;
    area.setAttribute('readonly', '');
    area.style.position = 'fixed';
    area.style.opacity = '0';
    document.body.appendChild(area);
    area.select();
    const ok = document.execCommand('copy');
    document.body.removeChild(area);
    return ok;
  } catch {
    return false;
  }
}

export const createHealthSlice: StateCreator<
  HealthSlice & ToastSlice & ProcSlice & UiSlice,
  [['zustand/subscribeWithSelector', never]],
  [],
  HealthSlice
> = (set, get) => {
  const patch = (update: Partial<HealthState>) => set({ health: { ...get().health, ...update } });

  return {
    health: initialHealthState(),

    applyHealth: (snapshot) => {
      if (!snapshot || !Array.isArray(snapshot.alerts)) return;
      const held = get().health;
      // An older snapshot (a slow `health_get` answering after an event) is ignored.
      if (held.snapshot && snapshot.at < held.snapshot.at) return;

      const toasted = new Set(held.toasted);
      const toastOf = new Map(held.toastOf);
      const byId = new Map(snapshot.alerts.map((a) => [a.id, a]));

      // Toasts whose alert ended, was dismissed, or is no longer `action`.
      for (const [id, toastId] of held.toastOf) {
        const alert = byId.get(id);
        if (!alert || !wantsToast(alert)) {
          get().dismissToast(toastId);
          toastOf.delete(id);
        }
      }
      for (const alert of snapshot.alerts) {
        if (!wantsToast(alert)) continue;
        const key = raiseKey(alert);
        if (toasted.has(key)) continue;
        toasted.add(key);
        const previous = toastOf.get(alert.id);
        if (previous !== undefined) get().dismissToast(previous);
        const id = get().pushToast({
          level: 'error',
          key: alert.titleKey,
          params: {},
          detail: null,
          actionId: null,
          undoToken: null,
          ttlMs: 0,
          healthAlertId: alert.id,
          actions: alert.actions,
        });
        toastOf.set(alert.id, id);
      }
      patch({ snapshot, toasted, toastOf });
    },

    loadHealth: async () => {
      try {
        get().applyHealth(await cmd.healthGet());
      } catch (e) {
        if (!isUnavailable(e)) console.warn('[gilde] health_get failed', e);
      }
    },

    setHealthPopover: (open) => patch({ popoverOpen: open }),

    dismissHealthAlert: async (id) => {
      const toastId = get().health.toastOf.get(id);
      if (toastId !== undefined) {
        get().dismissToast(toastId);
        const toastOf = new Map(get().health.toastOf);
        toastOf.delete(id);
        patch({ toastOf });
      }
      try {
        get().applyHealth(await cmd.healthDismiss(id));
      } catch (e) {
        if (!isUnavailable(e)) console.warn('[gilde] health_dismiss failed', e);
      }
    },

    runHealthAction: async (alert, action) => {
      switch (action.kind) {
        case 'copyCommand': {
          const command = action.command ?? (typeof alert.params?.command === 'string' ? alert.params.command : '');
          if (!command) return;
          const ok = await copyText(command);
          get().pushToast({
            level: ok ? 'success' : 'warn',
            key: ok ? 'health.copied' : 'health.copyFailed',
            params: { command },
            detail: null,
            actionId: null,
            undoToken: null,
            ttlMs: ok ? 3000 : 8000,
          });
          return;
        }
        case 'cleanUp': {
          const rows = cleanUpAgents(get().health.snapshot, action.pids ?? []).map((a) => a.row);
          if (rows.length === 0) return;
          get().setHealthPopover(false);
          get().openKillSheet(rows, { onlyHealthOrphans: true });
          return;
        }
        case 'details':
        case 'showAgents':
        default:
          get().setHealthPopover(true);
      }
    },
  };
};
