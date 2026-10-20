/**
 * Account node (T25, DESIGN §9-5).
 *
 * The account rank is projected only once there are at least two slots, so this
 * component normally never mounts in M1 — the slot costs nothing until multi
 * account lands (DECISIONS: M2). What it shows is the honest minimum: the
 * label, the vendor and whether that config directory is logged in.
 */

import type { NodeProps } from '@xyflow/react';

import { useI18n } from '../../i18n/useI18n';
import { formatPercentPoints } from '../../lib/format';
import { NodeShell, NodeSub, NodeTitle, StatusLine, useNode, useNodeMetrics } from './NodeShell';

export function AccountNode({ id }: NodeProps) {
  const { t, locale } = useI18n();
  const node = useNode(id);
  const metrics = useNodeMetrics(id);
  if (!node) return null;

  const quota = metrics?.quotaPercent;
  return (
    <NodeShell id={id} kind="account" title={node.label}>
      <NodeTitle
        status={node.status}
        badge={node.meta.vendor ?? undefined}
        label={node.label}
      >
        {node.badges.includes('detached') ? (
          <span className="badge">{t('graph.disabled')}</span>
        ) : null}
      </NodeTitle>
      <NodeSub>
        <StatusLine status={node.status} />
        {quota !== undefined ? <> · {formatPercentPoints(quota, locale)}</> : null}
      </NodeSub>
    </NodeShell>
  );
}
