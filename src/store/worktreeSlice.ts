/**
 * Worktree slice (T21) — the sidebar's command layer and the client-side
 * projector that keeps project/worktree nodes in the one graph store.
 *
 * Two rules shape this file.
 *
 * 1. **The tree renders from `graph.nodes` and nothing else** (M1-PLAN T21
 *    acceptance 1). `ProjectTree` never calls a command and never reads a list
 *    kept here, so the sidebar and the graph dashboard are two renderings of one
 *    projection and cannot disagree about what exists.
 * 2. Therefore everything this slice learns from the backend is *folded into
 *    that projection* rather than kept beside it. `project_list` /
 *    `worktree_list` are the same durable rows Rust projects at boot
 *    (`seed_graph` in `src-tauri/src/lib.rs`), so re-projecting them here is a
 *    refresh of the same view, not a second source of truth. Live numbers
 *    arrive as `gilde://worktree/changed` and are folded into the node's
 *    `metrics`, which is where every surface reads them from.
 *
 * Why the projection exists at all: `seed_graph` runs once, at boot, and T13's
 * `worktree_create` / `worktree_remove` do not publish graph ops yet. Without
 * this fold a worktree the user just created would be invisible until the app
 * restarted. When Rust starts publishing those ops the fold becomes a no-op —
 * it only ever writes what the roster says, in exactly the node and edge shape
 * `model/graph.rs` uses (`"<kind>:<source>><target>"` edge ids included).
 *
 * The roster kept in `projects` is *not* what the tree reads. It exists because
 * `NewWorktreeDialog` needs `repoSlug` and `defaultBaseRef`, which the graph
 * node does not carry.
 */

import type { StateCreator } from 'zustand';

import { onGildeEvent } from '../hooks/useEventBridge';
import { asGildeError, cmd, isUnavailable } from '../lib/commands';
import { WORKTREE_CHANGED } from '../lib/ipc';
import type {
  AliasProvider,
  AliasProviderKey,
  GildeErrorPayload,
  Badge as NodeBadge,
  CreateWorktreeReq,
  GraphEdge,
  GraphNode,
  LaunchSpec,
  Project,
  ProjectId,
  RemoveMode,
  RemovePreflight,
  Worktree,
  WorktreeChangedEvent,
  WorktreeId,
  WorktreeStats,
} from '../lib/ipc';
import type { GraphSlice, GraphView } from './graphSlice';
import { topologyDigest } from './graphSlice';
import type { SessionSlice } from './sessionSlice';
import type { ToastSlice } from './toastSlice';
import type { UiSlice } from './uiSlice';

// ---------------------------------------------------------------------------
// Types
// ---------------------------------------------------------------------------

/** One row of the roster: the project and the worktrees git still has. */
export interface RosterEntry {
  project: Project;
  worktrees: Worktree[];
}

export interface NewWorktreeState {
  open: boolean;
  /** Pre-selected project; `null` means "use the selected node's project". */
  projectId: ProjectId | null;
}

/** Everything `DirtyWorktreeDialog` renders, gathered before anything is destroyed. */
export interface RemoveDialogState {
  open: boolean;
  worktreeId: WorktreeId | null;
  /** Slug and branch, so the card names its target even while preflight loads. */
  label: string;
  branch: string;
  preflight: RemovePreflight | null;
  stats: WorktreeStats | null;
  loading: boolean;
  busy: boolean;
  error: GildeErrorPayload | null;
}

export interface WorktreeState {
  /** Dialog data only — never the source the tree renders from. */
  roster: RosterEntry[];
  loading: boolean;
  loaded: boolean;
  /**
   * The last refusal, rendered as one localized line in the sidebar. The toast
   * host lands with T26; a rejection the user cannot see is a rejection that
   * did not happen.
   */
  error: GildeErrorPayload | null;
  newWorktree: NewWorktreeState;
  remove: RemoveDialogState;
  /** Per-project default for `post_create_cmd` (DECISIONS §M1 세부 기본값). */
  postCreateCmd: Record<ProjectId, string>;
  /** Sidebar filter box. */
  filter: string;
  /** Node ids the user collapsed. Absent means expanded. */
  collapsed: Record<string, true>;
  /**
   * Bumped when `OrphanGroup` is clicked. `procSlice` (T23) watches it and
   * applies the our-orphan filter; the sidebar cannot reach into a slice that
   * does not exist yet.
   */
  orphanFilterRequestedAt: number;
}

export interface WorktreeSlice {
  worktree: WorktreeState;

  /** Re-read the durable rows and fold them into the graph projection. */
  loadRoster: () => Promise<void>;
  /** `plugin-dialog` folder picker → `project_add`. */
  addProject: () => Promise<Project | null>;
  removeProject: (id: ProjectId) => Promise<void>;
  createWorktree: (input: CreateWorktreeInput) => Promise<Worktree | null>;
  setWorktreeLocked: (id: WorktreeId, locked: boolean) => Promise<void>;

  setNewWorktreeOpen: (open: boolean, projectId?: ProjectId | null) => void;
  openRemoveDialog: (id: WorktreeId, label: string, branch: string) => Promise<void>;
  closeRemoveDialog: () => void;
  confirmRemove: (mode: RemoveMode) => Promise<boolean>;

  setSidebarFilter: (filter: string) => void;
  toggleCollapsed: (nodeId: string) => void;
  requestOrphanFilter: () => void;
  setPostCreateCmd: (projectId: ProjectId, cmd: string) => void;
  clearWorktreeError: () => void;
}

/** What `NewWorktreeDialog` submits. */
export interface CreateWorktreeInput {
  projectId: ProjectId;
  taskName: string;
  baseRef?: string;
  aliasProviders: AliasProviderKey[];
  postCreateCmd?: string;
  /** Opened by the front end after the worktree exists (see `createWorktree`). */
  launch?: LaunchSpec;
}

// ---------------------------------------------------------------------------
// Alias providers (DESIGN §7-6)
// ---------------------------------------------------------------------------

export interface AliasProviderOption {
  key: AliasProviderKey;
  /** DESIGN §7-6 `verified_against`. Cursor is not installed on this machine. */
  verified: boolean;
  /** Cursor ships unverified, so it starts unchecked. */
  defaultOn: boolean;
}

/** `custom` needs a path and has no sensible default, so it is not offered. */
export const ALIAS_PROVIDER_OPTIONS: readonly AliasProviderOption[] = [
  { key: 'claude-w', verified: true, defaultOn: true },
  { key: 'generic', verified: true, defaultOn: true },
  { key: 'cursor-worktree', verified: false, defaultOn: false },
];

export function aliasProviderOf(key: AliasProviderKey): AliasProvider | null {
  switch (key) {
    case 'claude-w':
      return { provider: 'claude-w' };
    case 'cursor-worktree':
      return { provider: 'cursor-worktree' };
    case 'generic':
      return { provider: 'generic' };
    default:
      // `custom` carries a link path the sidebar does not collect.
      return null;
  }
}

// ---------------------------------------------------------------------------
// Slug preview (DESIGN §7-1, mirrors `src-tauri/src/worktree/naming.rs`)
// ---------------------------------------------------------------------------

/** `naming.rs::MAX_SLUG_BODY`. */
export const MAX_SLUG_BODY = 40;
/** `naming.rs::SHORT_ID_LEN`. */
export const SHORT_ID_LEN = 6;
/** `naming.rs::FALLBACK_BODY` — what a name of pure Hangul or emoji becomes. */
export const FALLBACK_SLUG_BODY = 'task';

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
 * The `[a-z0-9]` pipeline of `naming.rs::sanitize`, in the browser.
 *
 * `String.normalize('NFKD')` plus a combining-mark strip is the same fold the
 * Rust side spells out by hand (it has no Unicode crate pinned): `é` → `e`,
 * fullwidth `Ａ` → `a`, and Hangul/CJK/emoji decompose to code points the
 * `[a-z0-9]` filter drops either way. The two must agree, because this is the
 * slug the dialog promises the user and Rust is the one that creates it.
 */
export function slugBody(name: string): string {
  const folded = name
    .normalize('NFKD')
    .replace(/[\u0300-\u036f]/g, '')
    .toLowerCase()
    .replace(/[æœßøđħıłŧ]/g, (ch) => LATIN_FOLD[ch] ?? ch);

  let out = '';
  let pendingDash = false;
  for (const ch of folded) {
    if (ch >= 'a' && ch <= 'z') {
      if (pendingDash && out.length > 0) out += '-';
      pendingDash = false;
      out += ch;
    } else if (ch >= '0' && ch <= '9') {
      if (pendingDash && out.length > 0) out += '-';
      pendingDash = false;
      out += ch;
    } else {
      pendingDash = true;
    }
  }

  let body = [...out].slice(0, MAX_SLUG_BODY).join('');
  while (body.endsWith('-')) body = body.slice(0, -1);
  return body;
}

/** Why the dialog refuses to submit. `null` means the name is usable. */
export type SlugIssue = 'empty' | 'unsupported' | 'duplicate';

/**
 * Rust appends a random 6-hex short id, so two worktrees can never collide on
 * disk. What *can* collide is the human-visible half — two `fix-login-*`
 * directories under one repository is exactly the confusion the short id was
 * meant to make survivable, not desirable. So the dialog blocks on a duplicate
 * body and says which one it clashes with.
 */
export function slugIssue(name: string, existingSlugs: readonly string[]): SlugIssue | null {
  if (name.trim().length === 0) return 'empty';
  const body = slugBody(name);
  if (body.length === 0) return 'unsupported';
  for (const slug of existingSlugs) {
    if (slugBodyOf(slug) === body) return 'duplicate';
  }
  return null;
}

/** Strip the `-<short6>` suffix Rust appended, to compare human halves. */
export function slugBodyOf(slug: string): string {
  const at = slug.lastIndexOf('-');
  if (at <= 0) return slug;
  const tail = slug.slice(at + 1);
  if (tail.length !== SHORT_ID_LEN || !/^[0-9a-f]+$/.test(tail)) return slug;
  return slug.slice(0, at);
}

/**
 * The slug Rust will produce, with the random half shown as a placeholder.
 *
 * `naming.rs` appends six hex characters from a fresh UUID, so the exact
 * directory cannot be known before the call. Showing the shape is honest and is
 * what the user actually needs: where it lands and under which name.
 */
export const SHORT_ID_PLACEHOLDER = '\u00b7'.repeat(SHORT_ID_LEN);

export function previewSlug(name: string): string {
  const body = slugBody(name) || FALLBACK_SLUG_BODY;
  return `${body}-${SHORT_ID_PLACEHOLDER}`;
}

/** `~/.gilde/worktrees/<repo-slug>/<slug>` (DESIGN §7-1). */
export function previewWorktreePath(
  worktreesRoot: string,
  repoSlug: string,
  name: string,
): string {
  const root = worktreesRoot.replace(/\/+$/, '');
  return `${root}/${repoSlug}/${previewSlug(name)}`;
}

/** `gilde/<slug>` — the branch the create will ask git for. */
export function previewBranch(name: string): string {
  return `gilde/${previewSlug(name)}`;
}

// ---------------------------------------------------------------------------
// Graph projection (pure — exercised by sidebar.test.tsx)
// ---------------------------------------------------------------------------

/** Mirrors `model::graph::GraphEdge::edge_id`. */
export function edgeId(kind: GraphEdge['kind'], source: string, target: string): string {
  return `${kind}:${source}>${target}`;
}

function withoutBadge(badges: readonly NodeBadge[], badge: NodeBadge): NodeBadge[] {
  return badges.filter((b) => b !== badge);
}

function withBadge(badges: readonly NodeBadge[], badge: NodeBadge, on: boolean): NodeBadge[] {
  const without = withoutBadge(badges, badge);
  return on ? [...without, badge] : without;
}

/**
 * Fold the roster into the node and edge maps.
 *
 * Project and worktree nodes are replaced wholesale — the roster is the same
 * durable table `seed_graph` reads, so anything of those two kinds that is not
 * in it no longer exists. Every other kind (sessions, runs, orphans) is left
 * exactly as it is, including their object identity, so a roster refresh cannot
 * repaint a terminal's node.
 *
 * Status, confidence, badges and metrics of a node that is already there are
 * preserved: those come from live events and the roster knows nothing about
 * them.
 */
export function projectRoster(graph: GraphView, roster: readonly RosterEntry[], now: number) {
  const nodes = new Map(graph.nodes);
  const edges = new Map(graph.edges);

  const keepProjects = new Set(roster.map((entry) => `project:${entry.project.id}`));
  const keepWorktrees = new Set(
    roster.flatMap((entry) => entry.worktrees.map((wt) => `worktree:${wt.id}`)),
  );

  for (const node of graph.nodes.values()) {
    if (node.kind === 'project' && !keepProjects.has(node.id)) nodes.delete(node.id);
    if (node.kind === 'worktree' && !keepWorktrees.has(node.id)) nodes.delete(node.id);
  }
  const ours = (nodeId: string) =>
    nodeId.startsWith('project:') || nodeId.startsWith('worktree:');
  for (const edge of graph.edges.values()) {
    // Prune only edges this projection can be responsible for: one whose ends
    // are both still alive belongs to another lane and is left alone.
    if (nodes.has(edge.source) && nodes.has(edge.target)) continue;
    if (ours(edge.source) || ours(edge.target)) edges.delete(edge.id);
  }

  for (const entry of roster) {
    const projectNodeId = `project:${entry.project.id}`;
    const previous = nodes.get(projectNodeId);
    const project: GraphNode = {
      id: projectNodeId,
      kind: 'project',
      parentId: null,
      label: entry.project.name,
      sublabel: entry.project.repoSlug,
      status: previous?.status ?? 'ok',
      badges: previous?.badges ?? [],
      metrics: previous?.metrics ?? {},
      meta: { ...previous?.meta, cwd: entry.project.canonicalPath },
      updatedAt: previous?.updatedAt ?? now,
      ...(previous?.confidence === undefined ? {} : { confidence: previous.confidence }),
      ...(previous?.evidence === undefined ? {} : { evidence: previous.evidence }),
    };
    nodes.set(projectNodeId, project);

    for (const worktree of entry.worktrees) {
      const id = `worktree:${worktree.id}`;
      const before = nodes.get(id);
      const badges = withBadge(before?.badges ?? [], 'locked', worktree.locked);
      const node: GraphNode = {
        id,
        kind: 'worktree',
        parentId: projectNodeId,
        label: worktree.slug,
        sublabel: worktree.branch,
        status: before?.status ?? 'clean',
        badges,
        metrics: before?.metrics ?? {},
        meta: {
          ...before?.meta,
          branch: worktree.branch,
          worktreeId: worktree.id,
          cwd: worktree.path,
        },
        updatedAt: before?.updatedAt ?? now,
        ...(before?.confidence === undefined ? {} : { confidence: before.confidence }),
        ...(before?.evidence === undefined ? {} : { evidence: before.evidence }),
      };
      nodes.set(id, node);

      const contains = edgeId('contains', projectNodeId, id);
      if (!edges.has(contains)) {
        edges.set(contains, {
          id: contains,
          source: projectNodeId,
          target: id,
          kind: 'contains',
          active: false,
        });
      }
    }
  }

  return { nodes, edges };
}

/**
 * Fold `gilde://worktree/changed` into the worktree node's metrics, status and
 * badges — the numbers the row shows are graph numbers, not a private cache.
 */
export function applyWorktreeChanged(graph: GraphView, event: WorktreeChangedEvent): GraphView {
  const id = `worktree:${event.worktreeId}`;
  const current = graph.nodes.get(id);
  if (!current) return graph;

  const dirty = event.dirty > 0;
  const unmerged = event.ahead > 0;
  let badges = withBadge(current.badges, 'dirty', dirty);
  badges = withBadge(badges, 'unmerged', unmerged);

  const next: GraphNode = {
    ...current,
    status: dirty ? 'dirty' : 'clean',
    badges,
    metrics: {
      ...current.metrics,
      dirtyFiles: event.dirty,
      insertions: event.added,
      deletions: event.removed,
      ahead: event.ahead,
      behind: event.behind,
    },
    updatedAt: event.at,
  };

  const nodes = new Map(graph.nodes);
  nodes.set(id, next);
  return { ...graph, nodes };
}

// ---------------------------------------------------------------------------
// Folder picker seam
// ---------------------------------------------------------------------------

export type FolderPicker = () => Promise<string | null>;

/**
 * `@tauri-apps/plugin-dialog` is imported lazily so the module graph stays
 * loadable outside a Tauri window (the unit tests bundle this file for Node).
 */
const tauriFolderPicker: FolderPicker = async () => {
  const { open } = await import('@tauri-apps/plugin-dialog');
  const picked = await open({ directory: true, multiple: false });
  return typeof picked === 'string' ? picked : null;
};

let folderPicker: FolderPicker = tauriFolderPicker;

/** Test seam; production never calls this. */
export function setFolderPicker(picker: FolderPicker): void {
  folderPicker = picker;
}

export function resetFolderPicker(): void {
  folderPicker = tauriFolderPicker;
}

// ---------------------------------------------------------------------------
// Slice
// ---------------------------------------------------------------------------

export function initialWorktreeState(): WorktreeState {
  return {
    roster: [],
    loading: false,
    loaded: false,
    error: null,
    newWorktree: { open: false, projectId: null },
    remove: {
      open: false,
      worktreeId: null,
      label: '',
      branch: '',
      preflight: null,
      stats: null,
      loading: false,
      busy: false,
      error: null,
    },
    postCreateCmd: {},
    filter: '',
    collapsed: {},
    orphanFilterRequestedAt: 0,
  };
}

export const createWorktreeSlice: StateCreator<
  WorktreeSlice & GraphSlice & UiSlice & ToastSlice & SessionSlice,
  [['zustand/subscribeWithSelector', never]],
  [],
  WorktreeSlice
> = (set, get) => {
  const patch = (update: Partial<WorktreeState>) =>
    set({ worktree: { ...get().worktree, ...update } });

  const fail = (e: unknown) => {
    const error = asGildeError(e);
    patch({ error });
    get().pushErrorToast(error);
    return error;
  };

  const commitProjection = (roster: RosterEntry[]) => {
    const graph = get().graph;
    const { nodes, edges } = projectRoster(graph, roster, Date.now());
    set({
      graph: {
        ...graph,
        nodes,
        edges,
        topologyHash: topologyDigest(nodes.keys(), edges.keys()),
      },
    });
  };

  /** The roster read in flight, and whether another was asked for meanwhile (G-b). */
  let rosterRun: Promise<void> | null = null;
  let rosterAgain = false;

  const loadRosterOnce = async (): Promise<void> => {
    patch({ loading: true });
    try {
      const projects = await cmd.projectList();
      const roster: RosterEntry[] = [];
      for (const project of projects) {
        let worktrees: Worktree[] = [];
        try {
          worktrees = await cmd.worktreeList(project.id);
        } catch (e) {
          if (!isUnavailable(e)) console.warn('[gilde] worktree_list failed', e);
        }
        roster.push({ project, worktrees });
      }
      commitProjection(roster);
      patch({ roster, loading: false, loaded: true });
    } catch (e) {
      patch({ loading: false, loaded: true });
      if (!isUnavailable(e)) console.warn('[gilde] project_list failed', e);
    }
  };

  // One fan-out subscription, deferred by a microtask for the same reason
  // `sessionSlice` defers its own: this runs while the store is being built.
  queueMicrotask(() => {
    onGildeEvent(WORKTREE_CHANGED, (event) => {
      const graph = applyWorktreeChanged(get().graph, event);
      if (graph !== get().graph) set({ graph });
    });
  });

  return {
    worktree: initialWorktreeState(),

    loadRoster: () => {
      // Live check G-b. One read at a time, and a request that arrives while a
      // read is in flight is NOT dropped: it runs again right after, and every
      // caller awaits the read that started after its own request. The old
      // guard (`if (loading) return;`) returned at once, so `removeProject` /
      // `addProject` "reloaded" without a read — the in-flight read, started
      // before the change, then landed and showed the stale folder list (and
      // count) until some later refresh read it again.
      if (rosterRun !== null) {
        rosterAgain = true;
        return rosterRun;
      }
      const run = async () => {
        do {
          rosterAgain = false;
          await loadRosterOnce();
        } while (rosterAgain);
      };
      rosterRun = run().finally(() => {
        rosterRun = null;
      });
      return rosterRun;
    },

    addProject: async () => {
      let path: string | null = null;
      try {
        path = await folderPicker();
      } catch (e) {
        fail(e);
        return null;
      }
      if (!path) return null;
      try {
        const project = await cmd.projectAdd(path);
        patch({ error: null });
        await get().loadRoster();
        get().selectNode(`project:${project.id}`);
        return project;
      } catch (e) {
        // `project_add` rejects a non-repository with a typed, localizable key
        // (`error.project.notARepository`); nothing here invents prose.
        fail(e);
        return null;
      }
    },

    removeProject: async (id) => {
      try {
        await cmd.projectRemove(id);
        patch({ error: null });
        await get().loadRoster();
      } catch (e) {
        fail(e);
      }
    },

    createWorktree: async (input) => {
      const providers = input.aliasProviders
        .map(aliasProviderOf)
        .filter((p): p is AliasProvider => p !== null);
      const req: CreateWorktreeReq = {
        projectId: input.projectId,
        taskName: input.taskName,
        aliasProviders: providers,
        ...(input.baseRef ? { baseRef: input.baseRef } : {}),
        ...(input.postCreateCmd ? { postCreateCmd: input.postCreateCmd } : {}),
      };
      try {
        const worktree = await cmd.worktreeCreate(req);
        patch({ error: null, newWorktree: { open: false, projectId: null } });
        if (input.postCreateCmd !== undefined) {
          patch({
            postCreateCmd: { ...get().worktree.postCreateCmd, [input.projectId]: input.postCreateCmd },
          });
        }
        await get().loadRoster();
        get().selectNode(`worktree:${worktree.id}`);

        // `CreateWorktreeReq.launch` is persisted by T13 but not acted on, so
        // the session is opened here — one code path, the same one ⌘T uses.
        if (input.launch) {
          const info = await get().openSession({
            projectId: input.projectId,
            worktreeId: worktree.id,
            agent: input.launch.agent,
            cols: input.launch.cols ?? 80,
            rows: input.launch.rows ?? 24,
            ...(input.launch.accountId ? { accountId: input.launch.accountId } : {}),
            ...(input.launch.prompt ? { prompt: input.launch.prompt } : {}),
          });
          if (info) {
            get().openTab({
              title: info.agent,
              sublabel: worktree.slug,
              descriptor: { view: 'terminal', sessionId: info.id },
              status: 'idle',
              closable: true,
            });
          }
        }
        return worktree;
      } catch (e) {
        fail(e);
        return null;
      }
    },

    setWorktreeLocked: async (id, locked) => {
      try {
        await cmd.worktreeLock(id, locked);
        await get().loadRoster();
      } catch (e) {
        fail(e);
      }
    },

    setNewWorktreeOpen: (open, projectId) =>
      patch({
        newWorktree: {
          open,
          projectId: projectId ?? (open ? get().worktree.newWorktree.projectId : null),
        },
      }),

    openRemoveDialog: async (id, label, branch) => {
      patch({
        remove: {
          open: true,
          worktreeId: id,
          label,
          branch,
          preflight: null,
          stats: null,
          loading: true,
          busy: false,
          error: null,
        },
      });
      let preflight: RemovePreflight | null = null;
      let stats: WorktreeStats | null = null;
      let error: GildeErrorPayload | null = null;
      try {
        preflight = await cmd.worktreeRemovePreflight(id);
      } catch (e) {
        error = asGildeError(e);
      }
      try {
        // `RemovePreflight` carries counts; `+A/−D` lives on `WorktreeStats`.
        stats = await cmd.worktreeStats(id);
      } catch {
        stats = null;
      }
      const current = get().worktree.remove;
      if (current.worktreeId !== id) return;
      patch({ remove: { ...current, preflight, stats, loading: false, error } });
    },

    closeRemoveDialog: () =>
      patch({ remove: { ...initialWorktreeState().remove } }),

    confirmRemove: async (mode) => {
      const state = get().worktree.remove;
      if (!state.worktreeId || state.busy) return false;
      patch({ remove: { ...state, busy: true, error: null } });
      try {
        const outcome = await cmd.worktreeRemove(state.worktreeId, mode);
        patch({ remove: { ...initialWorktreeState().remove } });
        get().pushToast({
          level: 'success',
          key: 'worktree.removed',
          params: { branch: outcome.branchKept },
          detail: null,
          actionId: null,
          undoToken: null,
          ttlMs: 6000,
        });
        get().selectNode(null);
        await get().loadRoster();
        return true;
      } catch (e) {
        const error = asGildeError(e);
        const now = get().worktree.remove;
        patch({ remove: { ...now, busy: false, error } });
        return false;
      }
    },

    setSidebarFilter: (filter) => patch({ filter }),

    toggleCollapsed: (nodeId) => {
      const collapsed = { ...get().worktree.collapsed };
      if (collapsed[nodeId]) delete collapsed[nodeId];
      else collapsed[nodeId] = true;
      patch({ collapsed });
    },

    requestOrphanFilter: () => patch({ orphanFilterRequestedAt: Date.now() }),

    setPostCreateCmd: (projectId, value) =>
      patch({ postCreateCmd: { ...get().worktree.postCreateCmd, [projectId]: value } }),

    clearWorktreeError: () => patch({ error: null }),
  };
};
