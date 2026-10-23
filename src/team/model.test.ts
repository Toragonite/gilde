/**
 * Pure Team rules (contract §7.1.3, §7.2.3–§7.2.5, §15.5, §15.6 F-49):
 * a control exists iff the capability allows it; counts are exact, "≈" or
 * unknown, never a 0 that looks like data; inferred items are the board when
 * task tools are absent; "never read" for a closed pane; crashed tiles stay
 * until closed.
 */

import type { Member } from '../lib/gen/Member';
import { session, snapshot, uid } from '../test/fixtures';
import { assert, assertDeepEqual, assertEqual, test } from '../test/harness';
import { emptyWorld, worldFromSnapshot } from '../store/worldSlice';
import type { WorldState } from '../store/worldSlice';
import {
  boardGroups,
  boardKind,
  canAddTask,
  canAskStop,
  canClosePane,
  canTell,
  canTellAll,
  cleanBody,
  deliveryNote,
  flat,
  inferredGroups,
  memberRoute,
  messageCount,
  messageEvent,
  noReport,
  orderedMembers,
  ownershipKey,
  memberTiles,
  panesAvailable,
  taskCount,
} from './model';
import { TK, boardTask, hostedCaps, inferredItem, lead, member, message, mkey, paneSession, team, watchedSession } from './testing';

function worldOf(parts: Parameters<typeof snapshot>[1]): WorldState {
  return worldFromSnapshot(snapshot(1, parts), emptyWorld()).world;
}

const hosted = () =>
  worldOf({
    teams: [team()],
    members: [lead(), member(2, 'api-agent'), member(3, 'db-agent')],
    sessions: [paneSession(1, {}, 'lead'), paneSession(2), paneSession(3)],
  });

test('orderedMembers: the lead first, then join order', () => {
  const w = worldOf({
    teams: [team()],
    members: [member(3, 'late', { joinedAt: 9_000 }), member(2, 'early', { joinedAt: 2_000 }), lead({ joinedAt: 5_000 })],
  });
  assertDeepEqual(orderedMembers(w, TK).map((m) => m.name), ['team-lead', 'early', 'late'], 'order');
});

test('routes: a hosted pane is typed into; others go through the lead only while it runs in Gilde; gone members get none', () => {
  const w = hosted();
  const t = w.teams.get(TK)!;
  assertEqual(memberRoute(w, t, w.members.get(mkey(2))!), 'pane', 'pane member');
  assertEqual(memberRoute(w, t, w.members.get(mkey(1))!), 'self', 'the lead types into its own composer');
  const inProcess = member(4, 'helper', { backend: 'inProcess', pane: null });
  assertEqual(memberRoute(w, t, inProcess), 'lead', 'in-process: via the lead');
  const gone: Member = member(5, 'gone', { presence: { kind: 'gone', reason: 'processExited', at: 1 } });
  assertEqual(memberRoute(w, t, gone), null, 'gone: no route');
  // The lead is watched (an external team): no via-lead route, no prefill.
  const ext = worldOf({
    teams: [team({ hosted: false, externalHost: 'iTerm2', caps: hostedCaps({ actSend: ['viaLead'], actStop: ['viaLead'] }) })],
    members: [lead(), member(2, 'api-agent', { backend: 'inProcess', pane: null })],
    sessions: [watchedSession(1), watchedSession(2)],
  });
  const et = ext.teams.get(TK)!;
  assertEqual(memberRoute(ext, et, ext.members.get(mkey(2))!), null, 'external: nothing to send through');
  assertEqual(canTell(ext, et, ext.members.get(mkey(2))!), false, 'no Tell…');
  assertEqual(canTellAll(ext, et), false, 'no Tell everyone…');
  assertEqual(canAskStop(ext, et, ext.members.get(mkey(2))!), false, 'no Ask the lead to stop');
  assertEqual(ownershipKey(et), 'team.external', 'iTerm2 sentence');
  assertEqual(ownershipKey({ ...et, externalHost: 'tmux' }), 'team.externalTmux', 'tmux sentence');
  assertEqual(ownershipKey({ ...et, source: 'orchestrator' }), 'orchestrator.messageFromLead', 'orchestrator sentence');
});

test('acts: Close pane only for live hosted panes; Ask the lead to stop needs the via-lead route; orchestrator teams act on nothing', () => {
  const w = hosted();
  const t = w.teams.get(TK)!;
  const m2 = w.members.get(mkey(2))!;
  assert(canClosePane(t, m2), 'pane member: Close pane');
  assert(!canClosePane(t, w.members.get(mkey(1))!), 'never the lead');
  assert(canAskStop(w, t, m2), 'Ask the lead to stop');
  assert(!canAskStop(w, t, w.members.get(mkey(1))!), 'not for the lead itself');
  const orch = { ...t, source: 'orchestrator' as const, caps: hostedCaps({ actSend: [], actBroadcast: [], actSpawn: [], actStop: [] }) };
  assert(!canClosePane(orch, m2) && !canTellAll(w, orch) && !canAskStop(w, orch, m2), 'orchestrator: no actions in v1');
  assert(panesAvailable(t) && !panesAvailable({ ...t, paneHosting: 'inProcessFallback' }), 'in-process fallback: no Panes');
});

test('F-49: "Ask the lead to add a task…" only when task tools exist (observe.tasks = Yes) and the lead runs in Gilde', () => {
  const w = hosted();
  const t = w.teams.get(TK)!;
  assertEqual(canAddTask(w, t), false, 'inferred board (2.1.307 default): hidden');
  assertEqual(canAddTask(w, { ...t, caps: hostedCaps({ observeTasks: 'yes' }) }), true, 'task tools: offered');
  const watchedLead = worldOf({ teams: [team()], members: [lead()], sessions: [watchedSession(1)] });
  assertEqual(canAddTask(watchedLead, { ...t, caps: hostedCaps({ observeTasks: 'yes' }) }), false, 'lead not in Gilde: hidden');
});

test('board kind and counts: inferred items are the board; "≈" when inferred from inboxes; no list → unknown, never 0', () => {
  const t = team();
  assertEqual(boardKind(t), 'inferred', '2.1.307 native default');
  assertEqual(boardKind({ ...t, caps: hostedCaps({ observeTasks: 'yes' }) }), 'board', 'task tools');
  assertEqual(boardKind({ ...t, caps: hostedCaps({ observeTasks: 'no' }) }), 'none', 'no board');
  const items = [inferredItem(1, mkey(2), 1), inferredItem(2, mkey(2), 2, 'reported')];
  assertDeepEqual(taskCount(t, items), { kind: 'exact', value: 2 }, 'exact with transcripts');
  assertDeepEqual(taskCount({ ...t, progress: { ...t.progress, approx: true } }, items), { kind: 'approx', value: 2 }, '≈ from inboxes');
  assertDeepEqual(taskCount({ ...t, caps: hostedCaps({ observeTasks: 'no' }) }, []), { kind: 'unknown' }, 'no list: unknown');
  assertDeepEqual(taskCount({ ...t, caps: hostedCaps({ observeTasks: 'yes' }) }, [boardTask(1, 'x', 'deleted')]), { kind: 'exact', value: 0 }, 'deleted never counted');
});

test('message counts are never "no messages": partial → "≈ n" or unknown; exact only when every message is observed (LP-15)', () => {
  const t = team();
  assertDeepEqual(messageCount(t, 0), { kind: 'unknown' }, 'partial, 0 → unknown');
  assertDeepEqual(messageCount(t, 4), { kind: 'approx', value: 4 }, 'partial → ≈');
  assertDeepEqual(messageCount({ ...t, caps: hostedCaps({ observeMessages: 'yes' }) }, 0), { kind: 'exact', value: 0 }, 'fully observed');
  assertDeepEqual(messageCount({ ...t, caps: hostedCaps({ observeMessages: 'no' }) }, 0), { kind: 'unknown' }, 'not observed');
});

test('task groups: In progress · Open · Done on the board; open then reported for inferred items; "no report" after 10 min Ready', () => {
  const groups = boardGroups([boardTask(3, 'c', 'done'), boardTask(1, 'a', 'inProgress'), boardTask(2, 'b', 'open'), boardTask(4, 'gone', 'deleted')]);
  assertDeepEqual(groups.map((g) => g.id), ['inProgress', 'open', 'done'], 'groups');
  assertEqual(groups.flatMap((g) => g.tasks).length, 3, 'deleted hidden');
  const inferred = inferredGroups([inferredItem(2, null, 2, 'reported'), inferredItem(1, null, 1), inferredItem(3, null, 3, 'inProgress')]);
  assertDeepEqual(inferred.map((g) => [g.id, g.tasks.map((x) => x.inferredK)]), [['open', [1, 3]], ['reported', [2]]], 'inferred groups');
  const ready = session(2, { activity: { ...session(2).activity, state: { kind: 'ready' }, since: 0 } });
  assert(noReport(inferredItem(1, mkey(2), 1), ready, 11 * 60 * 1000), 'Ready ≥ 10 min, still open → no report');
  assert(!noReport(inferredItem(1, mkey(2), 1), ready, 5 * 60 * 1000), 'not yet');
  assert(!noReport(inferredItem(1, mkey(2), 1, 'reported'), ready, 60 * 60 * 1000), 'reported items never');
});

test('messages: protocol frames are one system line with names as params (never keys); "never read" for a closed pane (LP-13)', () => {
  const name = (p: { kind: string }) => (p.kind === 'member' ? 'db-agent' : p.kind);
  const idle = messageEvent(message(1, mkey(3), mkey(1), { kind: 'idleNotification', idleReason: 'failed' }), name);
  assertDeepEqual(idle, { key: 'team.msg.idleFailed', params: { who: 'db-agent' } }, 'failed idle');
  assertEqual(messageEvent(message(2, mkey(1), mkey(3)), name), null, 'plain → from → to');
  assertEqual(messageEvent(message(3, mkey(1), mkey(3), { kind: 'shutdownRequest' }), name)?.key, 'team.msg.shutdownRequest', 'shutdown');
  const members = new Map([
    [mkey(3), member(3, 'db-agent', { presence: { kind: 'gone', reason: 'processExited', at: 1 } })],
    [mkey(2), member(2, 'api-agent')],
  ]);
  assertEqual(deliveryNote(message(4, mkey(1), mkey(3), { status: 'queued' }), members), 'neverRead', 'queued to a closed pane');
  assertEqual(deliveryNote(message(5, mkey(1), mkey(2), { status: 'queued' }), members), 'queued', 'queued to a live member');
  assertEqual(deliveryNote(message(6, mkey(1), mkey(2), { status: 'unknown' }), members), 'unknown', 'ambiguous join');
  assertEqual(deliveryNote(message(7, mkey(1), mkey(2)), members), null, 'delivered: no word');
});

test('member tiles: lead and present members; a crash keeps its tile until closed; exit 0 and gone members have none', () => {
  const crashed = paneSession(3, { activity: { ...session(3).activity, state: { kind: 'crashed', exit: { code: 1, signal: null } } } });
  const ended = paneSession(4, { activity: { ...session(4).activity, state: { kind: 'ended' } } });
  const w = worldOf({
    teams: [team()],
    members: [
      lead(),
      member(2, 'api-agent'),
      member(3, 'db-agent', { presence: { kind: 'gone', reason: 'processExited', at: 5 } }),
      member(4, 'docs-agent', { presence: { kind: 'gone', reason: 'shutdownApproved', at: 5 } }),
      member(5, 'new-agent', { sessionUid: null }),
    ],
    sessions: [paneSession(1, {}, 'lead'), paneSession(2), crashed, ended],
  });
  const members = orderedMembers(w, TK);
  const tiles = memberTiles(w, members, new Set());
  assertDeepEqual(tiles.map((x) => [x.member.name, x.kind]), [['team-lead', 'session'], ['api-agent', 'session'], ['db-agent', 'crashed'], ['new-agent', 'placeholder']], 'tiles');
  assertEqual(tiles[2]?.code, 1, 'exit code');
  assertEqual(memberTiles(w, members, new Set([mkey(3)])).length, 3, 'closed crash tile is gone');
  assertEqual(tiles[0]?.uid, uid(1), 'the lead session');
});

test('flat and cleanBody strip bidi overrides and controls (team strings are data)', () => {
  const evil = 'db\u202Egent\u0007 name\nnext';
  assertEqual(flat(evil, 40), 'dbgent name next', 'one display line');
  assertEqual(flat('x'.repeat(50), 10), `${'x'.repeat(9)}…`, 'capped with an ellipsis');
  assertEqual(cleanBody('line 1\nline\u2066 2\u0000'), 'line 1\nline 2', 'newlines kept, bidi and NUL gone');
});
