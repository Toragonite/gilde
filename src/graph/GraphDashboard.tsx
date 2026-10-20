/**
 * The live status graph (T25, DESIGN §9-5 / §9-6).
 *
 * It is a **pane**, not a modal: the same tile a terminal or a result card
 * lives in. That is what makes the graph part of the workspace instead of a
 * detour, and it is why double-clicking a node reveals the panel behind it —
 * the graph is a navigation surface, not a poster.
 *
 * Performance contract, all of it visible in this file:
 *
 *  - the node and edge arrays handed to React Flow are memoized on the
 *    *topology* and the filter result, never on metrics, so a 1 Hz RSS patch
 *    does not rebuild them;
 *  - positions come from `useGraphLayout`, which runs dagre only when
 *    `topologyHash` moves;
 *  - each node component subscribes to its own metrics, so that patch repaints
 *    exactly one node.
 *
 * Nothing here fetches: the graph store is fed by `gilde://graph/patch` through
 * `useEventBridge`, and the only commands called are `graph_focus` (navigation)
 * and `graph_layout_*` (manual positions).
 */

import { useCallback, useEffect, useMemo, useRef, useState, useSyncExternalStore } from 'react';
import { ReactFlow, type Edge, type Node, type NodeMouseHandler, type OnNodeDrag } from '@xyflow/react';
import '@xyflow/react/dist/style.css';

import { EmptyState } from '../components/EmptyState';
import { RefreshIcon } from '../components/icons';
import { useI18n } from '../i18n/useI18n';
import { cmd, isUnavailable } from '../lib/commands';
import { formatDuration } from '../lib/format';
import type { FocusResult, GraphNode } from '../lib/ipc';
import { parseNodeId } from '../lib/ipc';
import type { PaneViewProps } from '../panes/Pane';
import type { GraphView } from '../store/graphSlice';
import { useAppStore } from '../store/useAppStore';
import { DEFAULT_FILTERS, GraphFilters, filterGraph, projectOptions } from './GraphFilters';
import type { GraphFilterState } from './GraphFilters';
import { GraphLegend } from './GraphLegend';
import { TilesView } from './TilesView';
import { AccountNode } from './nodes/AccountNode';
import { DispatchNode } from './nodes/DispatchNode';
import { ProjectNode } from './nodes/ProjectNode';
import { RunNode } from './nodes/RunNode';
import { OrphanNode, SessionNode } from './nodes/SessionNode';
import { WorktreeNode } from './nodes/WorktreeNode';
import { StatusEdge } from './edges/StatusEdge';
import {
  accountModelsRevision,
  clearPins,
  loadManualPositions,
  manualPositions,
  pinNode,
  setAccountModels,
  setSoftRssMiB,
  subscribeAccountModels,
  subscribeLayout,
  useGraphLayout,
} from './useGraphLayout';
import type { ManualPositions, XY } from './useGraphLayout';
import './graph.css';

/** A patch gap this long turns the freshness chip amber (DESIGN §9-6). */
export const STALE_AFTER_MS = 10_000;

const NODE_TYPES = {
  project: ProjectNode,
  worktree: WorktreeNode,
  session: SessionNode,
  dispatch: DispatchNode,
  run: RunNode,
  account: AccountNode,
  orphan: OrphanNode,
};

const EDGE_TYPES = { status: StatusEdge };

const PRO_OPTIONS = { hideAttribution: true };

// ---------------------------------------------------------------------------
// Pure builders (unit tested)
// ---------------------------------------------------------------------------

export type FlowNode = Node<Record<string, never>>;

/**
 * React Flow nodes for one topology. Deliberately carries no live data: each
 * node component reads what it needs from the store itself, which is what keeps
 * this array stable while metrics stream.
 */
export function buildFlowNodes(
  graph: GraphView,
  positions: ReadonlyMap<string, XY>,
  visible: ReadonlySet<string>,
  matched: ReadonlySet<string>,
  dimUnmatched: boolean,
): FlowNode[] {
  const out: FlowNode[] = [];
  for (const node of graph.nodes.values()) {
    out.push({
      id: node.id,
      type: node.kind,
      position: positions.get(node.id) ?? { x: 0, y: 0 },
      hidden: !visible.has(node.id),
      className: dimUnmatched && !matched.has(node.id) ? 'gilde-dim' : undefined,
      data: {} as Record<string, never>,
      draggable: true,
      selectable: true,
      connectable: false,
    });
  }
  out.sort((a, b) => a.id.localeCompare(b.id));
  return out;
}

export function buildFlowEdges(graph: GraphView, visible: ReadonlySet<string>): Edge[] {
  const out: Edge[] = [];
  for (const edge of graph.edges.values()) {
    if (!graph.nodes.has(edge.source) || !graph.nodes.has(edge.target)) continue;
    out.push({
      id: edge.id,
      source: edge.source,
      target: edge.target,
      type: 'status',
      hidden: !visible.has(edge.source) || !visible.has(edge.target),
    });
  }
  out.sort((a, b) => a.id.localeCompare(b.id));
  return out;
}

/**
 * The front end's own copy of `graph_focus`, used when the command is not
 * registered yet. Same walk, same bound on the parent chain.
 */
export function localFocus(graph: GraphView, nodeId: string): FocusResult | null {
  const parsed = parseNodeId(nodeId);
  const node = graph.nodes.get(nodeId);
  if (!parsed || !node) return null;
  const result: FocusResult = { kind: parsed.kind };
  if (parsed.kind === 'session') result.sessionId = parsed.entityId;
  if (parsed.kind === 'worktree') result.worktreeId = parsed.entityId;
  if (parsed.kind === 'dispatch') result.taskId = parsed.entityId;
  if (!result.worktreeId && node.meta.worktreeId) result.worktreeId = node.meta.worktreeId;

  let cursor = node.parentId;
  for (let hops = 0; cursor && hops < 16; hops++) {
    const parent = parseNodeId(cursor);
    if (!parent) break;
    if (parent.kind === 'worktree' && !result.worktreeId) result.worktreeId = parent.entityId;
    if (parent.kind === 'dispatch' && !result.taskId) result.taskId = parent.entityId;
    if (parent.kind === 'session' && !result.sessionId) result.sessionId = parent.entityId;
    cursor = graph.nodes.get(cursor)?.parentId ?? null;
  }
  return result;
}

/** The most recently touched session or run under a worktree node. */
export function newestWorkUnder(graph: GraphView, worktreeNodeId: string): GraphNode | null {
  let newest: GraphNode | null = null;
  for (const node of graph.nodes.values()) {
    if (node.parentId !== worktreeNodeId) continue;
    if (node.kind !== 'session' && node.kind !== 'run') continue;
    if (!newest || node.updatedAt > newest.updatedAt) newest = node;
  }
  return newest;
}

// ---------------------------------------------------------------------------
// The pane
// ---------------------------------------------------------------------------

export function GraphDashboard(_props: PaneViewProps) {
  const { t, locale } = useI18n();
  const graph = useAppStore((s) => s.graph);
  const selectedNodeId = useAppStore((s) => s.ui.selectedNodeId);
  const selectNode = useAppStore((s) => s.selectNode);
  const openTab = useAppStore((s) => s.openTab);
  const focusTab = useAppStore((s) => s.focusTab);
  const setActiveView = useAppStore((s) => s.setActiveView);
  const setDrawerTab = useAppStore((s) => s.setDrawerTab);

  const [mode, setMode] = useState<'graph' | 'tiles'>('graph');
  const [filters, setFilters] = useState<GraphFilterState>(DEFAULT_FILTERS);
  const [now, setNow] = useState(() => Date.now());

  // The freshness chip has to tick on its own: a backend that stopped sending
  // patches must look stopped, not calm (DESIGN §9-6).
  useEffect(() => {
    const timer = setInterval(() => setNow(Date.now()), 1_000);
    return () => clearInterval(timer);
  }, []);

  const scope = filters.projectId ? (parseNodeId(filters.projectId)?.entityId ?? null) : null;
  useEffect(() => {
    void loadManualPositions(scope);
  }, [scope]);

  // The soft ceiling that turns a session's RSS bar amber lives in settings.
  useEffect(() => {
    let cancelled = false;
    void cmd
      .appSettingsGet()
      .then((config) => {
        if (!cancelled) setSoftRssMiB(config.supervision.softRssMiB);
      })
      .catch((e) => {
        if (!isUnavailable(e)) console.warn('[gilde] app_settings_get failed', e);
      });
    return () => {
      cancelled = true;
    };
  }, []);

  // The model a session node shows comes from its account slot's `modelHint`:
  // the interactive lane launches the CLI without a model flag, so this is the
  // only model anyone pinned for it (see `setAccountModels`). Read once, like
  // the soft ceiling — it is configuration, not live state — but *subscribed*
  // to, not read one-shot: the command resolves after the first render, and
  // without this the session nodes kept the model-less shape they were built
  // with until the topology happened to change.
  useEffect(() => {
    let cancelled = false;
    void cmd
      .accountsList()
      .then((accounts) => {
        if (cancelled) return;
        setAccountModels(
          accounts.flatMap((a) => (a.modelHint ? [[a.id, a.modelHint] as const] : [])),
        );
      })
      .catch((e) => {
        if (!isUnavailable(e)) console.warn('[gilde] accounts_list failed', e);
      });
    return () => {
      cancelled = true;
    };
  }, []);

  const manual = useSyncExternalStore<ManualPositions>(
    subscribeLayout,
    () => manualPositions(scope),
    () => manualPositions(scope),
  );
  const accountModelsRev = useSyncExternalStore(
    subscribeAccountModels,
    accountModelsRevision,
    accountModelsRevision,
  );

  const positions = useGraphLayout(graph, manual);
  const { visible, matched } = useMemo(() => filterGraph(graph, filters), [graph, filters]);
  const projects = useMemo(() => projectOptions(graph), [graph]);

  // Rebuild the React Flow arrays only when what is *on* the canvas changes —
  // never when a number inside a node does.
  const visibilityKey = useMemo(
    () => `${[...visible].sort().join(',')}#${[...matched].sort().join(',')}`,
    [visible, matched],
  );
  const latest = useRef(graph);
  latest.current = graph;
  const dimUnmatched = filters.query.trim().length > 0;
  const flowNodes = useMemo(
    () => buildFlowNodes(latest.current, positions, visible, matched, dimUnmatched),
    // `accountModelsRev` is a dependency for the same reason the others are:
    // it changes what a node renders, and nothing else would rebuild the array.
    // eslint-disable-next-line react-hooks/exhaustive-deps
    [positions, visibilityKey, dimUnmatched, accountModelsRev],
  );
  const flowEdges = useMemo(
    () => buildFlowEdges(latest.current, visible),
    // eslint-disable-next-line react-hooks/exhaustive-deps
    [graph.topologyHash, visibilityKey],
  );

  const visibleNodes = useMemo(() => {
    const out: GraphNode[] = [];
    for (const id of visible) {
      const node = graph.nodes.get(id);
      if (node) out.push(node);
    }
    return out;
  }, [graph, visible]);

  /** Reveal whatever a node stands for. Double-click, and the tiles view. */
  const openNode = useCallback(
    async (nodeId: string) => {
      const current = latest.current;
      let focus: FocusResult | null = null;
      try {
        focus = await cmd.graphFocus(nodeId);
      } catch (e) {
        if (!isUnavailable(e)) console.warn('[gilde] graph_focus failed', e);
        focus = localFocus(current, nodeId);
      }
      if (!focus) return;

      const store = useAppStore.getState();
      const tabs = store.ui.tabs;

      const revealSession = (sessionId: string) => {
        const existing = tabs.find(
          (tab) => tab.descriptor.view === 'terminal' && tab.descriptor.sessionId === sessionId,
        );
        if (existing) {
          focusTab(existing.id);
        } else {
          const node = current.nodes.get(`session:${sessionId}`);
          openTab({
            title: node?.meta.vendor ?? node?.label ?? sessionId,
            sublabel: node?.sublabel,
            descriptor: { view: 'terminal', sessionId },
            status: node?.status,
            closable: true,
          });
        }
        setActiveView('sessions');
      };

      if (focus.kind === 'orphan') {
        setDrawerTab('monitor');
        return;
      }
      if (focus.sessionId) {
        revealSession(focus.sessionId);
        return;
      }
      if (focus.taskId) {
        const taskId = focus.taskId;
        const existing = tabs.find(
          (tab) => tab.descriptor.view === 'dispatch' && tab.descriptor.taskId === taskId,
        );
        if (existing) focusTab(existing.id);
        else {
          const node = current.nodes.get(`dispatch:${taskId}`);
          openTab({
            title: node?.label ?? taskId,
            sublabel: node?.sublabel,
            descriptor: { view: 'dispatch', taskId },
            status: node?.status,
            closable: true,
          });
        }
        setActiveView('dispatch');
        return;
      }
      if (focus.worktreeId) {
        const newest = newestWorkUnder(current, `worktree:${focus.worktreeId}`);
        if (newest && newest.kind === 'session') {
          revealSession(newest.id.slice('session:'.length));
          return;
        }
        setActiveView('sessions');
      }
    },
    [focusTab, openTab, setActiveView, setDrawerTab],
  );

  const onNodeClick = useCallback<NodeMouseHandler>(
    (_event, node) => selectNode(node.id),
    [selectNode],
  );
  const onNodeDoubleClick = useCallback<NodeMouseHandler>(
    (_event, node) => {
      void openNode(node.id);
    },
    [openNode],
  );
  const onNodeDragStop = useCallback<OnNodeDrag<FlowNode>>(
    (_event, node) => {
      pinNode(scope, node.id, node.position);
    },
    [scope],
  );

  const age = graph.lastPatchAt > 0 ? Math.max(0, now - graph.lastPatchAt) : 0;
  const stale = graph.lastPatchAt > 0 && age >= STALE_AFTER_MS;
  const pinned = Object.keys(manual).length;

  return (
    <div className="gilde-graph">
      <div className="hdr gilde-graph-hdr">
        <span className="strong s12">{t('graph.title')}</span>
        <GraphFilters
          filters={filters}
          projects={projects}
          matches={matched.size}
          onChange={setFilters}
        />
        <div className="grow" />
        <span className="s11 muted mono" data-tone={stale ? 'warn' : undefined}>
          {graph.lastPatchAt === 0
            ? t('common.loading')
            : stale
              ? t('graph.stale', { duration: formatDuration(age, locale) })
              : t('graph.updatedAgo', { duration: formatDuration(age, locale) })}
        </span>
        {pinned > 0 ? (
          <button
            type="button"
            className="btn icon ghost"
            title={t('graph.resetLayout')}
            aria-label={t('graph.resetLayout')}
            onClick={() => clearPins(scope)}
          >
            <RefreshIcon size="sm" />
          </button>
        ) : null}
        <div className="seg" role="group" aria-label={t('graph.title')}>
          <button type="button" aria-pressed={mode === 'graph'} onClick={() => setMode('graph')}>
            {t('graph.view.graph')}
          </button>
          <button type="button" aria-pressed={mode === 'tiles'} onClick={() => setMode('tiles')}>
            {t('graph.view.tiles')}
          </button>
        </div>
      </div>

      {graph.nodes.size === 0 ? (
        <EmptyState message={t('graph.empty')} />
      ) : mode === 'tiles' ? (
        <TilesView
          nodes={visibleNodes}
          matched={matched}
          dimUnmatched={dimUnmatched}
          selectedId={selectedNodeId}
          onSelect={selectNode}
          onOpen={(id) => void openNode(id)}
        />
      ) : (
        <div className="gilde-canvas">
          <ReactFlow
            nodes={flowNodes}
            edges={flowEdges}
            nodeTypes={NODE_TYPES}
            edgeTypes={EDGE_TYPES}
            onNodeClick={onNodeClick}
            onNodeDoubleClick={onNodeDoubleClick}
            onNodeDragStop={onNodeDragStop}
            proOptions={PRO_OPTIONS}
            nodesConnectable={false}
            elementsSelectable={false}
            zoomOnDoubleClick={false}
            minZoom={0.2}
            maxZoom={2}
            fitView
            fitViewOptions={{ padding: 0.2, maxZoom: 1 }}
          />
          <GraphLegend />
        </div>
      )}
    </div>
  );
}
