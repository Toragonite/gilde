/**
 * One process row (T23, DESIGN §8-7).
 *
 * Density and column order come from `docs/ui-mockups/Main.dc.html`'s drawer
 * table: 26 px rows, monospace tabular numbers, right-aligned RSS and elapsed,
 * no vertical rules, hover background only.
 *
 * Two rules are structural rather than cosmetic:
 *
 *  - `argvDisplay` is the **only** argv this component can render. It was
 *    masked in Rust before the row left the process (DESIGN §8-7), and there is
 *    no unmasked field on the wire to render by accident.
 *  - a `protected` row has **no kill affordance at all** — not a disabled
 *    button, no button — and carries the reason as a tooltip.
 */

import { useEffect } from 'react';

import { Chip } from '../components/Badge';
import { Bar } from '../components/Bar';
import { Mono } from '../components/Mono';
import { StatusDot, statusMessageKey } from '../components/StatusDot';
import { Tooltip } from '../components/Tooltip';
import { GitBranchIcon } from '../components/icons';
import { useI18n } from '../i18n/useI18n';
import type { MessageKey } from '../i18n';
import { formatBytes, formatElapsedClock, formatNumber } from '../lib/format';
import type { NodeStatus, ProcRow } from '../lib/ipc';
import { describeCwd, protectionReason } from '../store/procSlice';

export interface ProcessRowProps {
  row: ProcRow;
  /** Largest RSS on screen — the scale of every bar (DESIGN §8-7). */
  maxRssBytes: number;
  cwd: string | null | undefined;
  selected: boolean;
  onToggleSelect: (row: ProcRow) => void;
  onKill: (row: ProcRow) => void;
  /** Lazy `procmon_cwd`, asked only for rows that actually render. */
  onNeedCwd: (pid: number) => void;
}

/** The row's status dot: the four semantic tokens, never a colour per group. */
export function rowStatus(row: ProcRow): NodeStatus {
  if (row.tags.includes('orphan')) return 'orphan';
  if (row.group === 'oursLive') return 'working';
  if (row.tags.includes('detached')) return 'detached';
  if (row.group === 'oursOrphan') return 'orphan';
  return 'idle';
}

/** Who the row belongs to, in one word. */
export function ownerLabelKey(row: ProcRow): MessageKey {
  if (row.spawnId) return row.owner?.sessionId ? 'monitor.owner.session' : 'monitor.owner.app';
  if (row.tags.includes('ours')) return 'monitor.owner.app';
  return 'monitor.owner.external';
}

/**
 * What the row *is*, not which group it landed in — the group is already the
 * header above it. An orphan says why it is one (`ppid 1`), the way the mockup's
 * drawer table does.
 */
export function stateLabel(row: ProcRow, t: (key: MessageKey, params?: Record<string, string | number>) => string): string {
  const status = rowStatus(row);
  const label = t(statusMessageKey(status));
  if (status === 'orphan' && row.ppid <= 1) return `${label} · ppid ${row.ppid}`;
  if (row.tags.includes('detached')) return t('status.detached');
  return label;
}

export function ProcessRow({
  row,
  maxRssBytes,
  cwd,
  selected,
  onToggleSelect,
  onKill,
  onNeedCwd,
}: ProcessRowProps) {
  const { t, locale } = useI18n();
  const protectedRow = row.tags.includes('protected');

  useEffect(() => {
    if (cwd === undefined) onNeedCwd(row.pid);
  }, [cwd, onNeedCwd, row.pid]);

  const resolved = cwd ?? (row.cwd || null);
  const parts = describeCwd(resolved);
  const status = rowStatus(row);
  const share = maxRssBytes > 0 ? row.rssBytes / maxRssBytes : 0;

  return (
    <div className="mon-row" role="row" aria-selected={selected} data-selected={selected || undefined}>
      <span className="mon-cell mon-select" role="gridcell">
        {protectedRow ? null : (
          <input
            type="checkbox"
            checked={selected}
            onChange={() => onToggleSelect(row)}
            aria-label={t('monitor.select', { pid: row.pid })}
          />
        )}
      </span>

      <Mono small className="mon-cell mon-pid">
        {row.pid}
      </Mono>

      <span className="mon-cell mon-name ell" role="gridcell" title={row.argvDisplay}>
        <span className="mon-name-text">{row.name}</span>
        <span className="muted s11 ell mon-argv">{row.argvDisplay}</span>
      </span>

      <Mono small className="mon-cell mon-pgid muted">
        {row.pgid}
      </Mono>

      <span className="mon-cell mon-rss" role="gridcell">
        <Mono small>{formatBytes(row.rssBytes, locale)}</Mono>
        <span className="mon-rss-bar">
          {/*
            A magnitude bar, not a threshold: the thresholds above it belong to
            the budget meter. Both trip points sit above the clamp so this bar
            is always neutral — 12 GB next to 300 MB must be preattentive
            without claiming that either is a problem.

            BACKLOG 16: `monitor.css` lifts the track from the shared `--n4` to
            `--n5` here — on this ground the old track was invisible, so a small
            RSS looked like no bar at all.
          */}
          <Bar value={share} warnAt={2} criticalAt={2} label={formatBytes(row.rssBytes, locale)} />
        </span>
      </span>

      <Mono small className="mon-cell mon-cpu muted">
        {formatNumber(row.cpuPct, locale, 1)}
      </Mono>

      <Mono small className="mon-cell mon-elapsed muted">
        {formatElapsedClock(row.elapsedSecs * 1000)}
      </Mono>

      <span className="mon-cell mon-cwd ell" role="gridcell" title={resolved ?? undefined}>
        {parts ? (
          row.tags.includes('inWorktree') && parts.highlight ? (
            <Chip title={resolved ?? undefined}>
              <GitBranchIcon size="sm" />
              {parts.highlight}
            </Chip>
          ) : (
            <span className="muted ell">
              {parts.prefix}
              {parts.highlight ? <span className="mon-cwd-hit">{parts.highlight}</span> : null}
              {parts.suffix}
            </span>
          )
        ) : (
          <span className="dim">—</span>
        )}
      </span>

      <span className="mon-cell mon-owner muted s11 ell" role="gridcell">
        {t(ownerLabelKey(row))}
      </span>

      <span className="mon-cell mon-status row gap-6" role="gridcell">
        <StatusDot status={status} pulse={false} />
        <span className="s11 ell">{stateLabel(row, t)}</span>
      </span>

      <span className="mon-cell mon-action" role="gridcell">
        {protectedRow ? (
          <Tooltip label={t(`monitor.reason.${protectionReason(row)}` as MessageKey)}>
            <span className="s11 dim">{t('monitor.protected')}</span>
          </Tooltip>
        ) : (
          <button type="button" className="btn ghost danger s11" onClick={() => onKill(row)}>
            {t('monitor.clean')}
          </button>
        )}
      </span>
    </div>
  );
}
