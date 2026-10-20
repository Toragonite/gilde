/**
 * The typed command registry behind ⌘K, plus the two pure pieces the keyboard
 * layer needs: the fuzzy matcher and the combo formatter (T27, DESIGN §9-8).
 *
 * Three rules shape this file.
 *
 *  1. **A palette entry is never a second implementation.** Every `run` calls a
 *     store action, a registered intent (`hooks/useHotkeys`) or a typed command
 *     from `lib/commands` — the same code path the button or the hotkey takes.
 *     A palette that drifts from the rest of the UI is worse than no palette.
 *  2. **Labels are keys, never prose.** `labelKey` indexes `i18n/ko.ts`, and a
 *     parameter that is itself a translated word is passed as `labelParamKeys`,
 *     so the whole list re-renders in the other locale (DESIGN §9-9).
 *  3. **An action that cannot run is listed with its reason, not hidden.** An
 *     agent that is not installed on this machine, a session command with no
 *     session: both stay visible and carry `disabledKey` (UI-GUIDELINES §2-2).
 *
 * `buildPaletteActions(ctx)` is pure given a context and `paletteContext(state)`
 * derives that context from one store snapshot, so both are unit tested with no
 * DOM in `palette.test.tsx`.
 */

import { experimentalLanes, railViews } from '../features';
import { activeHotkeyIds, focusTabCombo, runIntent, SHELL_HOTKEYS } from '../hooks/useHotkeys';
import type { ShellHotkeyId } from '../hooks/useHotkeys';
import type { MessageKey, MessageParams } from '../i18n';
import { agentDisplayName } from '../lib/agents';
import { cmd } from '../lib/commands';
import type { AgentBinary, AgentKindKey, Locale, SessionId, WorktreeId } from '../lib/ipc';
import { readSnapshotHeader, SNAPSHOT_HEADER_LEN } from '../terminal/terminalPool';
import { lastAgentFor, resolveActiveWorktree } from '../store/sessionSlice';
import { useAppStore } from '../store/useAppStore';
import type { AppStore } from '../store/useAppStore';
import type { SplitCount, ViewKey } from '../store/uiSlice';

// ---------------------------------------------------------------------------
// 1. Fuzzy matching
// ---------------------------------------------------------------------------

export interface FuzzyMatch {
  /** Higher is better. Only comparable between candidates for the same query. */
  score: number;
  /** Indices in the candidate the query consumed, for highlighting. */
  positions: number[];
}

const SEPARATORS = new Set([' ', '-', '_', '/', '.', ':', '·']);

/**
 * Subsequence match with three bonuses: a hit at the start of a word, a hit
 * right after the previous one, and a hit at position zero. Gaps cost a little,
 * and a longer candidate loses ties, so `그래프` ranks the graph panel above
 * "그래프 노드 필터".
 *
 * Case-insensitive and iterated by code point, so a Korean query matches a
 * Korean label exactly the way an English one does.
 */
export function fuzzyMatch(query: string, candidate: string): FuzzyMatch | null {
  const needles = [...query.toLowerCase().trim()].filter((ch) => ch !== ' ');
  const hay = [...candidate.toLowerCase()];
  if (needles.length === 0) return { score: 0, positions: [] };
  if (needles.length > hay.length) return null;

  const positions: number[] = [];
  let score = 0;
  let cursor = 0;
  let previous = -2;

  for (const needle of needles) {
    let found = -1;
    for (let i = cursor; i < hay.length; i++) {
      if (hay[i] === needle) {
        found = i;
        break;
      }
    }
    if (found < 0) return null;
    // Adjacency outweighs a word start on purpose: `graph` must rank the word
    // "graph" above a sentence whose words merely begin with g, r, a, p, h.
    if (found === previous + 1) score += 10;
    if (found === 0) score += 12;
    else if (SEPARATORS.has(hay[found - 1] ?? '')) score += 8;
    score -= Math.min(found - previous - 1, 6);
    positions.push(found);
    previous = found;
    cursor = found + 1;
  }

  score -= Math.min(hay.length / 8, 8);
  return { score, positions };
}

/** The best score across several haystacks (label, keywords, the shortcut). */
export function fuzzyBest(query: string, candidates: readonly string[]): FuzzyMatch | null {
  let best: FuzzyMatch | null = null;
  for (const candidate of candidates) {
    const match = fuzzyMatch(query, candidate);
    if (match && (!best || match.score > best.score)) best = match;
  }
  return best;
}

// ---------------------------------------------------------------------------
// 2. Combo rendering (DESIGN §9-8)
// ---------------------------------------------------------------------------

const KEY_GLYPH: Record<string, string> = {
  backslash: '\\',
  slash: '/',
  comma: ',',
  period: '.',
  escape: 'esc',
  enter: '↵',
  space: 'space',
  arrowleft: '←',
  arrowright: '→',
  arrowup: '↑',
  arrowdown: '↓',
};

const MODIFIER_PARTS = new Set(['mod', 'cmd', 'meta', 'alt', 'opt', 'shift']);

/** `mod+shift+backslash` → `⌘⇧\`. Modifier order follows DESIGN §9-8. */
export function formatCombo(combo: string): string {
  const parts = combo.split('+').map((part) => part.trim().toLowerCase());
  let out = '';
  if (parts.some((part) => part === 'mod' || part === 'cmd' || part === 'meta')) out += '⌘';
  if (parts.some((part) => part === 'alt' || part === 'opt')) out += '⌥';
  if (parts.includes('shift')) out += '⇧';
  const key = parts.find((part) => !MODIFIER_PARTS.has(part)) ?? '';
  out += KEY_GLYPH[key] ?? key.toUpperCase();
  return out;
}

// ---------------------------------------------------------------------------
// 3. Keyboard map — the data the help sheet (⌘/) renders
// ---------------------------------------------------------------------------

export type HelpGroup = 'panel' | 'session' | 'layout' | 'app';

export const HELP_GROUPS: readonly HelpGroup[] = ['panel', 'session', 'layout', 'app'];

export const HELP_GROUP_LABEL: Record<HelpGroup, MessageKey> = {
  panel: 'palette.group.view',
  session: 'palette.group.session',
  layout: 'help.group.layout',
  app: 'palette.group.app',
};

export interface HelpEntry {
  /** A shell binding id, or `focusTab` for the ⌘1–9 range. */
  id: ShellHotkeyId | 'focusTab';
  group: HelpGroup;
  labelKey: MessageKey;
  combo: string;
  /** Overrides the rendered combo where one row stands for a range. */
  display?: string;
}

/**
 * Every binding of DESIGN §9-8, in the order the sheet lists them.
 *
 * `palette.test.tsx` asserts this covers `SHELL_HOTKEYS` exactly — a binding
 * that exists but is undocumented, or a documented one that no longer exists,
 * fails the test instead of quietly rotting.
 */
export const HELP_ENTRIES: readonly HelpEntry[] = [
  { id: 'viewTeam', group: 'panel', labelKey: 'view.team', combo: SHELL_HOTKEYS.viewTeam },
  {
    id: 'viewWorkers',
    group: 'panel',
    labelKey: 'view.workers',
    combo: SHELL_HOTKEYS.viewWorkers,
  },
  { id: 'palette', group: 'app', labelKey: 'app.commandPalette', combo: SHELL_HOTKEYS.palette },
  { id: 'switcher', group: 'app', labelKey: 'switcher.title', combo: SHELL_HOTKEYS.switcher },
  { id: 'help', group: 'app', labelKey: 'help.title', combo: SHELL_HOTKEYS.help },
  { id: 'settings', group: 'app', labelKey: 'view.settings', combo: SHELL_HOTKEYS.settings },
  {
    id: 'closeOverlay',
    group: 'app',
    labelKey: 'help.closeOverlay',
    combo: SHELL_HOTKEYS.closeOverlay,
  },

  { id: 'newSession', group: 'session', labelKey: 'session.new', combo: SHELL_HOTKEYS.newSession },
  {
    id: 'viewDispatch',
    group: 'session',
    labelKey: 'palette.cmd.dispatch',
    combo: SHELL_HOTKEYS.viewDispatch,
  },
  {
    id: 'closePane',
    group: 'layout',
    labelKey: 'palette.cmd.closePane',
    combo: SHELL_HOTKEYS.closePane,
  },

  { id: 'viewGraph', group: 'panel', labelKey: 'view.graph', combo: SHELL_HOTKEYS.viewGraph },
  { id: 'viewMonitor', group: 'panel', labelKey: 'view.monitor', combo: SHELL_HOTKEYS.viewMonitor },
  {
    id: 'toggleSidebar',
    group: 'panel',
    labelKey: 'sidebar.toggle',
    combo: SHELL_HOTKEYS.toggleSidebar,
  },
  {
    id: 'toggleInspector',
    group: 'panel',
    labelKey: 'inspector.toggle',
    combo: SHELL_HOTKEYS.toggleInspector,
  },
  {
    id: 'toggleDrawer',
    group: 'panel',
    labelKey: 'drawer.toggle',
    combo: SHELL_HOTKEYS.toggleDrawer,
  },

  {
    id: 'splitRight',
    group: 'layout',
    labelKey: 'palette.cmd.splitRight',
    combo: SHELL_HOTKEYS.splitRight,
  },
  {
    id: 'splitDown',
    group: 'layout',
    labelKey: 'palette.cmd.splitDown',
    combo: SHELL_HOTKEYS.splitDown,
  },
  {
    id: 'focusTab',
    group: 'layout',
    labelKey: 'help.tabByNumber',
    combo: focusTabCombo(1),
    display: '⌘⌥1–9',
  },
  { id: 'prevTab', group: 'layout', labelKey: 'help.prevTab', combo: SHELL_HOTKEYS.prevTab },
  { id: 'nextTab', group: 'layout', labelKey: 'help.nextTab', combo: SHELL_HOTKEYS.nextTab },
];

/**
 * The help sheet's rows for the current build: exactly the bindings
 * `useShellHotkeys` claims (the experimental ones only with the lanes on).
 */
export function helpEntries(): HelpEntry[] {
  const active = new Set<string>(activeHotkeyIds());
  return HELP_ENTRIES.filter((entry) =>
    entry.id === 'focusTab' ? experimentalLanes() : active.has(entry.id),
  );
}

/** Every combo the window-level layer claims in the current build, ⌘⌥1–9 included. */
export function shellCombos(): string[] {
  const combos = activeHotkeyIds().map((id) => SHELL_HOTKEYS[id] as string);
  if (!experimentalLanes()) return combos;
  const digits: string[] = [];
  for (let i = 1; i <= 9; i++) digits.push(focusTabCombo(i));
  return [...combos, ...digits];
}

// ---------------------------------------------------------------------------
// 4. Palette actions
// ---------------------------------------------------------------------------

export type PaletteGroup = 'view' | 'session' | 'worktree' | 'dispatch' | 'process' | 'app';

export const PALETTE_GROUPS: readonly PaletteGroup[] = [
  'view',
  'session',
  'worktree',
  'dispatch',
  'process',
  'app',
];

export const PALETTE_GROUP_LABEL: Record<PaletteGroup, MessageKey> = {
  view: 'palette.group.view',
  session: 'palette.group.session',
  worktree: 'palette.group.worktree',
  dispatch: 'palette.group.dispatch',
  process: 'palette.group.process',
  app: 'palette.group.app',
};

export interface PaletteAction {
  id: string;
  group: PaletteGroup;
  labelKey: MessageKey;
  labelParams?: MessageParams;
  /** Parameters whose value is itself a message key (`{view}` → `view.graph`). */
  labelParamKeys?: Record<string, MessageKey>;
  /** Muted right-hand text: a count, a target, a machine fact. Not translated. */
  detail?: string;
  /** The keybinding that runs the same action, when there is one. */
  combo?: string;
  /** Extra match text — agent names, view ids, English aliases. */
  keywords?: readonly string[];
  /** Why this cannot run right now. Present means "listed but not runnable". */
  disabledKey?: MessageKey;
  run: () => void | Promise<void>;
}

export interface PaletteContext {
  locale: Locale;
  agents: readonly AgentBinary[];
  worktreeCount: number;
  activeWorktreeId: WorktreeId | null;
  activeWorktreeLabel: string | null;
  /** The session the active pane shows, when it shows one. */
  activeSessionId: SessionId | null;
  activeSessionLabel: string | null;
  /** The agent last launched in the active worktree — the ⌘T prefill. */
  suggestedAgent: AgentKindKey | null;
  orphans: number;
}

/** The pty is opened with a real winsize; codex renders nothing into 0×0. */
export const INITIAL_COLS = 80;
export const INITIAL_ROWS = 24;

/** The agent's product name (F13): `cursorAgent` → `Cursor`, `gemini` → `Antigravity`. */
export function agentLabel(kind: AgentKindKey): string {
  return agentDisplayName(kind);
}

/** One store snapshot in, one palette context out. Pure, so it is unit tested. */
export function paletteContext(state: AppStore): PaletteContext {
  const ui = state.ui;
  const slot = ui.panes.slots.find((candidate) => candidate.id === ui.activePaneId);
  const tab = ui.tabs.find((candidate) => candidate.id === slot?.tabId);
  const activeSessionId =
    tab && tab.descriptor.view === 'terminal' ? tab.descriptor.sessionId : null;

  const selected = ui.selectedNodeId ? state.graph.nodes.get(ui.selectedNodeId) : undefined;
  const worktreeIds: WorktreeId[] = [];
  for (const node of state.graph.nodes.values()) {
    if (node.kind === 'worktree') worktreeIds.push(node.id.slice('worktree:'.length));
  }

  const activeWorktreeId = resolveActiveWorktree({
    selectedNodeId: ui.selectedNodeId,
    selectedParentId: selected?.parentId ?? null,
    focusedSessionId: activeSessionId,
    sessions: state.sessions,
    worktreeIds,
  });

  const worktreeNode = activeWorktreeId
    ? state.graph.nodes.get(`worktree:${activeWorktreeId}`)
    : undefined;

  return {
    locale: ui.locale,
    agents: state.agents,
    worktreeCount: worktreeIds.length,
    activeWorktreeId,
    activeWorktreeLabel: worktreeNode?.label ?? null,
    activeSessionId,
    activeSessionLabel: activeSessionId ? (tab?.title ?? null) : null,
    suggestedAgent: lastAgentFor(state.lastAgentByWorktree, state.sessions, activeWorktreeId),
    orphans: state.graph.totals.orphans,
  };
}

const VIEW_LABEL: Record<ViewKey, MessageKey> = {
  team: 'view.team',
  workers: 'view.workers',
  sessions: 'view.sessions',
  dispatch: 'view.dispatch',
  graph: 'view.graph',
  monitor: 'view.monitor',
};

const VIEW_COMBO: Partial<Record<ViewKey, string>> = {
  team: SHELL_HOTKEYS.viewTeam,
  workers: SHELL_HOTKEYS.viewWorkers,
  dispatch: SHELL_HOTKEYS.viewDispatch,
  graph: SHELL_HOTKEYS.viewGraph,
  monitor: SHELL_HOTKEYS.viewMonitor,
};

/**
 * Show a view: raise the rail and put the view's own tab in front. One code
 * path with the rail and the hotkeys (`setActiveView` → `revealView`), which
 * also refuses a view the current build hides.
 */
export function openViewPane(view: ViewKey): void {
  useAppStore.getState().setActiveView(view);
}

/** Open a pty session directly — the per-agent ⌘K entries and the switcher. */
export async function openSessionWithAgent(
  agent: AgentKindKey,
  worktreeId: WorktreeId,
): Promise<void> {
  const state = useAppStore.getState();
  const node = state.graph.nodes.get(`worktree:${worktreeId}`);
  const projectId = node?.parentId?.startsWith('project:')
    ? node.parentId.slice('project:'.length)
    : '';
  const info = await state.openSession({
    projectId,
    worktreeId,
    agent,
    cols: INITIAL_COLS,
    rows: INITIAL_ROWS,
  });
  if (!info) return;
  useAppStore.getState().openTab({
    title: info.agent,
    sublabel: node?.label,
    descriptor: { view: 'terminal', sessionId: info.id },
    status: 'idle',
    closable: true,
  });
}

/**
 * The scrollback as text — the same `session_snapshot` decode the terminal
 * kebab performs (DESIGN §6-7: the ring is the durable copy). M1 ships no
 * command that writes a file for the front end, so "save" means the clipboard.
 */
async function copyTranscript(sessionId: SessionId): Promise<void> {
  const state = useAppStore.getState();
  try {
    const buffer = await cmd.sessionSnapshot(sessionId);
    const bytes = new Uint8Array(buffer);
    readSnapshotHeader(bytes);
    const text = new TextDecoder().decode(bytes.subarray(SNAPSHOT_HEADER_LEN));
    await navigator.clipboard.writeText(text);
    state.pushToast({
      level: 'success',
      key: 'session.transcript.copied',
      params: {},
      detail: null,
      actionId: null,
      undoToken: null,
      ttlMs: 4000,
    });
  } catch (e) {
    state.pushToast({
      level: 'error',
      key: 'session.transcript.failed',
      params: {},
      detail: e instanceof Error ? e.message : null,
      actionId: null,
      undoToken: null,
      ttlMs: 0,
    });
  }
}

async function reloadSignatures(): Promise<void> {
  if (runIntent('reloadSignatures')) return;
  const state = useAppStore.getState();
  try {
    const report = await cmd.agentsReloadSignatures();
    state.pushToast({
      level: 'success',
      key: 'palette.signatures.reloaded',
      params: { count: report.agents },
      detail: null,
      actionId: null,
      undoToken: null,
      ttlMs: 4000,
    });
  } catch (e) {
    state.pushToast({
      level: 'error',
      key: 'palette.signatures.failed',
      params: {},
      detail: e instanceof Error ? e.message : null,
      actionId: null,
      undoToken: null,
      ttlMs: 0,
    });
  }
}

async function restartSidecar(): Promise<void> {
  const state = useAppStore.getState();
  try {
    const status = await cmd.sidecarRestart();
    state.setSidecar({
      state: status.state,
      generation: status.generation,
      restarts: status.restarts,
      lastError: status.lastError,
    });
    state.pushToast({
      level: 'info',
      key: 'palette.sidecar.restarted',
      params: { state: status.state },
      detail: null,
      actionId: null,
      undoToken: null,
      ttlMs: 4000,
    });
  } catch (e) {
    state.pushToast({
      level: 'error',
      key: 'palette.sidecar.failed',
      params: {},
      detail: e instanceof Error ? e.message : null,
      actionId: null,
      undoToken: null,
      ttlMs: 0,
    });
  }
}

/** Open the Monitor with the our-orphan filter, exactly as the sidebar does. */
function openOrphans(): void {
  const state = useAppStore.getState();
  state.requestOrphanFilter();
  openViewPane('monitor');
  useAppStore.getState().setDrawerTab('monitor');
}

function openRemoveDialogFor(worktreeId: WorktreeId): void {
  const state = useAppStore.getState();
  const node = state.graph.nodes.get(`worktree:${worktreeId}`);
  void state.openRemoveDialog(
    worktreeId,
    node?.label ?? worktreeId,
    node?.meta.branch ?? node?.sublabel ?? '',
  );
}

/**
 * The experimental lanes' entries: sessions, worktrees, dispatch and the
 * process monitor. Only `buildPaletteActions` with the lanes on calls this.
 */
function pushLaneActions(actions: PaletteAction[], ctx: PaletteContext): void {
  const s = () => useAppStore.getState();

  // -- sessions -------------------------------------------------------------
  actions.push({
    id: 'session.new',
    group: 'session',
    labelKey: 'session.new',
    combo: SHELL_HOTKEYS.newSession,
    keywords: ['session', 'terminal'],
    run: () => {
      if (!runIntent('newSession')) s().setNewSessionOpen(true);
    },
  });

  const launchable = ctx.agents.filter((binary) => binary.kind !== 'shell');
  const shell = ctx.agents.find((binary) => binary.kind === 'shell');
  for (const binary of shell ? [...launchable, shell] : launchable) {
    const name = agentLabel(binary.kind);
    const disabledKey: MessageKey | undefined = !binary.installed
      ? 'session.new.notInstalled'
      : ctx.activeWorktreeId === null
        ? 'session.new.noWorktree'
        : undefined;
    actions.push({
      id: `session.new.${binary.kind}`,
      group: 'session',
      labelKey: 'palette.cmd.newSessionWith',
      labelParams: { agent: name },
      keywords: [name, binary.kind, 'session'],
      ...(ctx.activeWorktreeLabel ? { detail: ctx.activeWorktreeLabel } : {}),
      ...(disabledKey ? { disabledKey } : {}),
      run: () => {
        if (ctx.activeWorktreeId) void openSessionWithAgent(binary.kind, ctx.activeWorktreeId);
      },
    });
  }

  actions.push({
    id: 'session.transcript',
    group: 'session',
    labelKey: 'session.menu.transcript',
    keywords: ['transcript', 'copy'],
    ...(ctx.activeSessionLabel ? { detail: ctx.activeSessionLabel } : {}),
    ...(ctx.activeSessionId === null ? { disabledKey: 'palette.disabled.noSession' as const } : {}),
    run: () => {
      if (!ctx.activeSessionId) return;
      if (runIntent('saveTranscript')) return;
      void copyTranscript(ctx.activeSessionId);
    },
  });

  // -- worktrees ------------------------------------------------------------
  actions.push(
    {
      id: 'worktree.create',
      group: 'worktree',
      labelKey: 'worktree.create',
      keywords: ['worktree', 'create'],
      run: () => {
        if (!runIntent('newWorktree')) s().setNewWorktreeOpen(true);
      },
    },
    {
      id: 'worktree.remove',
      group: 'worktree',
      labelKey: 'worktree.remove',
      keywords: ['worktree', 'remove'],
      ...(ctx.activeWorktreeLabel ? { detail: ctx.activeWorktreeLabel } : {}),
      ...(ctx.activeWorktreeId === null
        ? { disabledKey: 'session.new.noWorktree' as const }
        : {}),
      run: () => {
        if (ctx.activeWorktreeId) openRemoveDialogFor(ctx.activeWorktreeId);
      },
    },
    {
      id: 'worktree.switch',
      group: 'worktree',
      labelKey: 'switcher.title',
      combo: SHELL_HOTKEYS.switcher,
      keywords: ['worktree', 'switch'],
      run: () => s().setSwitcherOpen(true),
    },
  );

  // -- dispatch -------------------------------------------------------------
  actions.push({
    id: 'dispatch.new',
    group: 'dispatch',
    labelKey: 'palette.cmd.dispatch',
    combo: SHELL_HOTKEYS.viewDispatch,
    keywords: ['dispatch', 'headless'],
    run: () => {
      openViewPane('dispatch');
      runIntent('newDispatch');
    },
  });

  // -- processes ------------------------------------------------------------
  actions.push({
    id: 'process.orphans',
    group: 'process',
    labelKey: 'palette.cmd.orphans',
    detail: String(ctx.orphans),
    keywords: ['orphan', 'monitor', 'cleanup'],
    run: openOrphans,
  });
}

/**
 * Everything ⌘K offers, in declaration order. Deliberately flat: grouping is a
 * render concern and a flat array is what the fuzzy filter ranks.
 */
export function buildPaletteActions(ctx: PaletteContext): PaletteAction[] {
  const s = () => useAppStore.getState();
  const actions: PaletteAction[] = [];

  const lanes = experimentalLanes();

  // -- panels ---------------------------------------------------------------
  for (const view of railViews()) {
    actions.push({
      id: `view.${view}`,
      group: 'view',
      labelKey: 'palette.cmd.view',
      labelParamKeys: { view: VIEW_LABEL[view] },
      keywords: [view],
      ...(VIEW_COMBO[view] ? { combo: VIEW_COMBO[view] } : {}),
      run: () => openViewPane(view),
    });
  }

  actions.push({
    id: 'view.settings',
    group: 'view',
    labelKey: 'palette.cmd.view',
    labelParamKeys: { view: 'view.settings' },
    keywords: ['settings', 'preferences'],
    combo: SHELL_HOTKEYS.settings,
    run: () => s().setSettingsOpen(true),
  });

  if (lanes) {
    actions.push({
      id: 'drawer.monitor',
      group: 'view',
      labelKey: 'palette.cmd.drawerTab',
      labelParamKeys: { tab: 'drawer.monitor' },
      keywords: ['monitor', 'drawer'],
      combo: SHELL_HOTKEYS.toggleDrawer,
      run: () => s().setDrawerTab('monitor'),
    });
  }

  actions.push({
    id: 'toggleSidebar',
    group: 'view',
    labelKey: 'sidebar.toggle',
    combo: SHELL_HOTKEYS.toggleSidebar,
    run: () => s().toggleSidebar(),
  });

  if (lanes) {
    actions.push(
      {
        id: 'toggleInspector',
        group: 'view',
        labelKey: 'inspector.toggle',
        combo: SHELL_HOTKEYS.toggleInspector,
        run: () => s().toggleInspector(),
      },
      {
        id: 'splitRight',
        group: 'view',
        labelKey: 'palette.cmd.splitRight',
        combo: SHELL_HOTKEYS.splitRight,
        run: () => {
          const split = s().ui.panes.split;
          s().setSplit(split >= 3 ? 1 : ((split + 1) as SplitCount));
        },
      },
      {
        id: 'splitDown',
        group: 'view',
        labelKey: 'palette.cmd.splitDown',
        combo: SHELL_HOTKEYS.splitDown,
        run: () => s().setSplit(s().ui.panes.split === 4 ? 2 : 4),
      },
    );
  }

  actions.push({
    id: 'closePane',
    group: 'view',
    labelKey: 'palette.cmd.closePane',
    combo: SHELL_HOTKEYS.closePane,
    run: () => s().closeActivePane(),
  });

  if (lanes) pushLaneActions(actions, ctx);

  // -- app ------------------------------------------------------------------
  // The language switch lives in the title bar and Settings › Appearance only (F16).
  if (lanes) {
    actions.push(
      {
        id: 'app.reloadSignatures',
        group: 'app',
        labelKey: 'palette.cmd.reloadSignatures',
        keywords: ['signature', 'reload'],
        run: reloadSignatures,
      },
      {
        id: 'app.restartSidecar',
        group: 'app',
        labelKey: 'palette.cmd.restartSidecar',
        keywords: ['sidecar', 'restart'],
        run: restartSidecar,
      },
    );
  }
  actions.push({
    id: 'app.help',
    group: 'app',
    labelKey: 'help.title',
    combo: SHELL_HOTKEYS.help,
    keywords: ['help', 'keyboard', 'shortcut'],
    run: () => s().setHelpOpen(true),
  });

  return actions;
}

// ---------------------------------------------------------------------------
// 5. Filtering
// ---------------------------------------------------------------------------

export interface RankedAction {
  action: PaletteAction;
  score: number;
}

/**
 * Filter and rank. The label arrives already translated, because the score has
 * to be computed against the text the user is actually reading.
 */
export function rankActions(
  query: string,
  actions: readonly PaletteAction[],
  label: (action: PaletteAction) => string,
): RankedAction[] {
  const trimmed = query.trim();
  const ranked: RankedAction[] = [];
  actions.forEach((action, index) => {
    const haystacks = [label(action), action.id, ...(action.keywords ?? [])];
    if (action.combo) haystacks.push(formatCombo(action.combo));
    const match = fuzzyBest(trimmed, haystacks);
    if (!match) return;
    // The index keeps the declared order stable between equal scores.
    ranked.push({ action, score: match.score - index / 1000 });
  });
  if (trimmed.length > 0) ranked.sort((a, b) => b.score - a.score);
  return ranked;
}
