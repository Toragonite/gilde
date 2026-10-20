/**
 * Worktree node (T25, DESIGN §9-6).
 *
 * Body: branch, `+N −M`, dirty file count, `↑k`, session count — the five facts
 * that decide whether this worktree is safe to remove, in the same words the
 * sidebar row uses.
 */

import type { NodeProps } from '@xyflow/react';

import { useI18n } from '../../i18n/useI18n';
import { formatDiffStat, formatNumber } from '../../lib/format';
import { NodeShell, NodeSub, NodeTitle, StatusLine, useNode, useNodeMetrics, useWorkCount } from './NodeShell';

export function WorktreeNode({ id }: NodeProps) {
  const { t, locale } = useI18n();
  const node = useNode(id);
  const metrics = useNodeMetrics(id);
  const sessions = useWorkCount(id, 'session', false);
  const runs = useWorkCount(id, 'run', false);
  if (!node) return null;

  const added = metrics?.insertions ?? 0;
  const removed = metrics?.deletions ?? 0;
  const dirty = metrics?.dirtyFiles ?? 0;
  const ahead = metrics?.ahead ?? 0;

  return (
    <NodeShell id={id} kind="worktree" title={node.meta.cwd}>
      <NodeTitle status={node.status} label={node.label}>
        {node.badges.includes('locked') ? <span className="badge">{t('graph.locked')}</span> : null}
      </NodeTitle>
      <NodeSub>
        <StatusLine status={node.status} />
        {added > 0 || removed > 0 ? <> · {formatDiffStat(added, removed, locale)}</> : null}
        {ahead > 0 ? <> · ↑{formatNumber(ahead, locale)}</> : null}
      </NodeSub>
      <NodeSub>
        {dirty > 0 ? <>{t('graph.dirtyFiles', { n: formatNumber(dirty, locale) })} · </> : null}
        {sessions > 0 || runs === 0 ? t('graph.count.sessions', { n: sessions }) : null}
        {runs > 0 ? (
          <>
            {sessions > 0 ? ' · ' : ''}
            {t('graph.count.runs', { n: runs })}
          </>
        ) : null}
      </NodeSub>
    </NodeShell>
  );
}
