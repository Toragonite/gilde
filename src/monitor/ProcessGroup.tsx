/**
 * One `ProcGroupKind` header (T23, DESIGN §8-7).
 *
 * The header is where the safety asymmetry becomes visible: it carries the
 * group's count, RSS sum and CPU sum, a "select all *in this group*" control —
 * and a bulk action **only for `OursOrphan`**, the one group whose every row is
 * provably ours and unowned. `AgentUnmanaged` never gets one (it is very likely
 * another orchestrator's live agent, DESIGN §13), and `ExternalResidue` /
 * `Other` are confirmed row by row.
 */

import { Badge } from '../components/Badge';
import { Mono } from '../components/Mono';
import { ChevronDownIcon, ChevronRightIcon } from '../components/icons';
import { useI18n } from '../i18n/useI18n';
import type { MessageKey } from '../i18n';
import { formatBytes, formatNumber } from '../lib/format';
import type { ProcGroupKind } from '../lib/ipc';
import { allowsBulkAction } from '../store/procSlice';
import type { ProcGroupView } from '../store/procSlice';

export interface ProcessGroupHeaderProps {
  view: ProcGroupView;
  /** How many rows of this group are selected. */
  selectedCount: number;
  onToggleCollapsed: (group: ProcGroupKind) => void;
  onSelectAll: (view: ProcGroupView) => void;
  onClearSelection: () => void;
  /** Only ever passed for `oursOrphan`. */
  onCleanSelected: (view: ProcGroupView) => void;
}

export function ProcessGroupHeader({
  view,
  selectedCount,
  onToggleCollapsed,
  onSelectAll,
  onClearSelection,
  onCleanSelected,
}: ProcessGroupHeaderProps) {
  const { t, locale } = useI18n();
  const allSelected = selectedCount > 0 && selectedCount === view.rows.length;

  return (
    <div className="mon-group" role="row">
      <button
        type="button"
        className="mon-group-toggle"
        aria-expanded={!view.collapsed}
        onClick={() => onToggleCollapsed(view.group)}
      >
        {view.collapsed ? <ChevronRightIcon size="sm" /> : <ChevronDownIcon size="sm" />}
        <span className="s12">{t(`monitor.group.${view.group}` as MessageKey)}</span>
        <Badge tone={view.group === 'oursOrphan' ? 'error' : 'neutral'}>
          {formatNumber(view.count, locale)}
        </Badge>
      </button>

      <div className="grow" />

      <Mono small className="muted">
        {t('monitor.group.sums', {
          rss: formatBytes(view.rssBytes, locale),
          cpu: formatNumber(view.cpuPct, locale, 1),
        })}
      </Mono>

      <button
        type="button"
        className="btn ghost s11"
        onClick={() => (allSelected ? onClearSelection() : onSelectAll(view))}
      >
        {allSelected ? t('monitor.selectNone') : t('monitor.selectAll')}
      </button>

      {allowsBulkAction(view.group) ? (
        <button
          type="button"
          className="btn danger s11"
          disabled={selectedCount === 0}
          onClick={() => onCleanSelected(view)}
        >
          {t('monitor.cleanSelected', { n: formatNumber(selectedCount, locale) })}
        </button>
      ) : null}
    </div>
  );
}
