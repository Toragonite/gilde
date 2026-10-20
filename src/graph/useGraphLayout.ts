/**
 * Layout for the live status graph (T25, DESIGN §9-6).
 *
 * The whole point of this file is one rule: **dagre runs only when
 * `topologyHash` changes.** Metrics arrive at 1 Hz per session; if a layout pass
 * were coupled to them the canvas would twitch every second and be unreadable.
 * `LayoutCache` therefore keys its result on the topology digest plus the manual
 * positions, and hands back the *same* `Map` instance otherwise — so a metrics
 * patch cannot move a node by even a fraction of a pixel.
 *
 * Manual positions win over dagre and survive later topology changes: a node the
 * user dragged is pinned, persisted per project in `settings_kv`
 * (`graph_layout_get` / `graph_layout_save`), and re-applied after every layout.
 */

import { useMemo, useRef } from 'react';
import dagre from '@dagrejs/dagre';

import { call } from '../lib/commands';
import type { GraphEdge, GraphNode, NodeKind } from '../lib/ipc';
import type { GraphView } from '../store/graphSlice';

export interface XY {
  x: number;
  y: number;
}

export interface PinnedPosition extends XY {
  pinned: boolean;
}

/** `nodeId → position`, exactly the shape `graph_layout_*` round-trips. */
export type ManualPositions = Record<string, PinnedPosition>;

// ---------------------------------------------------------------------------
// Node geometry
// ---------------------------------------------------------------------------

/**
 * Fixed per kind so dagre reserves the right space before React measures
 * anything. The mockup's 168px box is the floor; the kinds that carry three
 * lines of Korean (a worktree's diff stat, a session's status + clock + RSS)
 * are 184px, which is what stops the sub line from truncating mid-number.
 */
export const NODE_SIZE: Record<NodeKind, { width: number; height: number }> = {
  project: { width: 176, height: 44 },
  worktree: { width: 184, height: 58 },
  session: { width: 184, height: 66 },
  dispatch: { width: 184, height: 44 },
  run: { width: 184, height: 60 },
  account: { width: 168, height: 44 },
  orphan: { width: 184, height: 58 },
};

export function nodeSize(kind: NodeKind): { width: number; height: number } {
  return NODE_SIZE[kind] ?? NODE_SIZE.session;
}

/** DESIGN §9-6: left to right, 24px between nodes, 90px between ranks. */
export const DAGRE_OPTIONS = { rankdir: 'LR', nodesep: 24, ranksep: 90, marginx: 20, marginy: 20 };

// ---------------------------------------------------------------------------
// The layout itself
// ---------------------------------------------------------------------------

/**
 * Run dagre over the node/edge sets and apply the manual overrides.
 *
 * dagre reports node *centres*; React Flow positions by the top-left corner, so
 * every coordinate is shifted by half the node box here and nowhere else.
 */
export function computeLayout(
  nodes: readonly GraphNode[],
  edges: readonly GraphEdge[],
  manual: ManualPositions = {},
): Map<string, XY> {
  const positions = new Map<string, XY>();
  if (nodes.length === 0) return positions;

  const g = new dagre.graphlib.Graph();
  g.setGraph({ ...DAGRE_OPTIONS });
  g.setDefaultEdgeLabel(() => ({}));

  const known = new Set<string>();
  for (const node of nodes) {
    known.add(node.id);
    // A *fresh* label object per node: dagre writes the computed `x`/`y` into
    // the object it was handed, so sharing one would give every node of a kind
    // the same position (they would stack exactly on top of each other).
    g.setNode(node.id, { ...nodeSize(node.kind) });
  }
  for (const edge of edges) {
    // An edge to a node that is not drawn would make dagre invent a box for it.
    if (!known.has(edge.source) || !known.has(edge.target)) continue;
    g.setEdge(edge.source, edge.target);
  }

  dagre.layout(g);

  for (const node of nodes) {
    const laid = g.node(node.id) as { x?: number; y?: number } | undefined;
    const size = nodeSize(node.kind);
    const pin = manual[node.id];
    if (pin && pin.pinned && Number.isFinite(pin.x) && Number.isFinite(pin.y)) {
      positions.set(node.id, { x: pin.x, y: pin.y });
      continue;
    }
    positions.set(node.id, {
      x: (laid?.x ?? 0) - size.width / 2,
      y: (laid?.y ?? 0) - size.height / 2,
    });
  }
  return positions;
}

/** A stable key for a set of manual positions — cheap to compare each render. */
export function manualKey(manual: ManualPositions): string {
  const ids = Object.keys(manual).filter((id) => manual[id]?.pinned);
  ids.sort();
  return ids.map((id) => `${id}@${manual[id].x.toFixed(1)},${manual[id].y.toFixed(1)}`).join('|');
}

/**
 * Memoizes one layout per (topology, manual positions) pair.
 *
 * `runs` is public on purpose: the unit test asserts it stays at 1 across a
 * hundred metrics-only patches, which is the acceptance criterion for "dagre
 * only runs when the topology changes".
 */
export class LayoutCache {
  runs = 0;
  private hash = '';
  private manual = '';
  private cached = new Map<string, XY>();

  positions(graph: GraphView, manual: ManualPositions = {}): Map<string, XY> {
    const key = manualKey(manual);
    if (this.runs > 0 && this.hash === graph.topologyHash && this.manual === key) {
      return this.cached;
    }
    this.hash = graph.topologyHash;
    this.manual = key;
    this.cached = computeLayout([...graph.nodes.values()], [...graph.edges.values()], manual);
    this.runs += 1;
    return this.cached;
  }
}

/**
 * The hook form. `topologyHash` and the manual key are the only dependencies:
 * `graph` is read through a ref so that the thousands of metrics patches that
 * change `graph.nodes` between two topologies never re-enter dagre.
 */
export function useGraphLayout(graph: GraphView, manual: ManualPositions): Map<string, XY> {
  const cache = useRef<LayoutCache | null>(null);
  cache.current ??= new LayoutCache();
  const latest = useRef(graph);
  latest.current = graph;
  const key = manualKey(manual);
  return useMemo(
    () => cache.current!.positions(latest.current, manual),
    // eslint-disable-next-line react-hooks/exhaustive-deps
    [graph.topologyHash, key],
  );
}

// ---------------------------------------------------------------------------
// Persistence
// ---------------------------------------------------------------------------

/** `null` is the "everything" canvas — the graph's default scope. */
export type LayoutScope = string | null;

interface LayoutStore {
  positions: ManualPositions;
  loaded: boolean;
}

const stores = new Map<string, LayoutStore>();
const listeners = new Set<() => void>();
let saveTimer: ReturnType<typeof setTimeout> | null = null;
let savePending: { scope: LayoutScope; positions: ManualPositions } | null = null;

function scopeKey(scope: LayoutScope): string {
  return scope ?? 'all';
}

function store(scope: LayoutScope): LayoutStore {
  const key = scopeKey(scope);
  let found = stores.get(key);
  if (!found) {
    found = { positions: {}, loaded: false };
    stores.set(key, found);
  }
  return found;
}

function notify(): void {
  for (const listener of listeners) listener();
}

export function subscribeLayout(listener: () => void): () => void {
  listeners.add(listener);
  return () => listeners.delete(listener);
}

export function manualPositions(scope: LayoutScope): ManualPositions {
  return store(scope).positions;
}

/** Fetch the persisted positions once per scope; failures leave it empty. */
export async function loadManualPositions(scope: LayoutScope): Promise<void> {
  const slot = store(scope);
  if (slot.loaded) return;
  slot.loaded = true;
  try {
    const layout = await call('graph_layout_get', { projectId: scope });
    const positions: ManualPositions = {};
    for (const [id, p] of Object.entries(layout.positions ?? {})) {
      if (!Number.isFinite(p.x) || !Number.isFinite(p.y)) continue;
      positions[id] = { x: p.x, y: p.y, pinned: p.pinned !== false };
    }
    slot.positions = positions;
    notify();
  } catch {
    // The command lands with this task; an older backend simply has no saved
    // layout, which is indistinguishable from "nothing pinned yet".
  }
}

function schedulePersist(scope: LayoutScope, delayMs: number): void {
  savePending = { scope, positions: store(scope).positions };
  if (saveTimer) clearTimeout(saveTimer);
  saveTimer = setTimeout(() => {
    saveTimer = null;
    const payload = savePending;
    savePending = null;
    if (!payload) return;
    void call('graph_layout_save', {
      projectId: payload.scope,
      layout: { positions: payload.positions, updatedAt: Date.now() },
    }).catch(() => undefined);
  }, delayMs);
}

/** Drop a pending debounced write (teardown, and the unit tests). */
export function cancelPersistLayout(): void {
  if (saveTimer) clearTimeout(saveTimer);
  saveTimer = null;
  savePending = null;
}

/** Pin one node where the user dropped it. */
export function pinNode(scope: LayoutScope, nodeId: string, at: XY, delayMs = 500): void {
  const slot = store(scope);
  slot.positions = { ...slot.positions, [nodeId]: { x: at.x, y: at.y, pinned: true } };
  notify();
  schedulePersist(scope, delayMs);
}

export function unpinNode(scope: LayoutScope, nodeId: string, delayMs = 500): void {
  const slot = store(scope);
  if (!slot.positions[nodeId]) return;
  const next = { ...slot.positions };
  delete next[nodeId];
  slot.positions = next;
  notify();
  schedulePersist(scope, delayMs);
}

export function clearPins(scope: LayoutScope, delayMs = 500): void {
  const slot = store(scope);
  if (Object.keys(slot.positions).length === 0) return;
  slot.positions = {};
  notify();
  schedulePersist(scope, delayMs);
}

/** Test seam: forget every cached scope. */
export function resetLayoutStore(): void {
  cancelPersistLayout();
  stores.clear();
}

// ---------------------------------------------------------------------------
// Session memory budget
// ---------------------------------------------------------------------------

const MIB = 1024 * 1024;
let softRssBytes = 2048 * MIB;

/**
 * The per-session soft ceiling from `config.supervision.softRssMiB`, which is
 * what turns the RSS bar amber. Held here rather than in the store because it
 * is a constant of the machine, not a piece of live state.
 */
export function getSoftRssBytes(): number {
  return softRssBytes;
}

export function setSoftRssMiB(mib: number): void {
  if (Number.isFinite(mib) && mib > 0) softRssBytes = mib * MIB;
}

// ---------------------------------------------------------------------------
// Account → model
// ---------------------------------------------------------------------------

/**
 * The model a session runs under, keyed by account id.
 *
 * The interactive lane has no model of its own: `session_open` launches the
 * vendor CLI with the prompt as argv and no model flag, so `PtySessionInfo`
 * carries none and the graph projection has none to hand out. What does exist
 * is the account slot's `modelHint` (`AccountSlot.modelHint`, DESIGN §10-5) —
 * the model that account is pinned to — and that is what the session node
 * shows when it is set. A session on the ambient slot shows no model, which is
 * the truth: nobody pinned one. The headless lane is unaffected; a run node
 * keeps showing `RunResult.model`, the model that actually answered.
 *
 * Kept here, next to the soft-RSS ceiling, for the same reason: it is slow
 * configuration, not live state, so it must not push a store update (and a
 * re-layout) through the canvas on every account list refresh.
 *
 * It does, however, have to reach the canvas **once**. `accounts_list` is a
 * command: it resolves after the first render, and the node arrays are memoised
 * on the topology, so the session nodes built before it answered kept showing
 * no model until something unrelated changed the graph. The revision counter
 * below is the smallest fix for that — a store the canvas can subscribe to,
 * which ticks only when the map's *contents* changed, so an unchanged refresh
 * still costs no render.
 */
let accountModels: ReadonlyMap<string, string> = new Map();
let accountModelsRev = 0;
const accountModelListeners = new Set<() => void>();

function sameModels(a: ReadonlyMap<string, string>, b: ReadonlyMap<string, string>): boolean {
  if (a.size !== b.size) return false;
  for (const [id, model] of a) if (b.get(id) !== model) return false;
  return true;
}

export function setAccountModels(entries: Iterable<readonly [string, string]>): void {
  const next = new Map<string, string>();
  for (const [id, model] of entries) {
    const m = model.trim();
    if (id && m) next.set(id, m);
  }
  if (sameModels(accountModels, next)) return;
  accountModels = next;
  accountModelsRev += 1;
  for (const listener of accountModelListeners) listener();
}

/** `useSyncExternalStore` subscribe for the account → model map. */
export function subscribeAccountModels(listener: () => void): () => void {
  accountModelListeners.add(listener);
  return () => {
    accountModelListeners.delete(listener);
  };
}

/**
 * The snapshot for [`subscribeAccountModels`] — a number, not the map, so
 * `useSyncExternalStore` compares it by identity without allocating.
 */
export function accountModelsRevision(): number {
  return accountModelsRev;
}

export function getAccountModel(accountId: string | null | undefined): string | undefined {
  return accountId ? accountModels.get(accountId) : undefined;
}
