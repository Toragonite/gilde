/**
 * The sidebar tree [M3, ux-ia §2.5]: places stay put, only the queue
 * reorders; teams first, then sessions by start time; ended-today fold;
 * ended sessions that still need you keep their row; deferred reflow;
 * Earlier projects; persisted order and folds are validated.
 */

import type { ProjectId } from '../lib/gen/ProjectId';
import type { Session } from '../lib/gen/Session';
import type { SessionUid } from '../lib/gen/SessionUid';
import type { WorldState } from '../store/worldSlice';
import { emptyWorld } from '../store/worldSlice';
import { assert, assertDeepEqual, assertEqual, test } from '../test/harness';
import { project, session, uid } from '../test/fixtures';
import type { ProjectNode, TreeNode } from './tree';
import {
  EARLIER_KEY,
  buildTree,
  flatten,
  moveProject,
  orderedProjects,
  parseFolds,
  parseOrder,
  sessionOrder,
  startOfDay,
} from './tree';

const NOW = new Date(2026, 9, 23, 15, 0, 0).getTime();

function worldOf(projects: ReturnType<typeof project>[], sessions: Session[]): WorldState {
  const w = emptyWorld();
  return {
    ...w,
    projects: new Map(projects.map((p) => [p.id, p])),
    sessions: new Map(sessions.map((s) => [s.uid, s])),
  };
}

const live = (n: number, projectId: string, startedAt: number, extra: Partial<Session> = {}) =>
  session(n, { projectId: projectId as ProjectId, startedAt, ...extra });

const ended = (n: number, projectId: string, endedAt: number) =>
  session(n, {
    projectId: projectId as ProjectId,
    startedAt: endedAt - 1000,
    endedAt,
    control: { kind: 'notRunning', lastOrigin: null, endedHow: 'ended' },
    activity: { state: { kind: 'ended' }, since: endedAt, confidence: 'high', evidence: null, episode: 'e' as never },
  });

const input = (extra: Partial<Parameters<typeof buildTree>[1]> = {}) => ({
  order: null,
  folds: null,
  now: NOW,
  sticky: new Set<SessionUid>(),
  queued: new Set<SessionUid>(),
  ...extra,
});

const titles = (nodes: readonly TreeNode[]) =>
  flatten(nodes).map((f) => (f.node.kind === 'session' ? f.node.session.title : f.node.kind === 'project' ? `#${f.node.project.name}` : f.node.kind));

test('M3: projects keep the user order, then the core order; new projects append', () => {
  const ps = [project('a', { order: 0 }), project('b', { order: 1 }), project('c', { order: 2 })];
  assertDeepEqual(orderedProjects(ps, ['c' as ProjectId]).map((p) => p.id), ['c', 'a', 'b'], 'moved first, the rest by order');
  assertDeepEqual(orderedProjects(ps, []).map((p) => p.id), ['a', 'b', 'c'], 'core order');
  assertDeepEqual(moveProject(ps, 'b' as ProjectId, -1), ['b', 'a', 'c'], 'move up');
  assertDeepEqual(moveProject(ps, 'a' as ProjectId, -1), ['a', 'b', 'c'], 'top stays');
});

test('M3: sessions are ordered by start time and state changes never reorder them', () => {
  const p = project('p');
  const a = live(1, 'p', 300);
  const b = live(2, 'p', 100);
  const before = titles(buildTree(worldOf([p], [a, b]), input()));
  const busy = { ...a, activity: { ...a.activity, state: { kind: 'needsApproval' as const } } };
  const after = titles(buildTree(worldOf([p], [busy, b]), input()));
  assertDeepEqual(before, ['#p', 'session 2', 'session 1'], 'start order');
  assertDeepEqual(after, before, 'a state change moves nothing');
});

test('ended today fold into one row; earlier ended sessions are History only', () => {
  const p = project('p');
  const today = startOfDay(NOW);
  const tree = buildTree(worldOf([p], [live(1, 'p', 1), ended(2, 'p', today + 1000), ended(3, 'p', today - 1000)]), input());
  const kids = (tree[0] as ProjectNode).children;
  assertEqual(kids.length, 2, 'the live row and one fold');
  assertEqual(kids[1]?.kind, 'ended', 'the fold');
  assertEqual(kids[1]?.kind === 'ended' ? kids[1].children.length : -1, 1, 'only today counts');
  assertEqual(kids[1]?.kind === 'ended' ? kids[1].expanded : true, false, 'folded by default');
});

test('an ended session that still needs you keeps its row; a sticky one keeps its place', () => {
  const p = project('p');
  const today = startOfDay(NOW);
  const crashed = ended(2, 'p', today + 5000);
  const queued = buildTree(worldOf([p], [live(1, 'p', 1), crashed]), input({ queued: new Set([uid(2)]) }));
  assertDeepEqual(titles(queued), ['#p', 'session 1', 'session 2'], 'queued ended row stays');
  const sticky = buildTree(worldOf([p], [crashed, live(1, 'p', 1)]), input({ sticky: new Set([uid(2)]) }));
  assertDeepEqual(titles(sticky), ['#p', 'session 1', 'session 2'], 'the sticky ended row stays, in start order');
});

test('Earlier projects fold into one row, folded by default', () => {
  const tree = buildTree(worldOf([project('a'), project('old', { earlier: true, order: 1 })], []), input());
  assertEqual(tree.length, 2, 'a and the Earlier row');
  assertEqual(tree[1]?.key, EARLIER_KEY, 'Earlier last');
  assertEqual(flatten(tree).length, 2, 'its projects are hidden while folded');
  const open = buildTree(worldOf([project('a'), project('old', { earlier: true, order: 1 })], []), input({ folds: { [EARLIER_KEY]: true } }));
  assertEqual(flatten(open).length, 3, 'expanded shows them');
});

test('folds and order from untrusted JSON are validated', () => {
  assertDeepEqual(parseOrder(['a', 7, 'a', 'b'] as never), ['a', 'b'], 'strings only, no duplicates');
  assertDeepEqual(parseOrder({ a: 1 } as never), [], 'not an array');
  assertDeepEqual(parseFolds({ 'p:a': false, x: 'yes', y: true } as never), { 'p:a': false, y: true }, 'booleans only');
  assertDeepEqual(parseFolds([1] as never), {}, 'not an object');
  const p = project('p');
  const folded = buildTree(worldOf([p], [live(1, 'p', 1)]), input({ folds: { 'p:p': false } }));
  assertEqual(flatten(folded).length, 1, 'a folded project hides its rows');
});

test('⌥⌘↑/↓ walks live sessions in sidebar order', () => {
  const tree = buildTree(worldOf([project('a', { order: 0 }), project('b', { order: 1 })], [live(1, 'b', 1), live(2, 'a', 5), live(3, 'a', 2)]), input());
  assertDeepEqual(sessionOrder(tree), [uid(3), uid(2), uid(1)], 'project order, then start order');
  assert(sessionOrder([]).length === 0, 'empty');
});
