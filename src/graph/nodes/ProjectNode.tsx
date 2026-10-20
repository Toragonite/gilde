/**
 * Project node — rank 0 of the chain (T25, DESIGN §9-5).
 *
 * A project has no status of its own; what matters about it is how much is
 * hanging off it, so the sub line is the two counts the sidebar header also
 * shows. They are read with primitive selectors, which is why a metrics patch
 * on a grandchild cannot repaint this node.
 */

import type { NodeProps } from '@xyflow/react';

import { useI18n } from '../../i18n/useI18n';
import { NodeShell, NodeSub, NodeTitle, useChildCount, useNode, useWorkCount } from './NodeShell';

export function ProjectNode({ id }: NodeProps) {
  const { t } = useI18n();
  const node = useNode(id);
  const worktrees = useChildCount(id, 'worktree');
  const sessions = useWorkCount(id, 'session', true);
  const runs = useWorkCount(id, 'run', true);
  if (!node) return null;

  return (
    <NodeShell id={id} kind="project" title={node.meta.cwd}>
      <NodeTitle label={node.label} />
      <NodeSub>
        {t('graph.count.worktrees', { n: worktrees })} · {t('graph.count.sessions', { n: sessions })}
        {runs > 0 ? <> · {t('graph.count.runs', { n: runs })}</> : null}
      </NodeSub>
    </NodeShell>
  );
}
