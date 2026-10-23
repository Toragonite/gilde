/**
 * Accounts & health as pure functions over the World mirror (ux-ia §3.15,
 * §5.8, §5.9; contract §3.4.3, §3.4.10, A10.4/A19.5, A24.1/A24.8).
 *
 * Rules pinned here:
 *  - spend is "unknown" when no producer reported today, never 0 (A10.4); a
 *    team's figure adds only what is known and says "at least" when a member
 *    is unknown;
 *  - a quota share outside 0..=100 is absent, never clamped (§3.4.3);
 *  - memory unknown stays out of every sum (never 0-filled);
 *  - quota and sign-in state come from the store's `Account` (A24.1).
 */

import type { Account } from '../lib/gen/Account';
import type { AccountId } from '../lib/gen/AccountId';
import type { ProcessRecord } from '../lib/gen/ProcessRecord';
import type { QuotaWindow } from '../lib/gen/QuotaWindow';
import type { Session } from '../lib/gen/Session';
import type { SessionUid } from '../lib/gen/SessionUid';
import type { TeamKey } from '../lib/gen/TeamKey';
import type { UsageToday } from '../lib/gen/UsageToday';
import type { Vendor } from '../lib/gen/Vendor';
import { VENDORS } from '../lib/vendor';
import type { WorldState } from '../store/worldSlice';

/** The per-session memory figure turns `--warn` above this (ux-ia §3.15, R1 §2). */
export const MEM_WARN_BYTES = 4 * 1024 ** 3;
/** Rows "By session" shows before "Show all". */
export const HEALTH_ROWS = 8;
/** A quota window at or above this share is near its limit (ux-ia §5.8). */
export const NEAR_LIMIT_PCT = 90;

// ---------------------------------------------------------------------------
// Accounts
// ---------------------------------------------------------------------------

/** A share the UI may draw: 0..=100, else absent (never clamped). */
export function validPct(pct: number | null | undefined): number | null {
  return typeof pct === 'number' && Number.isFinite(pct) && pct >= 0 && pct <= 100 ? pct : null;
}

/** The accounts the Accounts list shows, by vendor in badge order; orchestrator accounts go to their own section. */
export function accountsByVendor(accounts: Iterable<Account>): { vendor: Vendor; accounts: Account[] }[] {
  const out: { vendor: Vendor; accounts: Account[] }[] = [];
  const list = [...accounts].filter((a) => a.source !== 'orchestrator');
  for (const vendor of VENDORS) {
    const of = list
      .filter((a) => a.vendor === vendor)
      .sort((a, b) => Number(b.isDefault) - Number(a.isDefault) || Number(b.usableForNew) - Number(a.usableForNew) || a.label.localeCompare(b.label));
    if (of.length > 0) out.push({ vendor, accounts: of });
  }
  return out;
}

export function orchestratorAccounts(accounts: Iterable<Account>): Account[] {
  return [...accounts].filter((a) => a.source === 'orchestrator').sort((a, b) => a.vendor.localeCompare(b.vendor) || a.label.localeCompare(b.label));
}

/** The latest quota reading among the accounts (the header's "Updated …"). */
export function lastQuotaFetch(accounts: Iterable<Account>): number | null {
  let at: number | null = null;
  for (const a of accounts) {
    const f = a.quota?.fetchedAt;
    if (typeof f === 'number' && Number.isFinite(f) && f > 0) at = at === null ? f : Math.max(at, f);
  }
  return at;
}

/** Windows in a stable order: 5-hour, week, then the others. */
export function orderedWindows(windows: readonly QuotaWindow[]): QuotaWindow[] {
  const rank = { fiveHour: 0, week: 1, other: 2 } as const;
  return [...windows].sort((a, b) => rank[a.kind] - rank[b.kind]);
}

/** A config folder for display: the home folder reads `~`. */
export function displayDir(path: string | null): string | null {
  if (!path) return null;
  return path.replace(/^\/Users\/[^/]+(?=\/|$)/, '~');
}

// ---------------------------------------------------------------------------
// Spend today (A10.4)
// ---------------------------------------------------------------------------

/** The local calendar day `YYYY-MM-DD` of an instant. */
export function localDay(at: number): string {
  const d = new Date(at);
  const pad = (n: number) => String(n).padStart(2, '0');
  return `${d.getFullYear()}-${pad(d.getMonth() + 1)}-${pad(d.getDate())}`;
}

/** Today's tokens (input + output): null when no producer reported today or both are absent. */
export function tokensToday(usage: UsageToday | null | undefined, today: string): number | null {
  if (!usage || usage.day !== today) return null;
  const parts = [usage.inputTokens, usage.outputTokens].filter((n): n is number => typeof n === 'number' && Number.isFinite(n) && n >= 0);
  if (parts.length === 0) return null;
  return parts.reduce((a, b) => a + b, 0);
}

/** Today's cost in micro-USD, only when the vendor reported one. */
export function costToday(usage: UsageToday | null | undefined, today: string): number | null {
  if (!usage || usage.day !== today) return null;
  return typeof usage.costMicros === 'number' && Number.isFinite(usage.costMicros) && usage.costMicros >= 0 ? usage.costMicros : null;
}

export interface SpendRow {
  key: string;
  uid: SessionUid | null;
  teamKey: TeamKey | null;
  /** The session (a team's lead, else its first member) whose name the row shows. */
  session: Session;
  accountId: AccountId | null;
  /** Known tokens, or null = unknown (never 0 for unknown). */
  tokens: number | null;
  /** Some members are unknown: the figure is a lower bound. */
  partial: boolean;
  costMicros: number | null;
  members: number;
}

function visible(s: Session): boolean {
  return !s.mergedInto;
}

/**
 * Rows of "Spent today by session": every running session and every session
 * that reported spend today; a team is one row (ux-ia §3.15). Known figures
 * first, largest first; unknown last.
 */
export function spendRows(world: Pick<WorldState, 'sessions' | 'teams'>, today: string): SpendRow[] {
  const solo: SpendRow[] = [];
  const teams = new Map<TeamKey, Session[]>();
  for (const s of world.sessions.values()) {
    if (!visible(s)) continue;
    const live = s.control.kind !== 'notRunning';
    if (!live && tokensToday(s.usageToday, today) === null) continue;
    if (s.team) {
      const list = teams.get(s.team.teamKey) ?? [];
      list.push(s);
      teams.set(s.team.teamKey, list);
      continue;
    }
    solo.push({
      key: s.uid,
      uid: s.uid,
      teamKey: null,
      session: s,
      accountId: s.accountId,
      tokens: tokensToday(s.usageToday, today),
      partial: false,
      costMicros: costToday(s.usageToday, today),
      members: 1,
    });
  }
  const rows = [...solo];
  for (const [teamKey, members] of teams) {
    let known = 0;
    let sum = 0;
    let cost: number | null = null;
    for (const m of members) {
      const n = tokensToday(m.usageToday, today);
      if (n !== null) {
        known++;
        sum += n;
      }
      const c = costToday(m.usageToday, today);
      if (c !== null) cost = (cost ?? 0) + c;
    }
    const lead = members.find((m) => m.team?.role === 'lead') ?? (members[0] as Session);
    rows.push({
      key: `team:${teamKey}`,
      uid: null,
      teamKey,
      session: lead,
      accountId: lead.accountId,
      tokens: known > 0 ? sum : null,
      partial: known > 0 && known < members.length,
      costMicros: cost,
      members: members.length,
    });
  }
  return rows.sort((a, b) => {
    if (a.tokens === null && b.tokens === null) return a.key.localeCompare(b.key);
    if (a.tokens === null) return 1;
    if (b.tokens === null) return -1;
    return b.tokens - a.tokens;
  });
}

// ---------------------------------------------------------------------------
// Health
// ---------------------------------------------------------------------------

export interface HealthRow {
  key: string;
  uid: SessionUid | null;
  teamKey: TeamKey | null;
  session: Session;
  /** Known bytes; null = unknown (stays out of every sum). */
  memory: number | null;
  partial: boolean;
  startedAt: number | null;
  /** Processes a team row stands for. */
  processes: number;
}

/** Running sessions by memory, a team grouped under its lead (ux-ia §3.15 "By session"). */
export function healthRows(world: Pick<WorldState, 'sessions' | 'teams'>): HealthRow[] {
  const rows: HealthRow[] = [];
  const teams = new Map<TeamKey, Session[]>();
  for (const s of world.sessions.values()) {
    if (!visible(s) || s.control.kind === 'notRunning') continue;
    if (s.team) {
      const list = teams.get(s.team.teamKey) ?? [];
      list.push(s);
      teams.set(s.team.teamKey, list);
      continue;
    }
    rows.push({ key: s.uid, uid: s.uid, teamKey: null, session: s, memory: s.memoryBytes, partial: false, startedAt: s.startedAt, processes: 1 });
  }
  for (const [teamKey, members] of teams) {
    let known = 0;
    let sum = 0;
    for (const m of members) {
      if (m.memoryBytes !== null) {
        known++;
        sum += m.memoryBytes;
      }
    }
    const lead = members.find((m) => m.team?.role === 'lead') ?? (members[0] as Session);
    const started = world.teams.get(teamKey)?.startedAt ?? lead.startedAt;
    rows.push({
      key: `team:${teamKey}`,
      uid: null,
      teamKey,
      session: lead,
      memory: known > 0 ? sum : null,
      partial: known > 0 && known < members.length,
      startedAt: started,
      processes: members.length,
    });
  }
  return rows.sort((a, b) => {
    if (a.memory === null && b.memory === null) return a.key.localeCompare(b.key);
    if (a.memory === null) return 1;
    if (b.memory === null) return -1;
    return b.memory - a.memory;
  });
}

/** The leftover processes (§3.4.10 `leftover`), largest first. */
export function leftoverList(world: Pick<WorldState, 'leftovers'>): ProcessRecord[] {
  return [...world.leftovers.values()].filter((p) => p.leftover !== null).sort((a, b) => b.rssBytes - a.rssBytes || a.key.localeCompare(b.key));
}
