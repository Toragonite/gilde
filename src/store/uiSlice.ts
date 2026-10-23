/**
 * UI slice — what the window shows, independent of the World mirror.
 *
 * Contract §2.2.4 (uiSlice REWORK) and ux-ia §2.4: one main view at a time
 * with browser-like history (`⌘[` / `⌘]`), N-tile Split (2–6), per-team
 * layouts, the Focus Home mode, per-session composer drafts, the `⌘J` chain
 * through the queue, sheets, zoom, and the window's visibility.
 *
 * Persisted keys (`ui_state_set`, contract §4.2.1 `UiKey`) are written by
 * `persist.ts`; this slice only holds the values and hydrates them from
 * `app_bootstrap.uiState`, which is untrusted JSON and is validated here.
 *
 * Every uid the UI holds is remapped when World reports a tombstone
 * (`remapSessionRefs`, contract §3.3.3, T-ui-mergedInto).
 */

import type { StateCreator } from 'zustand';

import { defaultLocale } from '../i18n';
import type { AppBootstrapRes } from '../lib/gen/AppBootstrapRes';
import type { AppFeatures } from '../lib/gen/AppFeatures';
import type { AttentionItem } from '../lib/gen/AttentionItem';
import type { LaunchInfo } from '../lib/gen/LaunchInfo';
import type { Locale } from '../lib/gen/Locale';
import type { MemberKey } from '../lib/gen/MemberKey';
import type { ProjectId } from '../lib/gen/ProjectId';
import type { Platform } from '../lib/gen/Platform';
import type { JsonValue } from '../lib/gen/serde_json/JsonValue';
import type { SessionUid } from '../lib/gen/SessionUid';
import type { TeamKey } from '../lib/gen/TeamKey';
import type { TeamViewMode } from '../lib/gen/TeamViewMode';
import type { Theme } from '../lib/gen/Theme';
import type { VisibilityEvent } from '../lib/gen/VisibilityEvent';

// ---------------------------------------------------------------------------
// Types
// ---------------------------------------------------------------------------

/** Session screen sub-views: Conversation · Changes · Terminal or Log (ux-ia §2.4). */
export type SessionSub = 'conversation' | 'changes' | 'terminal';

export type AccountsSection = 'accounts' | 'health';

/** The one main view (ux-ia §2.4 "Main: what you work on"). */
export type MainView =
  | { screen: 'home' }
  | { screen: 'session'; uid: SessionUid; sub: SessionSub }
  | { screen: 'split' }
  | { screen: 'team'; teamKey: TeamKey; mode: TeamViewMode }
  | { screen: 'floor' }
  | { screen: 'history' }
  | { screen: 'accounts'; section: AccountsSection }
  | { screen: 'settings' };

export type HomeMode = 'focus' | 'classic';

/** Layer-2 sheets (ux-ia §2.4). One at a time; sheets trap focus. */
export type SheetId =
  | 'newSession'
  | 'stopSession'
  | 'cleanUp'
  | 'commit'
  | 'merge'
  | 'discardWorktree'
  | 'closePane'
  | 'quit'
  | 'help';

/** What a sheet was opened for (A26.2): "+ on a project", "New session in {project}". */
export interface SheetContext {
  projectId?: ProjectId;
}

/** A History read's title (memory only; it is content). */
export interface ReadTitle {
  uid: SessionUid;
  title: string;
}

export interface SplitState {
  /** 0–6 sessions side by side, in tile order. */
  uids: SessionUid[];
  focused: SessionUid | null;
  zoomed: SessionUid | null;
}

export interface TeamLayout {
  mode: TeamViewMode;
  focused: MemberKey | null;
  zoomed: MemberKey | null;
  /** The lead column's share of Team › Panes, 0.2–0.8 (A30.3); null = the default. */
  leadRatio: number | null;
  /** The teammates' tile order in Team › Panes, ≤ 64 member keys (A30.3); null = join order. */
  tileOrder: MemberKey[] | null;
}

export const LEAD_RATIO_RANGE = { min: 0.2, max: 0.8 } as const;
export const MAX_TILE_ORDER = 64;

/** A layout with every field at its default. */
export function defaultTeamLayout(): TeamLayout {
  return { mode: 'panes', focused: null, zoomed: null, leadRatio: null, tileOrder: null };
}

/** The `⌘J` chain: where it started, and the queue item it is on. */
export interface QueueChain {
  origin: MainView;
  currentKey: string | null;
  /** Rank index of the current item when it was reached (the handled item's slot). */
  currentIndex: number;
}

/** A request to focus a session's top card when its screen arrives (⌘J, notification click). */
export interface FocusCardRequest {
  uid: SessionUid;
  token: number;
}

export interface WindowVisibility {
  frontmost: boolean;
  occluded: boolean;
  asleep: boolean;
  documentVisible: boolean;
}

export interface UiState {
  locale: Locale;
  theme: Theme;
  view: MainView;
  back: MainView[];
  forward: MainView[];
  sidebarVisible: boolean;
  split: SplitState;
  teamLayouts: Readonly<Record<string, TeamLayout>>;
  homeMode: HomeMode;
  /** Composer drafts per session (memory only). */
  drafts: Readonly<Record<string, string>>;
  chain: QueueChain | null;
  focusCard: FocusCardRequest | null;
  paletteOpen: boolean;
  sheet: SheetId | null;
  /** The context the open sheet was opened with; cleared by `closeSheet()` (A26.2). */
  sheetContext: SheetContext | null;
  /**
   * The session on Home's focus card (A26.1): set by Home, cleared when Home
   * unmounts or the card shows no session. `viewStateOf` reports it as
   * visible while the view is Home (notification rule 2, F-04).
   */
  homeFocusUid: SessionUid | null;
  /**
   * The title of a History read (A29.2, T31): set by the read-only view of a
   * session World does not hold, cleared when it unmounts. The top bar and the
   * `main` landmark use it only while the uid is absent from the store.
   */
  readTitle: ReadTitle | null;
  /**
   * The sessions a multi-session screen actually shows (A30.2, F-04): Split's
   * rendered tiles, Team › Panes' rendered tiles, the Board rows whose cards
   * are shown. Set by that screen, `null` when it unmounts; `viewStateOf`
   * reports it instead of its own derivation while it is non-null.
   */
  visibleUids: SessionUid[] | null;
  /** UI zoom, 0.8–2.0 (ux-ia §10 Text scaling). */
  zoom: number;
  visibility: WindowVisibility;
  /** `gilde://quit-requested` arrived; the shell shows the quit sheet. */
  quitRequested: boolean;
  /** The core runs another build than this WebView even after one reload (§4.7): "Restart Gilde to finish updating." */
  buildMismatch: boolean;
  launch: LaunchInfo | null;
  buildId: string | null;
  version: string | null;
  platform: Platform | null;
  features: AppFeatures;
  /** Opaque persisted values owned by their screens (validated as JSON only). */
  sidebarOrder: JsonValue | null;
  sidebarFolds: JsonValue | null;
  floorPrefs: JsonValue | null;
  hydrated: boolean;
}

export const MAX_SPLIT = 6;
export const MAX_VIEW_HISTORY = 50;
export const ZOOM_MIN = 0.8;
export const ZOOM_MAX = 2.0;
export const ZOOM_STEP = 0.1;

const HOME: MainView = { screen: 'home' };

export function initialUiState(): UiState {
  return {
    locale: defaultLocale(),
    theme: 'system',
    view: HOME,
    back: [],
    forward: [],
    sidebarVisible: true,
    split: { uids: [], focused: null, zoomed: null },
    teamLayouts: {},
    homeMode: 'focus',
    drafts: {},
    chain: null,
    focusCard: null,
    paletteOpen: false,
    sheet: null,
    sheetContext: null,
    homeFocusUid: null,
    readTitle: null,
    visibleUids: null,
    zoom: 1,
    visibility: { frontmost: true, occluded: false, asleep: false, documentVisible: true },
    quitRequested: false,
    buildMismatch: false,
    launch: null,
    buildId: null,
    version: null,
    platform: null,
    features: { orchestrator: false },
    sidebarOrder: null,
    sidebarFolds: null,
    floorPrefs: null,
    hydrated: false,
  };
}

// ---------------------------------------------------------------------------
// Pure helpers
// ---------------------------------------------------------------------------

export function sameView(a: MainView, b: MainView): boolean {
  return JSON.stringify(a) === JSON.stringify(b);
}

export function clampZoom(z: number): number {
  if (!Number.isFinite(z)) return 1;
  return Math.round(Math.min(ZOOM_MAX, Math.max(ZOOM_MIN, z)) * 100) / 100;
}

const SUBS: readonly SessionSub[] = ['conversation', 'changes', 'terminal'];
const TEAM_MODES: readonly TeamViewMode[] = ['panes', 'board', 'floor'];
const UID = /^s_[0-9a-f-]{36}$/;

function isObject(v: unknown): v is Record<string, unknown> {
  return typeof v === 'object' && v !== null && !Array.isArray(v);
}

function asUid(v: unknown): SessionUid | null {
  return typeof v === 'string' && UID.test(v) ? (v as SessionUid) : null;
}

function asKey(v: unknown): string | null {
  return typeof v === 'string' && v.length > 0 && v.length <= 256 ? v : null;
}

/** A persisted `lastView`, validated; anything malformed is Home. */
export function parseView(v: unknown): MainView {
  if (!isObject(v)) return HOME;
  switch (v.screen) {
    case 'home':
    case 'split':
    case 'floor':
    case 'history':
    case 'settings':
      return { screen: v.screen };
    case 'session': {
      const uid = asUid(v.uid);
      const sub = SUBS.includes(v.sub as SessionSub) ? (v.sub as SessionSub) : 'conversation';
      return uid ? { screen: 'session', uid, sub } : HOME;
    }
    case 'team': {
      const teamKey = asKey(v.teamKey);
      const mode = TEAM_MODES.includes(v.mode as TeamViewMode) ? (v.mode as TeamViewMode) : 'panes';
      return teamKey ? { screen: 'team', teamKey: teamKey as TeamKey, mode } : HOME;
    }
    case 'accounts':
      return { screen: 'accounts', section: v.section === 'health' ? 'health' : 'accounts' };
    default:
      return HOME;
  }
}

/** A persisted `splitSet`, validated: ≤ 6 distinct well-formed uids. */
export function parseSplit(v: unknown): SplitState {
  const out: SplitState = { uids: [], focused: null, zoomed: null };
  const list = isObject(v) ? v.uids : v;
  if (!Array.isArray(list)) return out;
  for (const item of list) {
    const uid = asUid(item);
    if (uid && !out.uids.includes(uid)) out.uids.push(uid);
    if (out.uids.length === MAX_SPLIT) break;
  }
  if (isObject(v)) {
    const f = asUid(v.focused);
    if (f && out.uids.includes(f)) out.focused = f;
  }
  return out;
}

const MEMBER_KEY = /^m_[0-9a-f]{16}$/;

/** A lead-column share within 0.2–0.8, else null (A30.3). */
export function parseLeadRatio(v: unknown): number | null {
  if (typeof v !== 'number' || !Number.isFinite(v)) return null;
  return v >= LEAD_RATIO_RANGE.min && v <= LEAD_RATIO_RANGE.max ? v : null;
}

/** ≤ 64 distinct well-formed member keys, else null: a malformed order is dropped whole (A30.3). */
export function parseTileOrder(v: unknown): MemberKey[] | null {
  if (!Array.isArray(v) || v.length > MAX_TILE_ORDER) return null;
  const out: MemberKey[] = [];
  for (const item of v) {
    if (typeof item !== 'string' || !MEMBER_KEY.test(item) || out.includes(item as MemberKey)) return null;
    out.push(item as MemberKey);
  }
  return out;
}

/** A persisted `teamLayout:<TeamKey>`, validated. */
export function parseTeamLayout(v: unknown): TeamLayout {
  const out = defaultTeamLayout();
  if (!isObject(v)) return out;
  if (TEAM_MODES.includes(v.mode as TeamViewMode)) out.mode = v.mode as TeamViewMode;
  const f = asKey(v.focused);
  if (f) out.focused = f as MemberKey;
  const z = asKey(v.zoomed);
  if (z) out.zoomed = z as MemberKey;
  out.leadRatio = parseLeadRatio(v.leadRatio);
  out.tileOrder = parseTileOrder(v.tileOrder);
  return out;
}

/** Push `next` as the main view, keeping a bounded back stack and clearing forward. */
export function pushView(ui: UiState, next: MainView): Pick<UiState, 'view' | 'back' | 'forward'> {
  if (sameView(ui.view, next)) return { view: ui.view, back: ui.back, forward: ui.forward };
  const back = [...ui.back, ui.view].slice(-MAX_VIEW_HISTORY);
  return { view: next, back, forward: [] };
}

function remapView(view: MainView, map: ReadonlyMap<SessionUid, SessionUid>): MainView {
  if (view.screen === 'session') {
    const to = map.get(view.uid);
    if (to) return { ...view, uid: to };
  }
  return view;
}

/** Collapse consecutive duplicates (a remap can make two neighbours equal). */
function dedupeStack(stack: MainView[]): MainView[] {
  const out: MainView[] = [];
  for (const v of stack) if (out.length === 0 || !sameView(out[out.length - 1] as MainView, v)) out.push(v);
  return out;
}

/** Move every reference from a tombstone uid to its survivor (contract §3.3.3). */
export function remapUi(ui: UiState, map: ReadonlyMap<SessionUid, SessionUid>): UiState {
  if (map.size === 0) return ui;
  const r = (uid: SessionUid | null): SessionUid | null => (uid ? (map.get(uid) ?? uid) : null);

  const uids: SessionUid[] = [];
  for (const u of ui.split.uids) {
    const to = map.get(u) ?? u;
    if (!uids.includes(to)) uids.push(to);
  }

  const drafts: Record<string, string> = { ...ui.drafts };
  for (const [from, to] of map) {
    const old = drafts[from];
    if (old === undefined) continue;
    delete drafts[from];
    const kept = drafts[to];
    drafts[to] = kept && kept.trim() !== '' ? (old.trim() !== '' ? `${kept}\n${old}` : kept) : old;
  }

  let chain = ui.chain;
  if (chain) {
    let currentKey = chain.currentKey;
    if (currentKey && currentKey.startsWith('q_')) {
      const to = map.get(currentKey.slice(2) as SessionUid);
      if (to) currentKey = `q_${to}`;
    }
    chain = { ...chain, origin: remapView(chain.origin, map), currentKey };
  }

  const focusCard = ui.focusCard ? { ...ui.focusCard, uid: r(ui.focusCard.uid) as SessionUid } : null;
  const homeFocusUid = r(ui.homeFocusUid);
  let visibleUids: SessionUid[] | null = null;
  if (ui.visibleUids) {
    visibleUids = [];
    for (const u of ui.visibleUids) {
      const to = map.get(u) ?? u;
      if (!visibleUids.includes(to)) visibleUids.push(to);
    }
  }

  return {
    ...ui,
    view: remapView(ui.view, map),
    back: dedupeStack(ui.back.map((v) => remapView(v, map))),
    forward: dedupeStack(ui.forward.map((v) => remapView(v, map))),
    split: { uids, focused: r(ui.split.focused), zoomed: r(ui.split.zoomed) },
    drafts,
    chain,
    focusCard,
    homeFocusUid,
    visibleUids,
  };
}

/** The view a queue item opens. */
export function viewForItem(item: AttentionItem): MainView {
  if (item.subject.kind === 'leftovers') return { screen: 'accounts', section: 'health' };
  const uid = item.subject.answerIn ?? item.subject.uid;
  return { screen: 'session', uid, sub: 'conversation' };
}

/**
 * The next item of the `⌘J` chain. `dir` 1 = next, -1 = previous. Returns
 * `null` when the chain runs off either end (the caller returns to the
 * chain's origin). When the current item was handled meanwhile, "next" is the
 * item that now holds its rank slot.
 */
export function chainStep(
  items: readonly AttentionItem[],
  chain: QueueChain | null,
  dir: 1 | -1,
): { item: AttentionItem; index: number } | null {
  if (items.length === 0) return null;
  if (!chain || chain.currentKey === null) {
    const index = dir === 1 ? 0 : items.length - 1;
    return { item: items[index] as AttentionItem, index };
  }
  const at = items.findIndex((i) => i.key === chain.currentKey);
  let index: number;
  if (at >= 0) index = at + dir;
  else index = dir === 1 ? Math.min(chain.currentIndex, items.length) : chain.currentIndex - 1;
  if (index < 0 || index >= items.length) return null;
  return { item: items[index] as AttentionItem, index };
}

// ---------------------------------------------------------------------------
// Slice
// ---------------------------------------------------------------------------

export interface UiSlice {
  ui: UiState;
  setLocale: (locale: Locale) => void;
  setTheme: (theme: Theme) => void;
  /** Show a main view (pushes history; leaving the chain's path ends the chain). */
  navigate: (view: MainView) => void;
  goBack: () => void;
  goForward: () => void;
  /** Open a session's screen, optionally asking its top card to take focus. */
  openSession: (uid: SessionUid, options?: { sub?: SessionSub; focusCard?: boolean }) => void;
  setSessionSub: (sub: SessionSub) => void;
  toggleSidebar: () => void;
  setSidebarVisible: (visible: boolean) => void;
  /** Add to Split (`⌘D`); false when the Split already holds 6 (ux-ia §9.2 split.max). */
  addToSplit: (uid: SessionUid) => boolean;
  removeFromSplit: (uid: SessionUid) => void;
  setSplitFocus: (uid: SessionUid | null) => void;
  toggleSplitZoom: (uid: SessionUid) => void;
  setTeamLayout: (teamKey: TeamKey, patch: Partial<TeamLayout>) => void;
  /** Panes / Board / Floor without a view-history entry; the mode is kept in the team's layout (A30.4). */
  setTeamMode: (teamKey: TeamKey, mode: TeamViewMode) => void;
  /** The sessions a multi-session screen shows; null when it unmounts (A30.2). */
  setVisibleUids: (uids: readonly SessionUid[] | null) => void;
  setHomeMode: (mode: HomeMode) => void;
  setDraft: (uid: SessionUid, text: string) => void;
  clearDraft: (uid: SessionUid) => void;
  /** `⌘J` / `⇧⌘J` over the visible queue items; returns the item reached or null (back at the origin). */
  chainStep: (items: readonly AttentionItem[], dir: 1 | -1) => AttentionItem | null;
  endChain: () => void;
  setPaletteOpen: (open: boolean) => void;
  /** Open a sheet, optionally with what it was opened for (A26.2). */
  openSheet: (sheet: SheetId, context?: SheetContext) => void;
  closeSheet: () => void;
  /** Home's focus card shows this session (A26.1); null when it shows none. */
  setHomeFocusUid: (uid: SessionUid | null) => void;
  /** Set a History read's title, or clear it (`null`); clearing for another uid is a no-op. */
  setReadTitle: (entry: ReadTitle | null, clearFor?: SessionUid) => void;
  setZoom: (zoom: number) => void;
  zoomBy: (steps: number) => void;
  setVisibility: (event: VisibilityEvent) => void;
  setDocumentVisible: (visible: boolean) => void;
  setQuitRequested: (requested: boolean) => void;
  setBuildMismatch: (mismatch: boolean) => void;
  setSidebarOrder: (value: JsonValue) => void;
  setSidebarFolds: (value: JsonValue) => void;
  setFloorPrefs: (value: JsonValue) => void;
  /** Apply `app_bootstrap` (locale, theme, launch, persisted UI state). */
  hydrateFromBootstrap: (res: AppBootstrapRes) => void;
  /** Called by the World slice for every new tombstone. */
  remapSessionRefs: (merged: ReadonlyMap<SessionUid, SessionUid>) => void;
}

let focusToken = 0;

export const createUiSlice: StateCreator<UiSlice, [['zustand/subscribeWithSelector', never]], [], UiSlice> = (
  set,
  get,
) => {
  const patch = (p: Partial<UiState>) => set((s) => ({ ui: { ...s.ui, ...p } }));

  return {
    ui: initialUiState(),

    setLocale: (locale) => patch({ locale }),
    setTheme: (theme) => patch({ theme }),

    navigate: (view) => {
      const ui = get().ui;
      patch({ ...pushView(ui, view), chain: null });
    },

    goBack: () => {
      const ui = get().ui;
      const prev = ui.back[ui.back.length - 1];
      if (!prev) return;
      patch({ view: prev, back: ui.back.slice(0, -1), forward: [ui.view, ...ui.forward], chain: null });
    },

    goForward: () => {
      const ui = get().ui;
      const next = ui.forward[0];
      if (!next) return;
      patch({ view: next, back: [...ui.back, ui.view].slice(-MAX_VIEW_HISTORY), forward: ui.forward.slice(1), chain: null });
    },

    openSession: (uid, options = {}) => {
      const ui = get().ui;
      const sub = options.sub ?? 'conversation';
      const focusCard = options.focusCard ? { uid, token: ++focusToken } : ui.focusCard;
      patch({ ...pushView(ui, { screen: 'session', uid, sub }), focusCard, chain: null });
    },

    setSessionSub: (sub) => {
      const ui = get().ui;
      if (ui.view.screen !== 'session' || ui.view.sub === sub) return;
      patch({ view: { ...ui.view, sub } });
    },

    toggleSidebar: () => patch({ sidebarVisible: !get().ui.sidebarVisible }),
    setSidebarVisible: (visible) => patch({ sidebarVisible: visible }),

    addToSplit: (uid) => {
      const split = get().ui.split;
      if (split.uids.includes(uid)) return true;
      if (split.uids.length >= MAX_SPLIT) return false;
      patch({ split: { ...split, uids: [...split.uids, uid] } });
      return true;
    },

    removeFromSplit: (uid) => {
      const split = get().ui.split;
      const at = split.uids.indexOf(uid);
      if (at < 0) return;
      const uids = split.uids.filter((u) => u !== uid);
      // Closing a tile moves focus to the previous tile in reading order (ux-ia §10).
      const focused = split.focused === uid ? (uids[Math.max(0, at - 1)] ?? null) : split.focused;
      patch({ split: { uids, focused, zoomed: split.zoomed === uid ? null : split.zoomed } });
    },

    setSplitFocus: (uid) => {
      const split = get().ui.split;
      if (uid !== null && !split.uids.includes(uid)) return;
      patch({ split: { ...split, focused: uid } });
    },

    toggleSplitZoom: (uid) => {
      const split = get().ui.split;
      if (!split.uids.includes(uid)) return;
      patch({ split: { ...split, zoomed: split.zoomed === uid ? null : uid } });
    },

    setTeamLayout: (teamKey, p) => {
      const layouts = get().ui.teamLayouts;
      const current = layouts[teamKey] ?? defaultTeamLayout();
      patch({ teamLayouts: { ...layouts, [teamKey]: { ...current, ...p } } });
    },

    setTeamMode: (teamKey, mode) => {
      const ui = get().ui;
      const current = ui.teamLayouts[teamKey] ?? defaultTeamLayout();
      const teamLayouts = current.mode === mode ? ui.teamLayouts : { ...ui.teamLayouts, [teamKey]: { ...current, mode } };
      const view = ui.view;
      const onTeam = view.screen === 'team' && view.teamKey === teamKey && view.mode !== mode;
      if (teamLayouts === ui.teamLayouts && !onTeam) return;
      // Like a Session sub-view: the view is replaced, never pushed (ux-ia K7, K8).
      patch(onTeam ? { teamLayouts, view: { screen: 'team', teamKey, mode } } : { teamLayouts });
    },

    setVisibleUids: (uids) => {
      const current = get().ui.visibleUids;
      if (uids === null) {
        if (current !== null) patch({ visibleUids: null });
        return;
      }
      const next: SessionUid[] = [];
      for (const u of uids) if (!next.includes(u)) next.push(u);
      if (current && current.length === next.length && current.every((u, i) => u === next[i])) return;
      patch({ visibleUids: next });
    },

    setHomeMode: (mode) => patch({ homeMode: mode }),

    setDraft: (uid, text) => {
      const drafts = get().ui.drafts;
      if (drafts[uid] === text) return;
      patch({ drafts: { ...drafts, [uid]: text } });
    },

    clearDraft: (uid) => {
      const drafts = { ...get().ui.drafts };
      if (!(uid in drafts)) return;
      delete drafts[uid];
      patch({ drafts });
    },

    chainStep: (items, dir) => {
      const ui = get().ui;
      const step = chainStep(items, ui.chain, dir);
      const origin = ui.chain?.origin ?? ui.view;
      if (!step) {
        patch({ ...pushView(ui, origin), chain: null });
        return null;
      }
      const view = viewForItem(step.item);
      const uid = view.screen === 'session' ? view.uid : null;
      patch({
        ...pushView(ui, view),
        chain: { origin, currentKey: step.item.key, currentIndex: step.index },
        focusCard: uid ? { uid, token: ++focusToken } : ui.focusCard,
      });
      return step.item;
    },

    endChain: () => patch({ chain: null }),

    setPaletteOpen: (open) => patch({ paletteOpen: open }),
    openSheet: (sheet, context) => patch({ sheet, sheetContext: context ?? null, paletteOpen: false }),
    closeSheet: () => patch({ sheet: null, sheetContext: null }),
    setHomeFocusUid: (uid) => {
      if (get().ui.homeFocusUid !== uid) patch({ homeFocusUid: uid });
    },
    setReadTitle: (entry, clearFor) => {
      const cur = get().ui.readTitle;
      if (entry === null) {
        if (cur !== null && (clearFor === undefined || cur.uid === clearFor)) patch({ readTitle: null });
        return;
      }
      if (cur?.uid !== entry.uid || cur.title !== entry.title) patch({ readTitle: entry });
    },

    setZoom: (zoom) => patch({ zoom: clampZoom(zoom) }),
    zoomBy: (steps) => patch({ zoom: clampZoom(get().ui.zoom + steps * ZOOM_STEP) }),

    setVisibility: (event) => {
      const v = get().ui.visibility;
      patch({ visibility: { ...v, frontmost: event.frontmost, occluded: event.occluded, asleep: event.asleep } });
    },

    setDocumentVisible: (visible) => {
      const v = get().ui.visibility;
      if (v.documentVisible === visible) return;
      patch({ visibility: { ...v, documentVisible: visible } });
    },

    setQuitRequested: (requested) => patch({ quitRequested: requested }),
    setBuildMismatch: (mismatch) => patch({ buildMismatch: mismatch }),

    setSidebarOrder: (value) => patch({ sidebarOrder: value }),
    setSidebarFolds: (value) => patch({ sidebarFolds: value }),
    setFloorPrefs: (value) => patch({ floorPrefs: value }),

    hydrateFromBootstrap: (res) => {
      const state = res.uiState ?? {};
      const teamLayouts: Record<string, TeamLayout> = {};
      for (const [key, value] of Object.entries(state)) {
        if (key.startsWith('teamLayout:')) {
          const teamKey = key.slice('teamLayout:'.length);
          if (asKey(teamKey)) teamLayouts[teamKey] = parseTeamLayout(value);
        }
      }
      // Restore the last view only on a restore launch; the other launch
      // kinds open Home (ux-ia §2.7).
      const restore = res.launch.kind === 'restore';
      const view = restore ? parseView(state.lastView) : HOME;
      patch({
        locale: res.locale,
        theme: res.theme,
        launch: res.launch,
        buildId: res.buildId,
        version: res.version,
        platform: res.platform,
        features: res.features,
        view,
        back: [],
        forward: [],
        split: parseSplit(state.splitSet),
        teamLayouts,
        homeMode: state.homeMode === 'classic' ? 'classic' : 'focus',
        sidebarOrder: state.sidebarOrder ?? null,
        sidebarFolds: state.sidebarFolds ?? null,
        floorPrefs: state.floorPrefs ?? null,
        hydrated: true,
      });
    },

    remapSessionRefs: (merged) => {
      set((s) => ({ ui: remapUi(s.ui, merged) }));
    },
  };
};
