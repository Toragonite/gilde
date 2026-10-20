/**
 * Dispatch task node (T25).
 *
 * One task fans out into several runs, so the useful number on the node is how
 * many of them have finished — the same `2/3 완료` the dispatch list shows.
 */

import type { NodeProps } from '@xyflow/react';

import { useI18n } from '../../i18n/useI18n';
import { formatElapsedClock } from '../../lib/format';
import { useAppStore } from '../../store/useAppStore';
import type { GraphView } from '../../store/graphSlice';
import { NodeShell, NodeSub, NodeTitle, StatusLine, useNode, useNodeMetrics } from './NodeShell';

/** Runs of a task, and how many of them are over. Counted off the edges the
 *  projection wrote, so a run that moved worktree is still counted. */
export function runTally(graph: GraphView, taskId: string): { total: number; finished: number } {
  let total = 0;
  let finished = 0;
  for (const edge of graph.edges.values()) {
    if (edge.kind !== 'dispatched-to' || edge.source !== taskId) continue;
    const run = graph.nodes.get(edge.target);
    if (!run) continue;
    total += 1;
    if (run.status === 'done' || run.status === 'error' || run.status === 'exited') finished += 1;
  }
  return { total, finished };
}

export function DispatchNode({ id }: NodeProps) {
  const { t } = useI18n();
  const node = useNode(id);
  const metrics = useNodeMetrics(id);
  const total = useAppStore((s) => runTally(s.graph, id).total);
  const finished = useAppStore((s) => runTally(s.graph, id).finished);
  if (!node) return null;

  return (
    <NodeShell id={id} kind="dispatch" title={node.label}>
      <NodeTitle status={node.status} label={node.label} />
      <NodeSub>
        <StatusLine status={node.status} />
        {total > 0 ? <> · {t('graph.runsFinished', { done: finished, total })}</> : null}
        {metrics?.elapsedMs !== undefined ? <> · {formatElapsedClock(metrics.elapsedMs)}</> : null}
      </NodeSub>
    </NodeShell>
  );
}
