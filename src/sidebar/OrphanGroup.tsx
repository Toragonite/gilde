/**
 * Orphan group — the last section of the sidebar (docs/ui-mockups/Main.dc.html).
 *
 * It is a count and a way in, not a second process monitor: clicking the header
 * opens the Monitor with the our-orphan filter applied. The filter itself is
 * owned by `procSlice` (T23), which is not in this wave, so the request is
 * published through `worktree.orphanFilterRequestedAt` and the panel picks it
 * up — the sidebar never reaches into a slice that may not exist.
 *
 * The rows come from `kind: 'orphan'` graph nodes. When the scanner has not
 * produced any yet but `totals.orphans` is non-zero, the header still shows the
 * honest number and no invented rows.
 */

import { Badge } from '../components/Badge';
import { StatusDot } from '../components/StatusDot';
import { useI18n } from '../i18n/useI18n';
import { formatBytes } from '../lib/format';
import type { GraphNode } from '../lib/ipc';

export interface OrphanGroupProps {
  nodes: readonly GraphNode[];
  /** `totals.orphans` — authoritative even before orphan nodes are published. */
  total: number;
  selectedNodeId: string | null;
  onOpenMonitor: () => void;
  onSelect: (nodeId: string) => void;
}

export function OrphanGroup({
  nodes,
  total,
  selectedNodeId,
  onOpenMonitor,
  onSelect,
}: OrphanGroupProps) {
  const { t, locale } = useI18n();
  const count = Math.max(total, nodes.length);
  if (count === 0) return null;

  return (
    <div className="col">
      <button
        type="button"
        className="sec sb-sec-btn"
        title={t('sidebar.orphans.open')}
        onClick={onOpenMonitor}
      >
        <span>{t('node.orphan')}</span>
        <Badge tone="error">{count}</Badge>
      </button>

      <div role="tree" aria-label={t('node.orphan')}>
        {nodes.map((node) => (
          <div
            key={node.id}
            className="tree-row"
            role="treeitem"
            aria-selected={node.id === selectedNodeId}
            aria-label={node.label}
            onClick={() => onSelect(node.id)}
            onDoubleClick={onOpenMonitor}
          >
            <StatusDot status="orphan" />
            <span className="ell grow s12">{node.label}</span>
            {node.meta.pid ? <span className="mono s11 muted">{node.meta.pid}</span> : null}
            {node.metrics.rssBytes ? (
              <span className="mono s11 muted">{formatBytes(node.metrics.rssBytes, locale)}</span>
            ) : null}
          </div>
        ))}
      </div>
    </div>
  );
}
