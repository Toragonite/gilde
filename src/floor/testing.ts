/**
 * Test helpers for the Floor (imported by `*.test.ts(x)` only): the
 * showcase's deterministic worlds as store snapshots, a "hard" world that
 * exercises every drawing path (two teams in one bay, bottlenecks, mail in
 * flight, busy traffic, a gone member, low quota), and an engine mounted on a
 * DOM canvas of a given width.
 */

import type { Locale } from '../i18n';
import type { Account } from '../lib/gen/Account';
import type { MessageKey } from '../lib/gen/MessageKey';
import type { StoreSnapshot } from '../lib/gen/StoreSnapshot';
import type { TeamMessage } from '../lib/gen/TeamMessage';
import { buildTimeline, snapshotAt } from '../showcase/timeline';
import { buildWorld } from '../showcase/world';
import type { TeamKind } from '../showcase/world';
import { createAppStore } from '../store/useAppStore';
import { FloorEngine } from './engine';
import type { FloorScope } from './layout';
import { ReplayRing } from './ring';
import { makeWords } from './words';

export const T0 = 1_792_000_000_000;

/** The fixture sizes of the M16 gate (scripts/gates/browser-gates.mjs, showcase scenes). */
export const FLOOR_FIXTURES: readonly { name: string; agents: number; teams: readonly TeamKind[] }[] = [
  { name: 'floor-1', agents: 1, teams: [] },
  { name: 'floor-8', agents: 8, teams: ['native'] },
  { name: 'floor-20', agents: 20, teams: ['nativeInferred', 'orchestrator'] },
  { name: 'floor-31', agents: 31, teams: ['native', 'nativeInferred', 'orchestrator'] },
  { name: 'team-floor', agents: 12, teams: ['native'] },
];

export const SAMPLE_TIMES = [0, 5, 14, 15, 27, 30] as const;

export function showcaseSnapshot(agents: number, teams: readonly TeamKind[], atMs: number, leftovers = 0): StoreSnapshot {
  const world = buildWorld({ agents, teams, pending: true, leftovers, history: 0 }, T0);
  return snapshotAt(world, buildTimeline(world), atMs).snapshot;
}

/** Every drawing path at once. */
export function hardSnapshot(): StoreSnapshot {
  const s = showcaseSnapshot(40, ['native', 'nativeInferred', 'orchestrator', 'native'], 15_000, 1);
  const p0 = s.projects[0]?.id ?? null;
  // two teams (and more) in one bay
  s.teams = s.teams.map((t, i) => ({ ...t, projectId: i < 2 ? p0 : t.projectId }));
  const inBay = new Set(s.teams.filter((t) => t.projectId === p0).map((t) => t.key));
  s.sessions = s.sessions.map((x) => (x.team && inBay.has(x.team.teamKey) ? { ...x, projectId: p0 } : x));
  // bottlenecks, mail in flight, busy traffic
  const msgs: TeamMessage[] = [];
  let n = 0;
  s.teams = s.teams.map((t) => {
    const ms = s.members.filter((m) => m.teamKey === t.key);
    const second = ms[1];
    const third = ms[2];
    for (let i = 0; i < ms.length; i++) {
      for (let j = 0; j < ms.length; j++) {
        if (i === j || (i + j) % 3 !== 0) continue;
        const from = ms[i];
        const to = ms[j];
        if (!from || !to) continue;
        for (let k = 0; k <= (i % 3) * 3; k++) {
          msgs.push({
            key: `g_${(++n).toString(16).padStart(16, '0')}` as MessageKey,
            teamKey: t.key,
            from: { kind: 'member', member: from.key },
            to: { kind: 'member', member: to.key },
            at: T0 + 15_000 - (k + 1) * 20_000,
            kind: 'plain',
            status: k === 0 && j === 2 ? 'queued' : 'delivered',
            inReplyTo: null,
            derived: false,
            idleReason: null,
            hasSummary: false,
          });
        }
      }
    }
    return { ...t, progress: { ...t.progress, inFlight: 2, bottlenecks: [second?.key, third?.key].filter((x): x is NonNullable<typeof x> => !!x) } };
  });
  s.messages = [...s.messages, ...msgs];
  // a gone member (its session ended)
  const goner = s.members.find((m) => m.role === 'member');
  if (goner) {
    s.members = s.members.map((m) => (m.key === goner.key ? { ...m, presence: { kind: 'gone', reason: 'shutdownApproved', at: T0 } } : m));
    s.sessions = s.sessions.map((x) =>
      x.uid === goner.sessionUid ? { ...x, activity: { ...x.activity, state: { kind: 'ended' } }, control: { kind: 'notRunning', lastOrigin: null, endedHow: 'ended' }, endedAt: T0 } : x,
    );
  }
  // low quota on every account
  s.accounts = s.accounts.map((a: Account) => ({ ...a, quota: { windows: [{ kind: 'fiveHour', label: null, usedPct: 92, resetsAt: null }], fetchedAt: T0, stale: false, unavailable: null } }));
  return s;
}

export interface Mounted {
  engine: FloorEngine;
  store: ReturnType<typeof createAppStore>;
  ring: ReplayRing;
  root: HTMLElement;
  canvas: HTMLCanvasElement;
  unmount: () => void;
}

/** An engine on a detached-from-layout DOM canvas whose wrapper is `width` CSS px wide. */
export function mountEngine(snapshot: StoreSnapshot, opts: { width?: number; height?: number; scope?: FloorScope; locale?: Locale } = {}): Mounted {
  const store = createAppStore();
  store.getState().applyStoreSnapshot(snapshot);
  const ring = new ReplayRing();
  ring.record(store.getState().world, T0);
  const engine = new FloorEngine(store, ring);
  const root = document.createElement('div');
  root.className = 'fl-root';
  const scroll = document.createElement('div');
  const wrap = document.createElement('div');
  const canvas = document.createElement('canvas');
  Object.defineProperty(wrap, 'clientWidth', { value: opts.width ?? 1408, configurable: true });
  Object.defineProperty(scroll, 'clientHeight', { value: opts.height ?? 760, configurable: true });
  wrap.appendChild(canvas);
  scroll.appendChild(wrap);
  root.appendChild(scroll);
  document.body.appendChild(root);
  engine.mount({ canvas, root, wrap, scroll, counter: null }, opts.scope ?? { kind: 'global' }, makeWords(opts.locale ?? 'ko'));
  return {
    engine,
    store,
    ring,
    root,
    canvas,
    unmount: () => {
      engine.unmountCanvas();
      root.remove();
    },
  };
}
