/**
 * The one edge type on the canvas (T25, UI-GUIDELINES §2-6).
 *
 * 1px orthogonal line, no arrowheads, no gradient. Colour is only ever
 * `--ok` / `--warn` / `--error` and comes from the *target* node's status, so a
 * line answers "how is the thing at the end of it doing". Only `active: true`
 * edges animate — the dashes are reserved for output actually flowing right
 * now, and `prefers-reduced-motion` stops them.
 */

import type { EdgeProps } from '@xyflow/react';
import { getSmoothStepPath } from '@xyflow/react';

import type { NodeStatus } from '../../lib/ipc';
import { useAppStore } from '../../store/useAppStore';

export type EdgeTone = 'ok' | 'warn' | 'error' | undefined;

/**
 * Only three statuses colour a line, and `clean` is not one of them: a canvas
 * where every edge is green says nothing. Neutral is the default, and colour
 * means "look here".
 */
export function edgeTone(status: NodeStatus | undefined): EdgeTone {
  switch (status) {
    case 'error':
    case 'orphan':
      return 'error';
    case 'waiting':
    case 'stalled':
      return 'warn';
    case 'done':
      return 'ok';
    default:
      return undefined;
  }
}

const SEVERITY: Record<Exclude<EdgeTone, undefined>, number> = { ok: 1, warn: 2, error: 3 };

/** The more urgent of the two ends. An orphan's line is red from either side. */
export function worseTone(a: EdgeTone, b: EdgeTone): EdgeTone {
  if (!a) return b;
  if (!b) return a;
  return SEVERITY[a] >= SEVERITY[b] ? a : b;
}

export function StatusEdge({
  id,
  sourceX,
  sourceY,
  targetX,
  targetY,
  sourcePosition,
  targetPosition,
}: EdgeProps) {
  const active = useAppStore((s) => s.graph.edges.get(id)?.active ?? false);
  const kind = useAppStore((s) => s.graph.edges.get(id)?.kind);
  const tone = useAppStore((s) => {
    const edge = s.graph.edges.get(id);
    if (!edge) return undefined;
    return worseTone(
      edgeTone(s.graph.nodes.get(edge.source)?.status),
      edgeTone(s.graph.nodes.get(edge.target)?.status),
    );
  });

  const [path] = getSmoothStepPath({
    sourceX,
    sourceY,
    targetX,
    targetY,
    sourcePosition,
    targetPosition,
    borderRadius: 4,
  });

  return (
    <path
      id={id}
      className="react-flow__edge-path gilde-edge"
      d={path}
      data-tone={tone}
      data-active={active ? 'true' : undefined}
      data-kind={kind}
      fill="none"
    />
  );
}
