/**
 * Session slice (T20) — everything the terminal lane needs *except* the bytes.
 *
 * DESIGN §9-3 rule 1 is the whole design of this file: **PTY bytes never enter
 * the store.** A chunk goes `Channel → base64 decode → term.write(Uint8Array)`
 * inside `terminal/terminalPool.ts` and is never seen by React. What lives here
 * is the small, slow-moving supervision state a human reads: which sessions
 * exist, what each one is doing, the evidence behind that claim, and whether it
 * rang the bell.
 *
 * Events are folded in through `onGildeEvent()` (the fan-out seam of
 * `hooks/useEventBridge.ts`), never through a second `listen()`.
 */

import { Channel } from '@tauri-apps/api/core';
import type { StateCreator } from 'zustand';

import { nodeStatusOfSession } from '../components/StatusDot';
import { onGildeEvent } from '../hooks/useEventBridge';
import { translate } from '../i18n';
import { asGildeError, cmd, isUnavailable } from '../lib/commands';
import { textToB64 } from '../lib/codec';
import { SESSION_EXIT, SESSION_STATUS } from '../lib/ipc';
import type {
  AgentKindKey,
  CloseMode,
  KillReport,
  OpenSessionReq,
  PathsInfo,
  ProjectId,
  PtyChunk,
  PtySessionInfo,
  PtySignal,
  SessionExitEvent,
  SessionId,
  SessionStatus,
  SessionStatusEvent,
  StatusEvidence,
  TerminalConfig,
  UnixMillis,
  WorktreeId,
} from '../lib/ipc';
import { closeTerminal } from '../terminal/terminalPool';
import type { ToastSlice } from './toastSlice';
import type { Tab, UiSlice } from './uiSlice';

// ---------------------------------------------------------------------------
// Types
// ---------------------------------------------------------------------------

export interface SessionExitFacts {
  code: number | null;
  signal: number | null;
  at: UnixMillis;
}

/** One live pty session as the UI knows it. */
export interface SessionView {
  id: SessionId;
  info: PtySessionInfo;
  status: SessionStatus;
  /** 0..1. Below 0.5 the pill is drawn hollow and dashed (DESIGN §6-4). */
  confidence: number;
  evidence: StatusEvidence[];
  /** When the current status started. */
  since: UnixMillis;
  updatedAt: UnixMillis;
  /** BEL was received and the tab has not been looked at since (DESIGN §6-4). */
  attention: boolean;
  exit: SessionExitFacts | null;
  /** The ring died with the old process: reattached, but with no scrollback. */
  scrollbackLost: boolean;
  /**
   * M1.5-6: this row outlived the process that owned it — the app was killed
   * rather than closed, so its pty died with it (SIGHUP) and nothing ever
   * recorded an exit. The row is here so the session does not vanish without a
   * trace; the only action left on it is to dismiss it.
   *
   * Optional so that a fixture describing a live session does not have to spell
   * out that it is not a corpse; `viewOfInfo` always sets it.
   */
  hostCrash?: boolean;
}

export interface NewSessionState {
  open: boolean;
  /** Pre-filled target; `null` means "resolve the active worktree on open". */
  worktreeId: WorktreeId | null;
}

export interface SessionSlice {
  sessions: Map<SessionId, SessionView>;
  /** worktree → the agent last launched there; the ⌘T prefill (acceptance 10). */
  lastAgentByWorktree: Record<WorktreeId, AgentKindKey>;
  terminalConfig: TerminalConfig;
  /** Resolved once; the evidence popover needs `agent-signatures.json`. */
  paths: PathsInfo | null;
  newSession: NewSessionState;

  loadSessions: () => Promise<void>;
  ensureTerminalConfig: () => Promise<TerminalConfig>;
  ensurePaths: () => Promise<PathsInfo | null>;
  openSession: (req: OpenSessionReq) => Promise<PtySessionInfo | null>;
  closeSession: (id: SessionId, mode: CloseMode) => Promise<KillReport | null>;
  restartSession: (id: SessionId) => Promise<PtySessionInfo | null>;
  signalSession: (id: SessionId, sig: PtySignal) => Promise<void>;
  /** Literal keys (`y`, `n`, `1`, Esc) and anything else typed for the user. */
  writeSession: (id: SessionId, text: string) => Promise<void>;
  sendPrompt: (id: SessionId, text: string) => Promise<boolean>;
  refreshSessionStatus: (id: SessionId) => Promise<void>;
  setAttention: (id: SessionId, attention: boolean) => void;
  markScrollbackLost: (id: SessionId) => void;
  setNewSessionOpen: (open: boolean, worktreeId?: WorktreeId | null) => void;
  /** M1.5-6: drop a host-crash row and close the tabs that were showing it. */
  dismissCrashedSession: (id: SessionId) => Promise<void>;
}

// ---------------------------------------------------------------------------
// Pure helpers (unit tested in terminal/terminal.test.ts)
// ---------------------------------------------------------------------------

export function defaultTerminalConfig(): TerminalConfig {
  return {
    fontSize: 12,
    fontFamily: '',
    scrollbackBytes: 2 * 1024 * 1024,
    webgl: true,
    shell: null,
  };
}

/**
 * A `session_list` row that was left behind by a crashed host (M1.5-6).
 *
 * `pid === 0` is not a heuristic: the manager adopts a `pty_session` row only
 * when nothing answers `kill(0)` behind it, and it refuses to invent a pid for a
 * process that is gone. A live session always carries a real one.
 */
export function isHostCrashRow(info: PtySessionInfo): boolean {
  return info.pid <= 0;
}

/**
 * The session id behind a graph node, or `null` when the node is not a session.
 *
 * `session:<id>` is the projection's node id for a pty session (`state/graph.rs`).
 * Four surfaces had to know that — the inspector, its action row, the sidebar
 * row and the pane — so the prefix is spelled once here.
 */
export function sessionIdOfNode(node: { kind: string; id: string }): SessionId | null {
  if (node.kind !== 'session' || !node.id.startsWith('session:')) return null;
  return node.id.slice('session:'.length) as SessionId;
}

/** The view a `session_list` row starts as, before any status event. */
export function viewOfInfo(info: PtySessionInfo, previous?: SessionView): SessionView {
  const hostCrash = isHostCrashRow(info);
  if (previous) return { ...previous, info, hostCrash };
  return {
    id: info.id,
    info,
    status: hostCrash
      ? { kind: 'exited' }
      : info.detached
        ? { kind: 'detached' }
        : { kind: 'starting' },
    confidence: hostCrash ? 1 : 0,
    evidence: [],
    since: info.openedAt,
    updatedAt: info.openedAt,
    attention: false,
    exit: hostCrash ? { code: null, signal: null, at: info.openedAt } : null,
    scrollbackLost: hostCrash,
    hostCrash,
  };
}

/** BEL is the one signal that means "a human is wanted" in all four CLIs. */
export function evidenceHasBell(evidence: readonly StatusEvidence[]): boolean {
  return evidence.some((item) => item.kind === 'bell');
}

export function withStatus(view: SessionView, event: SessionStatusEvent): SessionView {
  return {
    ...view,
    status: event.status,
    confidence: event.confidence,
    evidence: event.evidence,
    since: event.since,
    updatedAt: event.at,
    attention: view.attention || evidenceHasBell(event.evidence),
  };
}

export function withExit(view: SessionView, event: SessionExitEvent): SessionView {
  const status: SessionStatus = {
    kind: 'exited',
    ...(event.exitCode === null ? {} : { code: event.exitCode }),
    ...(event.signal === null ? {} : { signal: event.signal }),
  };
  return {
    ...view,
    status,
    confidence: 1,
    since: event.at,
    updatedAt: event.at,
    exit: { code: event.exitCode, signal: event.signal, at: event.at },
  };
}

/** A session is over: no writing, no prompts, no resize. */
export function isSessionOver(status: SessionStatus): boolean {
  return status.kind === 'exited' || status.kind === 'killed' || status.kind === 'failed';
}

/**
 * How many sessions are working, waiting and idle — the one count in the app
 * (M1.5 BACKLOG 15a).
 *
 * The status bar used to read `graph.totals.sessionsWorking/Waiting/Idle`,
 * which Rust folds out of the *graph projection*: a second mapping, refreshed
 * on its own cadence, over a copy of the status that the pill had already been
 * told about directly. A session sitting on a permission prompt was drawn as
 * `Waiting for input` by its pill and counted as `1 working` by the footer, and
 * both were reading real data. So the footer now counts the same
 * `SessionView.status` values the pill and the sidebar row render, through the
 * same `nodeStatusOfSession` normalization — one status, one mapping, one
 * count, and a disagreement becomes impossible rather than unlikely.
 *
 * The three buckets are the ones Rust's `Totals` names (`state/graph.rs`):
 * everything else — starting, stalled, exited, detached, failed — is counted in
 * none of them, because a footer that lumps a dead session in with the idle
 * ones is the same lie in a different place.
 */
export interface SessionStateCounts {
  working: number;
  waiting: number;
  idle: number;
}

export function sessionStateCounts(
  sessions: ReadonlyMap<SessionId, SessionView>,
): SessionStateCounts {
  const counts: SessionStateCounts = { working: 0, waiting: 0, idle: 0 };
  for (const view of sessions.values()) {
    switch (nodeStatusOfSession(view.status)) {
      case 'working':
        counts.working += 1;
        break;
      case 'waiting':
        counts.waiting += 1;
        break;
      case 'idle':
        counts.idle += 1;
        break;
      default:
        break;
    }
  }
  return counts;
}

/**
 * The selector every counter reads. Returns a fresh object, so a component
 * subscribes to it through `useShallow` — the three numbers are the identity.
 */
export function selectSessionStateCounts(state: {
  sessions: ReadonlyMap<SessionId, SessionView>;
}): SessionStateCounts {
  return sessionStateCounts(state.sessions);
}

/** Three buckets, because a percentage of a heuristic reads as false precision. */
export function confidenceBand(confidence: number): 'high' | 'medium' | 'low' {
  if (confidence >= 0.8) return 'high';
  if (confidence >= 0.5) return 'medium';
  return 'low';
}

/**
 * Which worktree a new session should default to (acceptance 10).
 *
 * In order: an explicitly selected worktree, the worktree of the selected or
 * focused session, the worktree of the most recently opened session, and
 * finally the only worktree there is.
 */
export function resolveActiveWorktree(input: {
  selectedNodeId: string | null;
  focusedSessionId: SessionId | null;
  sessions: Map<SessionId, SessionView>;
  worktreeIds: readonly WorktreeId[];
  /** `worktree:<id>` of the selected node's parent, when it has one. */
  selectedParentId?: string | null;
}): WorktreeId | null {
  const fromNode = (id: string | null | undefined): WorktreeId | null => {
    if (!id) return null;
    if (id.startsWith('worktree:')) return id.slice('worktree:'.length);
    if (id.startsWith('session:')) {
      const view = input.sessions.get(id.slice('session:'.length));
      return view ? view.info.worktreeId : null;
    }
    return null;
  };

  const selected = fromNode(input.selectedNodeId) ?? fromNode(input.selectedParentId);
  if (selected) return selected;

  if (input.focusedSessionId) {
    const view = input.sessions.get(input.focusedSessionId);
    if (view) return view.info.worktreeId;
  }

  let newest: SessionView | null = null;
  for (const view of input.sessions.values()) {
    if (!newest || view.info.openedAt > newest.info.openedAt) newest = view;
  }
  if (newest) return newest.info.worktreeId;

  return input.worktreeIds.length > 0 ? input.worktreeIds[0] : null;
}

/** The agent to pre-select for a worktree: the last one used there. */
export function lastAgentFor(
  lastAgentByWorktree: Record<WorktreeId, AgentKindKey>,
  sessions: Map<SessionId, SessionView>,
  worktreeId: WorktreeId | null,
): AgentKindKey | null {
  if (!worktreeId) return null;
  const remembered = lastAgentByWorktree[worktreeId];
  if (remembered) return remembered;
  let newest: SessionView | null = null;
  for (const view of sessions.values()) {
    if (view.info.worktreeId !== worktreeId) continue;
    if (!newest || view.info.openedAt > newest.info.openedAt) newest = view;
  }
  return newest ? newest.info.agent : null;
}

// ---------------------------------------------------------------------------
// Slice
// ---------------------------------------------------------------------------

/** A sink for the bytes produced between `session_open` and the first mount. */
function discardingChannel(): Channel<PtyChunk> {
  const channel = new Channel<PtyChunk>();
  // The ring is the durable copy; the view catches up with `session_snapshot`
  // when `XTermHost` attaches. Dropping here loses nothing.
  channel.onmessage = () => {};
  return channel;
}

export const createSessionSlice: StateCreator<
  SessionSlice & ToastSlice & UiSlice,
  [['zustand/subscribeWithSelector', never]],
  [],
  SessionSlice
> = (set, get) => {
  const patch = (id: SessionId, update: (view: SessionView) => SessionView): void => {
    const current = get().sessions.get(id);
    if (!current) return;
    const sessions = new Map(get().sessions);
    sessions.set(id, update(current));
    set({ sessions });
  };

  const put = (info: PtySessionInfo): SessionView => {
    const sessions = new Map(get().sessions);
    const view = viewOfInfo(info, sessions.get(info.id));
    sessions.set(info.id, view);
    const lastAgentByWorktree = { ...get().lastAgentByWorktree, [info.worktreeId]: info.agent };
    set({ sessions, lastAgentByWorktree });
    return view;
  };

  // ── event fan-out (one Tauri listener, in `useEventBridge`) ───────────────
  //
  // Deferred by a microtask on purpose: this slice runs while the store is
  // being created, and `useEventBridge` imports the store back. Whichever
  // module the entry point reaches first, subscribing on the next microtask
  // sees both fully initialized. Nothing is missed — the bridge itself only
  // starts listening when `App` mounts.
  queueMicrotask(() => {
    onGildeEvent(SESSION_STATUS, (event) => patch(event.sessionId, (view) => withStatus(view, event)));
    onGildeEvent(SESSION_EXIT, (event) => patch(event.sessionId, (view) => withExit(view, event)));
  });

  let configLoaded = false;

  return {
    sessions: new Map(),
    lastAgentByWorktree: {},
    terminalConfig: defaultTerminalConfig(),
    paths: null,
    newSession: { open: false, worktreeId: null },

    loadSessions: async () => {
      try {
        const rows = await cmd.sessionList();
        const previous = get().sessions;
        const sessions = new Map<SessionId, SessionView>();
        const lastAgentByWorktree = { ...get().lastAgentByWorktree };
        for (const info of rows) {
          sessions.set(info.id, viewOfInfo(info, previous.get(info.id)));
          if (!isHostCrashRow(info)) lastAgentByWorktree[info.worktreeId] = info.agent;
        }
        set({ sessions, lastAgentByWorktree });
        reconcileRestoredTabs(get, sessions);
      } catch (e) {
        if (!isUnavailable(e)) console.warn('[gilde] session_list failed', e);
      }
    },

    ensureTerminalConfig: async () => {
      if (configLoaded) return get().terminalConfig;
      configLoaded = true;
      try {
        const config = await cmd.appSettingsGet();
        if (config.terminal) set({ terminalConfig: { ...defaultTerminalConfig(), ...config.terminal } });
      } catch (e) {
        if (!isUnavailable(e)) console.warn('[gilde] app_settings_get failed', e);
      }
      return get().terminalConfig;
    },

    ensurePaths: async () => {
      const known = get().paths;
      if (known) return known;
      try {
        const info = await cmd.appBootstrap();
        set({ paths: info.paths });
        return info.paths;
      } catch (e) {
        if (!isUnavailable(e)) console.warn('[gilde] app_bootstrap failed', e);
        return null;
      }
    },

    openSession: async (req) => {
      try {
        const info = await cmd.sessionOpen(req, discardingChannel());
        put(info);
        return info;
      } catch (e) {
        get().pushErrorToast(asGildeError(e));
        return null;
      }
    },

    closeSession: async (id, mode) => {
      // M1.5-6: a host-crash row has nothing to close. Whatever affordance the
      // user reached for — the session menu, the tab, the inspector — means
      // "get this off my list", so it routes to the dismissal instead of asking
      // the backend to kill a process that is not there.
      if (get().sessions.get(id)?.hostCrash) {
        await get().dismissCrashedSession(id);
        return null;
      }
      try {
        const report = await cmd.sessionClose(id, mode);
        if (mode === 'detach') {
          patch(id, (view) => ({ ...view, status: { kind: 'detached' }, confidence: 1 }));
        } else {
          // The ring belongs to the session: closing it is the one thing that
          // disposes the pooled terminal (DESIGN §6-7). The last screen stays
          // readable until the pane showing it goes away.
          closeTerminal(id);
          patch(id, (view) => ({ ...view, status: { kind: 'killed' }, confidence: 1 }));
        }
        return report;
      } catch (e) {
        get().pushErrorToast(asGildeError(e));
        return null;
      }
    },

    restartSession: async (id) => {
      const view = get().sessions.get(id);
      if (!view) return null;
      const { projectId, worktreeId, agent, accountId, cols, rows, title } = view.info;
      await get().closeSession(id, 'standard');
      const req: OpenSessionReq = {
        projectId: projectId as ProjectId,
        worktreeId,
        agent,
        cols: cols || 80,
        rows: rows || 24,
        title,
        ...(accountId ? { accountId } : {}),
      };
      const info = await get().openSession(req);
      if (info) {
        // Re-point whatever tab was showing the old session, so "restart" is
        // one action and not "the pane now says the session is gone".
        get().updateSessionTabs(id, {
          descriptor: { view: 'terminal', sessionId: info.id },
          status: 'idle',
        });
      }
      return info;
    },

    signalSession: async (id, sig) => {
      try {
        await cmd.sessionSignal(id, sig);
      } catch (e) {
        get().pushErrorToast(asGildeError(e));
      }
    },

    writeSession: async (id, text) => {
      try {
        await cmd.sessionWrite(id, textToB64(text));
      } catch (e) {
        get().pushErrorToast(asGildeError(e));
      }
    },

    sendPrompt: async (id, text) => {
      try {
        const outcome = await cmd.sessionSendPrompt(id, text);
        if (!outcome.sent) {
          get().pushToast({
            level: 'warn',
            key: 'session.prompt.refused',
            params: { reason: outcome.refusedReason ?? 'unknown' },
            detail: null,
            actionId: null,
            undoToken: null,
            ttlMs: 6000,
          });
          return false;
        }
        return true;
      } catch (e) {
        get().pushErrorToast(asGildeError(e));
        return false;
      }
    },

    refreshSessionStatus: async (id) => {
      try {
        const report = await cmd.sessionStatus(id);
        patch(id, (view) => ({
          ...view,
          status: report.status,
          confidence: report.confidence,
          evidence: report.evidence,
          since: report.since,
          updatedAt: Date.now(),
        }));
      } catch (e) {
        if (!isUnavailable(e)) console.warn('[gilde] session_status failed', e);
      }
    },

    setAttention: (id, attention) => patch(id, (view) => ({ ...view, attention })),

    markScrollbackLost: (id) => patch(id, (view) => ({ ...view, scrollbackLost: true })),

    setNewSessionOpen: (open, worktreeId) =>
      set({ newSession: { open, worktreeId: worktreeId ?? (open ? get().newSession.worktreeId : null) } }),

    dismissCrashedSession: async (id) => {
      const view = get().sessions.get(id);
      if (!view?.hostCrash) return;
      // `session_close` on an adopted row is a dismissal: the manager marks the
      // `pty_session` row closed so the next launch does not offer it again, and
      // answers with a report that says nothing was signalled.
      try {
        await cmd.sessionClose(id, 'standard');
      } catch (e) {
        if (!isUnavailable(e)) console.warn('[gilde] dismissing a crashed session failed', e);
      }
      const sessions = new Map(get().sessions);
      sessions.delete(id);
      set({ sessions });
      for (const tab of get().ui.tabs) {
        if (tab.descriptor.view === 'terminal' && tab.descriptor.sessionId === id) {
          get().closeTab(tab.id);
        }
      }
    },
  };
};

/**
 * What a restored layout's tabs should say, given what the backend still has
 * (M1.5-6).
 *
 * A layout restored from `settings_kv` can name sessions that did not survive
 * the crash. Rather than showing them as if they were starting up, every such
 * tab is marked exited — and the ones whose row came back as a host-crash row
 * carry the reason as their badge, so the tab bar and the sidebar tell the same
 * story. Pure, and returns only the tabs that actually change.
 */
export function exitedTabPatches(
  tabs: readonly Tab[],
  sessions: Map<SessionId, SessionView>,
  crashLabel: string,
): { tabId: string; patch: Partial<Omit<Tab, 'id'>> }[] {
  const out: { tabId: string; patch: Partial<Omit<Tab, 'id'>> }[] = [];
  for (const tab of tabs) {
    if (tab.descriptor.view !== 'terminal') continue;
    const view = sessions.get(tab.descriptor.sessionId);
    if (view?.hostCrash) {
      if (tab.status === 'exited' && tab.badge === crashLabel) continue;
      out.push({ tabId: tab.id, patch: { status: 'exited', badge: crashLabel } });
    } else if (!view && tab.status !== 'exited') {
      out.push({ tabId: tab.id, patch: { status: 'exited' } });
    }
  }
  return out;
}

function reconcileRestoredTabs(
  get: () => SessionSlice & ToastSlice & UiSlice,
  sessions: Map<SessionId, SessionView>,
): void {
  const store = get();
  const crashLabel = translate(store.ui.locale, 'session.hostCrash');
  for (const { tabId, patch } of exitedTabPatches(store.ui.tabs, sessions, crashLabel)) {
    store.updateTab(tabId, patch);
  }
}

