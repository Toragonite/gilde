/**
 * The health popover — a small panel above the status bar, not a page.
 *
 * What the user needs and nothing more: one line that says whether everything
 * is running well; the alerts (with what to do, as buttons); per team, its
 * members with a dot and the memory their processes use; how many dispatch
 * hosts there are. No tables, and no process ids in the text — those are only
 * in tooltips. "Process details" (the full monitor) is a link only where the
 * experimental lanes exist.
 *
 * Dismissing an alert (its toast's ×, or the card's ×) snoozes it; it does not
 * fix anything. So the headline says "Everything is running well" only when
 * there is no alert at all, and a snoozed alert stays listed below the active
 * ones — muted, with when it comes back and the same buttons, so the user can
 * still fix it.
 */

import { useEffect } from 'react';

import { CloseIcon } from '../components/icons';
import { StatusDot } from '../components/StatusDot';
import { experimentalLanes } from '../features';
import { translate } from '../i18n';
import type { MessageKey } from '../i18n';
import { useI18n } from '../i18n/useI18n';
import type {
  HealthAction,
  HealthAlert,
  HealthMemberState,
  HealthTeam,
  Locale,
  NodeStatus,
} from '../lib/ipc';
import { useAppStore } from '../store/useAppStore';
import {
  activeAlerts,
  alertPidsTitle,
  alertText,
  formatClock,
  formatGb,
  indicatorLevel,
  overallReading,
  snoozedAlerts,
  snoozedUntil,
} from './healthText';
import './health.css';

export const ACTION_KEY: Record<HealthAction['kind'], MessageKey> = {
  cleanUp: 'health.action.cleanUp',
  details: 'health.action.details',
  copyCommand: 'health.action.copyCommand',
  showAgents: 'health.action.showAgents',
};

export const MEMBER_STATE_KEY: Record<HealthMemberState, MessageKey> = {
  running: 'health.member.running',
  idle: 'health.member.idle',
  leftover: 'health.member.leftover',
};

export function memberDot(state: HealthMemberState): NodeStatus {
  return state === 'leftover' ? 'orphan' : state === 'running' ? 'working' : 'idle';
}

/** An alert's buttons. `details` inside the popover is the popover itself: not shown there. */
export function AlertActions({
  alert,
  inPopover,
}: {
  alert: HealthAlert;
  inPopover: boolean;
}) {
  const { t } = useI18n();
  const run = useAppStore((s) => s.runHealthAction);
  const actions = (alert.actions ?? []).filter(
    (a) => !(inPopover && (a.kind === 'details' || a.kind === 'showAgents')),
  );
  if (actions.length === 0) return null;
  return (
    <div className="health-alert-actions">
      {actions.map((action, i) => (
        <button
          key={`${action.kind}-${i}`}
          type="button"
          className="btn"
          data-action={action.kind}
          title={action.kind === 'copyCommand' ? action.command : undefined}
          onClick={() => void run(alert, action)}
        >
          {t(ACTION_KEY[action.kind] ?? 'health.action.details')}
        </button>
      ))}
    </div>
  );
}

function AlertCard({ alert, locale }: { alert: HealthAlert; locale: Locale }) {
  const { t } = useI18n();
  const dismiss = useAppStore((s) => s.dismissHealthAlert);
  const text = alertText(alert, locale);
  return (
    <div className="health-alert" data-severity={alert.severity} data-alert-id={alert.id}>
      <div className="health-head">
        <StatusDot status={alert.severity === 'action' ? 'error' : 'stalled'} pulse={false} />
        <span className="strong grow" title={alertPidsTitle(alert, locale)}>
          {text.title}
        </span>
        <button
          type="button"
          className="health-close"
          aria-label={t('health.dismiss')}
          title={t('health.dismiss')}
          onClick={() => void dismiss(alert.id)}
        >
          <CloseIcon className="i sm" />
        </button>
      </div>
      <span>{text.body}</span>
      <AlertActions alert={alert} inPopover />
    </div>
  );
}

/**
 * A snoozed alert: compact and muted — its title, its body on one line, when it
 * comes back, and the same buttons as when it was active. No close button (it is
 * already dismissed) and no "unsnooze": it comes back by itself.
 */
function SnoozedAlert({ alert, locale }: { alert: HealthAlert; locale: Locale }) {
  const { t } = useI18n();
  const text = alertText(alert, locale);
  const until = snoozedUntil(alert);
  return (
    <div className="health-snoozed-alert" data-severity={alert.severity} data-alert-id={alert.id}>
      <div className="health-head">
        <span className="strong ell grow" title={alertPidsTitle(alert, locale)}>
          {text.title}
        </span>
        {until !== null ? (
          <span className="s11 muted health-back-at">{t('health.snoozed.backAt', { time: formatClock(until, locale) })}</span>
        ) : null}
      </div>
      <span className="ell health-snoozed-body" title={text.body}>
        {text.body}
      </span>
      <AlertActions alert={alert} inPopover />
    </div>
  );
}

function TeamBlock({ team, locale }: { team: HealthTeam; locale: Locale }) {
  const { t } = useI18n();
  return (
    <div className="health-team" data-team={team.teamId ?? team.team}>
      <div className="health-head">
        <span className="strong ell grow">{team.team || translate(locale, 'health.unknownTeam')}</span>
        {team.hostAlive ? null : <span className="s11 muted">{t('health.team.hostGone')}</span>}
        {team.rssBytes > 0 ? <span className="health-gb s11">{formatGb(team.rssBytes, locale)}</span> : null}
      </div>
      {team.members.map((m) => (
        <div
          className="health-member"
          key={m.name}
          data-state={m.state}
          title={m.pids.length > 0 ? translate(locale, 'health.pids', { pids: m.pids.join(' ') }) : undefined}
        >
          <StatusDot status={memberDot(m.state)} pulse={false} />
          <span className="ell grow">{m.name}</span>
          <span className="s11 muted health-member-state">{t(MEMBER_STATE_KEY[m.state] ?? 'health.member.idle')}</span>
          {m.pids.length > 0 ? <span className="health-gb s11">{formatGb(m.rssBytes, locale)}</span> : null}
        </div>
      ))}
    </div>
  );
}

export function HealthPopover() {
  const { t, locale } = useI18n();
  const snapshot = useAppStore((s) => s.health.snapshot);
  const setOpen = useAppStore((s) => s.setHealthPopover);
  const setActiveView = useAppStore((s) => s.setActiveView);

  useEffect(() => {
    if (typeof window === 'undefined') return;
    const onKey = (e: KeyboardEvent) => {
      if (e.key === 'Escape') setOpen(false);
    };
    window.addEventListener('keydown', onKey);
    return () => window.removeEventListener('keydown', onKey);
  }, [setOpen]);

  const level = indicatorLevel(snapshot);
  const overall = overallReading(snapshot, locale);
  const alerts = activeAlerts(snapshot);
  const snoozed = snoozedAlerts(snapshot);
  const teams = snapshot?.teams ?? [];
  const others = (snapshot?.agents ?? []).filter((a) => a.attribution.kind !== 'teamMember');
  const othersRss = others.reduce((sum, a) => sum + (a.rssBytes || 0), 0);
  const hosts = snapshot?.hosts ?? [];
  const hostsAttention = hosts.some((h) => h.leadAlive === false);

  return (
    <div className="health-popover" role="dialog" aria-label={t('health.popover.title')} data-testid="health-popover">
      <div className="health-head">
        <span className="strong grow">{t('health.popover.title')}</span>
        <button
          type="button"
          className="health-close"
          aria-label={t('common.close')}
          title={t('common.close')}
          onClick={() => setOpen(false)}
        >
          <CloseIcon className="i sm" />
        </button>
      </div>

      <div className="health-overall" data-level={level} data-overall={overall.kind} data-testid="health-overall">
        <StatusDot status={overall.dot} pulse={false} />
        <span>{overall.text}</span>
      </div>

      {alerts.map((alert) => (
        <AlertCard key={alert.id} alert={alert} locale={locale} />
      ))}

      {snoozed.length > 0 ? (
        <section className="health-snoozed" data-testid="health-snoozed" aria-label={t('health.snoozed.title')}>
          <span className="s11 muted health-snoozed-title">{t('health.snoozed.title')}</span>
          {snoozed.map((alert) => (
            <SnoozedAlert key={alert.id} alert={alert} locale={locale} />
          ))}
        </section>
      ) : null}

      {snapshot && !snapshot.teamsKnown && snapshot.at > 0 ? (
        <span className="s12 muted">{t('health.teams.unknown')}</span>
      ) : teams.length === 0 ? (
        <span className="s12 muted">{t('health.teams.none')}</span>
      ) : (
        teams.map((team) => <TeamBlock key={team.teamId ?? team.team} team={team} locale={locale} />)
      )}

      {others.length > 0 ? (
        <span className="s12 muted" data-testid="health-others">
          {t('health.others', { n: others.length, gb: formatGb(othersRss, locale) })}
        </span>
      ) : null}

      {hosts.length > 0 ? (
        <div className="health-overall s12" data-testid="health-hosts">
          <StatusDot status={hostsAttention ? 'stalled' : 'ok'} pulse={false} />
          <span>{t('health.hosts', { n: hosts.length })}</span>
          <span className="muted">{t(hostsAttention ? 'health.hosts.attention' : 'health.hosts.ok')}</span>
        </div>
      ) : null}

      {experimentalLanes() ? (
        <button
          type="button"
          className="health-link"
          data-testid="health-process-details"
          onClick={() => {
            setOpen(false);
            setActiveView('monitor');
          }}
        >
          {t('health.processDetails')}
        </button>
      ) : null}
    </div>
  );
}
