/**
 * Memory budget meter (T23, DESIGN §8-8) — total RSS, its share of physical
 * RAM, and the system pressure level — the block in `ProcessMonitorTable`'s
 * header. (The status bar shows one memory figure of its own, from
 * `graph.totals`; the second, portalled chip that sat next to it was removed.)
 *
 * Colour appears only at a threshold (UI-GUIDELINES §2-1a): a healthy meter is
 * neutral, so a coloured bar always means something. The thresholds are the
 * user's own `globalWarnPct` / `globalCriticalPct`, not a hard-coded 70/85.
 *
 * `gilde://budget/alert` is not handled here — `useEventBridge` already turns it
 * into a toast (`badge.over-budget`), and one event must have exactly one
 * subscriber.
 */

import { Bar } from '../components/Bar';
import { Mono } from '../components/Mono';
import { useI18n } from '../i18n/useI18n';
import type { MessageKey } from '../i18n';
import { formatBytes, formatPercentPoints } from '../lib/format';
import type { BudgetState } from '../lib/ipc';
import { useAppStore } from '../store/useAppStore';

export function BudgetMeter() {
  const { t, locale } = useI18n();
  const budget = useAppStore((s) => s.proc.budget);
  const totals = useAppStore((s) => s.proc.totals);

  // `budget_get` is authoritative (it is what the watchdog judged); the scan
  // totals answer until the first budget arrives, and "unknown" answers if
  // neither has. An unknown number is never rendered as a healthy one.
  const rssBytes = budget?.rssBytes ?? totals.rssBytes;
  const ramBytes = budget?.physicalRamBytes ?? totals.physicalRamBytes;
  const percent = budget?.percent ?? totals.rssPercent;
  const known = ramBytes > 0;
  const warnAt = (budget?.warnPct ?? 70) / 100;
  const criticalAt = (budget?.criticalPct ?? 85) / 100;
  const pressure = budget?.pressureLevel ?? totals.pressureLevel;
  const pressureLabel = t(`pressure.${pressure}` as MessageKey);

  return (
    <div className="mon-budget">
      <div className="row gap-8 s12">
        <span>{t('monitor.budget.title')}</span>
        <div className="grow" />
        <Mono small className="muted">
          {known
            ? t('monitor.budget.value', {
                rss: formatBytes(rssBytes, locale),
                ram: formatBytes(ramBytes, locale),
                percent: formatPercentPoints(percent, locale),
              })
            : t('common.unknown')}
        </Mono>
      </div>
      <Bar
        value={known ? percent / 100 : 0}
        warnAt={warnAt}
        criticalAt={criticalAt}
        label={t('monitor.budget.title')}
      />
      <div className="row gap-8 s11 muted">
        <span>{t('monitor.budget.pressure', { level: pressureLabel })}</span>
        <div className="grow" />
        <span>
          {t('monitor.budget.thresholds', {
            warn: formatPercentPoints(budget?.warnPct ?? 70, locale),
            critical: formatPercentPoints(budget?.criticalPct ?? 85, locale),
          })}
        </span>
      </div>
      <span className="s11 dim">{t('monitor.budget.softOnly')}</span>
    </div>
  );
}

/** Exported for the tests: the numbers the meter would print. */
export function budgetReadout(budget: BudgetState | null): {
  percent: number;
  known: boolean;
} {
  if (!budget || budget.physicalRamBytes === 0) return { percent: 0, known: false };
  return { percent: budget.percent, known: true };
}
