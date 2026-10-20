/**
 * Headless run node (T25, DESIGN §9-6).
 *
 * Same frame as a session — both lanes hang under the same worktree — plus the
 * token and cost chips, which only the headless lane can report.
 *
 * The model shown is the one that actually ran (`RunResult.model`), because the
 * vendor can silently answer with a different one than was requested.
 */

import type { NodeProps } from '@xyflow/react';

import { useI18n } from '../../i18n/useI18n';
import { formatCostUsd, formatElapsedClock, formatNumber } from '../../lib/format';
import { NodeShell, NodeSub, NodeTitle, StatusLine, useNode, useNodeMetrics } from './NodeShell';

export function RunNode({ id }: NodeProps) {
  const { t, locale } = useI18n();
  const node = useNode(id);
  const metrics = useNodeMetrics(id);
  if (!node) return null;

  const input = metrics?.inputTokens;
  const output = metrics?.outputTokens;
  const cost = metrics?.costUsd;

  return (
    <NodeShell id={id} kind="run" title={node.sublabel ?? node.label}>
      <NodeTitle
        status={node.status}
        badge={node.meta.vendor ?? undefined}
        label={node.sublabel ?? node.label}
      />
      <NodeSub>
        <StatusLine status={node.status} />
        {metrics?.elapsedMs !== undefined ? <> · {formatElapsedClock(metrics.elapsedMs)}</> : null}
        {cost !== undefined ? <> · {formatCostUsd(cost, locale)}</> : null}
      </NodeSub>
      {input !== undefined || output !== undefined ? (
        <div className="gilde-node-chips">
          {input !== undefined ? (
            <span className="chip mono s11">
              {t('graph.tokens.in')} {formatNumber(input, locale)}
            </span>
          ) : null}
          {output !== undefined ? (
            <span className="chip mono s11">
              {t('graph.tokens.out')} {formatNumber(output, locale)}
            </span>
          ) : null}
        </div>
      ) : null}
    </NodeShell>
  );
}
