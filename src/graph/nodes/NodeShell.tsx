/**
 * The frame every graph node shares (T25).
 *
 * Structure comes from `docs/ui-mockups/Graph.dc.html`: a flat 1px-bordered box
 * on `--n2`, a title line at `--n12` and a mono sub line at `--n8`. No shadow,
 * no gradient, no rounded avatar (UI-GUIDELINES §2-6).
 *
 * Two rules live here rather than in each node:
 *
 *  1. **Subscription granularity.** `useNodeMetrics` selects one node's metrics
 *     out of the store, so the 1 Hz RSS patch repaints that node and nothing
 *     else — the layout is not even consulted (DESIGN §9-6).
 *  2. **Status is never colour alone.** [`StatusLine`] always draws the dot
 *     *and* the localized status word, which is what makes the canvas readable
 *     to someone who cannot separate amber from green.
 */

import type { ReactNode } from 'react';
import { Handle, Position } from '@xyflow/react';

import { StatusDot, statusMessageKey, statusTone } from '../../components/StatusDot';
import { useI18n } from '../../i18n/useI18n';
import type { GraphNode, NodeKind, NodeMetrics, NodeStatus } from '../../lib/ipc';
import type { GraphView } from '../../store/graphSlice';
import { useAppStore } from '../../store/useAppStore';

// ---------------------------------------------------------------------------
// Selectors
// ---------------------------------------------------------------------------

/** The node itself. `undefined` while a patch removes it mid-render. */
export function useNode(id: string): GraphNode | undefined {
  return useAppStore((s) => s.graph.nodes.get(id));
}

/**
 * Metrics only. A metrics-only patch replaces just this node's object, so this
 * selector is what keeps a per-second RSS sample from touching the rest of the
 * canvas.
 */
export function useNodeMetrics(id: string): NodeMetrics | undefined {
  return useAppStore((s) => s.graph.nodes.get(id)?.metrics);
}

export function countChildren(graph: GraphView, parentId: string, kind: NodeKind): number {
  let n = 0;
  for (const node of graph.nodes.values()) {
    if (node.parentId === parentId && node.kind === kind) n += 1;
  }
  return n;
}

/**
 * Nodes of one kind under a parent. `deep` also counts what hangs off the
 * parent's worktrees, which is how a project reports the sessions inside it.
 *
 * Sessions and runs are counted separately on purpose: calling two headless
 * runs "sessions" would be a small lie in the one place that exists to be
 * trusted about what is running.
 */
export function countWork(
  graph: GraphView,
  parentId: string,
  kind: 'session' | 'run',
  deep: boolean,
): number {
  const parents = new Set<string>([parentId]);
  if (deep) {
    for (const node of graph.nodes.values()) {
      if (node.parentId === parentId && node.kind === 'worktree') parents.add(node.id);
    }
  }
  let n = 0;
  for (const node of graph.nodes.values()) {
    if (node.kind !== kind) continue;
    if (node.parentId && parents.has(node.parentId)) n += 1;
  }
  return n;
}

export function useChildCount(parentId: string, kind: NodeKind): number {
  return useAppStore((s) => countChildren(s.graph, parentId, kind));
}

export function useWorkCount(parentId: string, kind: 'session' | 'run', deep: boolean): number {
  return useAppStore((s) => countWork(s.graph, parentId, kind, deep));
}

// ---------------------------------------------------------------------------
// Frame
// ---------------------------------------------------------------------------

export interface NodeShellProps {
  id: string;
  kind: NodeKind;
  children: ReactNode;
  title?: string;
}

export function NodeShell({ id, kind, children, title }: NodeShellProps) {
  const selected = useAppStore((s) => s.ui.selectedNodeId === id);
  return (
    <div
      className="gilde-node"
      data-kind={kind}
      data-selected={selected ? 'true' : undefined}
      title={title}
    >
      <Handle type="target" position={Position.Left} isConnectable={false} className="gilde-handle" />
      {children}
      <Handle type="source" position={Position.Right} isConnectable={false} className="gilde-handle" />
    </div>
  );
}

/** Title line: the status dot, an optional vendor badge, then the name. */
export function NodeTitle({
  status,
  confidence,
  badge,
  label,
  children,
}: {
  status?: NodeStatus;
  confidence?: number;
  badge?: string;
  label: string;
  children?: ReactNode;
}) {
  return (
    <div className="gilde-node-title">
      {status ? <StatusDot status={status} confidence={confidence} /> : null}
      {badge ? <span className="badge">{badge}</span> : null}
      <span className="ell grow">{label}</span>
      {children}
    </div>
  );
}

/** Sub line: mono, tabular, `--n8` — the metrics that answer "how is it going". */
export function NodeSub({ children, tone }: { children: ReactNode; tone?: 'warn' | 'error' }) {
  return (
    <div className="gilde-node-sub mono" data-tone={tone}>
      {children}
    </div>
  );
}

/**
 * The status word itself. Always rendered next to the dot: colour alone is not
 * a status (UI-GUIDELINES §2-1a).
 */
export function StatusLine({ status }: { status: NodeStatus }) {
  const { t } = useI18n();
  const tone = statusTone(status);
  return (
    <span className="gilde-node-status" data-tone={tone === 'neutral' ? undefined : tone}>
      {t(statusMessageKey(status))}
    </span>
  );
}

/** `·` separator that never sits at the start or end of the line. */
export function Sep() {
  return <span className="gilde-sep">·</span>;
}
