/**
 * The Floor's view model reads only the store and never invents (pixel-office
 * G1/G2, contract §7.5): stations are Rust's, counts are the queue's, the
 * dependency shown is a declared one, missing data stays missing.
 */

import type { MemberKey } from '../lib/gen/MemberKey';
import type { Origin } from '../lib/gen/Origin';
import type { Session } from '../lib/gen/Session';
import type { StoreSnapshot } from '../lib/gen/StoreSnapshot';
import { createAppStore } from '../store/useAppStore';
import { assert, assertEqual, test } from '../test/harness';
import { metrics } from './constants';
import { SeatBook, buildRooms } from './layout';
import type { FlVm } from './model';
import { bottleneckOf, buildVm, cardAtCounter, counterBottleneck, oneLine } from './model';
import { T0, showcaseSnapshot } from './testing';
import { groupLabel, twinGroups } from './twinText';
import { changes, factsOf } from './useAnnouncements';
import { makeWords } from './words';

function vmOf(snap: StoreSnapshot, now = T0): FlVm {
  const store = createAppStore();
  store.getState().applyStoreSnapshot(snap);
  const w = makeWords('ko');
  return buildVm(store.getState().world, now, { untitled: (v) => w.t('floor.untitled', { vendor: v }), request: (k) => w.t('floor.request', { k }) });
}

test('T-floor-model: counter counts are the queue counts (I6), tickets keep queue order', () => {
  const snap = showcaseSnapshot(31, ['native', 'nativeInferred', 'orchestrator'], 0, 2);
  const vm = vmOf(snap);
  assertEqual(vm.needYou, snap.queue.needYou, 'needYou is the queue value');
  assertEqual(vm.also, snap.queue.also, 'also is the queue value');
  assertEqual(vm.queue.map((i) => i.key).join(','), snap.queue.items.map((i) => i.key).join(','), 'same order, nothing re-ranked');
  assertEqual(vm.waits.length, snap.queue.items.filter((i) => i.band <= 2 && i.subject.kind === 'session').length, 'waits = bands 1–2');
});

test('T-floor-model: stations come from Task.station; inferred items are "지시 k" and never blocked', () => {
  const snap = showcaseSnapshot(20, ['nativeInferred', 'orchestrator'], 0);
  const vm = vmOf(snap);
  const inferred = vm.teams.find((t) => t.inferred);
  const real = vm.teams.find((t) => !t.inferred);
  assert(inferred && real, 'both kinds of team');
  for (const it of inferred.items) {
    assert(/^지시 \d+$/.test(it.title), `inferred title "${it.title}" is a request number, never message text`);
    assertEqual(it.blockedBy, null, 'inferred items never have a dependency');
  }
  const raw = new Map(snap.tasks.map((t) => [t.key, t.station] as const));
  for (const it of [...inferred.items, ...real.items]) assertEqual(it.rawStation, raw.get(it.key), 'the station is Rust’s');
  const blocked = real.items.find((i) => i.station === 'blocked');
  assert(blocked && blocked.blockedBy, 'the declared dependency shows on the blocked card');
});

test('T-floor-model: a dependency on a finished task is not shown as blocking', () => {
  const snap = showcaseSnapshot(20, ['orchestrator'], 0);
  const t3 = snap.tasks[2];
  assert(t3, 'task 3 exists');
  snap.tasks = snap.tasks.map((t) => (t.key === t3.key ? { ...t, status: 'done', station: 'done' } : t));
  const vm = vmOf(snap);
  for (const team of vm.teams) for (const it of team.items) assert(it.blockedBy !== t3.key, 'a done blocker does not block');
});

test('T-floor-model: line order is lead first, then join order; gone members keep their desk', () => {
  const snap = showcaseSnapshot(12, ['native'], 0);
  const team = snap.teams[0];
  assert(team, 'a team');
  const members = snap.members.filter((m) => m.teamKey === team.key);
  // shuffle the store order: the line must not change
  snap.members = [...members].reverse().concat(snap.members.filter((m) => m.teamKey !== team.key));
  const gone = members[2];
  assert(gone, 'a third member');
  snap.members = snap.members.map((m) => (m.key === gone.key ? { ...m, presence: { kind: 'gone', reason: 'processExited', at: T0 } } : m));
  const vm = vmOf(snap);
  const line = vm.teams[0]?.line ?? [];
  assertEqual(line[0], team.lead, 'the lead is first');
  const joined = members.slice().sort((a, b) => (a.joinedAt ?? 0) - (b.joinedAt ?? 0)).map((m) => m.key);
  assertEqual(line.join(','), joined.join(','), 'join order');
  assert(line.includes(gone.key), 'a gone member keeps its desk');
});

test('T-floor-model: a member without a session is Status unknown, never Working', () => {
  const snap = showcaseSnapshot(8, ['native'], 0);
  const m = snap.members.find((x) => x.role === 'member');
  assert(m, 'a member');
  snap.members = snap.members.map((x) => (x.key === m.key ? { ...x, sessionUid: null } : x));
  const vm = vmOf(snap);
  const seat = vm.seats.get(m.key);
  assertEqual(seat?.state, 'statusUnknown', 'unknown is drawn as unknown (G2)');
  assertEqual(seat?.uid, null, 'nothing to open');
});

test('T-floor-model: ended standalone sessions are ghosts for 10 minutes, then leave the floor', () => {
  const snap = showcaseSnapshot(8, [], 0);
  const s = snap.sessions[0] as Session;
  const end = (at: number): StoreSnapshot => ({
    ...snap,
    sessions: snap.sessions.map((x) =>
      x.uid === s.uid ? { ...x, activity: { ...x.activity, state: { kind: 'ended' }, since: at }, control: { kind: 'notRunning', lastOrigin: null, endedHow: 'ended' }, endedAt: at } : x,
    ),
  });
  const fresh = vmOf(end(T0 - 5 * 60_000));
  assert(fresh.seats.get(s.uid)?.ghost, 'a ghost within the grace');
  const old = vmOf(end(T0 - 11 * 60_000));
  assert(!old.seats.has(s.uid), 'gone after 10 minutes');
});

test('T-floor-model: bottleneck words follow Rust’s list only', () => {
  const snap = showcaseSnapshot(12, ['native'], 0);
  const vm = vmOf(snap);
  const team = vm.teams[0];
  assert(team, 'a team');
  for (const id of team.line) assertEqual(bottleneckOf(team, id as MemberKey), null, 'no bottleneck unless Rust says so');
  const who = team.line[1] as MemberKey;
  snap.teams = snap.teams.map((t) => ({ ...t, progress: { ...t.progress, bottlenecks: [who] } }));
  const vm2 = vmOf(snap);
  const b = bottleneckOf(vm2.teams[0] as never, who);
  assert(b && b.n >= 1, 'the marked member has a reason and a count');
});

test('T-floor-model: the counter bottleneck needs ≥ 2 waiting and the oldest ≥ 5 minutes', () => {
  const snap = showcaseSnapshot(8, [], 0);
  const vm = vmOf(snap);
  const waiting = vm.queue.filter((i) => i.band <= 2 && i.subject.kind === 'session');
  const oldest = Math.max(...waiting.map((i) => T0 - i.since));
  const cb = counterBottleneck(vm);
  if (waiting.length >= 2 && oldest >= 5 * 60_000) assert(cb && cb.n === waiting.length, 'fires');
  else assertEqual(cb, null, 'does not fire');
  const early = vmOf(snap, Math.min(...waiting.map((i) => i.since)) + 60_000);
  assertEqual(counterBottleneck(early), null, 'a fresh wait is not a bottleneck');
});

test('T-floor-model: a ticket carries its member’s card waiting on you', () => {
  const snap = showcaseSnapshot(12, ['native'], 0);
  const asker = snap.members.find((m) => m.role === 'member');
  assert(asker && asker.sessionUid, 'a member with a session');
  const task = snap.tasks.find((t) => t.owner === asker.key) ?? snap.tasks[0];
  assert(task, 'a task');
  snap.tasks = snap.tasks.map((t) => (t.key === task.key ? { ...t, owner: asker.key, status: 'inProgress', station: 'needsYou' } : t));
  const vm = vmOf(snap);
  assertEqual(cardAtCounter(vm, asker.sessionUid)?.key, task.key, 'the card is on the ticket');
});

test('T-floor-model: desks with no project sit in their app’s bay (ChatGPT app, VS Code), others under "no project"', () => {
  const snap = showcaseSnapshot(8, [], 0);
  const [a, b, c] = snap.sessions as Session[];
  assert(a && b && c, 'three standalone sessions');
  const watching = (origin: Origin): Session['control'] => ({ kind: 'watching', origin, owner: null });
  snap.sessions = snap.sessions.map((x) =>
    x.uid === a.uid
      ? { ...x, projectId: null, control: watching('chatGptApp') }
      : x.uid === b.uid
        ? { ...x, projectId: null, control: watching('vsCode') }
        : x.uid === c.uid
          ? { ...x, projectId: null, control: watching('terminal') }
          : x,
  );
  const vm = vmOf(snap);
  const w = makeWords('ko');
  const rooms = buildRooms(vm, metrics(1), new SeatBook(), {
    scope: { kind: 'global' },
    noProjectLabel: w.t('floor.noProject'),
    appLabel: (o) => w.t(o === 'chatGptApp' ? 'origin.chatGptApp' : 'origin.vscode'),
  });
  const roomOf = (uid: string) => rooms.find((r) => r.seats.some((s) => s.id === uid));
  assertEqual(roomOf(a.uid)?.label, 'ChatGPT 앱', 'the desktop app’s bay');
  assertEqual(roomOf(b.uid)?.label, 'VS Code', 'the editor’s bay');
  assertEqual(roomOf(c.uid)?.label, '프로젝트 없음', 'a terminal session with no project');
  assertEqual(roomOf(a.uid)?.projectId, null, 'an app bay is no project');
  const labels = rooms.map((r) => r.label);
  assert(labels.indexOf('VS Code') < labels.indexOf('프로젝트 없음'), 'app bays come before "no project"');
  const twin = twinGroups(vm, { kind: 'global' });
  assertEqual(twin.map((g) => g.key).join(), rooms.map((r) => r.key).join(), 'the text twin has the canvas’s bays');
  const app = twin.find((g) => g.solo.some((s) => s.id === a.uid));
  assert(app && groupLabel('ko', app, false).includes('ChatGPT 앱'), 'the twin names the app');
});

test('T-floor-model: names are one line', () => {
  assertEqual(oneLine('a\nb\t c d'), 'a b c d', 'control characters and line breaks collapse');
});

test('T-floor-announce: a finished task, an unblocked task, a member joining and leaving are said once, in words', () => {
  const snap = showcaseSnapshot(12, ['native'], 0);
  const before = factsOf(vmOf(snap));
  const t3 = snap.tasks[2];
  const t4 = snap.tasks[3];
  const m = snap.members.find((x) => x.role === 'member');
  assert(t3 && t4 && m, 'fixtures');
  snap.tasks = snap.tasks.map((t) => (t.key === t3.key ? { ...t, status: 'done', station: 'done' } : t.key === t4.key ? { ...t, station: 'toDo' } : t));
  snap.members = snap.members.map((x) => (x.key === m.key ? { ...x, presence: { kind: 'gone', reason: 'shutdownApproved', at: T0 } } : x));
  snap.sessions = snap.sessions.map((x) => (x.uid === m.sessionUid ? { ...x, activity: { ...x.activity, state: { kind: 'ended' } }, control: { kind: 'notRunning', lastOrigin: null, endedHow: 'ended' } } : x));
  const said = changes('ko', before, factsOf(vmOf(snap)));
  assert(said.some((s) => s.startsWith('작업 완료: ')), `a finished task (${said.join(' | ')})`);
  assert(said.some((s) => s.startsWith('막힘 해제: ')), 'an unblocked task');
  assert(said.some((s) => s.endsWith('에서 나갔습니다.')), 'a member left');
  assertEqual(changes('ko', factsOf(vmOf(snap)), factsOf(vmOf(snap))).length, 0, 'nothing changed, nothing said');
});
