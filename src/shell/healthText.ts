/**
 * Health alerts as words — the one place a `HealthAlert` (keys + raw params from
 * Rust) becomes a title and a body. Pure: tested without a DOM.
 *
 * Rust decides every alert and never sends prose (DESIGN §9-9). It sends the
 * i18n keys and the raw values — bytes, names, lists — and this module formats
 * them the way a person reads them: memory in GB (`formatGb`), lists joined
 * with commas. Process ids are never part of the main text; `alertPidsTitle`
 * gives the tooltip that carries them.
 */

import { translate, translateDynamic } from '../i18n';
import type { MessageKey, MessageParams } from '../i18n';
import type {
  HealthAgent,
  HealthAlert,
  HealthLevel,
  HealthMember,
  HealthSnapshot,
  HealthTeam,
  Locale,
  NodeStatus,
} from '../lib/ipc';

const GIB = 1024 * 1024 * 1024;

const gbFormats = new Map<string, Intl.NumberFormat>();

function gbFormat(locale: Locale, digits: 0 | 1): Intl.NumberFormat {
  const key = `${locale}:${digits}`;
  let f = gbFormats.get(key);
  if (!f) {
    f = new Intl.NumberFormat(locale, { minimumFractionDigits: digits, maximumFractionDigits: digits });
    gbFormats.set(key, f);
  }
  return f;
}

/**
 * Memory as a person reads it, base 1024³, the same in both locales — the one
 * GB formatter of the health surface (indicator, popover, alerts and toasts,
 * member rows):
 *
 *   below 0.1 GB          `<0.1 GB` (a 1 MB process never reads "0.0 GB")
 *   0.1 up to 9.9 GB      one decimal: `0.1 GB`, `6.2 GB`, `9.9 GB`
 *   10 GB and more        whole GB: `10 GB`, `123 GB` (9.96 rounds up to `10 GB`)
 *
 * Zero, a negative or a non-finite number reads `<0.1 GB` (never `NaN`).
 */
export function formatGb(bytes: number, locale: Locale): string {
  const value = Number.isFinite(bytes) && bytes > 0 ? bytes / GIB : 0;
  if (value < 0.1) return '<0.1 GB';
  // Decide on the rounded value, so 9.96 is `10 GB`, not `10.0 GB`.
  const tenths = Math.round(value * 10) / 10;
  if (tenths >= 10) return `${gbFormat(locale, 0).format(Math.round(value))} GB`;
  return `${gbFormat(locale, 1).format(tenths)} GB`;
}

function str(value: unknown): string {
  if (typeof value === 'string') return value;
  if (typeof value === 'number' && Number.isFinite(value)) return String(value);
  return '';
}

function num(value: unknown): number | null {
  return typeof value === 'number' && Number.isFinite(value) ? value : null;
}

/** One `top` entry as Rust sends it. */
interface TopEntry {
  label?: unknown;
  rssBytes?: unknown;
}

/** The raw params of an alert → `{placeholder}` values. */
export function alertParams(alert: HealthAlert, locale: Locale): MessageParams {
  const p = alert.params ?? {};
  const out: MessageParams = {};
  for (const [k, v] of Object.entries(p)) {
    if (typeof v === 'string' || (typeof v === 'number' && Number.isFinite(v))) out[k] = v;
  }
  const rss = num(p.rssBytes);
  out.gb = formatGb(rss ?? 0, locale);
  const count = num(p.count);
  if (count !== null) out.n = count;
  if (Array.isArray(p.members)) out.members = p.members.map(str).filter(Boolean).join(', ');
  if (Array.isArray(p.top)) {
    const parts = (p.top as TopEntry[])
      .map((e) => (e && typeof e === 'object' ? `${str(e.label)} ${formatGb(num(e.rssBytes) ?? 0, locale)}`.trim() : ''))
      .filter(Boolean);
    out.top = parts.length > 0 ? parts.join(', ') : '—';
  }
  if (out.team === undefined || out.team === '') out.team = translate(locale, 'health.unknownTeam');
  if (out.members === undefined || out.members === '') out.members = '—';
  if (out.top === undefined) out.top = '—';
  return out;
}

export interface AlertText {
  title: string;
  body: string;
}

/** `{title, body}` of one alert; a key this build does not know degrades safely. */
export function alertText(alert: HealthAlert, locale: Locale): AlertText {
  const params = alertParams(alert, locale);
  return {
    title: translateDynamic(locale, alert.titleKey, params),
    body: translateDynamic(locale, alert.bodyKey, params),
  };
}

/** The tooltip that carries the process ids an alert is about (never the main text). */
export function alertPidsTitle(alert: HealthAlert, locale: Locale): string | undefined {
  const pids = new Set<number>();
  for (const action of alert.actions ?? []) {
    for (const pid of action.pids ?? []) pids.add(pid);
  }
  const pid = num(alert.params?.pid);
  if (pid !== null) pids.add(pid);
  if (pids.size === 0) return undefined;
  return translate(locale, 'health.pids', { pids: [...pids].join(' ') });
}

/** The alerts that count: not dismissed. */
export function activeAlerts(snapshot: HealthSnapshot | null): HealthAlert[] {
  return (snapshot?.alerts ?? []).filter((a) => !a.dismissed);
}

/**
 * The alerts the user dismissed (snoozed). Dismissing quiets an alert for the
 * cooldown; it does not make the condition go away, so these stay on the
 * health surface — muted, with when they come back — never as "all well".
 */
export function snoozedAlerts(snapshot: HealthSnapshot | null): HealthAlert[] {
  return (snapshot?.alerts ?? []).filter((a) => a.dismissed);
}

/** When a snoozed alert comes back, or null when the backend did not say. */
export function snoozedUntil(alert: HealthAlert): number | null {
  const until = alert.dismissedUntil;
  return typeof until === 'number' && Number.isFinite(until) && until > 0 ? until : null;
}

const clockFormats = new Map<string, Intl.DateTimeFormat>();

/**
 * Local wall-clock time `HH:MM` (24 h), with the date when it is not today —
 * the rule the app's other clock times follow (the worker accounts' "as of").
 */
export function formatClock(at: number, locale: Locale, now: number = Date.now()): string {
  if (!Number.isFinite(at)) return '—';
  const sameDay = new Date(at).toDateString() === new Date(now).toDateString();
  const key = `${locale}:${sameDay}`;
  let f = clockFormats.get(key);
  if (!f) {
    f = new Intl.DateTimeFormat(locale, {
      ...(sameDay ? {} : { month: 'numeric', day: 'numeric' }),
      hour: '2-digit',
      minute: '2-digit',
      hourCycle: 'h23',
    });
    clockFormats.set(key, f);
  }
  return f.format(new Date(at));
}

/** What the popover's headline is about. */
export type OverallKind = 'unknown' | 'ok' | 'attention' | 'action' | 'snoozed';

export interface OverallReading {
  kind: OverallKind;
  /** The headline sentence. */
  text: string;
  /** Its dot: green only when there is no alert at all, grey when the only ones are snoozed. */
  dot: NodeStatus;
}

/**
 * The popover's headline. "Everything is running well" ONLY when there is no
 * alert at all — a dismissed (snoozed) alert is still a problem, so with only
 * snoozed alerts the line is neutral and says how many and until when (the
 * earliest comeback). Active alerts decide the line as before.
 */
export function overallReading(snapshot: HealthSnapshot | null, locale: Locale, now: number = Date.now()): OverallReading {
  if (!snapshot || snapshot.at === 0) {
    return { kind: 'unknown', text: translate(locale, 'health.overall.unknown'), dot: 'idle' };
  }
  const level = indicatorLevel(snapshot);
  if (level === 'action') return { kind: 'action', text: translate(locale, 'health.overall.action'), dot: 'error' };
  if (level === 'attention') {
    return { kind: 'attention', text: translate(locale, 'health.overall.attention'), dot: 'stalled' };
  }
  const snoozed = snoozedAlerts(snapshot);
  if (snoozed.length === 0) return { kind: 'ok', text: translate(locale, 'health.overall.ok'), dot: 'ok' };
  const times = snoozed.map(snoozedUntil).filter((t): t is number => t !== null);
  const n = snoozed.length;
  let text: string;
  if (times.length === 0) {
    text = translate(locale, 'health.overall.snoozedNoTime', { n });
  } else {
    const time = formatClock(Math.min(...times), locale, now);
    text = translate(locale, n === 1 ? 'health.overall.snoozedOne' : 'health.overall.snoozedMany', { n, time });
  }
  return { kind: 'snoozed', text, dot: 'idle' };
}

/** The level the indicator shows — recomputed from the active alerts, never trusted blindly. */
export function indicatorLevel(snapshot: HealthSnapshot | null): HealthLevel {
  const alerts = activeAlerts(snapshot);
  if (alerts.some((a) => a.severity === 'action')) return 'action';
  if (alerts.length > 0) return 'attention';
  return 'ok';
}

/** The status dot colour of a level. */
export function levelStatus(level: HealthLevel): 'ok' | 'stalled' | 'error' {
  return level === 'action' ? 'error' : level === 'attention' ? 'stalled' : 'ok';
}

export interface IndicatorText {
  level: HealthLevel;
  /** "에이전트 3개 실행 중 · 2.4 GB" */
  summary: string;
  /** "주의 1" / "조치 필요 1", or null when no alert is active. */
  badge: string | null;
  /**
   * "· 알림 끔 1" — only when no alert is active but some are snoozed: the
   * trace of a dismissed problem never vanishes from the status bar.
   */
  snoozed: string | null;
  /** The dot: the level's colour; grey (not green) when the only alerts are snoozed. */
  dot: NodeStatus;
}

/** What the status-bar indicator says. */
export function indicatorText(snapshot: HealthSnapshot | null, locale: Locale): IndicatorText {
  const level = indicatorLevel(snapshot);
  const agents = snapshot?.agents.length ?? 0;
  const summary =
    agents === 0
      ? translate(locale, 'health.indicator.none')
      : translate(locale, 'health.indicator.running', {
          n: agents,
          gb: formatGb(snapshot?.memory.agentsRssBytes ?? 0, locale),
        });
  const n = activeAlerts(snapshot).length;
  const badgeKey: MessageKey | null =
    level === 'action' ? 'health.indicator.action' : level === 'attention' ? 'health.indicator.attention' : null;
  const quiet = n === 0 ? snoozedAlerts(snapshot).length : 0;
  return {
    level,
    summary,
    badge: badgeKey ? translate(locale, badgeKey, { n }) : null,
    snoozed: quiet > 0 ? translate(locale, 'health.indicator.snoozed', { n: quiet }) : null,
    dot: quiet > 0 ? 'idle' : levelStatus(level),
  };
}

/** The non-dismissed alerts that concern one team (by id, else by name). */
export function teamAlerts(
  snapshot: HealthSnapshot | null,
  teamId: string | null,
  teamName: string,
): HealthAlert[] {
  return activeAlerts(snapshot).filter((a) => {
    if (teamId !== null && a.teamId === teamId) return true;
    return a.teamId === null && teamName !== '' && str(a.params?.team) === teamName;
  });
}

/** The agents of one alert's clean-up action: exactly its pids, orphans only, never protected. */
export function cleanUpAgents(snapshot: HealthSnapshot | null, pids: readonly number[]): HealthAgent[] {
  const wanted = new Set(pids);
  return (snapshot?.agents ?? []).filter(
    (a) => wanted.has(a.pid) && a.orphan && !(a.row?.tags ?? []).includes('protected'),
  );
}

/** The health view of one team: by team id, else by name. */
export function healthTeamOf(
  snapshot: HealthSnapshot | null,
  teamId: string | null,
  teamName: string,
): HealthTeam | null {
  const teams = snapshot?.teams ?? [];
  return (
    (teamId !== null ? teams.find((t) => t.teamId === teamId) : undefined) ??
    teams.find((t) => t.teamId === null && t.team === teamName) ??
    null
  );
}

/** One member's health (memory, leftover), or null when health says nothing about it. */
export function healthMemberOf(
  snapshot: HealthSnapshot | null,
  teamId: string | null,
  teamName: string,
  member: string,
): HealthMember | null {
  return healthTeamOf(snapshot, teamId, teamName)?.members.find((m) => m.name === member) ?? null;
}
