/**
 * The sidebar tree as a pure model (ux-ia §2.5, M3 "places stay put; only the
 * queue reorders").
 *
 *  - Projects keep the user's order: `sidebarOrder` (a persisted UI value this
 *    screen owns) first, then the core's `Project.order` for projects the user
 *    never moved; new projects append. Projects the core marks `earlier` (no
 *    live session and no activity in 24 h) fold into one "Earlier projects"
 *    row and return to their place when they come alive again.
 *  - Inside a project: teams first (a team row only with ≥ 1 member besides
 *    the lead), then sessions by start time. State changes never reorder.
 *  - Team rows are never auto-expanded (expanding would move the rows below).
 *  - Sessions that ended today fold into one "◌ n ended today" row per
 *    project; a session that ended and still needs you (it is in the queue)
 *    keeps its row until it leaves the queue.
 *  - Deferred reflow: a session that ends keeps its row while the sidebar is
 *    hovered or focused (`sticky`), so a row never vanishes under the pointer.
 *
 * Fold state is `{ [nodeKey]: expanded }` in the persisted `sidebarFolds`.
 */

import type { JsonValue } from '../lib/gen/serde_json/JsonValue';
import type { Member } from '../lib/gen/Member';
import type { Project } from '../lib/gen/Project';
import type { ProjectId } from '../lib/gen/ProjectId';
import type { Session } from '../lib/gen/Session';
import type { SessionUid } from '../lib/gen/SessionUid';
import type { Team } from '../lib/gen/Team';
import type { WorldState } from '../store/worldSlice';
import { membersOfTeam } from '../store/worldSlice';

export type NodeKey = string;

export interface SessionNode {
  kind: 'session';
  key: NodeKey;
  level: number;
  session: Session;
  /** Set for team members. */
  member: Member | null;
}

/** A roster member without a session of its own yet (in-process, no transcript). */
export interface MemberNode {
  kind: 'member';
  key: NodeKey;
  level: number;
  member: Member;
}

export interface TeamNode {
  kind: 'team';
  key: NodeKey;
  level: number;
  team: Team;
  memberCount: number;
  expanded: boolean;
  children: (SessionNode | MemberNode)[];
}

export interface EndedNode {
  kind: 'ended';
  key: NodeKey;
  level: number;
  projectId: ProjectId;
  expanded: boolean;
  children: SessionNode[];
}

export interface ProjectNode {
  kind: 'project';
  key: NodeKey;
  level: number;
  project: Project;
  expanded: boolean;
  children: (TeamNode | SessionNode | EndedNode)[];
}

export interface EarlierNode {
  kind: 'earlier';
  key: NodeKey;
  level: number;
  expanded: boolean;
  children: ProjectNode[];
}

export type TreeNode = ProjectNode | EarlierNode | TeamNode | EndedNode | SessionNode | MemberNode;

export interface TreeInput {
  /** `ui.sidebarOrder`: an array of project ids (anything else is ignored). */
  order: JsonValue | null;
  /** `ui.sidebarFolds`: `{ [nodeKey]: expanded }` (anything else is ignored). */
  folds: JsonValue | null;
  /** Wall clock, for "ended today". */
  now: number;
  /** Sessions whose row stays where it is although they ended (deferred reflow). */
  sticky: ReadonlySet<SessionUid>;
  /** Sessions in the attention queue: an ended one keeps its row. */
  queued: ReadonlySet<SessionUid>;
}

export const EARLIER_KEY = 'earlier';

export const projectKey = (id: ProjectId): NodeKey => `p:${id}`;
export const teamNodeKey = (key: string): NodeKey => `t:${key}`;
export const sessionKey = (uid: SessionUid): NodeKey => `s:${uid}`;
export const endedKey = (id: ProjectId): NodeKey => `e:${id}`;

/** The persisted project order, validated (strings only, no duplicates). */
export function parseOrder(value: JsonValue | null): ProjectId[] {
  if (!Array.isArray(value)) return [];
  const out: ProjectId[] = [];
  for (const v of value) if (typeof v === 'string' && v.length <= 512 && !out.includes(v as ProjectId)) out.push(v as ProjectId);
  return out.slice(0, 500);
}

/** The persisted fold map, validated (boolean values only). */
export function parseFolds(value: JsonValue | null): Record<string, boolean> {
  const out: Record<string, boolean> = {};
  if (typeof value !== 'object' || value === null || Array.isArray(value)) return out;
  for (const [k, v] of Object.entries(value)) if (typeof v === 'boolean' && k.length <= 600) out[k] = v;
  return out;
}

/** Projects in the user's order (persisted ids first, then the core's order). */
export function orderedProjects(projects: Iterable<Project>, order: readonly ProjectId[]): Project[] {
  const rank = new Map(order.map((id, i) => [id, i]));
  return [...projects].sort((a, b) => {
    const ra = rank.get(a.id);
    const rb = rank.get(b.id);
    if (ra !== undefined && rb !== undefined) return ra - rb;
    if (ra !== undefined) return -1;
    if (rb !== undefined) return 1;
    return a.order - b.order || (a.id < b.id ? -1 : a.id > b.id ? 1 : 0);
  });
}

/** Local midnight of the day `now` falls in. */
export function startOfDay(now: number): number {
  const d = new Date(now);
  d.setHours(0, 0, 0, 0);
  return d.getTime();
}

const isLive = (s: Session) => s.control.kind !== 'notRunning';

function byStart(a: Session, b: Session): number {
  const sa = a.startedAt ?? a.lastActivityAt ?? 0;
  const sb = b.startedAt ?? b.lastActivityAt ?? 0;
  return sa - sb || (a.uid < b.uid ? -1 : a.uid > b.uid ? 1 : 0);
}

/** Members in roster order: the lead first, then by join time. */
export function rosterOrder(members: readonly Member[]): Member[] {
  return [...members].sort((a, b) => {
    if (a.role !== b.role) return a.role === 'lead' ? -1 : 1;
    return (a.joinedAt ?? 0) - (b.joinedAt ?? 0) || (a.key < b.key ? -1 : a.key > b.key ? 1 : 0);
  });
}

/** The whole tree: live projects in order, then the Earlier row when it has projects. */
export function buildTree(world: WorldState, input: TreeInput): TreeNode[] {
  const order = parseOrder(input.order);
  const folds = parseFolds(input.folds);
  const today = startOfDay(input.now);
  const expanded = (key: NodeKey, fallback: boolean) => folds[key] ?? fallback;

  // Sessions per project, and which sessions a team row shows.
  const byProject = new Map<string, Session[]>();
  for (const s of world.sessions.values()) {
    if (!s.projectId) continue;
    const list = byProject.get(s.projectId) ?? [];
    list.push(s);
    byProject.set(s.projectId, list);
  }
  const teamsByProject = new Map<string, Team[]>();
  for (const team of world.teams.values()) {
    if (!team.projectId) continue;
    const list = teamsByProject.get(team.projectId) ?? [];
    list.push(team);
    teamsByProject.set(team.projectId, list);
  }

  const projectNode = (project: Project, level: number): ProjectNode => {
    const key = projectKey(project.id);
    const sessions = byProject.get(project.id) ?? [];
    const children: (TeamNode | SessionNode | EndedNode)[] = [];
    const inTeam = new Set<SessionUid>();

    const teams = (teamsByProject.get(project.id) ?? []).sort(
      (a, b) => (a.startedAt ?? 0) - (b.startedAt ?? 0) || (a.key < b.key ? -1 : a.key > b.key ? 1 : 0),
    );
    for (const team of teams) {
      const members = rosterOrder(membersOfTeam(world, team.key));
      if (members.length < 2) continue;
      const tkey = teamNodeKey(team.key);
      const kids: (SessionNode | MemberNode)[] = [];
      for (const member of members) {
        const session = member.sessionUid ? world.sessions.get(member.sessionUid) : undefined;
        if (session) {
          inTeam.add(session.uid);
          kids.push({ kind: 'session', key: sessionKey(session.uid), level: level + 2, session, member });
        } else {
          kids.push({ kind: 'member', key: `m:${member.key}`, level: level + 2, member });
        }
      }
      children.push({
        kind: 'team',
        key: tkey,
        level: level + 1,
        team,
        memberCount: members.length,
        expanded: expanded(tkey, false),
        children: kids,
      });
    }

    const rest = sessions.filter((s) => !inTeam.has(s.uid)).sort(byStart);
    const endedToday: SessionNode[] = [];
    for (const s of rest) {
      const node: SessionNode = { kind: 'session', key: sessionKey(s.uid), level: level + 1, session: s, member: null };
      if (isLive(s) || input.sticky.has(s.uid) || input.queued.has(s.uid)) {
        children.push(node);
      } else if ((s.endedAt ?? 0) >= today) {
        endedToday.push({ ...node, level: level + 2 });
      }
    }
    if (endedToday.length > 0) {
      const ekey = endedKey(project.id);
      children.push({ kind: 'ended', key: ekey, level: level + 1, projectId: project.id, expanded: expanded(ekey, false), children: endedToday });
    }
    return { kind: 'project', key, level, project, expanded: expanded(key, !project.folded), children };
  };

  const projects = orderedProjects(world.projects.values(), order);
  const live = projects.filter((p) => !p.earlier);
  const earlier = projects.filter((p) => p.earlier);
  const out: TreeNode[] = live.map((p) => projectNode(p, 1));
  if (earlier.length > 0) {
    out.push({
      kind: 'earlier',
      key: EARLIER_KEY,
      level: 1,
      expanded: expanded(EARLIER_KEY, false),
      children: earlier.map((p) => projectNode(p, 2)),
    });
  }
  return out;
}

export interface FlatItem {
  node: TreeNode;
  /** Key of the parent item ("" at the top). */
  parent: NodeKey;
  /** Position among its siblings (1-based) and their count, for aria-posinset/setsize. */
  pos: number;
  size: number;
}

function hasChildren(node: TreeNode): node is ProjectNode | EarlierNode | TeamNode | EndedNode {
  return node.kind === 'project' || node.kind === 'earlier' || node.kind === 'team' || node.kind === 'ended';
}

export function isExpandable(node: TreeNode): boolean {
  return hasChildren(node) && node.children.length > 0;
}

export function isExpanded(node: TreeNode): boolean {
  return hasChildren(node) && node.expanded && node.children.length > 0;
}

/** The visible items, depth first, in display order. */
export function flatten(nodes: readonly TreeNode[], parent: NodeKey = ''): FlatItem[] {
  const out: FlatItem[] = [];
  nodes.forEach((node, i) => {
    out.push({ node, parent, pos: i + 1, size: nodes.length });
    if (hasChildren(node) && node.expanded) out.push(...flatten(node.children as TreeNode[], node.key));
  });
  return out;
}

/** Live sessions in sidebar order (⌥⌘↑/↓ walks this list). */
export function sessionOrder(nodes: readonly TreeNode[]): SessionUid[] {
  const out: SessionUid[] = [];
  const walk = (list: readonly TreeNode[]) => {
    for (const node of list) {
      if (node.kind === 'session') out.push(node.session.uid);
      else if (node.kind === 'project' || node.kind === 'team') walk(node.children);
    }
  };
  walk(nodes);
  return out;
}

/** The order after moving `id` one place up (-1) or down (+1) among the live projects. */
export function moveProject(current: readonly Project[], id: ProjectId, dir: -1 | 1): ProjectId[] {
  const ids = current.map((p) => p.id);
  const at = ids.indexOf(id);
  const to = at + dir;
  if (at < 0 || to < 0 || to >= ids.length) return ids;
  const next = [...ids];
  next[at] = ids[to] as ProjectId;
  next[to] = id;
  return next;
}
