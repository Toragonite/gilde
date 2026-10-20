/**
 * Tiles view (T25, DESIGN §9-6).
 *
 * The same `GraphSnapshot`, rendered as a CSS grid of status cards instead of a
 * canvas — for when the graph gets dense, or when all that is wanted is a wall
 * of state. Both views read one projection and share one filter function, so
 * there is exactly one thing that can be right or wrong.
 */

import { Bar } from '../components/Bar';
import { Sparkline } from '../components/Sparkline';
import { EmptyState } from '../components/EmptyState';
import { useI18n } from '../i18n/useI18n';
import { formatBytes, formatCostUsd, formatDiffStat, formatElapsedClock } from '../lib/format';
import type { GraphNode, NodeKind } from '../lib/ipc';
import { StatusLine, useNode, useNodeMetrics } from './nodes/NodeShell';
import { sessionLabel } from './nodes/SessionNode';
import { getAccountModel, getSoftRssBytes } from './useGraphLayout';

/** Rank order, so a tile wall reads top-down the way the canvas reads left to right. */
const RANK: Record<NodeKind, number> = {
  account: 0,
  project: 1,
  worktree: 2,
  dispatch: 3,
  session: 4,
  run: 5,
  orphan: 6,
};

export function sortForTiles(nodes: readonly GraphNode[]): GraphNode[] {
  return [...nodes].sort(
    (a, b) => RANK[a.kind] - RANK[b.kind] || a.label.localeCompare(b.label) || a.id.localeCompare(b.id),
  );
}

export interface TilesViewProps {
  nodes: readonly GraphNode[];
  matched: ReadonlySet<string>;
  dimUnmatched: boolean;
  selectedId: string | null;
  onSelect: (nodeId: string) => void;
  onOpen: (nodeId: string) => void;
}

export function TilesView({
  nodes,
  matched,
  dimUnmatched,
  selectedId,
  onSelect,
  onOpen,
}: TilesViewProps) {
  const { t } = useI18n();
  const ordered = sortForTiles(nodes);

  if (ordered.length === 0) {
    return <EmptyState message={t('graph.empty')} />;
  }

  return (
    <div className="gilde-tiles scroll-y">
      {ordered.map((node) => (
        <Tile
          key={node.id}
          id={node.id}
          dim={dimUnmatched && !matched.has(node.id)}
          selected={node.id === selectedId}
          onSelect={onSelect}
          onOpen={onOpen}
        />
      ))}
    </div>
  );
}

interface TileProps {
  id: string;
  dim: boolean;
  selected: boolean;
  onSelect: (nodeId: string) => void;
  onOpen: (nodeId: string) => void;
}

function Tile({ id, dim, selected, onSelect, onOpen }: TileProps) {
  const { t, locale } = useI18n();
  const node = useNode(id);
  const metrics = useNodeMetrics(id);
  if (!node) return null;

  const rss = metrics?.rssBytes ?? 0;
  const soft = getSoftRssBytes();
  // Same rule as the canvas node: the badge already says `claude`, so the
  // title must not say it again.
  const title =
    node.kind === 'session' || node.kind === 'run'
      ? sessionLabel(node.sublabel, node.meta.vendor) || node.label
      : node.label;

  return (
    <div
      className="tile gilde-tile"
      data-dim={dim ? 'true' : undefined}
      data-selected={selected ? 'true' : undefined}
      onClick={() => onSelect(id)}
      onDoubleClick={() => onOpen(id)}
    >
      <div className="row gap-6">
        {node.meta.vendor ? <span className="badge">{node.meta.vendor}</span> : null}
        <span className="ell grow strong s12">{title}</span>
        <span className="s11 muted">{t(`graph.kind.${node.kind}` as 'graph.kind.session')}</span>
      </div>
      <div className="row gap-6 s11 muted mono">
        <StatusLine status={node.status} />
        {/* Same model the canvas session node shows: the account slot's pin,
            because the interactive lane launches without a model flag. */}
        {node.kind === 'session' && getAccountModel(node.meta.accountId) ? (
          <span>{getAccountModel(node.meta.accountId)}</span>
        ) : null}
        {metrics?.elapsedMs !== undefined ? <span>{formatElapsedClock(metrics.elapsedMs)}</span> : null}
        {rss > 0 ? <span>{formatBytes(rss, locale)}</span> : null}
        {metrics?.insertions !== undefined || metrics?.deletions !== undefined ? (
          <span>{formatDiffStat(metrics.insertions ?? 0, metrics.deletions ?? 0, locale)}</span>
        ) : null}
        {metrics?.costUsd !== undefined ? <span>{formatCostUsd(metrics.costUsd, locale)}</span> : null}
      </div>
      {node.kind === 'session' ? (
        <div className="gilde-node-meters">
          <Bar value={soft > 0 ? rss / soft : 0} warnAt={1} criticalAt={1.5} label={t('graph.rss')} />
          <Sparkline values={metrics?.outputRate ?? []} width={72} height={14} label={t('graph.outputRate')} />
        </div>
      ) : null}
      {node.sublabel && node.sublabel !== title ? (
        <div className="ell s11 muted mono">{node.sublabel}</div>
      ) : null}
    </div>
  );
}
