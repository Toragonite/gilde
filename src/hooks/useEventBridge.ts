/**
 * The single event subscription point (DESIGN §9-3 rule 3).
 *
 * `App.tsx` mounts this exactly once. Every `gilde://` event is subscribed here and
 * nowhere else, which guarantees one listener per event and one clean teardown.
 * Components never call `listen()`; panels that need an event that has no slice
 * yet register a handler with `onGildeEvent()` from their own module.
 *
 * Nothing here is allowed to be expensive: handlers fold a payload into the
 * store and return. PTY bytes do not travel through events at all — they take
 * the `Channel` fast path straight into xterm.
 */

import { useEffect } from 'react';

import { experimentalLanes } from '../features';
import { listenAll } from '../lib/events';
import {
  AGENTS_DETECTED,
  BUDGET_ALERT,
  DISPATCH_EVENT,
  GRAPH_PATCH,
  GRAPH_RESYNC,
  HEALTH,
  LOG,
  PROC_REGISTRY,
  PROC_SCAN,
  PTY_RESYNC,
  RECOVERY_PENDING,
  SESSION_EXIT,
  SESSION_STATUS,
  SIDECAR_STATE,
  TEAM_SNAPSHOT,
  TOAST,
  WORKTREE_CHANGED,
} from '../lib/ipc';
import type { GildeEventName, GildeEventPayload } from '../lib/ipc';
import { useAppStore } from '../store/useAppStore';
import { nodeStatusOfSession } from '../components/StatusDot';

// ---------------------------------------------------------------------------
// Extra handlers — the seam for slices that land in later waves
// ---------------------------------------------------------------------------

type AnyHandler = (payload: unknown) => void;

const extra = new Map<GildeEventName, Set<AnyHandler>>();

/**
 * Subscribe to an event without opening a second Tauri listener. Returns the
 * unsubscribe function. Call it from a slice module (T20/T23/T24), never from a
 * component render.
 */
export function onGildeEvent<K extends GildeEventName>(
  name: K,
  handler: (payload: GildeEventPayload<K>) => void,
): () => void {
  let set = extra.get(name);
  if (!set) {
    set = new Set();
    extra.set(name, set);
  }
  const wrapped = handler as AnyHandler;
  set.add(wrapped);
  return () => {
    set?.delete(wrapped);
  };
}

function fanout<K extends GildeEventName>(name: K, payload: GildeEventPayload<K>): void {
  const set = extra.get(name);
  if (!set) return;
  for (const handler of set) {
    try {
      handler(payload);
    } catch (e) {
      console.warn('[gilde] event handler failed', name, e);
    }
  }
}

/** The watchdog's own budget toasts (`toast.budget.*`), which the preview leaves to the health alerts. */
export function isBudgetToastKey(key: string): boolean {
  return typeof key === 'string' && key.startsWith('toast.budget.');
}

// ---------------------------------------------------------------------------
// Bridge
// ---------------------------------------------------------------------------

export function useEventBridge(): void {
  useEffect(() => {
    const store = useAppStore.getState;

    return listenAll((add) => {
      add(GRAPH_PATCH, (patch) => {
        store().applyGraphPatch(patch);
        fanout(GRAPH_PATCH, patch);
      });

      add(GRAPH_RESYNC, (snapshot) => {
        store().applyGraphSnapshot(snapshot);
        fanout(GRAPH_RESYNC, snapshot);
      });

      add(SESSION_STATUS, (event) => {
        // Tab dots come straight off the status stream so a tab that is not the
        // active pane still shows that its agent is waiting for a human.
        store().updateSessionTabs(event.sessionId, { status: nodeStatusOfSession(event.status) });
        fanout(SESSION_STATUS, event);
      });

      add(SESSION_EXIT, (event) => {
        store().updateSessionTabs(event.sessionId, { status: 'exited' });
        fanout(SESSION_EXIT, event);
      });

      add(PTY_RESYNC, (event) => {
        fanout(PTY_RESYNC, event);
      });

      add(DISPATCH_EVENT, (event) => {
        fanout(DISPATCH_EVENT, event);
      });

      add(WORKTREE_CHANGED, (event) => {
        fanout(WORKTREE_CHANGED, event);
      });

      add(PROC_SCAN, (event) => {
        fanout(PROC_SCAN, event);
      });

      add(PROC_REGISTRY, (event) => {
        fanout(PROC_REGISTRY, event);
      });

      add(BUDGET_ALERT, (event) => {
        // Preview: the health surface's memory alerts replace the budget toast.
        if (!experimentalLanes()) {
          fanout(BUDGET_ALERT, event);
          return;
        }
        store().pushToast({
          level: event.level === 'soft' ? 'warn' : 'error',
          key: 'badge.over-budget',
          params: { level: event.level },
          detail: null,
          actionId: null,
          undoToken: event.undoToken,
          ttlMs: event.undoToken ? 10_000 : 8000,
        });
        fanout(BUDGET_ALERT, event);
      });

      add(AGENTS_DETECTED, (event) => {
        store().setAgents(event.agents);
        fanout(AGENTS_DETECTED, event);
      });

      add(SIDECAR_STATE, (event) => {
        store().setSidecar(event);
        fanout(SIDECAR_STATE, event);
      });

      add(RECOVERY_PENDING, (event) => {
        store().setRecoveryPending(event.count);
        fanout(RECOVERY_PENDING, event);
      });

      add(TOAST, (event) => {
        // Preview: the watchdog's budget toasts are replaced by the health alerts.
        if (experimentalLanes() || !isBudgetToastKey(event.key)) store().pushToastEvent(event);
        fanout(TOAST, event);
      });

      // No view lists log lines any more (the drawer's Logs tab is gone); the
      // event stays subscribed so a later consumer can register through
      // `onGildeEvent` without a second Tauri listener.
      add(LOG, (event) => {
        fanout(LOG, event);
      });

      // Team contract v2 §8.2: a whole team view, latest wins — the team slice
      // folds it in through `onGildeEvent` (store/teamSlice.ts).
      add(TEAM_SNAPSHOT, (event) => {
        fanout(TEAM_SNAPSHOT, event);
      });

      // The health surface: one whole snapshot per watchdog tick, latest wins.
      add(HEALTH, (snapshot) => {
        store().applyHealth(snapshot);
        fanout(HEALTH, snapshot);
      });
    });
  }, []);
}
