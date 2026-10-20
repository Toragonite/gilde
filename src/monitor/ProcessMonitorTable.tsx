/**
 * Process Monitor panel (T23, DESIGN §8-7).
 *
 * One component serves both surfaces the shell offers — the drawer's monitor
 * tab and a full pane — because they render the same store and differ only in
 * height. It is a table of rows grouped by `ProcGroupKind`, a fixed footer with
 * the machine's totals and the budget meter, and nothing else: every safety
 * rule it appears to enforce is really enforced in Rust, and the panel's job is
 * to make the rules *visible*.
 *
 * Reading rules that this file exists to honour (DESIGN §8-7 "가독성 규칙"):
 *
 *  - rows are keyed by pid and re-ordered at most once per scan, so nothing
 *    jumps under the cursor;
 *  - RSS is a number *and* a bar scaled to the largest row on screen;
 *  - `cwd` is resolved lazily, abbreviated, and the worktree segment is the
 *    part that survives;
 *  - the footer always says how old the scan is, or that polling is paused.
 */

import { useEffect, useMemo, useState } from 'react';

import { EmptyState } from '../components/EmptyState';
import { Mono } from '../components/Mono';
import { VirtualList } from '../components/VirtualList';
import { RefreshIcon, SearchIcon } from '../components/icons';
import { useI18n } from '../i18n/useI18n';
import type { MessageKey } from '../i18n';
import { formatBytes, formatDuration, formatNumber } from '../lib/format';
import type { ProcGroupKind, ProcRow } from '../lib/ipc';
import { GROUP_ORDER, SCAN_STALE_AFTER_MS, groupRows, maxRss } from '../store/procSlice';
import type { ProcGroupView } from '../store/procSlice';
import { useAppStore, useShallow } from '../store/useAppStore';
import { BudgetMeter } from './BudgetMeter';
import { ProcessGroupHeader } from './ProcessGroup';
import { ProcessRow } from './ProcessRow';
import './monitor.css';

/** Group header and data row share one height so the list can be windowed. */
export const MONITOR_ROW_H = 28;

type Item =
  | { kind: 'group'; view: ProcGroupView }
  | { kind: 'row'; row: ProcRow; group: ProcGroupKind };

/** Groups and their rows, flattened for the virtual list. */
export function flattenGroups(views: readonly ProcGroupView[]): Item[] {
  const items: Item[] = [];
  for (const view of views) {
    items.push({ kind: 'group', view });
    if (view.collapsed) continue;
    for (const row of view.rows) items.push({ kind: 'row', row, group: view.group });
  }
  return items;
}

function ColumnHeader() {
  const { t } = useI18n();
  return (
    <div className="mon-row mon-head" role="row">
      <span className="mon-cell mon-select" />
      <span className="mon-cell mon-pid">{t('monitor.col.pid')}</span>
      <span className="mon-cell mon-name">{t('monitor.col.name')}</span>
      <span className="mon-cell mon-pgid">{t('monitor.col.pgid')}</span>
      <span className="mon-cell mon-rss">{t('monitor.col.rss')}</span>
      <span className="mon-cell mon-cpu">{t('monitor.col.cpu')}</span>
      <span className="mon-cell mon-elapsed">{t('monitor.col.elapsed')}</span>
      <span className="mon-cell mon-cwd">{t('monitor.col.cwd')}</span>
      <span className="mon-cell mon-owner">{t('monitor.col.owner')}</span>
      <span className="mon-cell mon-status">{t('monitor.col.state')}</span>
      <span className="mon-cell mon-action" />
    </div>
  );
}

export function ProcessMonitorTable() {
  const { t, locale } = useI18n();

  const state = useAppStore(
    useShallow((s) => ({
      rows: s.proc.rows,
      totals: s.proc.totals,
      at: s.proc.at,
      loading: s.proc.loading,
      filter: s.proc.filter,
      collapsed: s.proc.collapsed,
      selection: s.proc.selection,
      windowFocused: s.proc.windowFocused,
    })),
  );
  const cwdCache = useAppStore((s) => s.proc.cwd);
  const scanProcesses = useAppStore((s) => s.scanProcesses);
  const setProcPanelVisible = useAppStore((s) => s.setProcPanelVisible);
  const setProcQuery = useAppStore((s) => s.setProcQuery);
  const toggleProcGroupFilter = useAppStore((s) => s.toggleProcGroupFilter);
  const toggleProcGroupCollapsed = useAppStore((s) => s.toggleProcGroupCollapsed);
  const toggleProcSelection = useAppStore((s) => s.toggleProcSelection);
  const selectProcGroup = useAppStore((s) => s.selectProcGroup);
  const clearProcSelection = useAppStore((s) => s.clearProcSelection);
  const ensureProcCwd = useAppStore((s) => s.ensureProcCwd);
  const openKillSheet = useAppStore((s) => s.openKillSheet);

  // Mounted means visible: polling goes to 3 s while this is on screen and back
  // to 15 s when it is not (DESIGN §8-7).
  useEffect(() => {
    setProcPanelVisible(true);
    return () => setProcPanelVisible(false);
  }, [setProcPanelVisible]);

  // The scan age has to keep ticking even when nothing else changes, or a dead
  // backend would look like a calm one.
  const [now, setNow] = useState(() => Date.now());
  useEffect(() => {
    const timer = setInterval(() => setNow(Date.now()), 1000);
    return () => clearInterval(timer);
  }, []);

  const groups = useMemo(
    () => groupRows(state.rows, state.filter, state.collapsed),
    [state.rows, state.filter, state.collapsed],
  );
  const items = useMemo(() => flattenGroups(groups), [groups]);
  const scale = useMemo(() => maxRss(groups), [groups]);
  const shown = useMemo(() => groups.reduce((sum, g) => sum + g.count, 0), [groups]);

  const orphans = groups.find((g) => g.group === 'oursOrphan');
  const selectedPids = state.selection.pids;

  const killRows = (rows: readonly ProcRow[]) => openKillSheet([...rows]);

  const cleanSelected = (view: ProcGroupView) => {
    const chosen = view.rows.filter((row) => selectedPids.includes(row.pid));
    killRows(chosen.length > 0 ? chosen : []);
  };

  const cleanAllOrphans = () => {
    if (!orphans) return;
    selectProcGroup('oursOrphan', orphans.rows.map((row) => row.pid));
    killRows(orphans.rows);
  };

  const age = state.at > 0 ? now - state.at : 0;
  const paused = !state.windowFocused;

  return (
    <div className="mon">
      <div className="hdr mon-toolbar">
        <label className="mon-search">
          <SearchIcon size="sm" />
          <input
            className="input"
            type="search"
            value={state.filter.query}
            placeholder={t('monitor.search')}
            aria-label={t('monitor.search')}
            onChange={(e) => setProcQuery(e.currentTarget.value)}
          />
        </label>

        <div className="row gap-4 mon-chips">
          {GROUP_ORDER.map((group) => {
            const active = state.filter.groups?.includes(group) ?? false;
            const count = state.totals.byGroup[group]?.count ?? 0;
            if (count === 0 && !active) return null;
            return (
              <button
                key={group}
                type="button"
                className="chip s11"
                data-selected={active ? 'true' : undefined}
                onClick={() => toggleProcGroupFilter(group)}
              >
                {t(`monitor.group.${group}` as MessageKey)}
                <Mono small className="muted">
                  {formatNumber(count, locale)}
                </Mono>
              </button>
            );
          })}
        </div>

        <div className="grow" />

        {orphans && orphans.count > 0 ? (
          <button type="button" className="btn danger s11" onClick={cleanAllOrphans}>
            {t('monitor.cleanAllOrphans', { n: formatNumber(orphans.count, locale) })}
          </button>
        ) : null}

        <button
          type="button"
          className="btn icon ghost"
          title={t('monitor.refresh')}
          aria-label={t('monitor.refresh')}
          disabled={state.loading}
          onClick={() => void scanProcesses(true)}
        >
          <RefreshIcon size="sm" />
        </button>
      </div>

      <ColumnHeader />

      {items.length === 0 ? (
        <EmptyState
          message={state.at === 0 ? t('drawer.monitor.empty') : t('monitor.noMatches')}
          actionLabel={t('monitor.refresh')}
          onAction={() => void scanProcesses(true)}
        />
      ) : (
        <VirtualList
          items={items}
          rowHeight={MONITOR_ROW_H}
          className="mon-body"
          keyOf={(item) => (item.kind === 'group' ? `g:${item.view.group}` : `p:${item.row.pid}`)}
          renderRow={(item) =>
            item.kind === 'group' ? (
              <ProcessGroupHeader
                view={item.view}
                selectedCount={
                  state.selection.group === item.view.group ? state.selection.pids.length : 0
                }
                onToggleCollapsed={toggleProcGroupCollapsed}
                onSelectAll={(view) =>
                  selectProcGroup(view.group, view.rows.map((row) => row.pid))
                }
                onClearSelection={clearProcSelection}
                onCleanSelected={cleanSelected}
              />
            ) : (
              <ProcessRow
                row={item.row}
                maxRssBytes={scale}
                cwd={cwdCache[item.row.pid]}
                selected={
                  state.selection.group === item.group && selectedPids.includes(item.row.pid)
                }
                onToggleSelect={toggleProcSelection}
                onKill={(row) => killRows([row])}
                onNeedCwd={ensureProcCwd}
              />
            )
          }
        />
      )}

      <footer className="mon-footer">
        <div className="row gap-12 s11 muted">
          <span>
            {t('monitor.footer.counts', {
              total: formatNumber(state.totals.processes, locale),
              shown: formatNumber(shown, locale),
            })}
          </span>
          <span>
            {t('monitor.footer.rss', { rss: formatBytes(state.totals.rssBytes, locale) })}
          </span>
          <div className="grow" />
          <span
            data-paused={paused ? 'true' : undefined}
            data-stale={!paused && state.at > 0 && age > SCAN_STALE_AFTER_MS ? 'true' : undefined}
          >
            {paused
              ? t('monitor.paused')
              : state.at === 0
                ? t('common.loading')
                : t('monitor.scanAge', { duration: formatDuration(age, locale) })}
          </span>
        </div>
        <BudgetMeter />
      </footer>

    </div>
  );
}

/** The pane view (`{ view: 'monitor' }`) and the drawer panel are the same UI. */
export function MonitorPane() {
  return <ProcessMonitorTable />;
}
