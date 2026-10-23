/**
 * History as pure functions (ux-ia §3.14; contract §4.2.8, A7.5).
 *
 *  - query ids (`^[a-z0-9-]{1,32}$`, a newer one supersedes older ones);
 *  - the "When" filter as a `from` bound (never `from > to`, A7.5);
 *  - day groups (Today, Yesterday, Earlier this week, Older);
 *  - what `⏎` does on a row: open a session that runs in Gilde, continue
 *    (resume) an ended one, continue a copy (fork) of one that runs
 *    elsewhere, or nothing when the core offers no way to continue;
 *  - the row's title: the vendor's title, else "{vendor} conversation · date"
 *    (cursor has no titles, C6), else "Untitled".
 */

import type { MessageKey, MessageParams } from '../i18n';
import type { HistoryRow } from '../lib/gen/HistoryRow';
import type { Locale } from '../lib/gen/Locale';
import type { Origin } from '../lib/gen/Origin';
import type { QueryId } from '../lib/gen/QueryId';
import type { Session } from '../lib/gen/Session';
import { vendorBadge } from '../lib/vendor';
import { clockTime } from '../shell/words';

export type T = (key: MessageKey, params?: MessageParams) => string;

/** Rows per `history_query` page (1..=100). */
export const PAGE = 100;
/** While titles are still being indexed the list re-asks this often (the cold start). */
export const COLD_START_POLL_MS = 1500;
/** Search text cap (contract §4.2.8: 0..=256 chars). */
export const MAX_TEXT = 256;

let counter = 0;

/** A fresh `QueryId`. */
export function newQueryId(): QueryId {
  counter = (counter + 1) % 1_000_000;
  return `q-${counter.toString(36)}-${Math.floor(Math.random() * 0x1000000).toString(36)}` as QueryId;
}

export type When = 'any' | 'today' | 'week' | 'month';
export const WHENS: readonly When[] = ['any', 'today', 'week', 'month'];
export const WHEN_KEY: Readonly<Record<When, MessageKey>> = {
  any: 'history.when.any',
  today: 'history.when.today',
  week: 'history.when.week',
  month: 'history.when.month',
};

const DAY = 24 * 3600_000;

function startOfDay(at: number): number {
  const d = new Date(at);
  d.setHours(0, 0, 0, 0);
  return d.getTime();
}

/** The `from` bound of a "When" choice (`to` stays open). */
export function fromOf(when: When, now: number): number | null {
  switch (when) {
    case 'any':
      return null;
    case 'today':
      return startOfDay(now);
    case 'week':
      return startOfDay(now) - 6 * DAY;
    case 'month':
      return startOfDay(now) - 29 * DAY;
  }
}

/** The search text as sent: NFC, at most 256 characters. */
export function queryText(text: string): string {
  return Array.from(text.normalize('NFC')).slice(0, MAX_TEXT).join('');
}

export type Group = 'today' | 'yesterday' | 'week' | 'older';

/** The instant a row is filed under. */
export function rowTime(row: Pick<HistoryRow, 'startedAt' | 'endedAt'>): number | null {
  return row.startedAt ?? row.endedAt ?? null;
}

export function groupOf(at: number | null, now: number): Group {
  if (at === null) return 'older';
  const today = startOfDay(now);
  if (at >= today) return 'today';
  if (at >= today - DAY) return 'yesterday';
  if (at >= today - 6 * DAY) return 'week';
  return 'older';
}

/** Consecutive runs of rows in the same group, in the order the core sent them. */
export function groupRows<R extends Pick<HistoryRow, 'startedAt' | 'endedAt'>>(rows: readonly R[], now: number): { group: Group; rows: R[] }[] {
  const out: { group: Group; rows: R[] }[] = [];
  for (const r of rows) {
    const g = groupOf(rowTime(r), now);
    const last = out[out.length - 1];
    if (last && last.group === g) last.rows.push(r);
    else out.push({ group: g, rows: [r] });
  }
  return out;
}

export const GROUP_KEY: Readonly<Record<Group, MessageKey>> = {
  today: 'history.group.today',
  yesterday: 'history.group.yesterday',
  week: 'history.group.week',
  older: 'history.group.older',
};

export const ORIGIN_KEY: Readonly<Record<Origin, MessageKey>> = {
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

/** The origins the "Where" filter offers; the orchestrator only when that layer is present (P8). */
export function originChoices(orchestrator: boolean): Origin[] {
  const all: Origin[] = ['terminal', 'vsCode', 'itermTeam', 'tmuxTeam', 'terminalTeam', 'background', 'headless', 'otherGilde'];
  return orchestrator ? [...all, 'orchestrator'] : all;
}

function dateOf(at: number, locale: Locale): string {
  return new Intl.DateTimeFormat(locale, { month: 'short', day: 'numeric' }).format(new Date(at));
}

/** The row's title (content, memory only). */
export function rowTitle(t: T, row: HistoryRow, locale: Locale): string {
  const title = row.title?.trim();
  if (title) return title;
  const at = rowTime(row);
  if (row.label === 'vendorDate' && at !== null) return t('history.vendorDate', { vendor: vendorBadge(row.vendor), date: dateOf(at, locale) });
  return t('history.untitled');
}

/** "09:12–10:40", "since 11:58", with the date when it is not today. */
export function rowWhen(t: T, row: HistoryRow, locale: Locale, now: number): string {
  const start = row.startedAt;
  const end = row.endedAt;
  const anchor = start ?? end;
  const prefix = anchor !== null && groupOf(anchor, now) !== 'today' ? `${dateOf(anchor, locale)} ` : '';
  if (row.running && start !== null) return `${prefix}${t('history.since', { time: clockTime(start, locale) })}`;
  if (start !== null && end !== null) return `${prefix}${clockTime(start, locale)}–${clockTime(end, locale)}`;
  if (anchor !== null) return `${prefix}${clockTime(anchor, locale)}`;
  return '—';
}

export type RowAction = { kind: 'open' } | { kind: 'resume' } | { kind: 'fork' } | { kind: 'none' };

/**
 * What `⏎` does (ux-ia §3.14 "Row actions"): a session that runs in Gilde
 * opens; one that runs elsewhere continues as a copy; an ended one resumes;
 * cursor/agy rows continue only when the core verified the id on disk (it
 * then lists `resume`).
 */
export function rowAction(row: Pick<HistoryRow, 'running' | 'continueOptions'>, session: Pick<Session, 'control'> | null): RowAction {
  if (session && session.control.kind === 'inGilde') return { kind: 'open' };
  const running = row.running || (session !== null && session.control.kind === 'watching');
  if (!running && row.continueOptions.includes('resume')) return { kind: 'resume' };
  if (row.continueOptions.includes('fork')) return { kind: 'fork' };
  return { kind: 'none' };
}

export const ACTION_KEY: Readonly<Record<Exclude<RowAction['kind'], 'none'>, MessageKey>> = {
  open: 'action.openInGilde',
  resume: 'action.continue',
  fork: 'action.continueCopy',
};
