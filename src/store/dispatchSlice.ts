/**
 * Dispatch slice (T24) — the headless lane's state: tasks, runs, the live
 * stream folded into cards, and the composer's form.
 *
 * Three rules shape this file.
 *
 * 1. **The files on disk are the record** (DESIGN §10-2). What lives here is a
 *    view of `dispatch_list` / `dispatch_get` rows plus whatever
 *    `gilde://dispatch/event` has added since. Every terminal event therefore
 *    schedules a re-read of the task, so a card settles on the persisted row
 *    (`modelActual`, `costUsd`, `textPath`) and not on what the stream guessed.
 * 2. **Text is merged before it reaches React.** Rust already coalesces at
 *    100 ms; a burst still arrives as many small events after a resync, so
 *    `TextCoalescer` batches them into one store write per 100 ms
 *    (acceptance 9). A `set()` per chunk is exactly how a card at 200 events/s
 *    drops frames.
 * 3. **Quota never gates anything** (acceptance 2). It is an advisory chip: an
 *    absent snapshot reads "알 수 없음", never "충분함", the composer opens
 *    while it is still loading, and no code path here consults it before
 *    dispatching.
 *
 * Events are folded in through `onGildeEvent()` (the fan-out seam of
 * `hooks/useEventBridge.ts`), never through a second `listen()`.
 */

import { Channel } from '@tauri-apps/api/core';
import type { StateCreator } from 'zustand';

import { onGildeEvent } from '../hooks/useEventBridge';
import { asGildeError, call, cmd, isUnavailable } from '../lib/commands';
import { DISPATCH_EVENT } from '../lib/ipc';
import type {
  AdapterInfo,
  GildeErrorPayload,
  DispatchEventPayload,
  DispatchRun,
  DispatchSpec,
  DispatchTask,
  DispatchTaskDetail,
  DispatchTarget,
  DispatchVendor,
  ModelInfo,
  PermissionMode,
  ProjectId,
  PtyChunk,
  QuotaSnapshot,
  QuotaWindow,
  RouteDecision,
  RunId,
  RunStatus,
  TaskId,
  TokenUsage,
  UnixMillis,
  Vendor,
  WorktreeId,
  WorktreeStrategy,
} from '../lib/ipc';
import type { GraphSlice } from './graphSlice';
import type { SessionSlice } from './sessionSlice';
import type { ToastSlice } from './toastSlice';
import type { UiSlice } from './uiSlice';

// ---------------------------------------------------------------------------
// Constants
// ---------------------------------------------------------------------------

/** How many lines a collapsed card body shows (DESIGN §9-7: "마지막 ~40줄"). */
export const STREAM_TAIL_LINES = 40;
/** How much streamed text a run keeps in memory. The full text is on disk. */
export const STREAM_TAIL_CHARS = 64 * 1024;
/** Store writes are batched to this cadence (acceptance 9). */
export const TEXT_MERGE_MS = 100;
/** `mesh_quota` refresh cadence (acceptance 2). */
export const QUOTA_REFRESH_MS = 4 * 60_000;
/** Older than this and the chip carries a "stale" badge (acceptance 2). */
export const QUOTA_STALE_MS = 30 * 60_000;
/** Worst window at or above this turns the chip amber (acceptance 2). */
export const QUOTA_WARN_PERCENT = 80;
/** How many tasks the board lists. */
export const TASK_LIST_LIMIT = 50;
/** `config.dispatch.defaultTimeoutMinutes` is read at boot; this is the floor. */
export const DEFAULT_TIMEOUT_MINUTES = 45;

/** The one target value the host interprets itself (DESIGN §9-7). */
export const AUTO_VENDOR = 'auto';

/** Vendors the dispatch lane can address, in the order the picker shows them. */
export const DISPATCH_VENDORS: readonly Vendor[] = ['claude', 'codex', 'cursor', 'gemini'];

/**
 * The permission ladder, in the order the composer offers it: the rung a
 * headless run can actually work under first, then read-only, then the two a
 * user has to reach for (M1.5-A-dispatch, BACKLOG 1).
 */
export const PERMISSION_MODES: readonly PermissionMode[] = [
  'acceptEdits',
  'plan',
  'default',
  'bypassPermissions',
];

/**
 * What a new dispatch starts under (BACKLOG 1).
 *
 * `default` means "ask a human", and a headless run has no human to ask:
 * claude refused with `write permission not granted`, codex with `workspace is
 * read-only`, and every dispatch came back having changed nothing. The
 * safeguard is the worktree, not the prompt — each run edits its own isolated
 * checkout — so the composer starts on the edit rung. `acceptEdits` is
 * claude's word; `dispatch::engine::permission_mode_for` translates it per
 * vendor (codex `workspace-write`, cursor `enabled`, gemini `accept-edits`).
 *
 * It is also what `config.dispatch.permissionMode` already defaulted to in
 * Rust — the composer was overriding that with `default` on every submit.
 */
export const DEFAULT_PERMISSION_MODE: PermissionMode = 'acceptEdits';

// ---------------------------------------------------------------------------
// Types
// ---------------------------------------------------------------------------

/** One run as a card reads it: the persisted row plus what the stream added. */
export interface RunView {
  run: DispatchRun;
  /** Streamed text, capped at `STREAM_TAIL_CHARS`. The whole answer is on disk. */
  stream: string;
  /** The tool the adapter last reported — the ticker line under the body. */
  tool: string | null;
  /** Usage folded from `usage` events; falls back to the row's own. */
  usage: TokenUsage | null;
  costUsd: number | null;
  /** When the last event for this run landed. */
  updatedAt: UnixMillis;
}

export interface QuotaEntry {
  snapshot: QuotaSnapshot | null;
  /** When this front end asked. `0` means "never asked". */
  fetchedAt: UnixMillis;
  loading: boolean;
  /** The vendor has no quota surface, or the call failed. */
  unavailable: boolean;
}

export interface RouteState {
  loading: boolean;
  decision: RouteDecision | null;
  error: GildeErrorPayload | null;
  /** The prompt the decision was made for; a changed prompt invalidates it. */
  forPrompt: string;
}

/** One row of the composer's agent list. */
export interface TargetDraft {
  selected: boolean;
  /** Empty means "the adapter's default model". */
  model: string;
}

export interface ComposerState {
  open: boolean;
  prompt: string;
  projectId: ProjectId | '';
  baseRef: string;
  taskSlug: string;
  /** Keyed by `DispatchVendor`, `'auto'` included. */
  targets: Record<string, TargetDraft>;
  strategy: WorktreeStrategy['strategy'];
  existingWorktreeId: WorktreeId | '';
  timeoutMinutes: number;
  permissionMode: PermissionMode;
  busy: boolean;
  error: GildeErrorPayload | null;
}

export interface DispatchState {
  tasks: Map<TaskId, DispatchTask>;
  /** Task ids, newest first. */
  order: TaskId[];
  runs: Map<RunId, RunView>;
  /** Task id → its run ids, in creation order. */
  runsByTask: Record<TaskId, RunId[]>;
  selectedTaskId: TaskId | null;
  /** The card `j`/`k` moves between and `s`/`t`/`↵` act on. */
  focusedRunId: RunId | null;
  /** Run ids the user expanded (`↵`). */
  expanded: Record<RunId, true>;
  /** ⌘⇧C: equal body heights and synchronised scrolling. */
  compare: boolean;
  /** Cards per row in the grid. */
  columns: 1 | 2 | 3 | 4;
  loading: boolean;
  loaded: boolean;
  error: GildeErrorPayload | null;
  adapters: AdapterInfo[];
  /** Vendor → `mesh_models`. Absent means "not asked yet". */
  models: Record<string, ModelInfo[]>;
  quota: Record<string, QuotaEntry>;
  route: RouteState;
  composer: ComposerState;
}

export interface DispatchSlice {
  dispatch: DispatchState;

  loadTasks: () => Promise<void>;
  loadTask: (taskId: TaskId) => Promise<void>;
  selectTask: (taskId: TaskId | null) => void;
  focusRun: (runId: RunId | null) => void;
  moveRunFocus: (delta: number) => void;
  toggleRunExpanded: (runId: RunId) => void;
  setCompare: (compare: boolean) => void;
  setColumns: (columns: 1 | 2 | 3 | 4) => void;

  loadAdapters: () => Promise<void>;
  loadModels: (vendor: Vendor) => Promise<void>;
  refreshQuota: (vendor: Vendor, force?: boolean) => Promise<void>;
  previewRoute: (prompt: string) => Promise<void>;

  setComposerOpen: (open: boolean) => void;
  patchComposer: (patch: Partial<ComposerState>) => void;
  setTargetSelected: (vendor: DispatchVendor, selected: boolean) => void;
  setTargetModel: (vendor: DispatchVendor, model: string) => void;
  submitComposer: () => Promise<DispatchTask | null>;

  cancelTask: (taskId: TaskId) => Promise<void>;
  cancelRun: (runId: RunId) => Promise<void>;
  resumeRun: (runId: RunId, message: string) => Promise<void>;
  rerunRun: (runId: RunId) => Promise<void>;
  openRunTerminal: (runId: RunId) => Promise<void>;
  revealRunWorktree: (runId: RunId) => Promise<void>;
  copyRunResult: (runId: RunId) => Promise<boolean>;
  clearDispatchError: () => void;
}

// ---------------------------------------------------------------------------
// Slug — the same pipeline `src-tauri/src/worktree/naming.rs::sanitize` runs
// ---------------------------------------------------------------------------

const LATIN_FOLD: Record<string, string> = {
  æ: 'ae',
  œ: 'oe',
  ß: 'ss',
  ø: 'o',
  đ: 'd',
  ħ: 'h',
  ı: 'i',
  ł: 'l',
  ŧ: 't',
};

/**
 * `naming.rs::sanitize` in the browser: lowercase, fold compatibility
 * characters, collapse every run outside `[a-z0-9]` into a single `-`, trim.
 *
 * `dispatch::engine::create` runs `sanitize()` on `spec.taskSlug` and refuses
 * `emptySlug` when the result is empty, so this is the client-side half of the
 * *same* rule (acceptance 1). It deliberately does **not** truncate: the
 * dispatch path does not, and `store/worktreeSlice.ts::slugBody` — which does,
 * because `naming::task_slug` truncates before appending its short id — is the
 * worktree dialog's promise, not this one's.
 */
export function sanitizeSlug(name: string): string {
  const folded = name
    .normalize('NFKD')
    .replace(/[\u0300-\u036f]/g, '')
    .toLowerCase()
    .replace(/[æœßøđħıłŧ]/g, (ch) => LATIN_FOLD[ch] ?? ch);

  let out = '';
  let pendingDash = false;
  for (const ch of folded) {
    if ((ch >= 'a' && ch <= 'z') || (ch >= '0' && ch <= '9')) {
      if (pendingDash && out.length > 0) out += '-';
      pendingDash = false;
      out += ch;
    } else {
      pendingDash = true;
    }
  }
  return out;
}

/** Why the composer refuses to submit. `null` means the slug is usable. */
export type SlugIssue = 'empty' | 'unsupported';

export function slugIssue(name: string): SlugIssue | null {
  if (name.trim().length === 0) return 'empty';
  return sanitizeSlug(name).length === 0 ? 'unsupported' : null;
}

/** The first line of the prompt, which is what Rust titles a task with. */
export function titleOfPrompt(prompt: string): string {
  const first = prompt.split('\n', 1)[0]?.trim() ?? '';
  return first.length > 80 ? `${first.slice(0, 80)}…` : first;
}

// ---------------------------------------------------------------------------
// Streaming text
// ---------------------------------------------------------------------------

/** Append a chunk and keep the tail bounded. */
export function appendStream(current: string, chunk: string): string {
  const next = current + chunk;
  return next.length > STREAM_TAIL_CHARS ? next.slice(next.length - STREAM_TAIL_CHARS) : next;
}

/** The last `lines` lines — what a collapsed card body renders. */
export function tailLines(text: string, lines = STREAM_TAIL_LINES): string {
  const all = text.split('\n');
  return all.length <= lines ? text : all.slice(all.length - lines).join('\n');
}

/**
 * Merges text chunks into one store write per `flushMs` (acceptance 9).
 *
 * Kept as a class with an injectable clock-free `flushNow()` so the merge is
 * testable without timers: the tests push a burst, assert the sink has not run,
 * then flush once and assert a single call carrying the concatenation.
 */
export class TextCoalescer {
  private pending = new Map<RunId, string>();
  private timer: ReturnType<typeof setTimeout> | null = null;

  constructor(
    private readonly sink: (batch: Map<RunId, string>) => void,
    private readonly flushMs: number = TEXT_MERGE_MS,
  ) {}

  push(runId: RunId, chunk: string): void {
    if (!chunk) return;
    this.pending.set(runId, (this.pending.get(runId) ?? '') + chunk);
    if (this.timer) return;
    this.timer = setTimeout(() => {
      this.timer = null;
      this.flushNow();
    }, this.flushMs);
  }

  /** Number of runs with unflushed text — the tests' only window in. */
  get pendingSize(): number {
    return this.pending.size;
  }

  flushNow(): void {
    if (this.pending.size === 0) return;
    const batch = this.pending;
    this.pending = new Map();
    this.sink(batch);
  }

  stop(): void {
    if (this.timer) clearTimeout(this.timer);
    this.timer = null;
    this.pending = new Map();
  }
}

// ---------------------------------------------------------------------------
// Quota (advisory only — acceptance 2)
// ---------------------------------------------------------------------------

export function emptyQuotaEntry(): QuotaEntry {
  return { snapshot: null, fetchedAt: 0, loading: false, unavailable: false };
}

/** The window closest to its limit — what the chip reports. */
export function worstWindow(snapshot: QuotaSnapshot | null): QuotaWindow | null {
  if (!snapshot || snapshot.windows.length === 0) return null;
  let worst = snapshot.windows[0];
  for (const window of snapshot.windows) {
    if (window.usedPercent > worst.usedPercent) worst = window;
  }
  return worst;
}

/** `'warn'` at or above `QUOTA_WARN_PERCENT`; `'neutral'` otherwise or unknown. */
export function quotaTone(entry: QuotaEntry | undefined): 'neutral' | 'warn' {
  const worst = worstWindow(entry?.snapshot ?? null);
  if (!worst) return 'neutral';
  return worst.usedPercent >= QUOTA_WARN_PERCENT ? 'warn' : 'neutral';
}

/**
 * True when the snapshot is older than 30 minutes. The vendor's own
 * `capturedAt` wins over the fetch time — a cached answer is as old as the
 * capture, not as old as the call that returned it.
 */
export function quotaStale(entry: QuotaEntry | undefined, now: number): boolean {
  if (!entry || !entry.snapshot) return false;
  const captured = entry.snapshot.capturedAt ? Date.parse(entry.snapshot.capturedAt) : NaN;
  const at = Number.isFinite(captured) ? captured : entry.fetchedAt;
  if (!at) return false;
  return now - at > QUOTA_STALE_MS;
}

// ---------------------------------------------------------------------------
// Event folding (pure — exercised directly by dispatch.test.tsx)
// ---------------------------------------------------------------------------

function num(value: unknown): number | null {
  return typeof value === 'number' && Number.isFinite(value) ? value : null;
}

function str(value: unknown): string | null {
  return typeof value === 'string' ? value : null;
}

function record(value: unknown): Record<string, unknown> | null {
  return typeof value === 'object' && value !== null ? (value as Record<string, unknown>) : null;
}

/** `RunResult.status` → `RunStatus`, mirroring `RunPhase::from_run_status`. */
export function phaseOfResultStatus(status: unknown): RunStatus {
  switch (status) {
    case 'error':
      return 'error';
    case 'stopped':
      return 'stopped';
    case 'incomplete':
      return 'incomplete';
    default:
      return 'done';
  }
}

function usageOf(value: unknown): TokenUsage | null {
  const raw = record(value);
  return raw ? (raw as TokenUsage) : null;
}

/** The view a `dispatch_get` row starts as, before any event. */
export function viewOfRun(run: DispatchRun, previous?: RunView): RunView {
  return {
    run,
    // A restored card shows the persisted tail; the live stream appends to it.
    stream: previous ? previous.stream : run.text,
    tool: previous?.tool ?? null,
    usage: run.usage ?? previous?.usage ?? null,
    costUsd: run.costUsd ?? previous?.costUsd ?? null,
    updatedAt: run.finishedAt ?? run.startedAt ?? previous?.updatedAt ?? 0,
  };
}

/**
 * Fold one `gilde://dispatch/event` into a run view.
 *
 * Every vendor takes this path. A codex run — which has no `stream`
 * capability, so the engine used `mesh.run` — produces exactly one `done`
 * event and lands here with the same code that a streaming claude run walks
 * chunk by chunk (acceptance 7). There is no vendor branch in this function,
 * and that is the point.
 */
export function foldRunEvent(view: RunView, event: DispatchEventPayload, at: number): RunView {
  const payload = record(event.payload);
  const run = view.run;

  switch (event.kind) {
    case 'routed':
      return {
        ...view,
        run: { ...run, routeNote: str(payload?.note) ?? run.routeNote },
        updatedAt: at,
      };

    case 'spawned':
      return {
        ...view,
        run: {
          ...run,
          status: 'spawning',
          startedAt: run.startedAt ?? at,
          modelRequested: str(payload?.model) ?? run.modelRequested,
          worktreeId: (str(payload?.worktreeId) as WorktreeId | null) ?? run.worktreeId,
        },
        updatedAt: at,
      };

    case 'text': {
      const chunk = str(payload?.text) ?? '';
      return {
        ...view,
        run: run.status === 'spawning' || run.status === 'queued' ? { ...run, status: 'running' } : run,
        stream: appendStream(view.stream, chunk),
        updatedAt: at,
      };
    }

    case 'tool':
      return {
        ...view,
        run: run.status === 'spawning' || run.status === 'queued' ? { ...run, status: 'running' } : run,
        tool: str(payload?.name),
        updatedAt: at,
      };

    case 'usage': {
      const usage = usageOf(payload?.usage);
      return {
        ...view,
        usage: usage ?? view.usage,
        costUsd: num(usage?.costUsd) ?? view.costUsd,
        updatedAt: at,
      };
    }

    case 'done': {
      const result = record(payload?.result);
      const usage = usageOf(result?.usage) ?? view.usage;
      const text = str(result?.text);
      return {
        ...view,
        run: {
          ...run,
          status: phaseOfResultStatus(result?.status),
          modelActual: str(result?.model) ?? run.modelActual,
          conversationId: str(result?.conversationId) ?? run.conversationId,
          // The adapter's note is shown verbatim (acceptance 4).
          note: str(result?.note) ?? run.note,
          finishedAt: run.finishedAt ?? at,
        },
        // The full answer replaces the streamed tail: it is the verbatim one.
        stream: text !== null && text.length > 0 ? appendStream('', text) : view.stream,
        tool: null,
        usage,
        costUsd: num(usage?.costUsd) ?? view.costUsd,
        updatedAt: at,
      };
    }

    case 'error':
      return {
        ...view,
        run: {
          ...run,
          status: 'error',
          error: str(payload?.message) ?? run.error,
          finishedAt: run.finishedAt ?? at,
        },
        tool: null,
        updatedAt: at,
      };

    case 'interrupted':
      return {
        ...view,
        run: {
          ...run,
          // Never `error`: the app failed, not the agent (DESIGN §5-10).
          status: 'interrupted',
          error: str(payload?.reason) || run.error,
          finishedAt: run.finishedAt ?? at,
        },
        tool: null,
        updatedAt: at,
      };

    case 'cancelled':
      return {
        ...view,
        run: { ...run, status: 'cancelled', finishedAt: run.finishedAt ?? at },
        tool: null,
        updatedAt: at,
      };

    default:
      return { ...view, updatedAt: at };
  }
}

/** How long a run has been going: `finishedAt` when it is over, else now. */
export function elapsedMs(run: DispatchRun, now: number): number {
  if (!run.startedAt) return 0;
  return Math.max(0, (run.finishedAt ?? now) - run.startedAt);
}

/** `2/3 완료` — how many runs of a task have reached a terminal status. */
export function taskProgress(runs: readonly RunView[]): { done: number; total: number } {
  let done = 0;
  for (const view of runs) if (isTerminalRun(view.run.status)) done += 1;
  return { done, total: runs.length };
}

export function isTerminalRun(status: RunStatus): boolean {
  return status !== 'queued' && status !== 'spawning' && status !== 'running';
}

/** A run can only be continued when the vendor handed back a conversation id. */
export function isResumable(run: DispatchRun): boolean {
  return Boolean(run.conversationId && run.conversationId.length > 0);
}

// ---------------------------------------------------------------------------
// Initial state
// ---------------------------------------------------------------------------

function initialTargets(): Record<string, TargetDraft> {
  const targets: Record<string, TargetDraft> = { [AUTO_VENDOR]: { selected: false, model: '' } };
  for (const vendor of DISPATCH_VENDORS) targets[vendor] = { selected: false, model: '' };
  return targets;
}

export function initialComposerState(): ComposerState {
  return {
    open: false,
    prompt: '',
    projectId: '',
    baseRef: '',
    taskSlug: '',
    targets: initialTargets(),
    strategy: 'perTarget',
    existingWorktreeId: '',
    timeoutMinutes: DEFAULT_TIMEOUT_MINUTES,
    permissionMode: DEFAULT_PERMISSION_MODE,
    busy: false,
    error: null,
  };
}

export function initialDispatchState(): DispatchState {
  return {
    tasks: new Map(),
    order: [],
    runs: new Map(),
    runsByTask: {},
    selectedTaskId: null,
    focusedRunId: null,
    expanded: {},
    compare: false,
    columns: 1,
    loading: false,
    loaded: false,
    error: null,
    adapters: [],
    models: {},
    quota: {},
    route: { loading: false, decision: null, error: null, forPrompt: '' },
    composer: initialComposerState(),
  };
}

/** Fold a `dispatch_get` answer into the state (acceptance 10). */
export function applyTaskDetail(state: DispatchState, detail: DispatchTaskDetail): DispatchState {
  const tasks = new Map(state.tasks);
  tasks.set(detail.task.id, detail.task);
  const order = state.order.includes(detail.task.id)
    ? state.order
    : [detail.task.id, ...state.order];

  const runs = new Map(state.runs);
  const ids: RunId[] = [];
  for (const run of detail.runs) {
    ids.push(run.id);
    runs.set(run.id, viewOfRun(run, runs.get(run.id)));
  }

  return {
    ...state,
    tasks,
    order,
    runs,
    runsByTask: { ...state.runsByTask, [detail.task.id]: ids },
  };
}

/** The runs of a task, in creation order. */
export function runsOfTask(
  state: Pick<DispatchState, 'runs' | 'runsByTask'>,
  taskId: TaskId | null,
): RunView[] {
  if (!taskId) return [];
  const ids = state.runsByTask[taskId] ?? [];
  const out: RunView[] = [];
  for (const id of ids) {
    const view = state.runs.get(id);
    if (view) out.push(view);
  }
  return out;
}

// ---------------------------------------------------------------------------
// Clipboard seam — `navigator.clipboard` does not exist under the SSR tests
// ---------------------------------------------------------------------------

export type ClipboardWriter = (text: string) => Promise<void>;

let clipboardWriter: ClipboardWriter | null = null;

export function setClipboardWriter(writer: ClipboardWriter | null): void {
  clipboardWriter = writer;
}

/** Copy one string, honouring the injected writer the tests install. */
export async function copyToClipboard(text: string): Promise<boolean> {
  const writer =
    clipboardWriter ??
    (typeof navigator !== 'undefined' && navigator.clipboard
      ? (value: string) => navigator.clipboard.writeText(value)
      : null);
  if (!writer) return false;
  try {
    await writer(text);
    return true;
  } catch {
    return false;
  }
}

// ---------------------------------------------------------------------------
// Slice
// ---------------------------------------------------------------------------

/** A sink for the bytes a dispatch-opened pty produces before the pane mounts. */
function discardingChannel(): Channel<PtyChunk> {
  const channel = new Channel<PtyChunk>();
  // The ring is the durable copy; `XTermHost` catches up with
  // `session_snapshot` when it attaches. Dropping here loses nothing.
  channel.onmessage = () => {};
  return channel;
}

export const createDispatchSlice: StateCreator<
  DispatchSlice & GraphSlice & SessionSlice & ToastSlice & UiSlice,
  [['zustand/subscribeWithSelector', never]],
  [],
  DispatchSlice
> = (set, get) => {
  const patch = (update: Partial<DispatchState>) =>
    set({ dispatch: { ...get().dispatch, ...update } });

  const patchComposerState = (update: Partial<ComposerState>) =>
    patch({ composer: { ...get().dispatch.composer, ...update } });

  const patchRun = (runId: RunId, update: (view: RunView) => RunView): void => {
    const current = get().dispatch.runs.get(runId);
    if (!current) return;
    const runs = new Map(get().dispatch.runs);
    runs.set(runId, update(current));
    patch({ runs });
  };

  // ── streamed text: one store write per 100 ms, never one per chunk ────────
  const coalescer = new TextCoalescer((batch) => {
    const state = get().dispatch;
    let runs: Map<RunId, RunView> | null = null;
    const at = Date.now();
    for (const [runId, chunk] of batch) {
      const current = (runs ?? state.runs).get(runId);
      if (!current) continue;
      if (!runs) runs = new Map(state.runs);
      runs.set(runId, {
        ...current,
        run:
          current.run.status === 'spawning' || current.run.status === 'queued'
            ? { ...current.run, status: 'running' }
            : current.run,
        stream: appendStream(current.stream, chunk),
        updatedAt: at,
      });
    }
    if (runs) patch({ runs });
  });

  // Terminal events settle the card against the persisted row rather than
  // against what the stream guessed. Coalesced so a fan-out of four finishing
  // at once costs one read.
  const pendingRefresh = new Set<TaskId>();
  let refreshTimer: ReturnType<typeof setTimeout> | null = null;
  const scheduleRefresh = (taskId: TaskId) => {
    pendingRefresh.add(taskId);
    if (refreshTimer) return;
    refreshTimer = setTimeout(() => {
      refreshTimer = null;
      const ids = [...pendingRefresh];
      pendingRefresh.clear();
      for (const id of ids) void get().loadTask(id);
    }, 250);
  };

  // ── event fan-out (one Tauri listener, in `useEventBridge`) ───────────────
  //
  // Deferred by a microtask for the same reason `sessionSlice` defers: this
  // slice runs while the store is being created and `useEventBridge` imports
  // the store back.
  queueMicrotask(() => {
    onGildeEvent(DISPATCH_EVENT, (event) => {
      const state = get().dispatch;
      if (!state.runs.has(event.runId)) {
        // A run this board has never seen — a task created in another window,
        // or one older than the list. Pull the task in rather than guess.
        if (state.tasks.has(event.taskId)) scheduleRefresh(event.taskId);
        return;
      }
      if (event.kind === 'text') {
        const chunk = record(event.payload)?.text;
        coalescer.push(event.runId, typeof chunk === 'string' ? chunk : '');
        return;
      }
      // Any other kind orders against the pending text: flush first.
      coalescer.flushNow();
      const at = Date.now();
      patchRun(event.runId, (view) => foldRunEvent(view, event, at));
      if (event.kind === 'done' || event.kind === 'error' || event.kind === 'interrupted' ||
          event.kind === 'cancelled') {
        scheduleRefresh(event.taskId);
      }
    });
  });

  return {
    dispatch: initialDispatchState(),

    // -- board -------------------------------------------------------------

    loadTasks: async () => {
      patch({ loading: true });
      try {
        const rows = await cmd.dispatchList('recent', TASK_LIST_LIMIT);
        const tasks = new Map<TaskId, DispatchTask>();
        const order: TaskId[] = [];
        for (const task of rows) {
          tasks.set(task.id, task);
          order.push(task.id);
        }
        const selectedTaskId =
          get().dispatch.selectedTaskId && tasks.has(get().dispatch.selectedTaskId as TaskId)
            ? get().dispatch.selectedTaskId
            : (order[0] ?? null);
        patch({ tasks, order, selectedTaskId, loading: false, loaded: true, error: null });
        if (selectedTaskId) await get().loadTask(selectedTaskId);
      } catch (e) {
        if (isUnavailable(e)) {
          patch({ loading: false, loaded: true });
          return;
        }
        patch({ loading: false, loaded: true, error: asGildeError(e) });
      }
    },

    loadTask: async (taskId) => {
      try {
        const detail = await cmd.dispatchGet(taskId);
        const next = applyTaskDetail(get().dispatch, detail);
        const focusedRunId =
          next.focusedRunId && next.runs.has(next.focusedRunId)
            ? next.focusedRunId
            : (next.runsByTask[taskId]?.[0] ?? next.focusedRunId);
        set({ dispatch: { ...next, focusedRunId } });
      } catch (e) {
        if (!isUnavailable(e)) patch({ error: asGildeError(e) });
      }
    },

    selectTask: (taskId) => {
      const runIds = taskId ? (get().dispatch.runsByTask[taskId] ?? []) : [];
      patch({ selectedTaskId: taskId, focusedRunId: runIds[0] ?? null });
      if (taskId) void get().loadTask(taskId);
    },

    focusRun: (focusedRunId) => patch({ focusedRunId }),

    moveRunFocus: (delta) => {
      const state = get().dispatch;
      const ids = state.selectedTaskId ? (state.runsByTask[state.selectedTaskId] ?? []) : [];
      if (ids.length === 0) return;
      const current = state.focusedRunId ? ids.indexOf(state.focusedRunId) : -1;
      const base = current < 0 ? (delta > 0 ? -1 : 0) : current;
      const next = Math.min(ids.length - 1, Math.max(0, base + delta));
      patch({ focusedRunId: ids[next] });
    },

    toggleRunExpanded: (runId) => {
      const expanded = { ...get().dispatch.expanded };
      if (expanded[runId]) delete expanded[runId];
      else expanded[runId] = true;
      patch({ expanded });
    },

    setCompare: (compare) => patch({ compare }),
    setColumns: (columns) => patch({ columns }),

    // -- adapters, models, quota, route -------------------------------------

    loadAdapters: async () => {
      try {
        const adapters = await cmd.meshAdapters();
        patch({ adapters });
      } catch (e) {
        if (!isUnavailable(e)) console.warn('[gilde] mesh_adapters failed', e);
      }
    },

    loadModels: async (vendor) => {
      if (get().dispatch.models[vendor]) return;
      try {
        const models = await cmd.meshModels(vendor);
        patch({ models: { ...get().dispatch.models, [vendor]: models } });
      } catch (e) {
        if (!isUnavailable(e)) console.warn('[gilde] mesh_models failed', e);
        // An empty list means "no list", which the picker renders as the
        // adapter's default. It never blocks selecting the vendor.
        patch({ models: { ...get().dispatch.models, [vendor]: [] } });
      }
    },

    refreshQuota: async (vendor, force = false) => {
      const state = get().dispatch;
      const entry = state.quota[vendor] ?? emptyQuotaEntry();
      if (entry.loading) return;
      if (!force && entry.fetchedAt && Date.now() - entry.fetchedAt < QUOTA_REFRESH_MS) return;
      patch({ quota: { ...state.quota, [vendor]: { ...entry, loading: true } } });
      try {
        const snapshot = await cmd.meshQuota(vendor);
        patch({
          quota: {
            ...get().dispatch.quota,
            [vendor]: {
              snapshot,
              fetchedAt: Date.now(),
              loading: false,
              unavailable: snapshot === null,
            },
          },
        });
      } catch (e) {
        if (!isUnavailable(e)) console.warn('[gilde] mesh_quota failed', e);
        // Unknown, not "fine". The chip says so and the composer stays open.
        patch({
          quota: {
            ...get().dispatch.quota,
            [vendor]: { snapshot: null, fetchedAt: Date.now(), loading: false, unavailable: true },
          },
        });
      }
    },

    previewRoute: async (prompt) => {
      const trimmed = prompt.trim();
      if (!trimmed) {
        patch({ route: { loading: false, decision: null, error: null, forPrompt: '' } });
        return;
      }
      patch({ route: { ...get().dispatch.route, loading: true, error: null } });
      try {
        // `dispatch_route` has no wrapper in `lib/commands.ts` (T05 owns that
        // file); `call()` is the typed entry point and gives the same checking.
        const decision = await call('dispatch_route', { prompt: trimmed });
        patch({ route: { loading: false, decision, error: null, forPrompt: trimmed } });
      } catch (e) {
        patch({
          route: {
            loading: false,
            decision: null,
            error: isUnavailable(e) ? null : asGildeError(e),
            forPrompt: trimmed,
          },
        });
      }
    },

    // -- composer -----------------------------------------------------------

    setComposerOpen: (open) =>
      patchComposerState(open ? { open: true, error: null } : { open: false, busy: false }),

    patchComposer: (update) => patchComposerState(update),

    setTargetSelected: (vendor, selected) => {
      const composer = get().dispatch.composer;
      const current = composer.targets[vendor] ?? { selected: false, model: '' };
      patchComposerState({
        targets: { ...composer.targets, [vendor]: { ...current, selected } },
      });
    },

    setTargetModel: (vendor, model) => {
      const composer = get().dispatch.composer;
      const current = composer.targets[vendor] ?? { selected: false, model: '' };
      patchComposerState({
        targets: { ...composer.targets, [vendor]: { ...current, model } },
      });
    },

    submitComposer: async () => {
      const composer = get().dispatch.composer;
      // The note the user was already shown travels with the target, so the
      // run row records the decision the user actually saw (DESIGN §9-7).
      const routeNote = get().dispatch.route.decision?.note ?? '';
      const targets: DispatchTarget[] = [];
      for (const [vendor, draft] of Object.entries(composer.targets)) {
        if (!draft.selected) continue;
        targets.push({
          vendor: vendor as DispatchVendor,
          ...(draft.model ? { model: draft.model } : {}),
          ...(vendor === AUTO_VENDOR && routeNote ? { routeNote } : {}),
        });
      }
      if (!composer.projectId || targets.length === 0) return null;
      if (slugIssue(composer.taskSlug)) return null;

      const worktreeStrategy: WorktreeStrategy =
        composer.strategy === 'existing'
          ? { strategy: 'existing', worktreeId: composer.existingWorktreeId as WorktreeId }
          : { strategy: composer.strategy };

      const spec: DispatchSpec = {
        prompt: composer.prompt,
        projectId: composer.projectId,
        taskSlug: sanitizeSlug(composer.taskSlug),
        targets,
        worktreeStrategy,
        timeoutMs: Math.max(1, composer.timeoutMinutes) * 60_000,
        permissionMode: composer.permissionMode,
        ...(composer.baseRef.trim() ? { baseRef: composer.baseRef.trim() } : {}),
        ...(titleOfPrompt(composer.prompt) ? { title: titleOfPrompt(composer.prompt) } : {}),
      };

      patchComposerState({ busy: true, error: null });
      try {
        const task = await cmd.dispatchCreate(spec);
        const state = get().dispatch;
        const tasks = new Map(state.tasks);
        tasks.set(task.id, task);
        set({
          dispatch: {
            ...state,
            tasks,
            order: [task.id, ...state.order.filter((id) => id !== task.id)],
            selectedTaskId: task.id,
            composer: { ...initialComposerState(), projectId: composer.projectId },
          },
        });
        await get().loadTask(task.id);
        return task;
      } catch (e) {
        patchComposerState({ busy: false, error: asGildeError(e) });
        return null;
      }
    },

    // -- run actions --------------------------------------------------------

    cancelTask: async (taskId) => {
      try {
        await cmd.dispatchCancel(taskId);
        await get().loadTask(taskId);
      } catch (e) {
        get().pushErrorToast(asGildeError(e));
      }
    },

    cancelRun: async (runId) => {
      const view = get().dispatch.runs.get(runId);
      try {
        await cmd.dispatchCancelRun(runId);
        if (view) await get().loadTask(view.run.taskId);
      } catch (e) {
        get().pushErrorToast(asGildeError(e));
      }
    },

    resumeRun: async (runId, message) => {
      const view = get().dispatch.runs.get(runId);
      try {
        await cmd.dispatchResume(runId, message);
        if (view) await get().loadTask(view.run.taskId);
      } catch (e) {
        get().pushErrorToast(asGildeError(e));
      }
    },

    rerunRun: async (runId) => {
      const state = get().dispatch;
      const view = state.runs.get(runId);
      const task = view ? state.tasks.get(view.run.taskId) : undefined;
      if (!view || !task) return;
      const spec: DispatchSpec = {
        prompt: task.prompt,
        projectId: task.projectId,
        taskSlug: task.taskSlug,
        targets: [
          {
            vendor: view.run.vendor,
            ...(view.run.modelRequested ? { model: view.run.modelRequested } : {}),
          },
        ],
        worktreeStrategy: task.worktreeStrategy,
        timeoutMs: task.timeoutMs,
        permissionMode: task.permissionMode,
        ...(task.baseRef ? { baseRef: task.baseRef } : {}),
        title: task.title,
      };
      try {
        const created = await cmd.dispatchCreate(spec);
        patch({ selectedTaskId: created.id });
        await get().loadTasks();
      } catch (e) {
        get().pushErrorToast(asGildeError(e));
      }
    },

    openRunTerminal: async (runId) => {
      const view = get().dispatch.runs.get(runId);
      try {
        const info = await cmd.dispatchOpenTerminal(runId, discardingChannel());
        // The pty lane owns the session map; re-reading it is how this stays
        // out of `sessionSlice`'s internals.
        await get().loadSessions();
        get().openTab({
          title: info.agent,
          sublabel: view?.run.vendor,
          descriptor: { view: 'terminal', sessionId: info.id },
          status: 'idle',
          closable: true,
        });
      } catch (e) {
        get().pushErrorToast(asGildeError(e));
      }
    },

    revealRunWorktree: async (runId) => {
      const view = get().dispatch.runs.get(runId);
      if (!view) return;
      const node = get().graph.nodes.get(`run:${runId}`);
      const cwd =
        node?.meta.cwd ??
        (view.run.worktreeId
          ? get().graph.nodes.get(`worktree:${view.run.worktreeId}`)?.meta.cwd
          : undefined);
      if (!cwd) {
        get().pushToast({
          level: 'warn',
          key: 'dispatch.card.noWorktree',
          params: {},
          detail: null,
          actionId: null,
          undoToken: null,
          ttlMs: 6000,
        });
        return;
      }
      try {
        await cmd.appReveal(cwd);
      } catch (e) {
        get().pushErrorToast(asGildeError(e));
      }
    },

    copyRunResult: async (runId) => {
      const view = get().dispatch.runs.get(runId);
      if (!view) return false;
      const ok = await copyToClipboard(view.stream || view.run.text);
      get().pushToast({
        level: ok ? 'success' : 'warn',
        key: ok ? 'dispatch.card.copied' : 'dispatch.card.copyFailed',
        params: {},
        detail: null,
        actionId: null,
        undoToken: null,
        ttlMs: 4000,
      });
      return ok;
    },

    clearDispatchError: () => patch({ error: null }),
  };
};
