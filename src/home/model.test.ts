/**
 * Focus Home's model [M15, contract §8.2]: the focus card is rank 0 (or the
 * held item), Next is the rest of bands 1–2 (≤ 5 then "n more"), chips count
 * bands 3–6 — and focus + next + chips always add up to the visible queue.
 */

import type { AttentionItem } from '../lib/gen/AttentionItem';
import type { EpisodeId } from '../lib/gen/EpisodeId';
import type { MemberKey } from '../lib/gen/MemberKey';
import type { Team } from '../lib/gen/Team';
import type { TeamKey } from '../lib/gen/TeamKey';
import { emptyWorld } from '../store/worldSlice';
import type { WorldState } from '../store/worldSlice';
import { assert, assertDeepEqual, assertEqual, property, test } from '../test/harness';
import { queueItem } from '../test/fixtures';
import { pausedText } from '../sidebar/SidebarFooter';
import { translate } from '../i18n';
import type { MessageKey, MessageParams } from '../i18n';
import { PAUSED_UNTIL_RESUME } from '../shell/words';
import { NEXT_LIMIT, accountName, chipCount, quotaWarnings, splitQueue, teamLines, visibleNext } from './model';

const leftovers = (count: number): AttentionItem => ({
  key: 'q_leftovers',
  band: 6,
  subject: { kind: 'leftovers', count, bytes: count * 1_000_000 },
  reason: { kind: 'leftovers' },
  since: 0,
  waitedMs: 0,
  episode: 'e_leftovers' as EpisodeId,
  laterUntil: null,
  primary: 'cleanUp',
  watched: false,
});

test('M15: the focus card is rank 0; Next is the rest of bands 1–2; chips are bands 3–6', () => {
  const items = [queueItem(1, 1), queueItem(2, 1), queueItem(3, 2), queueItem(4, 3), queueItem(5, 5), queueItem(6, 5), leftovers(3)];
  const split = splitQueue(items, items[0]?.key ?? null);
  assertDeepEqual(split.next.map((i) => i.key), [items[1]?.key, items[2]?.key], 'Next keeps rank order');
  assertDeepEqual(split.chips.map((c) => c.band), [3, 5, 6], 'one chip per band that has items');
  assertDeepEqual(split.chips.map((c) => c.items.length), [1, 2, 1], 'items per chip');
  assertEqual(chipCount(split.chips[2] as never), 3, 'the leftover chip counts processes (Q8)');
});

test('M15: a held focus item leaves the rest in rank order (a new #1 waits in Next)', () => {
  const items = [queueItem(9, 1), queueItem(1, 1), queueItem(2, 2)];
  const split = splitQueue(items, items[1]?.key ?? null);
  assertDeepEqual(split.next.map((i) => i.key), [items[0]?.key, items[2]?.key], 'the newcomer is in Next');
});

test('M15 density: Next shows at most 5 rows, then "n more"', () => {
  const rows = Array.from({ length: 8 }, (_, i) => i);
  const folded = visibleNext(rows, false);
  assertEqual(folded.shown.length, NEXT_LIMIT, '5 rows');
  assertEqual(folded.more, 3, '3 more');
  assertEqual(visibleNext(rows, true).shown.length, 8, 'expanded shows all');
  assertEqual(visibleNext(rows.slice(0, 5), false).more, 0, 'exactly five needs no "more"');
});

test('counts equal the queue: focus + Next + chip items = the visible queue, for any queue', async () => {
  await property(
    'split covers the queue',
    (r) => {
      const n = r.int(0, 30);
      const items = Array.from({ length: n }, (_, i) => queueItem(i + 1, r.int(1, 5)));
      if (r.chance(0.3)) items.push(leftovers(r.int(1, 5)));
      items.sort((a, b) => a.band - b.band);
      const focus = items.length === 0 ? null : r.chance(0.8) ? (items[0]?.key ?? null) : (items[r.int(0, items.length - 1)]?.key ?? null);
      return { items, focus };
    },
    ({ items, focus }) => {
      const split = splitQueue(items, focus);
      const shown = (focus === null ? 0 : 1) + split.next.length + split.chips.reduce((n, c) => n + c.items.length, 0);
      assertEqual(shown, items.length, 'every item appears exactly once');
      const keys = new Set([focus, ...split.next.map((i) => i.key), ...split.chips.flatMap((c) => c.items.map((i) => i.key))]);
      assertEqual(keys.size - (focus === null ? 1 : 0), items.length, 'no item twice');
      assert(split.next.every((i) => i.band <= 2), 'Next holds bands 1–2 only');
      assert(split.chips.every((c) => c.items.every((i) => i.band === c.band)), 'a chip holds its band only');
    },
    { runs: 300 },
  );
});

test('teams: one line per live team, bottlenecks by member name', () => {
  const world: WorldState = emptyWorld();
  const key = 't_auth' as TeamKey;
  const memberKey = 'm_db' as MemberKey;
  const teams = new Map(world.teams);
  teams.set(key, {
    key,
    source: 'claudeNative',
    name: 'auth-split',
    rawId: 'session-1',
    projectId: null,
    lead: null,
    leadBinding: 'hosted',
    hosted: true,
    externalHost: null,
    caps: {} as never,
    glyphState: { kind: 'needsApproval' },
    progress: { done: 3, total: 7, inferred: false, approx: false, inFlight: null, bottlenecks: [memberKey] },
    startedAt: 0,
    endedAt: null,
    paneHosting: 'hosted',
    rosterStale: false,
    paneTiles: [],
  });
  const members = new Map(world.members);
  members.set(memberKey, {
    key: memberKey,
    teamKey: key,
    sessionUid: null,
    name: 'db-agent',
    agentId: null,
    role: 'member',
    colour: 'green',
    vendor: 'claude',
    backend: 'pane',
    joinedAt: 0,
    presence: { kind: 'present' },
    pane: null,
    planModeRequired: false,
    currentTask: null,
  });
  const lines = teamLines({ ...world, teams, members });
  assertEqual(lines.length, 1, 'one line');
  assertDeepEqual(lines[0]?.bottlenecks, ['db-agent'], 'bottleneck names');
  const ended = new Map(teams);
  ended.set(key, { ...(teams.get(key) as Team), endedAt: 5 });
  assertEqual(teamLines({ ...world, teams: ended, members }).length, 0, 'an ended team has no line');
});

test('warnings: only accounts at ≥ 90 % of a window, named "vendor label"', () => {
  const world = emptyWorld();
  const accounts = new Map(world.accounts);
  const account = (id: string, label: string, pct: number | null, isDefault = false) => ({
    id: id as never,
    vendor: 'claude' as const,
    configDir: null,
    label,
    isDefault,
    source: 'user' as const,
    usableForNew: true,
    signIn: 'signedIn' as const,
    quota: { windows: [{ kind: 'fiveHour' as const, label: null, usedPct: pct, resetsAt: null }], fetchedAt: 0, stale: false, unavailable: null },
    liveSessions: 0,
  });
  accounts.set('a1' as never, account('a1', 'Work', 95));
  accounts.set('a2' as never, account('a2', 'Personal', 42));
  accounts.set('a3' as never, account('a3', 'Spare', null));
  const warnings = quotaWarnings({ ...world, accounts });
  assertEqual(warnings.length, 1, 'one warning');
  assertEqual(warnings[0]?.pct, 95, 'its share');
  assertEqual(accountName(account('x', 'Work', 1)), 'claude Work', 'labelled account');
  assertEqual(accountName(account('x', 'Default', 1, true)), 'claude', 'the default login is the vendor');
});

test('A16.2: paused_until ≥ PAUSED_UNTIL_RESUME reads "until you resume"; null is not paused', () => {
  const t = (key: MessageKey, params?: MessageParams) => translate('en', key, params);
  assertEqual(PAUSED_UNTIL_RESUME, 8_640_000_000_000_000, 'the core constant');
  assertEqual(pausedText(t, null, 'en'), 'Alerts on', 'not paused');
  assertEqual(pausedText(t, PAUSED_UNTIL_RESUME, 'en'), 'Paused until you resume', 'open-ended');
  assertEqual(pausedText(t, PAUSED_UNTIL_RESUME + 1, 'en'), 'Paused until you resume', 'anything above it too');
  const nine = new Date(2026, 9, 24, 9, 0, 0).getTime();
  assertEqual(pausedText(t, nine, 'en'), 'Paused until 09:00', 'a time');
  const tk = (key: MessageKey, params?: MessageParams) => translate('ko', key, params);
  assertEqual(pausedText(tk, PAUSED_UNTIL_RESUME, 'ko'), '다시 켤 때까지 알림 끔', 'Korean');
});
