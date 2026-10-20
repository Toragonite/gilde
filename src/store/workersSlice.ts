/**
 * Workers slice — the READ-ONLY view of the worker accounts the Claude Code
 * Orchestrator ENGINE knows (Gilde plan #3c): their plan-usage windows,
 * cooldowns and login state, from `list_workers`' structured answer.
 *
 * Gilde displays; it decides nothing. There is no quota arithmetic, no
 * selection, no cooldown timer here — only the engine's values as the sidecar
 * (`cco.workers`) checked them, and the times they were true.
 *
 * Three rules:
 *
 * 1. **One poll for every consumer.** However many panes show the accounts,
 *    one timer polls, one request is in flight, and the sidecar floors calls
 *    to the host at the engine's WORKERS_SNAPSHOT_MIN_POLL_MS (60 s). The next
 *    automatic poll is at the sidecar's `nextPollAt`, never sooner than
 *    WORKERS_POLL_MIN_DELAY_MS nor later than the floor. The user's refresh is
 *    a plain call that may come back cached (`fromCache`).
 * 2. **Stale, never blank.** When an answer carries no snapshot (the sidecar
 *    restarted, no host is left) the last snapshot shown is kept and marked
 *    stale with its own times; an error keeps it too.
 * 3. **Watching is lazy.** Nothing is asked until the accounts view is shown.
 */

import type { StateCreator } from 'zustand';

import { onGildeEvent } from '../hooks/useEventBridge';
import { asGildeError, cmd, isUnavailable } from '../lib/commands';
import { SIDECAR_STATE } from '../lib/ipc';
import type { CcoWorkersAnswer, GildeErrorPayload } from '../lib/ipc';

/** The engine's floor (host-protocol WORKERS_SNAPSHOT_MIN_POLL_MS), used until an answer states it. */
export const WORKERS_POLL_FLOOR_MS = 60_000;
/** The soonest an automatic poll follows the previous one (a gone host, no host). */
export const WORKERS_POLL_MIN_DELAY_MS = 5_000;

export interface WorkersViewState {
  /** The last answer, its snapshot carried over by rule 2; null before the first. */
  answer: CcoWorkersAnswer | null;
  /** The backend has no `cco_workers` command (an older build). */
  commandMissing: boolean;
  loading: boolean;
  /** A request is in flight. */
  busy: boolean;
  error: GildeErrorPayload | null;
  /** How many views are watching (one timer for all). */
  watchers: number;
}

export interface WorkersSlice {
  workers: WorkersViewState;
  /** Ask the sidecar now (`explicit`: the user's refresh). Never throws; joins a request in flight. */
  refreshWorkers: (explicit: boolean) => Promise<void>;
  /** A view is shown: read now, then poll. Returns the stop function. */
  startWorkersWatch: () => () => void;
}

export function initialWorkersState(): WorkersViewState {
  return { answer: null, commandMissing: false, loading: false, busy: false, error: null, watchers: 0 };
}

/**
 * Rule 2, pure: an answer without a snapshot keeps the previous one, marked
 * stale, with the previous host and receive time — the panel says what it
 * shows and since when.
 */
export function mergeWorkersAnswer(prev: CcoWorkersAnswer | null, next: CcoWorkersAnswer): CcoWorkersAnswer {
  if (next.snapshot !== null || prev === null || prev.snapshot === null) return next;
  return { ...next, snapshot: prev.snapshot, hostKey: prev.hostKey, receivedAt: prev.receivedAt, stale: true };
}

/** When to poll next after `answer` (rule 1), in ms from `now`. */
export function nextWorkersPollDelay(answer: CcoWorkersAnswer | null, now: number): number {
  const floor = answer !== null && Number.isFinite(answer.minPollMs) && answer.minPollMs > 0 ? answer.minPollMs : WORKERS_POLL_FLOOR_MS;
  if (answer === null || !answer.available || !Number.isFinite(answer.nextPollAt)) return floor;
  return Math.min(floor, Math.max(WORKERS_POLL_MIN_DELAY_MS, answer.nextPollAt - now));
}

export const createWorkersSlice: StateCreator<
  WorkersSlice,
  [['zustand/subscribeWithSelector', never]],
  [],
  WorkersSlice
> = (set, get) => {
  const patch = (update: Partial<WorkersViewState>) => set({ workers: { ...get().workers, ...update } });

  let inFlight: Promise<void> | null = null;
  let timer: ReturnType<typeof setTimeout> | null = null;

  const schedule = () => {
    if (timer !== null) clearTimeout(timer);
    timer = null;
    if (get().workers.watchers <= 0) return;
    timer = setTimeout(() => {
      timer = null;
      void get().refreshWorkers(false).finally(schedule);
    }, nextWorkersPollDelay(get().workers.answer, Date.now()));
  };

  const ask = async (explicit: boolean): Promise<void> => {
    patch({ busy: true, loading: get().workers.answer === null });
    try {
      const next = await cmd.ccoWorkers(explicit);
      patch({
        answer: mergeWorkersAnswer(get().workers.answer, next),
        commandMissing: false,
        loading: false,
        busy: false,
        error: null,
      });
    } catch (e) {
      if (isUnavailable(e)) patch({ commandMissing: true, loading: false, busy: false, error: null });
      else patch({ loading: false, busy: false, error: asGildeError(e) });
    }
  };

  queueMicrotask(() => {
    onGildeEvent(SIDECAR_STATE, (event) => {
      // A sidecar that came back holds nothing yet: ask it (the floor is its own).
      if (event.state === 'ready' && get().workers.watchers > 0) void get().refreshWorkers(false);
    });
  });

  return {
    workers: initialWorkersState(),

    refreshWorkers: (explicit) => {
      if (inFlight) return inFlight;
      inFlight = ask(explicit).finally(() => {
        inFlight = null;
      });
      return inFlight;
    },

    startWorkersWatch: () => {
      patch({ watchers: get().workers.watchers + 1 });
      if (get().workers.watchers === 1) void get().refreshWorkers(false).finally(schedule);
      let stopped = false;
      return () => {
        if (stopped) return;
        stopped = true;
        patch({ watchers: Math.max(0, get().workers.watchers - 1) });
        if (get().workers.watchers === 0 && timer !== null) {
          clearTimeout(timer);
          timer = null;
        }
      };
    },
  };
};
