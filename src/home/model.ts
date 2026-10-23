/**
 * Focus Home as a pure model [M15, contract §8.2].
 *
 * Home = header summary · at most one notice · ONE focus card for queue #1 ·
 * the next items as one-liners (≤ 5, then "n more") · bands 3–6 as count
 * chips · one line per team · "N running · M projects" · one warning line
 * only when something is off · a calm sentence when nothing waits.
 *
 * Everything comes from the queue as delivered (`AttentionQueue`, ranked in
 * Rust): no re-ranking, no recounting. The focus card is rank 0 — or the
 * item the card is holding while the user is on it (`useHeldFocus`) — and
 * the rows and chips are the rest, so focus + next + chips always add up to
 * the visible queue ("counts equal the queue").
 */

import type { Account } from '../lib/gen/Account';
import type { AttentionItem } from '../lib/gen/AttentionItem';
import type { QuotaWindowKind } from '../lib/gen/QuotaWindowKind';
import type { Team } from '../lib/gen/Team';
import type { WorldState } from '../store/worldSlice';
import { membersOfTeam } from '../store/worldSlice';

/** Next rows shown before "n more" [M15]. */
export const NEXT_LIMIT = 5;

/** A quota window at or above this share shows on Home's warning line. */
export const QUOTA_WARN_PCT = 90;

export type ChipBand = 3 | 4 | 5 | 6;

export interface Chip {
  band: ChipBand;
  items: AttentionItem[];
}

export interface QueueSplit {
  /** Bands 1–2 after the focus item, in rank order. */
  next: AttentionItem[];
  /** One chip per band 3–6 that has items (the focus item excluded). */
  chips: Chip[];
}

/** Split the visible queue around the item the focus card shows. */
export function splitQueue(items: readonly AttentionItem[], focusKey: string | null): QueueSplit {
  const rest = items.filter((i) => i.key !== focusKey);
  const next = rest.filter((i) => i.band <= 2);
  const chips: Chip[] = [];
  for (const band of [3, 4, 5, 6] as const) {
    const inBand = rest.filter((i) => i.band === band);
    if (inBand.length > 0) chips.push({ band, items: inBand });
  }
  return { next, chips };
}

/**
 * The number a chip shows: its items, except band 6, whose one aggregated
 * item stands for `count` leftover processes — the chip reads "Leftover
 * processes 3", as the item does (Q8).
 */
export function chipCount(chip: Chip): number {
  if (chip.band !== 6) return chip.items.length;
  return chip.items.reduce((n, i) => n + (i.subject.kind === 'leftovers' ? i.subject.count : 1), 0);
}

/** The rows the Next list shows: all when expanded, else the first `NEXT_LIMIT`. */
export function visibleNext<T>(next: readonly T[], expanded: boolean): { shown: T[]; more: number } {
  if (expanded || next.length <= NEXT_LIMIT) return { shown: [...next], more: 0 };
  return { shown: next.slice(0, NEXT_LIMIT), more: next.length - NEXT_LIMIT };
}

// ---------------------------------------------------------------------------
// Teams
// ---------------------------------------------------------------------------

export interface TeamLine {
  team: Team;
  /** Members currently present. */
  present: number;
  /** Bottleneck member names (Team.progress.bottlenecks). */
  bottlenecks: string[];
}

/** One line per active team: not ended, at least one member present. */
export function teamLines(world: WorldState): TeamLine[] {
  const out: TeamLine[] = [];
  for (const team of world.teams.values()) {
    if (team.endedAt !== null) continue;
    const members = membersOfTeam(world, team.key);
    const present = members.filter((m) => m.presence.kind === 'present').length;
    if (present === 0) continue;
    const names = team.progress.bottlenecks
      .map((key) => world.members.get(key)?.name)
      .filter((n): n is string => typeof n === 'string' && n !== '');
    out.push({ team, present, bottlenecks: names });
  }
  out.sort(
    (a, b) => (a.team.startedAt ?? 0) - (b.team.startedAt ?? 0) || (a.team.key < b.team.key ? -1 : a.team.key > b.team.key ? 1 : 0),
  );
  return out;
}

// ---------------------------------------------------------------------------
// The one warning line
// ---------------------------------------------------------------------------

export interface QuotaWarning {
  account: Account;
  window: QuotaWindowKind;
  label: string | null;
  pct: number;
}

/** Accounts with a quota window at ≥ 90 % (the highest window of each). */
export function quotaWarnings(world: WorldState): QuotaWarning[] {
  const out: QuotaWarning[] = [];
  for (const account of world.accounts.values()) {
    const quota = account.quota;
    if (!quota || quota.unavailable) continue;
    let worst: QuotaWarning | null = null;
    for (const w of quota.windows) {
      if (w.usedPct === null || w.usedPct < QUOTA_WARN_PCT) continue;
      if (!worst || w.usedPct > worst.pct) worst = { account, window: w.kind, label: w.label, pct: w.usedPct };
    }
    if (worst) out.push(worst);
  }
  return out;
}

/** The account as Home names it: "claude Work", or just the vendor for its default login. */
export function accountName(account: Pick<Account, 'vendor' | 'label' | 'isDefault'>): string {
  const label = account.label.trim();
  return label === '' || (account.isDefault && label === 'Default') ? account.vendor : `${account.vendor} ${label}`;
}
