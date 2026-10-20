/**
 * Status bar — 24px, always visible (DESIGN §9-1, §9-6).
 *
 * The preview build shows TWO things here: whether the agents are running well
 * (the health indicator, which opens the health popover) and whether Gilde is
 * connected — the sidecar that watches the Claude Code Orchestrator's dispatch
 * hosts — in plain words. Anything but "Connected" is a button that opens
 * Settings › Diagnostics.
 *
 * With the experimental lanes on (`src/features.ts`) the inventory comes back on
 * the left: sessions, worktrees, active runs, orphans, memory and pressure from
 * `graph.totals` (Rust recomputes and ships them as a `totals` op, so the
 * counters cannot drift from the graph), and a "no update for …" warning after
 * 10 s without a patch — a stalled backend must look stalled, not calm.
 *
 * **The state distribution is not read from `totals`** (M1.5 BACKLOG 15a): it
 * is derived from the same `SessionView.status` the pill and the sidebar render,
 * through `selectSessionStateCounts`, so the two surfaces cannot word one
 * session two ways.
 */

import { useEffect, useState } from 'react';

import { experimentalLanes } from '../features';
import { useI18n } from '../i18n/useI18n';
import type { MessageKey } from '../i18n';
import { Bar } from '../components/Bar';
import { StatusDot } from '../components/StatusDot';
import { Mono } from '../components/Mono';
import { formatBytes, formatDuration, formatPercentPoints } from '../lib/format';
import type { NodeStatus, SidecarLifecycle } from '../lib/ipc';
import { selectSessionStateCounts } from '../store/sessionSlice';
import { useAppStore, useShallow } from '../store/useAppStore';
import { HealthPopover } from './HealthPopover';
import { indicatorText } from './healthText';

/** A graph that has not moved for this long is reported as stale, not as calm. */
export const STALE_AFTER_MS = 10_000;

export interface ConnectionReading {
  status: NodeStatus;
  key: MessageKey;
  /** Healthy needs no action; anything else opens the diagnostics. */
  healthy: boolean;
}

/** The sidecar's lifecycle as one plain sentence (the preview's only status). */
export function connectionReading(state: SidecarLifecycle | null | undefined): ConnectionReading {
  switch (state) {
    case 'ready':
      return { status: 'ok', key: 'statusbar.connection.ready', healthy: true };
    case 'starting':
    case 'restarting':
    case null:
    case undefined:
      // Before the first `sidecar_status` answer there is nothing to report yet.
      return { status: 'stalled', key: 'statusbar.connection.starting', healthy: false };
    case 'degraded':
      return { status: 'stalled', key: 'statusbar.connection.degraded', healthy: false };
    case 'down':
      return { status: 'error', key: 'statusbar.connection.down', healthy: false };
    default:
      return { status: 'error', key: 'statusbar.connection.down', healthy: false };
  }
}

function ConnectionIndicator() {
  const { t } = useI18n();
  // The live lifecycle only: `graph.totals.sidecarState` starts as `down`
  // before Rust has said anything, which would read as a failure at launch.
  const sidecar = useAppStore((s) => s.sidecar);
  const setSettingsOpen = useAppStore((s) => s.setSettingsOpen);
  const reading = connectionReading(sidecar?.state);

  if (reading.healthy) {
    return (
      <span className="row gap-6" data-testid="connection" title={t('statusbar.connection.title')}>
        <StatusDot status={reading.status} pulse={false} />
        {t(reading.key)}
      </span>
    );
  }
  return (
    <button
      type="button"
      data-testid="connection"
      onClick={() => setSettingsOpen(true, 'diagnostics')}
      title={t('statusbar.connection.details')}
    >
      <StatusDot status={reading.status} pulse={false} />
      {t(reading.key)}
    </button>
  );
}

function LaneInventory() {
  const { t, locale } = useI18n();
  const totals = useAppStore((s) => s.graph.totals);
  const states = useAppStore(useShallow(selectSessionStateCounts));
  const lastPatchAt = useAppStore((s) => s.graph.lastPatchAt);
  const resyncing = useAppStore((s) => s.graph.resyncing);
  const setDrawerTab = useAppStore((s) => s.setDrawerTab);

  const [now, setNow] = useState(() => Date.now());
  useEffect(() => {
    const timer = setInterval(() => setNow(Date.now()), 1000);
    return () => clearInterval(timer);
  }, []);

  const stale = lastPatchAt > 0 && now - lastPatchAt > STALE_AFTER_MS;
  const budget = totals.rssBudgetPercent / 100;

  return (
    <>
      <span>
        {t('statusbar.counts', {
          sessions: totals.sessions,
          worktrees: totals.worktrees,
          runs: totals.runsActive,
        })}
      </span>
      <span>
        {t('statusbar.states', {
          working: states.working,
          waiting: states.waiting,
          idle: states.idle,
        })}
      </span>
      {totals.orphans > 0 ? (
        <button
          type="button"
          onClick={() => setDrawerTab('monitor')}
          title={t('drawer.monitor')}
        >
          <StatusDot status="orphan" pulse={false} />
          {t('statusbar.orphans', { n: totals.orphans })}
        </button>
      ) : null}

      <div className="grow" />

      {resyncing ? <span>{t('graph.resyncing')}</span> : null}
      {stale ? (
        <span style={{ color: 'var(--warn)' }}>
          {t('statusbar.graphStale', { duration: formatDuration(now - lastPatchAt, locale) })}
        </span>
      ) : null}

      <span className="row gap-6" title={formatBytes(totals.rssBytes, locale)}>
        <Mono small>{t('statusbar.memory', { percent: formatPercentPoints(totals.rssBudgetPercent, locale) })}</Mono>
        <span style={{ width: 48 }}>
          <Bar value={budget} label={t('statusbar.memory', { percent: '' })} />
        </span>
      </span>

      <span>
        {t('statusbar.pressure', { level: t(`pressure.${totals.pressureLevel}` as MessageKey) })}
      </span>
    </>
  );
}

/**
 * The health indicator (both builds): whether the agents are running well, in
 * one glance — a dot, how many agents run and the memory they use; yellow or
 * red with the number of open alerts when something needs a look; a grey dot
 * and a muted "· 1 snoozed" when the only alerts are dismissed ones (the trace
 * of a known problem never vanishes). A click opens the health popover.
 */
export function HealthIndicator() {
  const { t, locale } = useI18n();
  const snapshot = useAppStore((s) => s.health.snapshot);
  const open = useAppStore((s) => s.health.popoverOpen);
  const setOpen = useAppStore((s) => s.setHealthPopover);
  const text = indicatorText(snapshot, locale);
  return (
    <button
      type="button"
      className="health-indicator"
      data-testid="health"
      data-level={text.level}
      aria-expanded={open}
      title={t('health.indicator.title')}
      onClick={() => setOpen(!open)}
    >
      <StatusDot status={text.dot} pulse={false} />
      <span>{text.summary}</span>
      {text.badge ? <span className="health-badge">{text.badge}</span> : null}
      {text.snoozed ? (
        <span className="health-snoozed-count" data-testid="health-snoozed-count">
          {text.snoozed}
        </span>
      ) : null}
    </button>
  );
}

export function StatusBar() {
  const lanes = experimentalLanes();
  const popoverOpen = useAppStore((s) => s.health.popoverOpen);
  return (
    <>
      <footer className="statusbar">
        {lanes ? <LaneInventory /> : <div className="grow" />}
        <HealthIndicator />
        <ConnectionIndicator />
      </footer>
      {popoverOpen ? <HealthPopover /> : null}
    </>
  );
}
