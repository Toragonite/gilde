/**
 * The showcase's patch script: what changes in the world over time, as the
 * core would publish it (contract §4.3 `StorePatch`, strictly ordered by rev).
 *
 * The steps sit on the sample times of the Floor geometry gate (§14.3:
 * t ∈ {0, 5, 14, 15, 27, 30} s), so a gate can freeze the world at any of
 * them: 5 s a working session asks for approval, 14 s a teammate reports to
 * its lead, 15 s the approval is answered and a task completes, 27 s a reply
 * lands, 30 s a session ends.
 *
 * `foldSnapshot` applies a patch to a snapshot (the backend keeps its current
 * snapshot that way, so a resubscribe always returns the state the patches
 * reached). Both functions are pure.
 */

import type { ActivityState } from '../lib/gen/ActivityState';
import type { Entity } from '../lib/gen/Entity';
import type { EntityKind } from '../lib/gen/EntityKind';
import type { Session } from '../lib/gen/Session';
import type { SessionUid } from '../lib/gen/SessionUid';
import type { StoreOp } from '../lib/gen/StoreOp';
import type { StorePatch } from '../lib/gen/StorePatch';
import type { StoreSnapshot } from '../lib/gen/StoreSnapshot';
import type { TeamMessage } from '../lib/gen/TeamMessage';
import { messageKey } from './ids';
import { approvalCard, buildProjects, capsFor, computeQueue, liveCounts } from './world';
import type { ShowcaseWorld } from './world';

/**
 * One step of the script: at `at` ms after the scene started, the ops `make`
 * computes against the snapshot of that moment (so a user action on a live
 * page earlier in the script is never overwritten by a stale patch).
 */
export interface TimelineStep {
  at: number;
  label: string;
  make: (snapshot: StoreSnapshot, now: number) => StoreOp[];
}

/** The gate's sample times (seconds). */
export const SAMPLE_TIMES_S = [0, 5, 14, 15, 27, 30] as const;

const SNAPSHOT_FIELD: Readonly<Record<EntityKind, keyof StoreSnapshot>> = {
  project: 'projects',
  location: 'locations',
  session: 'sessions',
  team: 'teams',
  member: 'members',
  task: 'tasks',
  message: 'messages',
  account: 'accounts',
  leftover: 'leftovers',
};

function entityId(entity: Entity): string {
  switch (entity.kind) {
    case 'session':
      return entity.value.uid;
    case 'project':
    case 'location':
    case 'account':
      return entity.value.id;
    default:
      return entity.value.key;
  }
}

function idOf(kind: EntityKind, value: unknown): string {
  const v = value as { uid?: string; id?: string; key?: string };
  if (kind === 'session') return v.uid ?? '';
  if (kind === 'project' || kind === 'location' || kind === 'account') return v.id ?? '';
  return v.key ?? '';
}

/** `snapshot` with `ops` applied; untouched lists keep their identity. `rev` becomes `toRev`. */
export function foldSnapshot(snapshot: StoreSnapshot, patch: StorePatch): StoreSnapshot {
  const next: StoreSnapshot = { ...snapshot, rev: patch.toRev };
  const lists = next as unknown as Record<string, unknown[]>;
  for (const op of patch.ops) {
    switch (op.kind) {
      case 'upsert': {
        const field = SNAPSHOT_FIELD[op.entity.kind];
        const id = entityId(op.entity);
        const list = [...(lists[field] ?? [])];
        const at = list.findIndex((v) => idOf(op.entity.kind, v) === id);
        if (at >= 0) list[at] = op.entity.value;
        else list.push(op.entity.value);
        lists[field] = list;
        break;
      }
      case 'remove': {
        const field = SNAPSHOT_FIELD[op.entityKind];
        lists[field] = (lists[field] ?? []).filter((v) => idOf(op.entityKind, v) !== op.id);
        break;
      }
      case 'queue':
        next.queue = op.queue;
        break;
      case 'boot':
        next.boot = op.status;
        break;
      case 'notices':
        next.notices = op.notices;
        break;
      case 'compat':
        next.compat = op.list;
        break;
      case 'sources':
        next.sources = op.statuses;
        break;
      case 'integrations':
        next.integrations = op.state;
        break;
      case 'quit':
      case 'resync':
        break;
    }
  }
  return next;
}

/**
 * Change one session and publish what follows from it in the same patch, as
 * World does: the session row, the project counts when liveness changed, and
 * the queue (Q6: an answered item leaves in the same tick).
 */
export function sessionChange(
  snapshot: StoreSnapshot,
  uid: SessionUid,
  change: (s: Session) => Session,
  now: number,
  later: ReadonlyMap<SessionUid, number> = new Map(),
): StoreOp[] {
  const before = snapshot.sessions.find((s) => s.uid === uid);
  if (!before) return [];
  const after = change(before);
  const sessions = snapshot.sessions.map((s) => (s.uid === uid ? after : s));
  const ops: StoreOp[] = [{ kind: 'upsert', entity: { kind: 'session', value: after } }];
  if ((before.control.kind === 'notRunning') !== (after.control.kind === 'notRunning')) {
    const counts = liveCounts(sessions);
    for (const p of buildProjects(now, counts)) {
      const old = snapshot.projects.find((x) => x.id === p.id);
      if (old && old.liveSessions !== p.liveSessions) ops.push({ kind: 'upsert', entity: { kind: 'project', value: { ...old, liveSessions: p.liveSessions } } });
    }
  }
  ops.push({ kind: 'queue', queue: computeQueue(sessions, snapshot.leftovers, snapshot.queue.rev + 1, now, later) });
  return ops;
}

function withState(s: Session, state: ActivityState, at: number): Session {
  return { ...s, activity: { ...s.activity, state, since: at, confidence: 'high' }, lastActivityAt: at };
}

/**
 * The script for a world. Subjects are picked from the initial snapshot; the
 * ops are made when a step runs. Steps whose subject does not exist in a
 * small world are left out.
 */
export function buildTimeline(world: ShowcaseWorld): TimelineStep[] {
  const steps: TimelineStep[] = [];
  const snap = world.snapshot;
  const t0 = world.now;
  const solo = snap.sessions.filter((s) => !s.team && s.control.kind === 'inGilde' && s.control.mode === 'conversation');
  const asker = solo.find((s) => s.activity.state.kind === 'working' && s.pending.length === 0);

  // 5 s: a working Conversation session asks to run a command.
  if (asker) {
    steps.push({
      at: 5000,
      label: 'approval asked',
      make: (cur) => sessionChange(cur, asker.uid, (s) => withState({ ...s, pending: [approvalCard(s.uid, 9, t0 + 5000)] }, { kind: 'needsApproval' }, t0 + 5000), t0 + 5000),
    });
  }

  // 14 s: a teammate reports to its lead (a message row; its text stays out of the store).
  const team = snap.teams.find((t) => t.source === 'claudeNative');
  const reporter = team ? snap.members.find((m) => m.teamKey === team.key && m.role === 'member') : undefined;
  if (team && team.lead && reporter) {
    const lead = team.lead;
    steps.push({
      at: 14000,
      label: 'teammate message',
      make: () => {
        const msg: TeamMessage = {
          key: messageKey(team.name, 900),
          teamKey: team.key,
          from: { kind: 'member', member: reporter.key },
          to: { kind: 'member', member: lead },
          at: t0 + 14000,
          kind: 'plain',
          status: 'delivered',
          inReplyTo: null,
          derived: false,
          idleReason: null,
          hasSummary: true,
        };
        return [{ kind: 'upsert', entity: { kind: 'message', value: msg } }];
      },
    });
  }

  // 15 s: the 5 s approval is answered (the card leaves, the session works) and a task completes.
  if (asker) {
    steps.push({
      at: 15000,
      label: 'approval answered, task done',
      make: (cur) => {
        const ops = sessionChange(cur, asker.uid, (s) => (s.pending.some((c) => c.requestId.endsWith('_9'))
          ? withState({ ...s, pending: s.pending.filter((c) => !c.requestId.endsWith('_9')) }, { kind: 'working' }, t0 + 15000)
          : s), t0 + 15000);
        const task = cur.tasks.find((t) => t.status === 'inProgress' && !t.inferred);
        if (task) {
          ops.push({ kind: 'upsert', entity: { kind: 'task', value: { ...task, status: 'done', station: 'done', completedAt: t0 + 15000 } } });
          const owner = task.owner ? cur.members.find((m) => m.key === task.owner) : undefined;
          if (owner && owner.currentTask === task.key) ops.push({ kind: 'upsert', entity: { kind: 'member', value: { ...owner, currentTask: null } } });
          const t = cur.teams.find((x) => x.key === task.teamKey);
          if (t) ops.push({ kind: 'upsert', entity: { kind: 'team', value: { ...t, progress: { ...t.progress, done: t.progress.done + 1 } } } });
        }
        return ops;
      },
    });
  }

  // 27 s: a reply lands in a session that was ready.
  const replier = snap.sessions.find((s) => !s.team && s.activity.state.kind === 'ready' && s.control.kind === 'inGilde');
  if (replier) {
    steps.push({
      at: 27000,
      label: 'new reply',
      make: (cur) => sessionChange(cur, replier.uid, (s) => (s.activity.state.kind === 'ready' ? withState(s, { kind: 'newReply' }, t0 + 27000) : s), t0 + 27000),
    });
  }

  // 30 s: a session ends.
  const ender = snap.sessions.find((s) => !s.team && s.uid !== asker?.uid && s.uid !== replier?.uid && s.control.kind === 'inGilde' && s.activity.state.kind === 'working');
  if (ender) {
    steps.push({
      at: 30000,
      label: 'session ended',
      make: (cur) => sessionChange(cur, ender.uid, (s) => {
        const control = { kind: 'notRunning' as const, lastOrigin: null, endedHow: 'ended' as const };
        return { ...withState({ ...s, pending: [] }, { kind: 'ended' }, t0 + 30000), control, caps: capsFor(control), endedAt: t0 + 30000, memoryBytes: null };
      }, t0 + 30000),
    });
  }
  return steps;
}

/** The snapshot after every step at or before `atMs`, and how many steps that was. */
export function snapshotAt(world: ShowcaseWorld, steps: readonly TimelineStep[], atMs: number): { snapshot: StoreSnapshot; applied: number } {
  let snapshot = world.snapshot;
  let applied = 0;
  for (const step of steps) {
    if (step.at > atMs) break;
    const ops = step.make(snapshot, world.now + step.at);
    if (ops.length > 0) snapshot = foldSnapshot(snapshot, { fromRev: snapshot.rev, toRev: snapshot.rev + 1, ops });
    applied++;
  }
  return { snapshot, applied };
}
