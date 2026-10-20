/**
 * Process monitor slice (T23) — DESIGN §8-7, §8-8.
 *
 * The panel is a *reader* of two things Rust already computes: the scan
 * (`procmon_scan`, answered from the watchdog's cache) and the budget
 * (`budget_get`). Nothing here re-derives a safety rule; the refusals, the
 * identity fences and the masking all live in Rust, and this file's job is to
 * hold the last scan, the user's filter and selection, and the kill sheet's
 * state.
 *
 * Three rules shape the code:
 *
 *  1. **The sort is recomputed at most once per scan** (acceptance 2). The
 *     order is computed in `applyScan` and stored; components render `rows` in
 *     the order they find it, so a row never jumps under the cursor between two
 *     repaints of the same scan.
 *  2. **The backend is always asked for an unfiltered scan.** Filtering happens
 *     here, so the footer totals stay honest ("total processes" means the
 *     machine, not the filter) and a filter change is instant instead of a
 *     round trip.
 *  3. **Multi-selection never spans two groups** (acceptance 4). The selection
 *     carries the group it belongs to and selecting a row in another group
 *     replaces it rather than extending it — bulk rules differ per group and a
 *     mixed selection could not be judged by one of them.
 *
 * Events arrive through `onGildeEvent()` (the fan-out seam of `useEventBridge`),
 * never through a second `listen()`.
 */

import type { StateCreator } from 'zustand';

import { onGildeEvent } from '../hooks/useEventBridge';
import { asGildeError, cmd, isUnavailable } from '../lib/commands';
import { PROC_SCAN } from '../lib/ipc';
import type {
  GildeErrorPayload,
  BudgetState,
  KillOutcome,
  KillTarget,
  ProcGroupKind,
  ProcRow,
  ProcTotals,
  ScanResult,
  SupervisionConfig,
} from '../lib/ipc';
import type { ToastSlice } from './toastSlice';
import type { UiSlice } from './uiSlice';

// ---------------------------------------------------------------------------
// Constants
// ---------------------------------------------------------------------------

/**
 * Group order. `OursOrphan` leads because it is the reason the panel gets
 * opened: it is the only group that is provably ours *and* unowned, and the
 * only one with a bulk action (DESIGN §8-7).
 */
export const GROUP_ORDER: readonly ProcGroupKind[] = [
  'oursOrphan',
  'oursLive',
  'agentUnmanaged',
  'mcpServer',
  'externalResidue',
  'other',
];

/**
 * Collapsed on first paint. DESIGN §8-7 names `ExternalResidue`; `other` is
 * collapsed for the same reason — it is the machine (≈800 rows on this laptop),
 * not the app, and an expanded default would bury every row that matters.
 */
export const DEFAULT_COLLAPSED: readonly ProcGroupKind[] = ['externalResidue', 'other'];

/** Panel visible (DESIGN §8-7). */
export const POLL_ACTIVE_MS = 3_000;
/** Panel not visible. The watchdog keeps its own 5 s tick regardless. */
export const POLL_BACKGROUND_MS = 15_000;

/** A scan older than this is reported as stale in the footer. */
export const SCAN_STALE_AFTER_MS = 12_000;

// ---------------------------------------------------------------------------
// Types
// ---------------------------------------------------------------------------

export interface ProcFilterState {
  /** `null` = every group. */
  groups: ProcGroupKind[] | null;
  /** Matches name or masked argv, case-insensitively. */
  query: string;
}

/** What the kill sheet is confirming. */
export interface KillPlanState {
  open: boolean;
  /** The rows exactly as the table showed them — masked argv included. */
  rows: ProcRow[];
  /** More than one row means the bulk refusals of DESIGN §8-7 apply. */
  bulk: boolean;
  dryRun: boolean;
  /** What a dry run said would happen. Cleared when the target set changes. */
  preview: KillOutcome[] | null;
  /** What actually happened. */
  outcomes: KillOutcome[] | null;
  running: boolean;
  error: GildeErrorPayload | null;
  /**
   * Opened by a health alert's Clean up: Rust refuses every target that is not
   * a confirmed leftover of the latest health snapshot (`onlyHealthOrphans`).
   */
  onlyHealthOrphans: boolean;
}

/** How the kill sheet was opened. */
export interface KillSheetOptions {
  onlyHealthOrphans?: boolean;
}

export interface ProcSelection {
  /** The one group the selection lives in (DESIGN §8-7). */
  group: ProcGroupKind | null;
  pids: number[];
}

export interface ProcState {
  /** The last scan, already ordered. Never re-sorted between scans. */
  rows: ProcRow[];
  totals: ProcTotals;
  /** Unix millis of the scan, 0 before the first one. */
  at: number;
  /** Increments once per applied scan — the "one sort per cycle" witness. */
  scanCount: number;
  loading: boolean;
  error: GildeErrorPayload | null;
  /** A monitor surface is mounted (pane or drawer). */
  panelVisible: boolean;
  /** The window has focus. False stops polling and shows "일시정지". */
  windowFocused: boolean;
  filter: ProcFilterState;
  collapsed: Partial<Record<ProcGroupKind, boolean>>;
  selection: ProcSelection;
  /** Lazily resolved (`procmon_cwd`), one entry per rendered pid. */
  cwd: Record<number, string | null>;
  budget: BudgetState | null;
  /** Read once for the escalation the kill sheet promises. */
  supervision: SupervisionConfig | null;
  kill: KillPlanState;
}

export interface ProcSlice {
  proc: ProcState;

  scanProcesses: (force?: boolean) => Promise<void>;
  applyScan: (result: ScanResult) => void;
  refreshBudget: () => Promise<void>;

  setProcPanelVisible: (visible: boolean) => void;
  setProcQuery: (query: string) => void;
  toggleProcGroupFilter: (group: ProcGroupKind) => void;
  /** `null` clears the group filter. Used by the sidebar's orphan group. */
  setProcGroupFilter: (groups: ProcGroupKind[] | null) => void;
  toggleProcGroupCollapsed: (group: ProcGroupKind) => void;
  setProcGroupCollapsed: (group: ProcGroupKind, collapsed: boolean) => void;

  toggleProcSelection: (row: ProcRow) => void;
  selectProcGroup: (group: ProcGroupKind, pids: number[]) => void;
  clearProcSelection: () => void;

  ensureProcCwd: (pid: number) => void;

  openKillSheet: (rows: ProcRow[], options?: KillSheetOptions) => void;
  closeKillSheet: () => void;
  setKillDryRun: (dryRun: boolean) => void;
  runKill: () => Promise<void>;
}

// ---------------------------------------------------------------------------
// Pure helpers (unit tested in monitor/monitor.test.tsx)
// ---------------------------------------------------------------------------

export function emptyProcTotals(): ProcTotals {
  return {
    processes: 0,
    rssBytes: 0,
    physicalRamBytes: 0,
    rssPercent: 0,
    pressureLevel: 'unknown',
    byGroup: {},
  };
}

export function initialProcState(): ProcState {
  const collapsed: Partial<Record<ProcGroupKind, boolean>> = {};
  for (const group of DEFAULT_COLLAPSED) collapsed[group] = true;
  return {
    rows: [],
    totals: emptyProcTotals(),
    at: 0,
    scanCount: 0,
    loading: false,
    error: null,
    panelVisible: false,
    windowFocused: true,
    filter: { groups: null, query: '' },
    collapsed,
    selection: { group: null, pids: [] },
    cwd: {},
    budget: null,
    supervision: null,
    kill: {
      open: false,
      rows: [],
      bulk: false,
      dryRun: false,
      preview: null,
      outcomes: null,
      running: false,
      error: null,
      onlyHealthOrphans: false,
    },
  };
}

function groupRank(group: ProcGroupKind): number {
  const index = GROUP_ORDER.indexOf(group);
  return index < 0 ? GROUP_ORDER.length : index;
}

/**
 * Group order first, then RSS descending, then pid so equal rows keep a stable
 * position between scans. Called exactly once per scan.
 */
export function sortRows(rows: readonly ProcRow[]): ProcRow[] {
  return [...rows].sort((a, b) => {
    const byGroup = groupRank(a.group) - groupRank(b.group);
    if (byGroup !== 0) return byGroup;
    if (a.rssBytes !== b.rssBytes) return b.rssBytes - a.rssBytes;
    return a.pid - b.pid;
  });
}

/** Client-side mirror of `ScanFilter::matches` (Rust), for an instant filter. */
export function matchesFilter(row: ProcRow, filter: ProcFilterState): boolean {
  if (filter.groups && !filter.groups.includes(row.group)) return false;
  const query = filter.query.trim().toLowerCase();
  if (query.length === 0) return true;
  return (
    row.name.toLowerCase().includes(query) ||
    row.argvDisplay.toLowerCase().includes(query) ||
    String(row.pid).includes(query)
  );
}

export interface ProcGroupView {
  group: ProcGroupKind;
  rows: ProcRow[];
  count: number;
  rssBytes: number;
  cpuPct: number;
  collapsed: boolean;
}

/**
 * The rows the panel draws, grouped and summed. The header totals are the sums
 * of the *visible* rows, so a filtered group header never claims rows the table
 * is not showing; the machine-wide numbers live in the footer.
 */
export function groupRows(
  rows: readonly ProcRow[],
  filter: ProcFilterState,
  collapsed: Partial<Record<ProcGroupKind, boolean>>,
): ProcGroupView[] {
  const views = new Map<ProcGroupKind, ProcGroupView>();
  for (const row of rows) {
    if (!matchesFilter(row, filter)) continue;
    let view = views.get(row.group);
    if (!view) {
      view = {
        group: row.group,
        rows: [],
        count: 0,
        rssBytes: 0,
        cpuPct: 0,
        collapsed: collapsed[row.group] === true,
      };
      views.set(row.group, view);
    }
    view.rows.push(row);
    view.count += 1;
    view.rssBytes += row.rssBytes;
    view.cpuPct += row.cpuPct;
  }
  return [...views.values()].sort((a, b) => groupRank(a.group) - groupRank(b.group));
}

/** The largest RSS on screen — the scale for every row's bar (DESIGN §8-7). */
export function maxRss(groups: readonly ProcGroupView[]): number {
  let max = 0;
  for (const group of groups) {
    if (group.collapsed) continue;
    for (const row of group.rows) if (row.rssBytes > max) max = row.rssBytes;
  }
  return max;
}

export interface CwdParts {
  prefix: string;
  /** The worktree segment, drawn emphasised. Empty when there is none. */
  highlight: string;
  suffix: string;
}

/**
 * `…/worktrees/gilde-code-9f2a13/fix-pty-resize` — the worktree segment
 * is the part a human is actually looking for, so it is the part that is kept
 * whole and emphasised while the rest is abbreviated (DESIGN §8-7).
 */
export function describeCwd(cwd: string | null | undefined): CwdParts | null {
  if (!cwd) return null;
  const parts = cwd.split('/').filter(Boolean);
  const index = parts.lastIndexOf('worktrees');
  if (index >= 0 && index + 2 < parts.length) {
    return {
      prefix: `…/${parts[index]}/${parts[index + 1]}/`,
      highlight: parts[index + 2],
      suffix: parts.length > index + 3 ? `/${parts.slice(index + 3).join('/')}` : '',
    };
  }
  if (parts.length <= 2) return { prefix: cwd, highlight: '', suffix: '' };
  return { prefix: `…/${parts.slice(-2).join('/')}`, highlight: '', suffix: '' };
}

export interface KillTally {
  terminated: number;
  survived: number;
  /** Refusal code → how many rows carried it, in first-seen order. */
  refused: { reason: string; count: number }[];
  dryRun: boolean;
}

/**
 * The honest report of DESIGN §8-6: "6개 종료, 1개 거부(argv 불일치), 1개
 * SIGKILL 생존". A refusal and a failure are different facts and are counted
 * separately; a survivor is neither.
 */
export function tallyOutcomes(outcomes: readonly KillOutcome[]): KillTally {
  const tally: KillTally = { terminated: 0, survived: 0, refused: [], dryRun: false };
  for (const outcome of outcomes) {
    const report = outcome.report;
    if (report.dryRun) tally.dryRun = true;
    if (report.refusedReason) {
      const existing = tally.refused.find((r) => r.reason === report.refusedReason);
      if (existing) existing.count += 1;
      else tally.refused.push({ reason: report.refusedReason, count: 1 });
      continue;
    }
    if (report.survivors.length > 0) tally.survived += 1;
    else tally.terminated += 1;
  }
  return tally;
}

/** The pids a dry run says it would signal, and the ones it would skip. */
export interface DryRunSets {
  targets: number[];
  excluded: { pid: number; reason: string }[];
  /** pid → process name, as Rust's plan saw it (every target it could name). */
  names: Record<number, string>;
}

export function dryRunSets(outcomes: readonly KillOutcome[]): DryRunSets {
  const targets: number[] = [];
  const excluded: { pid: number; reason: string }[] = [];
  const names: Record<number, string> = {};
  for (const outcome of outcomes) {
    for (const pid of outcome.report.requested) if (!targets.includes(pid)) targets.push(pid);
    for (const named of outcome.report.names ?? []) {
      if (named.name && outcome.report.requested.includes(named.pid)) names[named.pid] = named.name;
    }
    for (const row of outcome.report.excluded) {
      if (!excluded.some((e) => e.pid === row.pid)) excluded.push(row);
    }
    if (outcome.report.refusedReason && outcome.report.requested.length === 0) {
      const pid = outcome.target.target === 'pid' ? outcome.target.pid : null;
      if (pid !== null && !excluded.some((e) => e.pid === pid)) {
        excluded.push({ pid, reason: outcome.report.refusedReason });
      }
    }
  }
  return { targets, excluded, names };
}

/**
 * How a row is addressed for a kill. A registry row is killed by `spawnId` so
 * Rust can run the identity fence against what it recorded; a row we only know
 * from `ps` is killed by pid and reported as `unfenced`.
 */
export function killTargetOf(row: ProcRow): KillTarget {
  return row.spawnId
    ? { target: 'spawn', spawnId: row.spawnId }
    : { target: 'pid', pid: row.pid };
}

/**
 * How a row is addressed for a health Clean up: ALWAYS by pid. Rust accepts
 * only a pid that is a confirmed leftover of its latest health snapshot (pid +
 * start time) and refuses a `spawn` target outright — a leftover of a Gilde
 * session carries a `spawnId`, and addressing it by that would make the
 * clean-up impossible.
 */
export function healthKillTargetOf(row: ProcRow): KillTarget {
  return { target: 'pid', pid: row.pid };
}

/** A protected row carries no kill affordance at all; this is why (DESIGN §8-7). */
export function protectionReason(row: ProcRow): string {
  if (row.pid <= 1) return 'init';
  if (row.tags.includes('appBundleMain')) return 'appBundleMain';
  if (!row.userOwned) return 'foreignUid';
  return 'protected';
}

/** Bulk actions exist only where every row is provably ours (DESIGN §8-7). */
export function allowsBulkAction(group: ProcGroupKind): boolean {
  return group === 'oursOrphan';
}

// ---------------------------------------------------------------------------
// Polling — visible 3 s, otherwise 15 s, paused while the window is blurred
// ---------------------------------------------------------------------------

let pollTimer: ReturnType<typeof setInterval> | null = null;
let pollEveryMs = 0;
let listenersInstalled = false;
/** Set once a monitor surface has been mounted; before that nothing polls. */
let pollingArmed = false;
/** How many monitor surfaces (drawer panel, pane) are mounted right now. */
let mountedSurfaces = 0;

function clearPoll(): void {
  if (pollTimer) clearInterval(pollTimer);
  pollTimer = null;
  pollEveryMs = 0;
}

/** Stop the timer and forget the listeners. Tests and teardown. */
export function resetProcPolling(): void {
  clearPoll();
  pollingArmed = false;
  mountedSurfaces = 0;
}

export function procPollIntervalMs(state: ProcState): number {
  if (!state.windowFocused) return 0;
  return state.panelVisible ? POLL_ACTIVE_MS : POLL_BACKGROUND_MS;
}

// ---------------------------------------------------------------------------
// Slice
// ---------------------------------------------------------------------------

/** In-flight `procmon_cwd` pids, so a re-render never asks twice. */
const cwdInFlight = new Set<number>();

export const createProcSlice: StateCreator<
  ProcSlice & ToastSlice & UiSlice,
  [['zustand/subscribeWithSelector', never]],
  [],
  ProcSlice
> = (set, get) => {
  const patch = (next: Partial<ProcState>): void => set({ proc: { ...get().proc, ...next } });

  const patchKill = (next: Partial<KillPlanState>): void =>
    patch({ kill: { ...get().proc.kill, ...next } });

  const reschedule = (): void => {
    if (!pollingArmed) return;
    const wanted = procPollIntervalMs(get().proc);
    if (wanted === pollEveryMs) return;
    clearPoll();
    if (wanted === 0) return;
    pollEveryMs = wanted;
    pollTimer = setInterval(() => void get().scanProcesses(false), wanted);
  };

  const installListeners = (): void => {
    if (listenersInstalled || typeof window === 'undefined') return;
    listenersInstalled = true;
    const onFocus = () => {
      patch({ windowFocused: true });
      reschedule();
      void get().scanProcesses(false);
    };
    const onBlur = () => {
      patch({ windowFocused: false });
      reschedule();
    };
    window.addEventListener('focus', onFocus);
    window.addEventListener('blur', onBlur);
    if (typeof document !== 'undefined') {
      document.addEventListener('visibilitychange', () => {
        if (document.visibilityState === 'hidden') onBlur();
        else onFocus();
      });
      patch({ windowFocused: document.visibilityState !== 'hidden' && document.hasFocus() });
    }
  };

  // The watchdog broadcasts an unfiltered scan every 5 s; folding it in is free
  // and keeps the panel fresh between polls. Deferred by a microtask for the
  // same reason `sessionSlice` defers: the store is still being created here.
  queueMicrotask(() => {
    onGildeEvent(PROC_SCAN, (event) => {
      get().applyScan({ rows: event.rows, totals: event.totals, at: event.at });
    });
  });

  return {
    proc: initialProcState(),

    applyScan: (result) => {
      const state = get().proc;
      // Stale answers can overtake fresh ones: a forced scan and a poll can be
      // in flight together. The newest wins, and nothing older is applied.
      if (result.at > 0 && result.at < state.at) return;
      const rows = sortRows(result.rows);
      const live = new Set(rows.map((row) => row.pid));
      const selection = state.selection.pids.filter((pid) => live.has(pid));
      patch({
        rows,
        totals: result.totals,
        at: result.at,
        scanCount: state.scanCount + 1,
        loading: false,
        error: null,
        selection:
          selection.length === state.selection.pids.length
            ? state.selection
            : {
                group: selection.length > 0 ? state.selection.group : null,
                pids: selection,
              },
      });
    },

    scanProcesses: async (force = false) => {
      if (get().proc.loading && !force) return;
      patch({ loading: true });
      try {
        // Deliberately unfiltered: the footer totals describe the machine and
        // the filter is applied here (see the header comment).
        const result = await cmd.procmonScan({}, force);
        get().applyScan(result);
      } catch (e) {
        patch({ loading: false, error: isUnavailable(e) ? null : asGildeError(e) });
        if (!isUnavailable(e)) console.warn('[gilde] procmon_scan failed', e);
      }
      void get().refreshBudget();
    },

    refreshBudget: async () => {
      try {
        const budget = await cmd.budgetGet();
        patch({ budget });
      } catch (e) {
        if (!isUnavailable(e)) console.warn('[gilde] budget_get failed', e);
      }
      if (get().proc.supervision === null) {
        try {
          const config = await cmd.appSettingsGet();
          patch({ supervision: config.supervision });
        } catch (e) {
          if (!isUnavailable(e)) console.warn('[gilde] app_settings_get failed', e);
        }
      }
    },

    /**
     * Counted, not a flag: the drawer panel and a monitor pane can be mounted
     * at the same time, and the first one to unmount must not tell the poller
     * that the monitor is off screen.
     */
    setProcPanelVisible: (visible) => {
      installListeners();
      pollingArmed = true;
      mountedSurfaces = Math.max(0, mountedSurfaces + (visible ? 1 : -1));
      patch({ panelVisible: mountedSurfaces > 0 });
      reschedule();
      if (visible) void get().scanProcesses(false);
    },

    setProcQuery: (query) => patch({ filter: { ...get().proc.filter, query } }),

    setProcGroupFilter: (groups) =>
      patch({
        filter: { ...get().proc.filter, groups: groups && groups.length > 0 ? groups : null },
      }),

    toggleProcGroupFilter: (group) => {
      const current = get().proc.filter.groups;
      if (!current) {
        get().setProcGroupFilter([group]);
        return;
      }
      const next = current.includes(group)
        ? current.filter((g) => g !== group)
        : [...current, group];
      get().setProcGroupFilter(next);
    },

    toggleProcGroupCollapsed: (group) =>
      get().setProcGroupCollapsed(group, get().proc.collapsed[group] !== true),

    setProcGroupCollapsed: (group, collapsed) =>
      patch({ collapsed: { ...get().proc.collapsed, [group]: collapsed } }),

    toggleProcSelection: (row) => {
      const selection = get().proc.selection;
      // A selection that reached into a second group could not be judged by one
      // set of bulk rules, so a foreign group replaces the selection.
      if (selection.group !== row.group) {
        patch({ selection: { group: row.group, pids: [row.pid] } });
        return;
      }
      const pids = selection.pids.includes(row.pid)
        ? selection.pids.filter((pid) => pid !== row.pid)
        : [...selection.pids, row.pid];
      patch({ selection: { group: pids.length > 0 ? row.group : null, pids } });
    },

    selectProcGroup: (group, pids) =>
      patch({ selection: { group: pids.length > 0 ? group : null, pids: [...pids] } }),

    clearProcSelection: () => patch({ selection: { group: null, pids: [] } }),

    ensureProcCwd: (pid) => {
      if (pid <= 0) return;
      if (pid in get().proc.cwd || cwdInFlight.has(pid)) return;
      cwdInFlight.add(pid);
      void cmd
        .procmonCwd(pid)
        .then((cwd) => patch({ cwd: { ...get().proc.cwd, [pid]: cwd } }))
        .catch((e) => {
          if (!isUnavailable(e)) patch({ cwd: { ...get().proc.cwd, [pid]: null } });
        })
        .finally(() => cwdInFlight.delete(pid));
    },

    openKillSheet: (rows, options) => {
      // A protected row has no kill affordance anywhere in the UI; filtering
      // here means no code path can smuggle one into a bulk request either.
      const targets = rows.filter((row) => !row.tags.includes('protected'));
      if (targets.length === 0) return;
      const bulk = targets.length > 1;
      const health = options?.onlyHealthOrphans === true;
      // A bulk request — and EVERY health Clean up, one row too — opens on its
      // dry run: the user sees the exact target set Rust computed (every pid
      // that would be signalled, named) before anything is signalled.
      const preview = bulk || health;
      patch({
        kill: {
          open: true,
          rows: targets,
          bulk,
          dryRun: preview,
          preview: null,
          outcomes: null,
          running: false,
          error: null,
          onlyHealthOrphans: health,
        },
      });
      if (preview) void get().runKill();
    },

    closeKillSheet: () =>
      patchKill({ open: false, rows: [], preview: null, outcomes: null, onlyHealthOrphans: false }),

    setKillDryRun: (dryRun) => {
      patchKill({ dryRun, outcomes: null });
      if (dryRun) void get().runKill();
    },

    /**
     * Runs the sheet's current intent. With `dryRun` on this asks Rust for the
     * target and exclusion sets and sends no signal; with it off this is the
     * real thing, and the answer is rendered exactly as it comes back.
     */
    runKill: async () => {
      const plan = get().proc.kill;
      if (plan.rows.length === 0 || plan.running) return;
      // The real run happens once. Re-opening the sheet is how a second one is
      // requested, and that resets `outcomes`.
      if (!plan.dryRun && plan.outcomes) return;
      patchKill({ running: true, error: null });
      // A health Clean up that has not shown its dry run yet does not run.
      if (!plan.dryRun && plan.onlyHealthOrphans && !plan.preview) return;
      const targets = plan.rows.map(plan.onlyHealthOrphans ? healthKillTargetOf : killTargetOf);
      try {
        const outcomes = await cmd.procmonKill(targets, {
          dryRun: plan.dryRun,
          // A `.app` bundle main is never a target of a monitor action; DESIGN
          // §8-7 removes the affordance rather than confirming it away.
          allowBundleMain: false,
          // Health Clean up: Rust accepts only the confirmed leftovers.
          ...(plan.onlyHealthOrphans ? { onlyHealthOrphans: true } : {}),
        });
        if (plan.dryRun) patchKill({ running: false, preview: outcomes });
        else patchKill({ running: false, outcomes });
        // A health Clean up is followed by the next health tick (5 s), not by a
        // monitor scan: the monitor may not exist in this build.
        if (!plan.dryRun && !plan.onlyHealthOrphans) {
          get().clearProcSelection();
          void get().scanProcesses(true);
        }
      } catch (e) {
        patchKill({ running: false, error: isUnavailable(e) ? null : asGildeError(e) });
        if (!isUnavailable(e)) console.warn('[gilde] procmon_kill failed', e);
      }
    },
  };
};
