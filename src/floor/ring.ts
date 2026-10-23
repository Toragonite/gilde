/**
 * The replay ring (pixel-office §2.7, contract §7.5): an in-memory ring of
 * store change events — ids, states, stations, message statuses, waits and
 * timestamps only, never content — built in the WebView from store patches.
 * It keeps the last 60 minutes and at most 20,000 events, is never persisted
 * and is dropped with the window.
 *
 * Replay rebuilds the structure of a moment by applying the events up to it
 * to the ring's base (the state just before its oldest kept event); nothing
 * is re-read. The flow strip sweeps the same events across its 10 minutes.
 */

import { clock } from '../lib/clock';
import type { DeliveryStatus } from '../lib/gen/DeliveryStatus';
import type { MemberKey } from '../lib/gen/MemberKey';
import type { MessageKey } from '../lib/gen/MessageKey';
import type { SessionUid } from '../lib/gen/SessionUid';
import type { Station } from '../lib/gen/Station';
import type { TaskKey } from '../lib/gen/TaskKey';
import type { WorldState } from '../store/worldSlice';
import type { FlState, Structure } from './model';

export const RING_MAX_EVENTS = 20_000;
export const RING_MAX_AGE_MS = 60 * 60_000;

type SessionFact = { kind: FlState; since: number; alive: boolean };
type TaskFact = { station: Station; owner: MemberKey | null };

export type RingEvent =
  | { t: number; k: 's'; id: SessionUid; v: SessionFact | null }
  | { t: number; k: 'p'; id: MemberKey; v: boolean }
  | { t: number; k: 't'; id: TaskKey; v: TaskFact | null }
  | { t: number; k: 'm'; id: MessageKey; v: DeliveryStatus | null }
  | { t: number; k: 'w'; v: SessionUid[] };

export function emptyStructure(t: number): Structure {
  return { t, sessions: new Map(), members: new Set(), tasks: new Map(), msgs: new Map(), waits: [] };
}

export function cloneStructure(s: Structure): Structure {
  return { t: s.t, sessions: new Map(s.sessions), members: new Set(s.members), tasks: new Map(s.tasks), msgs: new Map(s.msgs), waits: s.waits.slice() };
}

/** The structure the store holds now. */
export function structureOf(world: WorldState, t: number): Structure {
  const s = emptyStructure(t);
  for (const x of world.sessions.values()) s.sessions.set(x.uid, { kind: x.activity.state.kind, since: x.activity.since, alive: x.control.kind !== 'notRunning' });
  for (const m of world.members.values()) s.members.add(m.key);
  for (const k of world.tasks.values()) if (k.status !== 'deleted') s.tasks.set(k.key, { station: k.station, owner: k.owner });
  for (const m of world.messages.values()) s.msgs.set(m.key, m.status);
  for (const i of world.queue.items) {
    if (i.band <= 2 && i.subject.kind === 'session' && (i.laterUntil === null || i.laterUntil <= t)) s.waits.push(i.subject.uid);
  }
  return s;
}

export function applyEvent(s: Structure, e: RingEvent): void {
  s.t = e.t;
  switch (e.k) {
    case 's':
      if (e.v) s.sessions.set(e.id, e.v);
      else s.sessions.delete(e.id);
      break;
    case 'p':
      if (e.v) s.members.add(e.id);
      else s.members.delete(e.id);
      break;
    case 't':
      if (e.v) s.tasks.set(e.id, e.v);
      else s.tasks.delete(e.id);
      break;
    case 'm':
      if (e.v) s.msgs.set(e.id, e.v);
      else s.msgs.delete(e.id);
      break;
    case 'w':
      s.waits = e.v.slice();
      break;
  }
}

/** The events that turn `a` into `b`, stamped `t`. */
export function diffStructures(a: Structure, b: Structure, t: number): RingEvent[] {
  const out: RingEvent[] = [];
  for (const [id, v] of b.sessions) {
    const o = a.sessions.get(id);
    if (!o || o.kind !== v.kind || o.since !== v.since || o.alive !== v.alive) out.push({ t, k: 's', id, v });
  }
  for (const id of a.sessions.keys()) if (!b.sessions.has(id)) out.push({ t, k: 's', id, v: null });
  for (const id of b.members) if (!a.members.has(id)) out.push({ t, k: 'p', id, v: true });
  for (const id of a.members) if (!b.members.has(id)) out.push({ t, k: 'p', id, v: false });
  for (const [id, v] of b.tasks) {
    const o = a.tasks.get(id);
    if (!o || o.station !== v.station || o.owner !== v.owner) out.push({ t, k: 't', id, v });
  }
  for (const id of a.tasks.keys()) if (!b.tasks.has(id)) out.push({ t, k: 't', id, v: null });
  for (const [id, v] of b.msgs) if (a.msgs.get(id) !== v) out.push({ t, k: 'm', id, v });
  for (const id of a.msgs.keys()) if (!b.msgs.has(id)) out.push({ t, k: 'm', id, v: null });
  if (a.waits.length !== b.waits.length || a.waits.some((u, i) => u !== b.waits[i])) out.push({ t, k: 'w', v: b.waits.slice() });
  return out;
}

export class ReplayRing {
  private base: Structure | null = null;
  private last: Structure | null = null;
  private events: RingEvent[] = [];
  private head = 0;
  /** Bumped on every recorded change (the Floor's redraw signature). */
  version = 0;

  constructor(
    private readonly maxEvents = RING_MAX_EVENTS,
    private readonly maxAgeMs = RING_MAX_AGE_MS,
  ) {}

  get size(): number {
    return this.events.length - this.head;
  }

  /** Time of the oldest moment replay can show. */
  get start(): number | null {
    return this.base ? this.base.t : null;
  }

  /** Record the store's structure at wall time `t`. */
  record(world: WorldState, t: number = clock.wall()): void {
    const now = structureOf(world, t);
    if (!this.last || !this.base) {
      this.base = now;
      this.last = cloneStructure(now);
      this.version++;
      return;
    }
    const evs = diffStructures(this.last, now, Math.max(t, this.last.t));
    if (evs.length === 0) return;
    for (const e of evs) {
      this.events.push(e);
      applyEvent(this.last, e);
    }
    this.version++;
    this.evict(t);
  }

  private evict(now: number): void {
    const base = this.base;
    if (!base) return;
    while (this.head < this.events.length) {
      const e = this.events[this.head] as RingEvent;
      if (this.events.length - this.head <= this.maxEvents && e.t >= now - this.maxAgeMs) break;
      applyEvent(base, e);
      this.head++;
    }
    if (this.head > 4096 && this.head * 2 > this.events.length) {
      this.events = this.events.slice(this.head);
      this.head = 0;
    }
  }

  /** The structure at wall time `T` (clamped to what the ring holds). */
  at(T: number): Structure | null {
    if (!this.base) return null;
    const s = cloneStructure(this.base);
    for (let i = this.head; i < this.events.length; i++) {
      const e = this.events[i] as RingEvent;
      if (e.t > T) break;
      applyEvent(s, e);
    }
    s.t = Math.max(s.t, Math.min(T, this.last?.t ?? T));
    return s;
  }

  /**
   * Walk the window [T0, T1]: `visit` sees the structure at T0 (with `ev`
   * null), then the same structure after each event inside the window, with
   * the event and the value it replaced.
   */
  sweep(T0: number, T1: number, visit: (t: number, s: Structure, ev: RingEvent | null, prev: unknown) => void): void {
    if (!this.base) return;
    const s = cloneStructure(this.base);
    let i = this.head;
    for (; i < this.events.length; i++) {
      const e = this.events[i] as RingEvent;
      if (e.t > T0) break;
      applyEvent(s, e);
    }
    visit(Math.max(T0, this.base.t), s, null, null);
    for (; i < this.events.length; i++) {
      const e = this.events[i] as RingEvent;
      if (e.t > T1) break;
      const prev = e.k === 's' ? s.sessions.get(e.id) : e.k === 't' ? s.tasks.get(e.id) : e.k === 'm' ? s.msgs.get(e.id) : null;
      applyEvent(s, e);
      visit(e.t, s, e, prev ?? null);
    }
  }

  /** Approximate memory in bytes (budget ≤ 2 MB, pixel-office §8). */
  approxBytes(): number {
    return this.size * 96;
  }

  clear(): void {
    this.base = null;
    this.last = null;
    this.events = [];
    this.head = 0;
    this.version++;
  }
}

/** The Floor's ring: recorded from the moment the Floor module loads (the app's start), visible or not. */
export const floorRing = new ReplayRing();

let stopRing: (() => void) | null = null;

interface RingStore {
  getState(): { world: WorldState };
  subscribe(listener: (state: { world: WorldState }, prev: { world: WorldState }) => void): () => void;
}

/** Start recording `store` into the ring (idempotent). Returns the stop function. */
export function startFloorRing(store: RingStore, ring: ReplayRing = floorRing): () => void {
  if (ring === floorRing && stopRing) return stopRing;
  ring.record(store.getState().world);
  const unsub = store.subscribe((state, prev) => {
    if (state.world !== prev.world) ring.record(state.world);
  });
  const stop = () => {
    unsub();
    if (ring === floorRing) stopRing = null;
  };
  if (ring === floorRing) stopRing = stop;
  return stop;
}
