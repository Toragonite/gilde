/**
 * UI slice — shell layout, locale, theme, tabs and panes (DESIGN §9-1, §9-3).
 *
 * A pane is content agnostic: it holds a tab id, and the tab carries a
 * descriptor naming which view to render. That is what lets a terminal, a
 * dispatch board, the graph and the process monitor all be "just a tile".
 *
 * Layout is persisted through `app_settings_set({ layout })`, which the Rust
 * side stores in `settings_kv` (DESIGN §10-4). Writes are debounced so dragging
 * the sidebar does not hammer the backend.
 */

import type { StateCreator } from 'zustand';

import { experimentalLanes, isPaneViewEnabled, isViewEnabled } from '../features';
import { cmd, isUnavailable } from '../lib/commands';
import type {
  AgentBinary,
  AppConfig,
  Locale,
  LocalePreference,
  NodeStatus,
  SessionId,
  SidecarStateEvent,
  TaskId,
  ThemePreference,
} from '../lib/ipc';
import { resolveLocale, translate } from '../i18n';
import type { MessageKey } from '../i18n';

// ---------------------------------------------------------------------------
// Types
// ---------------------------------------------------------------------------

/**
 * The views the rail switches between. `team` and `workers` are the preview;
 * the other four are the experimental lanes (`src/features.ts`). Settings is a
 * sheet, not a view.
 */
export type ViewKey = 'team' | 'workers' | 'sessions' | 'dispatch' | 'graph' | 'monitor';

/** The view the app opens on (and falls back to when a saved one is hidden). */
export const DEFAULT_VIEW: ViewKey = 'team';

/** What a pane should render. Panes themselves know nothing about content. */
export type PaneDescriptor =
  | { view: 'terminal'; sessionId: SessionId }
  | { view: 'dispatch'; taskId?: TaskId }
  | { view: 'graph' }
  | { view: 'monitor' }
  // T8: the read-only team preview (team contract v2 §8.2), src/team/.
  | { view: 'team' }
  // Plan #3c: the orchestrator's worker accounts, read-only, src/workers/.
  | { view: 'workers' };

export interface Tab {
  id: string;
  title: string;
  sublabel?: string;
  descriptor: PaneDescriptor;
  /** Drives the tab's status dot; kept in sync from the graph projection. */
  status?: NodeStatus;
  badge?: string;
  closable: boolean;
}

export interface PaneSlot {
  id: string;
  tabId: string | null;
}

export type SplitCount = 1 | 2 | 3 | 4;

export interface PaneLayout {
  split: SplitCount;
  slots: PaneSlot[];
}

/** The bottom drawer holds the process monitor only (experimental lanes). */
export type DrawerTab = 'monitor';

/**
 * The Team view's scope: every workspace's teams, or only those whose repository
 * is the selected project folder's (worktree-aware, `team/grouping.ts`).
 */
export type TeamScope = 'all' | 'workspace';
/** How the Team view's sidebar groups the teams. */
export type TeamGroupBy = 'repository' | 'folder' | 'status';

export const TEAM_SCOPES: readonly TeamScope[] = ['all', 'workspace'];
export const TEAM_GROUP_BYS: readonly TeamGroupBy[] = ['repository', 'folder', 'status'];

/** Which part of the settings sheet to bring into view when it opens. */
export type SettingsFocus = 'diagnostics' | null;

export interface UiState {
  /** The language shown — `localePref` resolved against the OS language. */
  locale: Locale;
  /** What the user chose: a language, or `system` (the default, F1). */
  localePref: LocalePreference;
  theme: ThemePreference;
  sidebarVisible: boolean;
  sidebarWidth: number;
  inspectorOpen: boolean;
  inspectorWidth: number;
  drawerOpen: boolean;
  drawerHeight: number;
  drawerTab: DrawerTab;
  activeView: ViewKey;
  panes: PaneLayout;
  activePaneId: string;
  tabs: Tab[];
  paletteOpen: boolean;
  switcherOpen: boolean;
  helpOpen: boolean;
  settingsOpen: boolean;
  settingsFocus: SettingsFocus;
  /** Graph node id shown in the inspector. */
  selectedNodeId: string | null;
  /** Team view: all workspaces (default) or this workspace. Persisted. */
  teamScope: TeamScope;
  /** Team view: how the sidebar groups teams (default: repository). Persisted. */
  teamGroupBy: TeamGroupBy;
  /**
   * Team view: the project folder "This workspace" means. `null` (or a folder
   * that was removed) = the first project folder. Persisted.
   */
  teamFolderId: string | null;
  hydrated: boolean;
}

export interface UiSlice {
  ui: UiState;
  agents: AgentBinary[];
  sidecar: SidecarStateEvent | null;
  recoveryPending: number;

  /** A language, or `system` to follow the OS. Persisted as given. */
  setLocale: (locale: LocalePreference) => void;
  setTheme: (theme: ThemePreference) => void;
  toggleSidebar: () => void;
  setSidebarWidth: (width: number) => void;
  toggleInspector: () => void;
  setInspectorWidth: (width: number) => void;
  toggleDrawer: () => void;
  setDrawerTab: (tab: DrawerTab) => void;
  setDrawerHeight: (height: number) => void;
  setActiveView: (view: ViewKey) => void;
  /**
   * M1.5-3: put the view's own surface in front, opening its tab the first
   * time. `setActiveView` calls it, so the rail, ⌘⇧G/⌘⇧M and the palette all
   * move the main area rather than only a highlight.
   */
  revealView: (view: ViewKey) => void;
  /** Focus a tab where it already is, or place it by the pane rule below. */
  revealTab: (tabId: string) => void;
  setSplit: (split: SplitCount) => void;
  setActivePane: (paneId: string) => void;
  openTab: (tab: Omit<Tab, 'id'> & { id?: string }) => string;
  updateTab: (tabId: string, patch: Partial<Omit<Tab, 'id'>>) => void;
  /** Patch every tab whose descriptor points at `sessionId`. */
  updateSessionTabs: (sessionId: SessionId, patch: Partial<Omit<Tab, 'id'>>) => void;
  closeTab: (tabId: string) => void;
  closeActivePane: () => void;
  focusTab: (tabId: string) => void;
  focusTabIndex: (index: number) => void;
  cycleTab: (delta: number) => void;
  assignTab: (paneId: string, tabId: string | null) => void;
  setPaletteOpen: (open: boolean) => void;
  setSwitcherOpen: (open: boolean) => void;
  setHelpOpen: (open: boolean) => void;
  setSettingsOpen: (open: boolean, focus?: SettingsFocus) => void;
  closeOverlays: () => void;
  selectNode: (nodeId: string | null) => void;
  setTeamScope: (scope: TeamScope) => void;
  setTeamGroupBy: (groupBy: TeamGroupBy) => void;
  setTeamFolder: (projectId: string | null) => void;
  setAgents: (agents: AgentBinary[]) => void;
  setSidecar: (state: SidecarStateEvent) => void;
  setRecoveryPending: (count: number) => void;
  hydrateFromConfig: (config: AppConfig) => void;
}

// ---------------------------------------------------------------------------
// Constants and helpers
// ---------------------------------------------------------------------------

export const SIDEBAR_MIN = 260;
export const SIDEBAR_MAX = 420;
export const INSPECTOR_MIN = 300;
export const INSPECTOR_MAX = 480;
export const DRAWER_MIN = 120;
export const DRAWER_MAX = 520;

export function clamp(value: number, min: number, max: number): number {
  if (!Number.isFinite(value)) return min;
  return Math.min(max, Math.max(min, Math.round(value)));
}

function makePanes(split: SplitCount, previous?: PaneLayout): PaneLayout {
  const slots: PaneSlot[] = [];
  for (let i = 0; i < split; i++) {
    slots.push(previous?.slots[i] ?? { id: `pane-${i + 1}`, tabId: null });
  }
  return { split, slots };
}

let tabCounter = 0;
function nextTabId(): string {
  tabCounter += 1;
  return `tab-${tabCounter}`;
}

/**
 * Restored ids must never be handed out again (M1.5-6).
 *
 * A layout that comes back from `settings_kv` carries ids like `tab-4`; without
 * this the next `openTab` would mint `tab-1` … `tab-4` and silently overwrite a
 * restored tab.
 */
function reserveTabIds(tabs: readonly Tab[]): void {
  for (const tab of tabs) {
    const match = /^tab-(\d+)$/.exec(tab.id);
    if (match) tabCounter = Math.max(tabCounter, Number(match[1]));
  }
}

/**
 * Which pane a tab should land in — BACKLOG 9.
 *
 * An empty pane is an invitation: in a 2/3/4 split a new session belongs in the
 * one that is still blank, not on top of whatever the user is reading in the
 * focused pane. In order: the pane already showing this tab, the active pane if
 * it is empty, the first empty pane, and only then the active pane.
 */
export function placementPaneId(ui: UiState, tabId: string): string {
  const slots = ui.panes.slots;
  const showing = slots.find((slot) => slot.tabId === tabId);
  if (showing) return showing.id;
  const active = slots.find((slot) => slot.id === ui.activePaneId);
  if (active && active.tabId === null) return active.id;
  const empty = slots.find((slot) => slot.tabId === null);
  if (empty) return empty.id;
  return active?.id ?? slots[0].id;
}

/** The pane view each rail view puts in front, and the tab's title key. */
export const VIEW_PANE: Partial<Record<ViewKey, { descriptor: PaneDescriptor; title: MessageKey }>> = {
  dispatch: { descriptor: { view: 'dispatch' }, title: 'dispatch.title' },
  graph: { descriptor: { view: 'graph' }, title: 'graph.title' },
  monitor: { descriptor: { view: 'monitor' }, title: 'view.monitor' },
  team: { descriptor: { view: 'team' }, title: 'view.team' },
  workers: { descriptor: { view: 'workers' }, title: 'view.workers' },
};

/** The title key of a view's own tab, so a tab title follows the locale. */
export function viewTabTitleKey(descriptor: PaneDescriptor): MessageKey | null {
  for (const entry of Object.values(VIEW_PANE)) {
    if (entry && entry.descriptor.view === descriptor.view) return entry.title;
  }
  return null;
}

export function initialUiState(): UiState {
  return {
    locale: resolveLocale('system'),
    localePref: 'system',
    theme: 'system',
    sidebarVisible: true,
    sidebarWidth: 300,
    // F5: closed by default. The Team view opens it for a selected member.
    inspectorOpen: false,
    inspectorWidth: 360,
    drawerOpen: false,
    drawerHeight: 220,
    drawerTab: 'monitor',
    activeView: DEFAULT_VIEW,
    panes: makePanes(1),
    activePaneId: 'pane-1',
    tabs: [],
    paletteOpen: false,
    switcherOpen: false,
    helpOpen: false,
    settingsOpen: false,
    settingsFocus: null,
    selectedNodeId: null,
    teamScope: 'all',
    teamGroupBy: 'repository',
    teamFolderId: null,
    hydrated: false,
  };
}

/**
 * The subset of `UiState` that round-trips through `config.layout`
 * (`LayoutConfig` in `lib/ipc.ts`).
 *
 * Only `sidebarWidth`, `inspectorOpen` and `inspectorWidth` are declared fields
 * of `LayoutConfig`; everything else the shell wants to remember goes inside
 * `layout.panes`, which the contract declares as free-form
 * (`Record<string, unknown>`). That way nothing depends on the Rust struct
 * growing a field, and an older build simply ignores keys it does not know.
 */
export interface PersistedPanes {
  sidebarVisible: boolean;
  drawerOpen: boolean;
  drawerHeight: number;
  drawerTab: DrawerTab;
  activeView: ViewKey;
  split: SplitCount;
  /**
   * M1.5-6: the tabs and which pane each one sat in.
   *
   * Without these a SIGKILLed host came back with an empty main area even
   * though every session row was still in the registry. What is restored is the
   * *arrangement*, never a claim about liveness: a terminal tab comes back with
   * no status, and `loadSessions` is what marks it exited when the session
   * behind it did not survive.
   */
  tabs?: PersistedTab[];
  slots?: (string | null)[];
  activePaneId?: string;
  /** Team view preferences (older layouts lack them: the defaults apply). */
  teamScope?: TeamScope;
  teamGroupBy?: TeamGroupBy;
  teamFolderId?: string | null;
}

/** A tab as it round-trips through `settings_kv`. Status is deliberately not. */
export interface PersistedTab {
  id: string;
  title: string;
  sublabel?: string;
  descriptor: PaneDescriptor;
  closable: boolean;
}

export interface PersistedLayout {
  sidebarWidth: number;
  inspectorOpen: boolean;
  inspectorWidth: number;
  panes: PersistedPanes;
}

export function pickPersistedLayout(ui: UiState): PersistedLayout {
  return {
    sidebarWidth: ui.sidebarWidth,
    inspectorOpen: ui.inspectorOpen,
    inspectorWidth: ui.inspectorWidth,
    panes: {
      sidebarVisible: ui.sidebarVisible,
      drawerOpen: ui.drawerOpen,
      drawerHeight: ui.drawerHeight,
      drawerTab: ui.drawerTab,
      activeView: ui.activeView,
      split: ui.panes.split,
      tabs: ui.tabs.map((tab) => ({
        id: tab.id,
        title: tab.title,
        ...(tab.sublabel === undefined ? {} : { sublabel: tab.sublabel }),
        descriptor: tab.descriptor,
        closable: tab.closable,
      })),
      slots: ui.panes.slots.map((slot) => slot.tabId),
      activePaneId: ui.activePaneId,
      teamScope: ui.teamScope,
      teamGroupBy: ui.teamGroupBy,
      teamFolderId: ui.teamFolderId,
    },
  };
}

// Debounced write-through to `settings_kv` via `app_settings_set`.
let persistTimer: ReturnType<typeof setTimeout> | null = null;
let persistPending: PersistedLayout | null = null;

/** Drop a pending debounced write (teardown, and the unit tests). */
export function cancelPersistLayout(): void {
  if (persistTimer) clearTimeout(persistTimer);
  persistTimer = null;
  persistPending = null;
}

export function persistLayout(layout: PersistedLayout, delayMs = 500): void {
  // The unit-test bundle renders the shell on Node, where there is no Tauri
  // host to invoke. Scheduling a write there would fire after the suite has
  // finished and log a failure for something that was never going to work.
  if (typeof window === 'undefined') return;
  persistPending = layout;
  if (persistTimer) clearTimeout(persistTimer);
  persistTimer = setTimeout(() => {
    persistTimer = null;
    const payload = persistPending;
    persistPending = null;
    if (!payload) return;
    void cmd.appSettingsSet({ layout: payload }).catch((e) => {
      // The settings command lands with T03; until then the layout simply is
      // not durable. Anything else is worth surfacing in the log drawer.
      if (!isUnavailable(e)) console.warn('[gilde] layout persist failed', e);
    });
  }, delayMs);
}

// ---------------------------------------------------------------------------
// Slice
// ---------------------------------------------------------------------------

export const createUiSlice: StateCreator<
  UiSlice,
  [['zustand/subscribeWithSelector', never]],
  [],
  UiSlice
> = (set, get) => {
  const patchUi = (patch: Partial<UiState>, persist = false) => {
    const ui = { ...get().ui, ...patch };
    set({ ui });
    if (persist) persistLayout(pickPersistedLayout(ui));
  };

  return {
    ui: initialUiState(),
    agents: [],
    sidecar: null,
    recoveryPending: 0,

    setLocale: (pref) => {
      patchUi({ localePref: pref, locale: resolveLocale(pref) });
      void cmd.appSettingsSet({ locale: pref }).catch(() => undefined);
    },

    setTheme: (theme) => {
      patchUi({ theme });
      void cmd.appSettingsSet({ theme }).catch(() => undefined);
    },

    toggleSidebar: () => patchUi({ sidebarVisible: !get().ui.sidebarVisible }, true),

    setSidebarWidth: (width) =>
      patchUi({ sidebarWidth: clamp(width, SIDEBAR_MIN, SIDEBAR_MAX) }, true),

    toggleInspector: () => patchUi({ inspectorOpen: !get().ui.inspectorOpen }, true),

    setInspectorWidth: (width) =>
      patchUi({ inspectorWidth: clamp(width, INSPECTOR_MIN, INSPECTOR_MAX) }, true),

    // The drawer belongs to the experimental lanes; in the preview it never opens.
    toggleDrawer: () => {
      if (!experimentalLanes()) return;
      patchUi({ drawerOpen: !get().ui.drawerOpen }, true);
    },

    setDrawerTab: (drawerTab) => {
      if (!experimentalLanes()) return;
      patchUi({ drawerTab, drawerOpen: true }, true);
    },

    setDrawerHeight: (height) => patchUi({ drawerHeight: clamp(height, DRAWER_MIN, DRAWER_MAX) }, true),

    // BACKLOG 3: one store field drives the rail highlight, the sidebar
    // landmark and what the main area shows. Switching the view is therefore
    // never only a highlight — it reveals the surface that view is about.
    setActiveView: (activeView) => {
      // A hidden lane is never entered — not from a stale caller, not from a
      // restored layout (`hydrateFromConfig` applies the same rule).
      if (!isViewEnabled(activeView)) return;
      patchUi({ activeView }, true);
      get().revealView(activeView);
    },

    revealView: (view) => {
      const target = VIEW_PANE[view];
      if (!target) {
        // `sessions` owns no tile of its own. It focuses a
        // terminal that is already open; it never opens a session on its own,
        // because starting a process is a decision the user makes.
        if (view !== 'sessions') return;
        const terminal = get().ui.tabs.find((tab) => tab.descriptor.view === 'terminal');
        if (terminal) get().revealTab(terminal.id);
        return;
      }
      const ui = get().ui;
      const existing = ui.tabs.find((tab) => tab.descriptor.view === target.descriptor.view);
      if (existing) {
        get().revealTab(existing.id);
        return;
      }
      get().openTab({
        title: translate(ui.locale, target.title),
        descriptor: target.descriptor,
        closable: true,
      });
    },

    revealTab: (tabId) => {
      const ui = get().ui;
      if (!ui.tabs.some((tab) => tab.id === tabId)) return;
      const showing = ui.panes.slots.find((slot) => slot.tabId === tabId);
      if (showing) {
        get().setActivePane(showing.id);
        return;
      }
      get().assignTab(placementPaneId(ui, tabId), tabId);
    },

    setSplit: (split) => {
      const ui = get().ui;
      const panes = makePanes(split, ui.panes);
      const activePaneId = panes.slots.some((s) => s.id === ui.activePaneId)
        ? ui.activePaneId
        : panes.slots[0].id;
      patchUi({ panes, activePaneId }, true);
    },

    setActivePane: (activePaneId) => patchUi({ activePaneId }),

    openTab: (input) => {
      const ui = get().ui;
      const id = input.id ?? nextTabId();
      const existing = ui.tabs.find((t) => t.id === id);
      const tab: Tab = { ...input, id };
      const tabs = existing ? ui.tabs.map((t) => (t.id === id ? tab : t)) : [...ui.tabs, tab];
      // BACKLOG 9: a blank pane takes the new tab before the focused one does.
      const paneId = placementPaneId(ui, id);
      const slots = ui.panes.slots.map((slot) =>
        slot.id === paneId ? { ...slot, tabId: id } : slot,
      );
      patchUi({ tabs, panes: { ...ui.panes, slots }, activePaneId: paneId }, true);
      return id;
    },

    updateTab: (tabId, patch) => {
      const ui = get().ui;
      if (!ui.tabs.some((t) => t.id === tabId)) return;
      patchUi({ tabs: ui.tabs.map((t) => (t.id === tabId ? { ...t, ...patch } : t)) });
    },

    updateSessionTabs: (sessionId, patch) => {
      const ui = get().ui;
      let touched = false;
      const tabs = ui.tabs.map((t) => {
        if (t.descriptor.view !== 'terminal' || t.descriptor.sessionId !== sessionId) return t;
        touched = true;
        return { ...t, ...patch };
      });
      if (touched) patchUi({ tabs });
    },

    closeTab: (tabId) => {
      const ui = get().ui;
      const tabs = ui.tabs.filter((t) => t.id !== tabId);
      const slots = ui.panes.slots.map((slot) =>
        slot.tabId === tabId ? { ...slot, tabId: tabs[tabs.length - 1]?.id ?? null } : slot,
      );
      patchUi({ tabs, panes: { ...ui.panes, slots } }, true);
    },

    closeActivePane: () => {
      const ui = get().ui;
      const slot = ui.panes.slots.find((s) => s.id === ui.activePaneId);
      if (!slot?.tabId) return;
      get().closeTab(slot.tabId);
    },

    focusTab: (tabId) => {
      const ui = get().ui;
      if (!ui.tabs.some((t) => t.id === tabId)) return;
      const slots = ui.panes.slots.map((slot) =>
        slot.id === ui.activePaneId ? { ...slot, tabId } : slot,
      );
      patchUi({ panes: { ...ui.panes, slots } }, true);
    },

    focusTabIndex: (index) => {
      const tab = get().ui.tabs[index];
      if (tab) get().focusTab(tab.id);
    },

    cycleTab: (delta) => {
      const ui = get().ui;
      if (ui.tabs.length === 0) return;
      const slot = ui.panes.slots.find((s) => s.id === ui.activePaneId);
      const current = ui.tabs.findIndex((t) => t.id === slot?.tabId);
      const base = current < 0 ? 0 : current;
      const next = (base + delta + ui.tabs.length) % ui.tabs.length;
      get().focusTab(ui.tabs[next].id);
    },

    assignTab: (paneId, tabId) => {
      const ui = get().ui;
      const slots = ui.panes.slots.map((slot) => (slot.id === paneId ? { ...slot, tabId } : slot));
      patchUi({ panes: { ...ui.panes, slots }, activePaneId: paneId }, true);
    },

    setPaletteOpen: (paletteOpen) => patchUi({ paletteOpen }),
    setSwitcherOpen: (switcherOpen) => patchUi({ switcherOpen }),
    setHelpOpen: (helpOpen) => patchUi({ helpOpen }),
    setSettingsOpen: (settingsOpen, focus = null) =>
      patchUi({ settingsOpen, settingsFocus: settingsOpen ? focus : null }),

    closeOverlays: () =>
      patchUi({
        paletteOpen: false,
        switcherOpen: false,
        helpOpen: false,
        settingsOpen: false,
        settingsFocus: null,
      }),

    selectNode: (selectedNodeId) => patchUi({ selectedNodeId }),

    setTeamScope: (teamScope) => {
      if (!TEAM_SCOPES.includes(teamScope)) return;
      patchUi({ teamScope }, true);
    },

    setTeamGroupBy: (teamGroupBy) => {
      if (!TEAM_GROUP_BYS.includes(teamGroupBy)) return;
      patchUi({ teamGroupBy }, true);
    },

    setTeamFolder: (teamFolderId) => patchUi({ teamFolderId }, true),

    setAgents: (agents) => set({ agents }),

    setSidecar: (sidecar) => set({ sidecar }),

    setRecoveryPending: (recoveryPending) => set({ recoveryPending }),

    hydrateFromConfig: (config) => {
      const layout = (config.layout ?? {}) as unknown as Partial<PersistedLayout>;
      const panes = (layout.panes ?? {}) as Partial<PersistedPanes>;
      const ui = get().ui;
      const split = ([1, 2, 3, 4] as const).includes(panes.split as SplitCount)
        ? (panes.split as SplitCount)
        : ui.panes.split;

      // M1.5-6: the tabs and the arrangement come back before anything is known
      // about the sessions behind them. Only tabs that name a view a pane can
      // render in THIS build survive the trip (a layout saved with the
      // experimental lanes on does not bring a hidden lane back into the
      // preview), and a slot pointing at a tab that did not is simply empty.
      const tabs: Tab[] = (Array.isArray(panes.tabs) ? panes.tabs : [])
        .filter(
          (tab) =>
            tab &&
            typeof tab.id === 'string' &&
            typeof tab.descriptor?.view === 'string' &&
            isPaneViewEnabled(tab.descriptor.view),
        )
        .map((tab) => ({
          id: tab.id,
          title: tab.title,
          ...(tab.sublabel === undefined ? {} : { sublabel: tab.sublabel }),
          descriptor: tab.descriptor,
          closable: tab.closable !== false,
        }));
      reserveTabIds(tabs);
      const known = new Set(tabs.map((tab) => tab.id));
      const restored = makePanes(split, ui.panes);
      const slotTabs = Array.isArray(panes.slots) ? panes.slots : [];
      restored.slots = restored.slots.map((slot, i) => {
        const tabId = slotTabs[i];
        return typeof tabId === 'string' && known.has(tabId) ? { ...slot, tabId } : slot;
      });
      const activePaneId = restored.slots.some((slot) => slot.id === panes.activePaneId)
        ? (panes.activePaneId as string)
        : restored.slots[0].id;

      const lanes = experimentalLanes();
      const localePref: LocalePreference =
        config.locale === 'ko' || config.locale === 'en' ? config.locale : 'system';
      const savedView = panes.activeView;
      const activeView: ViewKey =
        typeof savedView === 'string' && isViewEnabled(savedView) ? savedView : ui.activeView;

      set({
        ui: {
          ...ui,
          // F1: `system` (the default) follows the OS; only an explicit choice wins.
          localePref,
          locale: resolveLocale(localePref),
          theme: config.theme ?? ui.theme,
          sidebarVisible: panes.sidebarVisible ?? ui.sidebarVisible,
          sidebarWidth: clamp(layout.sidebarWidth ?? ui.sidebarWidth, SIDEBAR_MIN, SIDEBAR_MAX),
          inspectorOpen: layout.inspectorOpen ?? ui.inspectorOpen,
          inspectorWidth: clamp(
            layout.inspectorWidth ?? ui.inspectorWidth,
            INSPECTOR_MIN,
            INSPECTOR_MAX,
          ),
          drawerOpen: lanes ? (panes.drawerOpen ?? ui.drawerOpen) : false,
          drawerHeight: clamp(panes.drawerHeight ?? ui.drawerHeight, DRAWER_MIN, DRAWER_MAX),
          // Logs and Events were removed from the drawer; an old saved tab lands on the monitor.
          drawerTab: 'monitor',
          activeView,
          panes: restored,
          activePaneId,
          tabs,
          teamScope: TEAM_SCOPES.includes(panes.teamScope as TeamScope)
            ? (panes.teamScope as TeamScope)
            : ui.teamScope,
          teamGroupBy: TEAM_GROUP_BYS.includes(panes.teamGroupBy as TeamGroupBy)
            ? (panes.teamGroupBy as TeamGroupBy)
            : ui.teamGroupBy,
          teamFolderId:
            typeof panes.teamFolderId === 'string' && panes.teamFolderId !== ''
              ? panes.teamFolderId
              : ui.teamFolderId,
          hydrated: true,
        },
      });
    },
  };
};
