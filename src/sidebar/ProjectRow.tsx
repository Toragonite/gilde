/**
 * Project row — 26 px, one line (docs/ui-mockups/Main.dc.html).
 *
 * Everything it shows comes off the graph node it is handed: the label, the
 * repo slug behind the tooltip and the number of worktree children. It never
 * fetches.
 */

import { useI18n } from '../i18n/useI18n';
import { ChevronDownIcon, ChevronRightIcon, PlusIcon } from '../components/icons';
import type { GraphNode } from '../lib/ipc';

export interface ProjectRowProps {
  node: GraphNode;
  depth: number;
  collapsed: boolean;
  /** Worktree children in the projection — the count the row prints. */
  worktreeCount: number;
  selected: boolean;
  onToggle: () => void;
  onSelect: () => void;
  onNewWorktree: () => void;
}

export function ProjectRow({
  node,
  depth,
  collapsed,
  worktreeCount,
  selected,
  onToggle,
  onSelect,
  onNewWorktree,
}: ProjectRowProps) {
  const { t } = useI18n();

  return (
    <div
      className="tree-row sb-project"
      style={{ paddingLeft: 8 + depth * 12 }}
      role="treeitem"
      aria-expanded={!collapsed}
      aria-selected={selected}
      aria-label={node.label}
      onClick={onSelect}
      onDoubleClick={onToggle}
    >
      <button
        type="button"
        className="sb-caret"
        aria-label={node.label}
        onClick={(event) => {
          event.stopPropagation();
          onToggle();
        }}
      >
        {collapsed ? <ChevronRightIcon size="sm" /> : <ChevronDownIcon size="sm" />}
      </button>

      <span className="ell sb-name" title={node.sublabel ?? node.meta.cwd}>
        {node.label}
      </span>

      <div className="grow" />

      <span className="sb-actions">
        <button
          type="button"
          className="sb-iconbtn"
          title={t('worktree.new')}
          aria-label={t('worktree.new')}
          onClick={(event) => {
            event.stopPropagation();
            onNewWorktree();
          }}
        >
          <PlusIcon size="sm" />
        </button>
      </span>

      <span className="muted s11">{t('sidebar.worktreeCount', { n: worktreeCount })}</span>
    </div>
  );
}
