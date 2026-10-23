/**
 * World slice — the WebView's mirror of the core's World (contract §4.2.1
 * `store_subscribe`, §4.3 `StorePatch`).
 *
 * One subscription feeds everything the screens show: projects, locations,
 * sessions (with their pending cards), teams, members, tasks, messages, the
 * attention queue, accounts, leftovers, compat, notices, boot, sources,
 * integrations and the quit sweep's progress. Screens never fetch these.
 *
 * Rules (each one is a test in worldSlice.test.ts):
 *  - A patch applies only when `patch.fromRev === rev`. A patch whose `toRev`
 *    is not ahead is a duplicate and is dropped. Anything else is a gap, and a
 *    gap — like a `Resync` op — resubscribes (T-ui-revGap): a mirror that
 *    silently diverges is worse than a visible refetch.
 *  - Patches that arrive on the new channel before `store_subscribe` answers
 *    are buffered and applied after the snapshot (the core sends every patch
 *    with `fromRev ≥ snapshot.rev` to the new channel).
 *  - A session with `mergedInto` is a tombstone (§3.3.3): it is never shown;
 *    its uid is recorded so every reference held by the UI (open session,
 *    Split set, focus, drafts) is remapped to the survivor (T-ui-mergedInto).
 *  - Entities an op did not touch keep their object identity, so a selector on
 *    one session does not re-render for another.
 *  - Pending cards live in `Session.pending` and therefore re-render from the
 *    snapshot after a reload (T-ui-reload, PM-05).
 */

import type { Channel } from '@tauri-apps/api/core';
import type { StateCreator } from 'zustand';

import { createChannel, disposeChannel } from '../lib/channel';
import { clock } from '../lib/clock';
import { commands } from '../lib/commands';
import type { Account } from '../lib/gen/Account';
import type { AccountId } from '../lib/gen/AccountId';
import type { AttentionQueue } from '../lib/gen/AttentionQueue';
import type { BootStatus } from '../lib/gen/BootStatus';
import type { ClaudeCompat } from '../lib/gen/ClaudeCompat';
import type { Entity } from '../lib/gen/Entity';
import type { EntityKind } from '../lib/gen/EntityKind';
import type { IntegrationsState } from '../lib/gen/IntegrationsState';
import type { Location } from '../lib/gen/Location';
import type { LocationId } from '../lib/gen/LocationId';
import type { Member } from '../lib/gen/Member';
import type { MemberKey } from '../lib/gen/MemberKey';
import type { MessageKey } from '../lib/gen/MessageKey';
import type { Notice } from '../lib/gen/Notice';
import type { ProcessKey } from '../lib/gen/ProcessKey';
import type { ProcessRecord } from '../lib/gen/ProcessRecord';
import type { Project } from '../lib/gen/Project';
import type { ProjectId } from '../lib/gen/ProjectId';
import type { QuitPhase } from '../lib/gen/QuitPhase';
import type { Session } from '../lib/gen/Session';
import type { SessionUid } from '../lib/gen/SessionUid';
import type { SourceStatus } from '../lib/gen/SourceStatus';
import type { StoreOp } from '../lib/gen/StoreOp';
import type { StorePatch } from '../lib/gen/StorePatch';
import type { StoreSnapshot } from '../lib/gen/StoreSnapshot';
import type { Task } from '../lib/gen/Task';
import type { TaskKey } from '../lib/gen/TaskKey';
import type { Team } from '../lib/gen/Team';
import type { TeamKey } from '../lib/gen/TeamKey';
import type { TeamMessage } from '../lib/gen/TeamMessage';
import type { IpcError } from '../lib/ipcError';
import { isNotBuilt } from '../lib/ipcError';

// ---------------------------------------------------------------------------
// State
// ---------------------------------------------------------------------------

/** Where the subscription is. `unavailable` = the core answered `UNSUPPORTED{notBuilt}`. */
export type WorldStatus = 'idle' | 'subscribing' | 'live' | 'resyncing' | 'retrying' | 'unavailable';

export interface QuitProgress {
  stopped: number;
  total: number;
  phase: QuitPhase;
}

export interface WorldState {
  /** Last applied revision; 0 = nothing applied yet. */
  rev: number;
  status: WorldStatus;
  projects: ReadonlyMap<ProjectId, Project>;
  locations: ReadonlyMap<LocationId, Location>;
  /** Live and ended sessions; tombstones are never in here. */
  sessions: ReadonlyMap<SessionUid, Session>;
  teams: ReadonlyMap<TeamKey, Team>;
  members: ReadonlyMap<MemberKey, Member>;
  tasks: ReadonlyMap<TaskKey, Task>;
  messages: ReadonlyMap<MessageKey, TeamMessage>;
  accounts: ReadonlyMap<AccountId, Account>;
  leftovers: ReadonlyMap<ProcessKey, ProcessRecord>;
  queue: AttentionQueue;
  compat: readonly ClaudeCompat[];
  notices: readonly Notice[];
  boot: BootStatus;
  sources: readonly SourceStatus[];
  integrations: IntegrationsState;
  /** Progress of the quit sweep (§10.2), once it started. */
  quit: QuitProgress | null;
  /** Tombstone uid → the uid it was merged into (§3.3.3). */
  mergedInto: ReadonlyMap<SessionUid, SessionUid>;
  /** How many resubscribes a gap or Resync caused (diagnostics). */
  resyncCount: number;
  /** When the last snapshot or patch landed (monotonic ms). */
  lastPatchAt: number;
  /** The last subscribe failure, if the subscription is not live. */
  lastError: IpcError | null;
}

export function emptyQueue(): AttentionQueue {
  return { rev: 0, items: [], needYou: 0, also: 0, pausedUntil: null };
}

export function emptyBoot(): BootStatus {
  return { spawnGateOpen: false, firstDiscoveryDone: false, agentsDetected: false, memoryOnly: null, agents: [], agentsScannedAt: null, recovery: null };
}

export function emptyWorld(): WorldState {
  return {
    rev: 0,
    status: 'idle',
    projects: new Map(),
    locations: new Map(),
    sessions: new Map(),
    teams: new Map(),
    members: new Map(),
    tasks: new Map(),
    messages: new Map(),
    accounts: new Map(),
    leftovers: new Map(),
    queue: emptyQueue(),
    compat: [],
    notices: [],
    boot: emptyBoot(),
    sources: [],
    integrations: { orchestrator: 'absent' },
    quit: null,
    mergedInto: new Map(),
    resyncCount: 0,
    lastPatchAt: 0,
    lastError: null,
  };
}

// ---------------------------------------------------------------------------
// Pure folding (exercised directly by the tests)
// ---------------------------------------------------------------------------

/** What a patch means for a mirror at `rev`. */
export type PatchDecision = 'apply' | 'stale' | 'gap';

export function decidePatch(rev: number, patch: Pick<StorePatch, 'fromRev' | 'toRev'>): PatchDecision {
  if (patch.fromRev === rev && patch.toRev >= patch.fromRev) return 'apply';
  if (patch.toRev <= rev) return 'stale';
  return 'gap';
}

/** The id an entity is keyed by in its map. */
export function entityId(entity: Entity): string {
  switch (entity.kind) {
    case 'project':
      return entity.value.id;
    case 'location':
      return entity.value.id;
    case 'session':
      return entity.value.uid;
    case 'team':
      return entity.value.key;
    case 'member':
      return entity.value.key;
    case 'task':
      return entity.value.key;
    case 'message':
      return entity.value.key;
    case 'account':
      return entity.value.id;
    case 'leftover':
      return entity.value.key;
  }
}

type MapKind = Exclude<EntityKind, never>;

const MAP_OF: Readonly<Record<MapKind, keyof WorldState>> = {
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

/** The result of folding ops: the next state, the tombstones seen, and whether a Resync was asked. */
export interface FoldResult {
  world: WorldState;
  /** Newly seen tombstones: uid → survivor. */
  merged: Map<SessionUid, SessionUid>;
  resync: boolean;
}

/**
 * Fold `ops` into `world`. Maps are copied once per kind on first write, so
 * untouched kinds and untouched entities keep their identity. Folding stops
 * at a `Resync` op: the caller resubscribes and the rest of the patch is moot.
 */
export function foldOps(world: WorldState, ops: readonly StoreOp[]): FoldResult {
  const next: WorldState = { ...world };
  const copied = new Set<keyof WorldState>();
  const merged = new Map<SessionUid, SessionUid>();
  let mergedMap: Map<SessionUid, SessionUid> | null = null;

  const writable = <K extends keyof WorldState>(field: K): Map<string, unknown> => {
    if (!copied.has(field)) {
      (next as unknown as Record<string, unknown>)[field] = new Map(world[field] as ReadonlyMap<string, unknown>);
      copied.add(field);
    }
    return (next as unknown as Record<string, Map<string, unknown>>)[field] as Map<string, unknown>;
  };

  for (const op of ops) {
    switch (op.kind) {
      case 'upsert': {
        const entity = op.entity;
        if (entity.kind === 'session' && entity.value.mergedInto !== null) {
          const uid = entity.value.uid;
          const survivor = entity.value.mergedInto;
          if (survivor !== uid) {
            writable('sessions').delete(uid);
            mergedMap ??= new Map(world.mergedInto);
            mergedMap.set(uid, survivor);
            merged.set(uid, survivor);
          }
          break;
        }
        writable(MAP_OF[entity.kind]).set(entityId(entity), entity.value);
        break;
      }
      case 'remove': {
        const field = MAP_OF[op.entityKind];
        if ((world[field] as ReadonlyMap<string, unknown>).has(op.id) || copied.has(field)) {
          writable(field).delete(op.id);
        }
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
        next.quit = { stopped: op.stopped, total: op.total, phase: op.phase };
        break;
      case 'resync':
        if (mergedMap) next.mergedInto = mergedMap;
        return { world: next, merged, resync: true };
    }
  }
  if (mergedMap) next.mergedInto = mergedMap;
  return { world: next, merged, resync: false };
}

/** A fresh world from a snapshot; diagnostics and the tombstone map carry over. */
export function worldFromSnapshot(snapshot: StoreSnapshot, previous: WorldState): FoldResult {
  const byId = <T>(items: readonly T[], id: (item: T) => string) => {
    const m = new Map<string, T>();
    for (const item of items) m.set(id(item), item);
    return m;
  };
  const sessions = new Map<SessionUid, Session>();
  const mergedInto = new Map(previous.mergedInto);
  const merged = new Map<SessionUid, SessionUid>();
  for (const s of snapshot.sessions) {
    if (s.mergedInto !== null && s.mergedInto !== s.uid) {
      mergedInto.set(s.uid, s.mergedInto);
      merged.set(s.uid, s.mergedInto);
    } else {
      sessions.set(s.uid, s);
    }
  }
  const world: WorldState = {
    rev: snapshot.rev,
    status: 'live',
    projects: byId(snapshot.projects, (p) => p.id) as Map<ProjectId, Project>,
    locations: byId(snapshot.locations, (l) => l.id) as Map<LocationId, Location>,
    sessions,
    teams: byId(snapshot.teams, (t) => t.key) as Map<TeamKey, Team>,
    members: byId(snapshot.members, (m) => m.key) as Map<MemberKey, Member>,
    tasks: byId(snapshot.tasks, (t) => t.key) as Map<TaskKey, Task>,
    messages: byId(snapshot.messages, (m) => m.key) as Map<MessageKey, TeamMessage>,
    accounts: byId(snapshot.accounts, (a) => a.id) as Map<AccountId, Account>,
    leftovers: byId(snapshot.leftovers, (l) => l.key) as Map<ProcessKey, ProcessRecord>,
    queue: snapshot.queue,
    compat: snapshot.compat,
    notices: snapshot.notices,
    boot: snapshot.boot,
    sources: snapshot.sources,
    integrations: snapshot.integrations,
    quit: previous.quit,
    mergedInto,
    resyncCount: previous.resyncCount,
    lastPatchAt: clock.now(),
    lastError: null,
  };
  return { world, merged, resync: false };
}

/** Follow `mergedInto` to the surviving uid (bounded, so a corrupt cycle cannot hang). */
export function resolveUid(world: Pick<WorldState, 'mergedInto'>, uid: SessionUid): SessionUid {
  let current = uid;
  for (let hops = 0; hops < 8; hops++) {
    const next = world.mergedInto.get(current);
    if (next === undefined || next === current) return current;
    current = next;
  }
  return current;
}

// ---------------------------------------------------------------------------
// Selectors
// ---------------------------------------------------------------------------

/** The session a (possibly merged) uid refers to now. */
export function sessionOf(world: WorldState, uid: SessionUid | null | undefined): Session | null {
  if (!uid) return null;
  return world.sessions.get(resolveUid(world, uid)) ?? null;
}

/** Sessions of one project, in the core's map order. */
export function sessionsOfProject(world: WorldState, projectId: ProjectId): Session[] {
  const out: Session[] = [];
  for (const s of world.sessions.values()) if (s.projectId === projectId) out.push(s);
  return out;
}

export function membersOfTeam(world: WorldState, teamKey: TeamKey): Member[] {
  const out: Member[] = [];
  for (const m of world.members.values()) if (m.teamKey === teamKey) out.push(m);
  return out;
}

export function tasksOfTeam(world: WorldState, teamKey: TeamKey): Task[] {
  const out: Task[] = [];
  for (const t of world.tasks.values()) if (t.teamKey === teamKey) out.push(t);
  return out;
}

export function messagesOfTeam(world: WorldState, teamKey: TeamKey): TeamMessage[] {
  const out: TeamMessage[] = [];
  for (const m of world.messages.values()) if (m.teamKey === teamKey) out.push(m);
  out.sort((a, b) => a.at - b.at);
  return out;
}

/** Queue items not hidden by Later at `now` (wall clock), in the core's rank order. */
export function visibleQueueItems(world: WorldState, now: number = clock.wall()) {
  return world.queue.items.filter((item) => item.laterUntil === null || item.laterUntil <= now);
}

// ---------------------------------------------------------------------------
// Subscription driver (no React; the event bridge starts it once)
// ---------------------------------------------------------------------------

/** Backoff after a failed subscribe: 0.5 s doubling to 8 s. */
export function retryDelayMs(attempt: number): number {
  return Math.min(8000, 500 * 2 ** Math.max(0, attempt - 1));
}

export interface WorldSlice {
  world: WorldState;
  /** Subscribe (or resubscribe) to the core's store. Resolves when the snapshot is applied or the attempt failed. */
  subscribeWorld: () => Promise<void>;
  /** Drop the subscription (tests, window teardown). */
  unsubscribeWorld: () => void;
  /** Apply one patch from the live channel (exposed for tests and the showcase). */
  applyStorePatch: (patch: StorePatch) => void;
  /** Apply a snapshot directly (tests and the showcase). */
  applyStoreSnapshot: (snapshot: StoreSnapshot) => void;
}

/** Implemented by the UI slice: move every reference from a tombstone uid to its survivor. */
export interface MergeSink {
  remapSessionRefs?: (merged: ReadonlyMap<SessionUid, SessionUid>) => void;
}

interface SubscriptionState {
  generation: number;
  channel: Channel<StorePatch> | null;
  /** Patches that arrived before the snapshot. */
  buffer: StorePatch[] | null;
  attempt: number;
  retryTimer: ReturnType<typeof setTimeout> | null;
  inFlight: Promise<void> | null;
}

export const createWorldSlice: StateCreator<
  WorldSlice & MergeSink,
  [['zustand/subscribeWithSelector', never]],
  [],
  WorldSlice
> = (set, get) => {
  const sub: SubscriptionState = {
    generation: 0,
    channel: null,
    buffer: null,
    attempt: 0,
    retryTimer: null,
    inFlight: null,
  };

  const commit = (result: FoldResult) => {
    set({ world: result.world });
    if (result.merged.size > 0) get().remapSessionRefs?.(result.merged);
  };

  const resubscribe = () => {
    const w = get().world;
    set({ world: { ...w, status: 'resyncing', resyncCount: w.resyncCount + 1 } });
    void subscribe();
  };

  /** Apply a patch for subscription `generation`; ignores patches of a replaced channel. */
  const onPatch = (generation: number, patch: StorePatch) => {
    if (generation !== sub.generation) return;
    if (sub.buffer) {
      sub.buffer.push(patch);
      return;
    }
    applyLive(patch);
  };

  const applyLive = (patch: StorePatch) => {
    const world = get().world;
    if (world.status === 'resyncing' || world.status === 'subscribing') return;
    const decision = decidePatch(world.rev, patch);
    if (decision === 'stale') return;
    if (decision === 'gap') {
      resubscribe();
      return;
    }
    const result = foldOps(world, patch.ops);
    result.world.rev = patch.toRev;
    result.world.lastPatchAt = clock.now();
    commit(result);
    if (result.resync) resubscribe();
  };

  const scheduleRetry = () => {
    if (sub.retryTimer) clearTimeout(sub.retryTimer);
    sub.attempt += 1;
    sub.retryTimer = setTimeout(() => {
      sub.retryTimer = null;
      void subscribe();
    }, retryDelayMs(sub.attempt));
  };

  const subscribe = (): Promise<void> => {
    if (sub.retryTimer) {
      clearTimeout(sub.retryTimer);
      sub.retryTimer = null;
    }
    const generation = ++sub.generation;
    if (sub.channel) disposeChannel(sub.channel);
    sub.buffer = [];
    const channel = createChannel<StorePatch>((patch) => onPatch(generation, patch));
    sub.channel = channel;
    const w = get().world;
    if (w.status !== 'resyncing') set({ world: { ...w, status: 'subscribing' } });

    const run = commands.store_subscribe({}, channel).then(
      (snapshot) => {
        if (generation !== sub.generation) return;
        sub.attempt = 0;
        const buffered = sub.buffer ?? [];
        sub.buffer = null;
        commit(worldFromSnapshot(snapshot, get().world));
        for (const patch of buffered) {
          if (generation !== sub.generation) return;
          applyLive(patch);
        }
      },
      (error: IpcError) => {
        if (generation !== sub.generation) return;
        sub.buffer = null;
        const unavailable = isNotBuilt(error);
        set({ world: { ...get().world, status: unavailable ? 'unavailable' : 'retrying', lastError: error } });
        if (!unavailable) scheduleRetry();
      },
    );
    sub.inFlight = run;
    return run;
  };

  return {
    world: emptyWorld(),

    subscribeWorld: () => subscribe(),

    unsubscribeWorld: () => {
      sub.generation += 1;
      if (sub.retryTimer) clearTimeout(sub.retryTimer);
      sub.retryTimer = null;
      if (sub.channel) disposeChannel(sub.channel);
      sub.channel = null;
      sub.buffer = null;
      sub.attempt = 0;
      set({ world: { ...get().world, status: 'idle' } });
    },

    applyStorePatch: (patch) => applyLive(patch),

    applyStoreSnapshot: (snapshot) => {
      commit(worldFromSnapshot(snapshot, get().world));
    },
  };
};
