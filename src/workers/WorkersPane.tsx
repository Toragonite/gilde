/**
 * WorkersPane — the READ-ONLY view of the worker accounts the Claude Code
 * Orchestrator ENGINE knows (Gilde plan #3c): each account's login state, plan
 * windows with their resets, cooldown, overage, and which accounts share one
 * claude.ai login — as the dispatch host's `list_workers` answered, "as of" its
 * time (and, subtly, "cached by the host" when the host answered from its own
 * 60 s observer cache — `servedFromCache`).
 *
 * What is NOT here, on purpose: re-login, preferred toggles, model pickers,
 * remove, any setting. The only control is a local refresh (a plain call that
 * may come back cached). Accounts are managed in VS Code.
 *
 * All data comes from the store (`store/workersSlice.ts`); every string of it is
 * the engine's, goes through `flat()` and is rendered as text.
 */

import { useEffect, useState } from 'react';
import type { ReactNode } from 'react';

import { Badge } from '../components/Badge';
import { EmptyState } from '../components/EmptyState';
import { RefreshIcon } from '../components/icons';
import { StatusDot, statusTone } from '../components/StatusDot';
import { useI18n } from '../i18n/useI18n';
import { formatDuration, formatNumber, formatPercentPoints } from '../lib/format';
import type { CcoUsageWindow, CcoWorkerAccount, CcoWorkersAnswer, CcoWorkersSnapshot, Locale } from '../lib/ipc';
import type { PaneViewProps } from '../panes/Pane';
import { useAppStore, useShallow } from '../store/useAppStore';
import {
  WCAP,
  flat,
  formatClock,
  isCoolingDown,
  isOtherVendor,
  loginDetailKey,
  loginPill,
  unavailableKey,
  windowTone,
} from './model';
import './workers.css';

/** How often the relative times ("resets in", "left") re-render while shown. */
const CLOCK_TICK_MS = 30_000;

export function WorkersPane(_props: PaneViewProps) {
  const { t, td, locale } = useI18n();
  const startWorkersWatch = useAppStore((s) => s.startWorkersWatch);
  const refreshWorkers = useAppStore((s) => s.refreshWorkers);
  const { answer, commandMissing, loading, busy, error } = useAppStore(
    useShallow((s) => ({
      answer: s.workers.answer,
      commandMissing: s.workers.commandMissing,
      loading: s.workers.loading,
      busy: s.workers.busy,
      error: s.workers.error,
    })),
  );
  const [now, setNow] = useState(() => Date.now());

  // Watch while shown (one poll for every view), and keep the relative times current.
  useEffect(() => startWorkersWatch(), [startWorkersWatch]);
  useEffect(() => {
    const timer = setInterval(() => setNow(Date.now()), CLOCK_TICK_MS);
    return () => clearInterval(timer);
  }, []);

  const snapshot = answer?.snapshot ?? null;

  return (
    <div className="workers">
      <div className="hdr workers-hdr">
        <span className="strong">{t('view.workers')}</span>
        {snapshot ? (
          <span className="mono s12 muted" data-testid="workers-as-of">
            {t('workers.asOf', { time: formatClock(snapshot.generatedAt, locale, now) })}
          </span>
        ) : null}
        {snapshot?.servedFromCache === true ? (
          <span className="s12 muted" data-testid="workers-host-cached" title={t('workers.hostCachedHint')}>
            {t('workers.hostCached')}
          </span>
        ) : null}
        <span className="grow" />
        <button
          type="button"
          className="btn workers-refresh"
          onClick={() => void refreshWorkers(true)}
          disabled={busy}
          aria-label={t('workers.refresh')}
          title={t('workers.refresh')}
        >
          <RefreshIcon size="sm" />
        </button>
      </div>
      <div className="workers-preview s12 muted">{t('workers.preview')}</div>
      <div className="workers-body scroll-y">
        {commandMissing ? (
          <EmptyState message={t('workers.commandMissing')} />
        ) : answer === null ? (
          error ? (
            <EmptyState
              message={td(error.key, error.params)}
              detail={error.detail}
              actionLabel={t('common.retry')}
              onAction={() => void refreshWorkers(true)}
            />
          ) : loading ? (
            <div className="workers-note s12 muted">{t('common.loading')}</div>
          ) : null
        ) : !answer.supported ? (
          <EmptyState message={t('workers.unsupported')} />
        ) : (
          <>
            <WorkersStatus answer={answer} locale={locale} now={now} errorDetail={error ? td(error.key, error.params) : null} />
            {snapshot ? <WorkersList snapshot={snapshot} locale={locale} now={now} /> : null}
          </>
        )}
      </div>
    </div>
  );
}

/** What is wrong, if anything: no sidecar, no host, an older/newer host, a stale list. */
function WorkersStatus({
  answer,
  locale,
  now,
  errorDetail,
}: {
  answer: CcoWorkersAnswer;
  locale: Locale;
  now: number;
  errorDetail: string | null;
}) {
  const { t } = useI18n();
  const lines: ReactNode[] = [];
  if (!answer.available) {
    lines.push(
      <div key="down" className="workers-banner s12" data-tone="warn" data-reason="sidecar-down">
        {t('workers.sidecarDown')}
      </div>,
    );
  }
  if (answer.unavailable) {
    const reason = String(answer.unavailable.reason);
    lines.push(
      <div key="reason" className="workers-banner s12" data-tone="warn" data-reason={flat(reason, 24)}>
        <span>{t(unavailableKey(reason))}</span>
        {answer.unavailable.detail ? (
          <span className="mono workers-detail"> {flat(answer.unavailable.detail, reason === 'schema-unknown' ? WCAP.schema : WCAP.detail)}</span>
        ) : null}
      </div>,
    );
  }
  if (errorDetail) {
    lines.push(
      <div key="error" className="workers-banner s12" data-tone="error">
        {errorDetail}
      </div>,
    );
  }
  if (answer.snapshot && answer.stale) {
    lines.push(
      <div key="stale" className="workers-banner s12" data-tone="warn" data-stale="true">
        {t('workers.stale', { time: formatClock(answer.receivedAt ?? answer.snapshot.generatedAt, locale, now) })}
      </div>,
    );
  }
  if (lines.length === 0) return null;
  return <div className="workers-status col">{lines}</div>;
}

function WorkersList({ snapshot, locale, now }: { snapshot: CcoWorkersSnapshot; locale: Locale; now: number }) {
  const { t } = useI18n();
  const guard = (v: 'allow' | 'block') => (v === 'allow' ? t('workers.guard.allow') : t('workers.guard.block'));
  return (
    <div className="workers-list col">
      <div className="workers-guards s12 muted">
        {t('workers.guards', {
          overage: guard(snapshot.guards.overageWorkerDispatch),
          frontier: guard(snapshot.guards.frontierWorkerDispatch),
        })}
      </div>
      <section className="workers-section">
        <div className="sec">{t('workers.section.accounts', { n: snapshot.accounts.length })}</div>
        {snapshot.accounts.length === 0 ? (
          <div className="workers-note s12 muted">{t('workers.empty')}</div>
        ) : (
          snapshot.accounts.map((account) => <AccountRow key={account.key} account={account} locale={locale} now={now} />)
        )}
      </section>
      {snapshot.unsupportedAccounts.length > 0 ? (
        <section className="workers-section">
          <div className="sec">{t('workers.section.unsupported', { n: snapshot.unsupportedAccounts.length })}</div>
          <ul className="workers-unsupported s12 muted">
            {snapshot.unsupportedAccounts.map((u, i) => (
              <li key={`${i}-${u.name}`}>
                <span className="mono">{flat(u.name, WCAP.name) || t('workers.unnamed')}</span> · {flat(u.detail, WCAP.detail)}
              </li>
            ))}
          </ul>
        </section>
      ) : null}
    </div>
  );
}

/** One account: who it is, its login, its windows, and the engine's notes on it. Not a control. */
function AccountRow({ account, locale, now }: { account: CcoWorkerAccount; locale: Locale; now: number }) {
  const { t } = useI18n();
  const pill = loginPill(account);
  const tone = statusTone(pill.status);
  const detailKey = loginDetailKey(account.loginDetail);
  const cooling = isCoolingDown(account, now);
  const nf = (n: number) => formatNumber(n, locale);
  const plan = account.planLabel ?? account.plan;

  return (
    <div className="workers-account col" data-login={flat(account.login, 24)} data-cooldown={cooling ? 'true' : undefined}>
      <div className="row gap-6 workers-account-line">
        <StatusDot status={pill.status} pulse={false} />
        <span className="strong ell">{flat(account.name, WCAP.name)}</span>
        {account.isMain ? <Badge>{t('workers.badge.main')}</Badge> : null}
        {account.isWorker ? <Badge>{t('workers.badge.worker')}</Badge> : null}
        {account.preferred ? <Badge>{t('workers.badge.preferred')}</Badge> : null}
        {isOtherVendor(account) ? (
          <Badge tone="warn" title={flat(account.provider, 32)}>
            {flat(account.providerLabel || account.provider, WCAP.label)}
          </Badge>
        ) : null}
        {plan ? <span className="s12 muted ell">{flat(plan, WCAP.plan)}</span> : null}
        <span className="grow" />
        <span className="s12 workers-tone" data-tone={tone}>
          {pill.key ? t(pill.key) : flat(pill.raw, 24)}
        </span>
      </div>

      {account.loginDetail ? (
        <div className="s12 workers-tone workers-detail" data-tone={tone}>
          {detailKey ? t(detailKey) : flat(account.loginDetail, WCAP.detail)}
        </div>
      ) : null}

      {cooling && account.cooldownUntil !== null ? (
        <div className="s12 workers-tone" data-tone="warn" data-testid="workers-cooldown">
          {t('workers.cooldown', {
            at: formatClock(account.cooldownUntil, locale, now),
            in: formatDuration(account.cooldownUntil - now, locale),
          })}
        </div>
      ) : null}

      {account.overage?.enabled ? (
        <div className="row gap-6 s12 workers-tone" data-tone="warn" data-testid="workers-overage">
          <span>{t('workers.overage')}</span>
          {account.overage.percent !== null ? (
            <span className="mono">{t('workers.overageUsed', { percent: formatPercentPoints(account.overage.percent, locale, 1) })}</span>
          ) : null}
          {account.overage.spendLabel ? <span className="mono">{flat(account.overage.spendLabel, WCAP.spend)}</span> : null}
        </div>
      ) : null}

      {account.sameAccountAs.length > 0 ? (
        <div className="s12 muted" data-testid="workers-same-account">
          {t('workers.sameAccount', { names: account.sameAccountAs.map((n) => flat(n, WCAP.name)).join(', ') })}
        </div>
      ) : null}

      {account.windowsStaleSince !== null ? (
        <div className="s12 workers-tone" data-tone="warn" data-testid="workers-windows-stale">
          {t('workers.windowsStale', { time: formatClock(account.windowsStaleSince, locale, now) })}
        </div>
      ) : null}

      {account.windows.length > 0 ? (
        <div className="workers-windows" data-stale={account.windowsStaleSince !== null ? 'true' : undefined}>
          {account.windows.map((w, i) => (
            <WindowRow key={`${w.kind}-${i}`} window={w} locale={locale} now={now} />
          ))}
        </div>
      ) : account.login === 'ok' ? (
        <div className="s12 muted">{t('workers.noWindows')}</div>
      ) : null}

      <div className="row gap-8 s12 muted workers-meta">
        {account.running > 0 ? <span>{t('workers.running', { n: nf(account.running) })}</span> : null}
        {account.defaultModel ? (
          <span className="mono ell">{t('workers.defaultModel', { model: flat(account.defaultModel, WCAP.model) })}</span>
        ) : null}
        {account.modelRanAs && account.modelRanAs !== account.defaultModel ? (
          <span className="mono ell">{t('workers.ranAs', { model: flat(account.modelRanAs, WCAP.model) })}</span>
        ) : null}
        {account.week.tasks > 0 ? (
          <span className="mono">
            {t('workers.week', { tasks: nf(account.week.tasks), in: nf(account.week.inputTokens), out: nf(account.week.outputTokens) })}
          </span>
        ) : null}
      </div>

      {account.lastError ? (
        <div className="s12 muted workers-detail">
          <span>{t('workers.lastError')}: </span>
          <span className="mono">{flat(account.lastError, WCAP.detail)}</span>
        </div>
      ) : null}
    </div>
  );
}

/** One plan window: its label, the engine's percent (as given) on a bar toned by the engine's severity, and its reset. */
function WindowRow({ window: w, locale, now }: { window: CcoUsageWindow; locale: Locale; now: number }) {
  const { t } = useI18n();
  const pct = Number.isFinite(w.percent) ? Math.min(100, Math.max(0, w.percent)) : 0;
  const label = flat(w.label, WCAP.label);
  const reset =
    w.resetsAt === null
      ? t('workers.resetUnknown')
      : w.resetsAt > now
        ? t('workers.resetsIn', { in: formatDuration(w.resetsAt - now, locale), at: formatClock(w.resetsAt, locale, now) })
        : t('workers.resetPassed', { at: formatClock(w.resetsAt, locale, now) });
  return (
    <div className="workers-window row gap-8 s12" data-severity={flat(w.severity, 16)}>
      <span className="workers-window-label ell">{label}</span>
      <div
        className="bar workers-window-bar"
        role="meter"
        aria-valuenow={Math.round(pct)}
        aria-valuemin={0}
        aria-valuemax={100}
        aria-label={label}
      >
        <span style={{ width: `${pct}%` }} data-tone={windowTone(w.severity)} />
      </div>
      <span className="mono workers-window-pct workers-tone" data-tone={windowTone(w.severity) ?? 'neutral'}>
        {formatPercentPoints(pct, locale, 1)}
      </span>
      <span className="muted ell">{reset}</span>
    </div>
  );
}
