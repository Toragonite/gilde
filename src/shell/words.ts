/**
 * The words and names every overview surface shares — sidebar rows, the
 * Needs-you list, Home and the palette — so one session reads the same
 * everywhere (ux-ia §2.5 row grammar, §4.2 state words, §2.9 ownership words).
 *
 * Pure functions over the World mirror and a `t`; no React.
 */

import type { MessageKey, MessageParams } from '../i18n';
import type { ErrorSub } from '../lib/gen/ErrorSub';
import type { AttentionItem } from '../lib/gen/AttentionItem';
import type { Control } from '../lib/gen/Control';
import type { Locale } from '../lib/gen/Locale';
import type { Origin } from '../lib/gen/Origin';
import type { Session } from '../lib/gen/Session';
import { formatDuration } from '../lib/format';
import type { GlyphKind } from '../components/StateGlyph';
import type { WorldState } from '../store/worldSlice';
import { sessionOf } from '../store/worldSlice';

export type T = (key: MessageKey, params?: MessageParams) => string;

/**
 * `paused_until` value of an open-ended pause (`attention::PAUSED_UNTIL_RESUME`,
 * contract A16.2): the largest JavaScript `Date`. Any value at or above it
 * reads "until you resume"; `null` means not paused.
 */
export const PAUSED_UNTIL_RESUME = 8_640_000_000_000_000;

/** A state, or the shape of one (a queue reason carries the error sub-case too). */
export interface StateLike {
  kind: GlyphKind;
  sub?: ErrorSub;
}

/** The state word key of an activity (ux-ia §4.2; error sub-cases by kind). */
export function stateKey(state: StateLike): MessageKey {
  switch (state.kind) {
    case 'starting':
      return 'state.starting';
    case 'working':
      return 'state.working';
    case 'needsApproval':
      return 'state.needsApproval';
    case 'needsAnswer':
      return 'state.needsAnswer';
    case 'error':
      switch (state.sub?.kind) {
        case 'usageLimit':
          return 'state.error.usageLimit';
        case 'signInNeeded':
          return 'state.error.signIn';
        case 'didntStart':
          return 'state.error.didntStart';
        case 'agentError':
          return 'state.error.agent';
      }
      return 'state.error';
    case 'crashed':
      return 'state.crashed';
    case 'noProgress':
      return 'state.noProgress';
    case 'newReply':
      return 'state.newReply';
    case 'ready':
      return 'state.ready';
    case 'ended':
      return 'state.ended';
    case 'stopped':
      return 'state.stopped';
    case 'statusUnknown':
      return 'state.unknown';
    case 'leftover':
      return 'state.leftover';
  }
}

/** The state word of a session, with "likely" when the evidence is weak (ux-ia §4.4). */
export function stateWord(t: T, session: Pick<Session, 'activity'>): string {
  const word = t(stateKey(session.activity.state));
  return session.activity.confidence === 'likely' ? t('state.likely', { state: word }) : word;
}

/** The glyph of a session's state. */
export function glyphOf(session: Pick<Session, 'activity'>): GlyphKind {
  return session.activity.state.kind;
}

const ORIGIN_KEYS: Readonly<Record<Origin, MessageKey>> = {
  terminal: 'origin.terminal',
  vsCode: 'origin.vscode',
  itermTeam: 'origin.itermTeam',
  tmuxTeam: 'origin.tmuxTeam',
  terminalTeam: 'origin.terminalTeam',
  background: 'origin.background',
  headless: 'origin.headless',
  orchestrator: 'origin.orchestrator',
  otherGilde: 'origin.otherGilde',
  unknown: 'origin.unknown',
};

/** The owner word of a watched session ("VS Code", "Terminal" …); null when In Gilde or not running. */
export function ownerWord(t: T, control: Control): string | null {
  if (control.kind !== 'watching') return null;
  return t(ORIGIN_KEYS[control.origin] ?? 'origin.unknown');
}

/** The owner a "Go to {owner}" names: the app's display name when known, else the origin word. */
export function ownerApp(t: T, control: Control): string | null {
  if (control.kind !== 'watching') return null;
  return control.owner?.appName || ownerWord(t, control);
}

const timeFormats = new Map<Locale, Intl.DateTimeFormat>();

/** `13:02` — hours and minutes, 24 h. */
export function clockTime(at: number, locale: Locale): string {
  if (!Number.isFinite(at)) return '—';
  let f = timeFormats.get(locale);
  if (!f) {
    f = new Intl.DateTimeFormat(locale, { hour: '2-digit', minute: '2-digit', hourCycle: 'h23' });
    timeFormats.set(locale, f);
  }
  return f.format(new Date(at));
}

/**
 * A row title (ux-ia §2.5): the vendor's title, else "untitled · 13:02".
 * Titles are content held in memory only; they are never persisted here.
 */
export function sessionTitle(t: T, session: Session, locale: Locale): string {
  const title = session.title?.trim();
  if (title) return title;
  const at = session.startedAt ?? session.lastActivityAt;
  return t('sidebar.untitled', { time: at === null ? '—' : clockTime(at, locale) });
}

/** The member name of a team session, if it is one. */
export function memberName(world: WorldState, session: Session): string | null {
  if (!session.team) return null;
  return world.members.get(session.team.memberKey)?.name ?? null;
}

/** The name a team row and a member item use for the team. */
export function teamName(world: WorldState, session: Session): string | null {
  if (!session.team) return null;
  return world.teams.get(session.team.teamKey)?.name ?? null;
}

/**
 * The name of a session in the queue and on Home: a team member reads
 * "db-agent · auth-split" (ux-ia §2.5), anything else its title.
 */
export function displayName(t: T, world: WorldState, session: Session, locale: Locale): string {
  const member = memberName(world, session);
  const team = teamName(world, session);
  if (member && team) return t('sidebar.member', { member, team });
  return sessionTitle(t, session, locale);
}

/** The name of a queue item ("leftover processes" for the aggregated band-6 item). */
export function itemName(t: T, world: WorldState, item: AttentionItem, locale: Locale): string {
  if (item.subject.kind === 'leftovers') return t('state.leftover');
  const session = sessionOf(world, item.subject.uid);
  return session ? displayName(t, world, session, locale) : t('common.unknown');
}

/** The glyph of a queue item. */
export function itemGlyph(world: WorldState, item: AttentionItem): GlyphKind {
  if (item.subject.kind === 'leftovers') return 'leftover';
  const session = sessionOf(world, item.subject.uid);
  if (session) return session.activity.state.kind;
  switch (item.reason.kind) {
    case 'needsApproval':
      return 'needsApproval';
    case 'needsAnswer':
      return 'needsAnswer';
    case 'error':
      return 'error';
    case 'crashed':
      return 'crashed';
    case 'noProgress':
      return 'noProgress';
    case 'newReply':
      return 'newReply';
    case 'leftovers':
      return 'leftover';
  }
}

/** The state word of a queue item. */
export function itemStateWord(t: T, world: WorldState, item: AttentionItem): string {
  if (item.subject.kind === 'leftovers') return t('state.leftover');
  const session = sessionOf(world, item.subject.uid);
  if (session) return stateWord(t, session);
  const reason = item.reason;
  switch (reason.kind) {
    case 'error':
      return t(stateKey({ kind: 'error', sub: reason.sub }));
    case 'noProgress':
      return t('state.noProgress');
    case 'leftovers':
      return t('state.leftover');
    default:
      return t(stateKey({ kind: reason.kind }));
  }
}

/**
 * How long something has waited, short: "방금"/"now" under a minute, else the
 * largest unit ("4분", "1 hr"). `waitedMs` is the core's uptime-based age at
 * publish time; the caller adds what elapsed since it arrived.
 */
export function waitAge(t: T, ms: number, locale: Locale): string {
  if (!Number.isFinite(ms) || ms < 60_000) return t('shell.justNow');
  return formatDuration(Math.floor(ms / 60_000) * 60_000, locale);
}

/** Live: has a process (In Gilde or Watching). */
export function isLive(session: Pick<Session, 'control'>): boolean {
  return session.control.kind !== 'notRunning';
}

/** Running count (ux-ia §2.5 Counts: sessions, so each team member counts once). */
export function runningCount(world: WorldState): number {
  let n = 0;
  for (const s of world.sessions.values()) if (isLive(s)) n++;
  return n;
}

/** Projects with at least one live session. */
export function liveProjectCount(world: WorldState): number {
  const ids = new Set<string>();
  for (const s of world.sessions.values()) if (isLive(s) && s.projectId) ids.add(s.projectId);
  return ids.size;
}

/** Total agent memory of live sessions (bytes), or null when nothing reports it. */
export function liveMemory(world: WorldState): number | null {
  let total = 0;
  let any = false;
  for (const s of world.sessions.values()) {
    if (isLive(s) && s.memoryBytes !== null) {
      total += s.memoryBytes;
      any = true;
    }
  }
  return any ? total : null;
}
