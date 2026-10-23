/**
 * The Floor's view model: everything it draws, derived from T13's store mirror
 * only (contract §7.5: Project, Team, Member, Session.activity, Task.station,
 * TeamMessage edges, TeamProgress incl. the Rust-computed bottlenecks, and the
 * attention queue). The Floor never invents a state: stations are
 * `Task.station` as Rust derived them, bottlenecks are `TeamProgress.bottlenecks`,
 * counts are the queue's `needYou`/`also` [D7, I6]. Missing data stays missing
 * (pixel-office G2): no owner → no tray, no `blockedBy` → no rack, no
 * `inFlight` → no envelopes.
 *
 * In replay the same builder runs with a `Structure` from the replay ring: the
 * states, stations, message statuses and waits of that moment override the
 * live values (names and colours stay live; nothing else is re-read).
 */

import type { ActivityState } from '../lib/gen/ActivityState';
import type { AttentionItem } from '../lib/gen/AttentionItem';
import type { DeliveryStatus } from '../lib/gen/DeliveryStatus';
import type { IdentityColour } from '../lib/gen/IdentityColour';
import type { Member } from '../lib/gen/Member';
import type { MemberKey } from '../lib/gen/MemberKey';
import type { MemberRole } from '../lib/gen/MemberRole';
import type { MessageKey } from '../lib/gen/MessageKey';
import type { Origin } from '../lib/gen/Origin';
import type { ProjectId } from '../lib/gen/ProjectId';
import type { Session } from '../lib/gen/Session';
import type { SessionUid } from '../lib/gen/SessionUid';
import type { Station } from '../lib/gen/Station';
import type { Task } from '../lib/gen/Task';
import type { TaskKey } from '../lib/gen/TaskKey';
import type { Team } from '../lib/gen/Team';
import type { TeamKey } from '../lib/gen/TeamKey';
import type { TeamSource } from '../lib/gen/TeamSource';
import type { Vendor } from '../lib/gen/Vendor';
import type { WorldState } from '../store/worldSlice';
import { translate } from '../i18n';
import { GHOST_GRACE_MS } from './constants';

export type FlState = ActivityState['kind'];

/** Where a card sits (pixel-office §2.1); `done` covers Done and Reported. */
export type FlStation = 'todo' | 'blocked' | 'assigned' | 'doing' | 'you' | 'done';

export const STATION_OF: Readonly<Record<Station, FlStation>> = {
  toDo: 'todo',
  blocked: 'blocked',
  assigned: 'assigned',
  inProgress: 'doing',
  needsYou: 'you',
  done: 'done',
  reported: 'done',
};

/** Queue band per state (M4): the nameplate of bands 1–2 is drawn in `--warn`. */
export const BAND_OF: Readonly<Partial<Record<FlState, number>>> = {
  needsApproval: 1,
  needsAnswer: 2,
  error: 3,
  crashed: 3,
  noProgress: 4,
  newReply: 5,
};

/** Worst first (band order), for a team's lane on the global strip. */
export const WORST: readonly FlState[] = ['needsApproval', 'needsAnswer', 'error', 'crashed', 'noProgress', 'newReply', 'working', 'starting', 'ready', 'statusUnknown'];

const CROSS = translate('en', 'floor.glyph.cross');

/** The nameplate glyph per state (pixel-office §4.3, F3). */
export const GLYPH_CH: Readonly<Record<FlState, string>> = {
  starting: '◌',
  working: '◉',
  needsApproval: '▲',
  needsAnswer: '?',
  error: CROSS,
  crashed: CROSS,
  noProgress: '~',
  newReply: '●',
  ready: '○',
  ended: '◌',
  stopped: '◌',
  statusUnknown: '⊙',
};

/** One character (a drawn seat). */
export interface FlSeat {
  /** Seat key: the MemberKey for team members, the SessionUid for standalone sessions. */
  id: string;
  /** The session a click opens (`null`: an in-process member without a session yet). */
  uid: SessionUid | null;
  name: string;
  vendor: Vendor;
  colour: IdentityColour | null;
  state: FlState;
  /** Wall ms the state began. */
  since: number;
  /** `Likely` confidence (the word gets "(추정)"). */
  likely: boolean;
  /** Watching: a glass pane, view only. */
  watch: boolean;
  ownerApp: string | null;
  origin: Origin | null;
  alive: boolean;
  /** Ended or stopped: drawn as a ghost with the chair pushed back. */
  ghost: boolean;
  projectId: ProjectId | null;
  teamKey: TeamKey | null;
  role: MemberRole | null;
  /** Percent of quota left on the session's account (the battery under 20 %). */
  quotaLeft: number | null;
  errorSub: string | null;
  startedAt: number | null;
}

export interface FlItem {
  key: TaskKey;
  title: string;
  inferred: boolean;
  station: FlStation;
  /** Rust's station word (Reported for inferred items). */
  rawStation: Station;
  owner: MemberKey | null;
  /** The first declared blocker that is not done (declared only, never inferred). */
  blockedBy: TaskKey | null;
  /** In progress since (wall ms); `approx` = Gilde saw it late ("≥ {t}"). */
  started: number | null;
  approx: boolean;
  completedAt: number | null;
  /** Done duration, when both ends are known. */
  took: number | null;
  /** An inferred item open while its member has been Ready ≥ 10 min (F-13). */
  noReport: boolean;
}

export interface FlMsg {
  key: MessageKey;
  from: MemberKey;
  to: MemberKey;
  at: number;
  status: DeliveryStatus;
  /** An unread inbox entry (only when the team has in-flight signals). */
  inFlight: boolean;
}

export interface FlTeam {
  key: TeamKey;
  name: string;
  projectId: ProjectId | null;
  source: TeamSource;
  /** Inferred items (native team without a task list, §2.10). */
  inferred: boolean;
  /** Counts carry "≈" (only inboxes were readable). */
  approx: boolean;
  /** No task list and nothing inferred: the wall shows state counts and "작업 목록 없음". */
  noTaskList: boolean;
  lead: MemberKey | null;
  /** Desks in line order: lead first, then join order (I5); every member, gone ones too. */
  line: MemberKey[];
  items: FlItem[];
  msgs: FlMsg[];
  done: number;
  total: number;
  /** `TeamProgress.inFlight` (`null` = no in-flight signal: the header omits it). */
  inFlight: number | null;
  /** Rust-computed bottleneck members (§3.4, pixel-office §2.3). */
  bottlenecks: MemberKey[];
  startedAt: number | null;
  /** Any member alive (the global Floor shows only these teams). */
  alive: boolean;
}

export interface FlProject {
  id: ProjectId;
  name: string;
}

export interface FlVm {
  /** Wall ms this model describes. */
  now: number;
  seats: Map<string, FlSeat>;
  seatOfUid: Map<SessionUid, string>;
  teams: FlTeam[];
  projects: FlProject[];
  /** Standalone sessions shown on the global Floor, by project, in stable order. */
  solo: string[];
  /** Visible queue items in queue order (Later-ed ones hidden). */
  queue: AttentionItem[];
  needYou: number;
  also: number;
  /** Sessions in bands 1–2 (the "you" lane and the counter). */
  waits: SessionUid[];
}

/** What the replay ring holds for one moment: structure only, never content. */
export interface Structure {
  t: number;
  sessions: Map<SessionUid, { kind: FlState; since: number; alive: boolean }>;
  members: Set<MemberKey>;
  tasks: Map<TaskKey, { station: Station; owner: MemberKey | null }>;
  msgs: Map<MessageKey, DeliveryStatus>;
  waits: SessionUid[];
}

export interface VmOptions {
  /** A standalone session with no title: "{vendor} 세션". */
  untitled: (vendor: Vendor) => string;
  /** An inferred item's title: "지시 {k}". */
  request: (k: number) => string;
  /** The structure of a past moment (replay), or `null` for live. */
  structure?: Structure | null;
}

const HANDOFF_KINDS = new Set(['plain', 'taskAssignment', 'idleNotification']);

/** Collapse whitespace and control characters (a name is one line on the canvas). */
export function oneLine(s: string): string {
  let out = '';
  for (const ch of s) {
    const c = ch.codePointAt(0) ?? 0;
    out += c < 32 || c === 127 || (c >= 0x2028 && c <= 0x2029) ? ' ' : ch;
  }
  return out.replace(/\s+/g, ' ').trim();
}

function quotaLeft(world: WorldState, s: Session): number | null {
  if (!s.accountId) return null;
  const a = world.accounts.get(s.accountId);
  const ws = a?.quota?.windows ?? [];
  let used: number | null = null;
  for (const w of ws) if (w.usedPct !== null && w.usedPct >= 0 && w.usedPct <= 100) used = Math.max(used ?? 0, w.usedPct);
  return used === null ? null : 100 - used;
}

function seatFromSession(world: WorldState, s: Session, id: string, opts: VmOptions): FlSeat {
  const st = opts.structure?.sessions.get(s.uid);
  const kind: FlState = st ? st.kind : s.activity.state.kind;
  const alive = st ? st.alive : s.control.kind !== 'notRunning';
  const control = s.control;
  return {
    id,
    uid: s.uid,
    name: oneLine(s.title ?? '') || opts.untitled(s.vendor),
    vendor: s.vendor,
    colour: s.team?.colour ?? null,
    state: kind,
    since: st ? st.since : s.activity.since,
    likely: !st && s.activity.confidence === 'likely',
    watch: control.kind === 'watching',
    ownerApp: control.kind === 'watching' ? (control.owner?.appName ?? null) : null,
    origin: control.kind === 'watching' ? control.origin : control.kind === 'notRunning' ? control.lastOrigin : null,
    alive,
    ghost: kind === 'ended' || kind === 'stopped',
    projectId: s.projectId,
    teamKey: s.team?.teamKey ?? null,
    role: s.team?.role ?? null,
    quotaLeft: alive ? quotaLeft(world, s) : null,
    errorSub: s.activity.state.kind === 'error' ? s.activity.state.sub.kind : null,
    startedAt: s.startedAt,
  };
}

/** Members in line order: the lead first, then `joinedAt`, then key (I5). */
export function lineOrder(team: Team, members: readonly Member[]): Member[] {
  return members.slice().sort((a, b) => {
    const la = a.key === team.lead || a.role === 'lead' ? 0 : 1;
    const lb = b.key === team.lead || b.role === 'lead' ? 0 : 1;
    if (la !== lb) return la - lb;
    const ja = a.joinedAt ?? Number.MAX_SAFE_INTEGER;
    const jb = b.joinedAt ?? Number.MAX_SAFE_INTEGER;
    if (ja !== jb) return ja - jb;
    return a.key < b.key ? -1 : a.key > b.key ? 1 : 0;
  });
}

function taskOrder(a: Task, b: Task): number {
  const ca = a.createdAt ?? Number.MAX_SAFE_INTEGER;
  const cb = b.createdAt ?? Number.MAX_SAFE_INTEGER;
  if (ca !== cb) return ca - cb;
  const na = a.nativeId !== null ? Number(a.nativeId) : Number.NaN;
  const nb = b.nativeId !== null ? Number(b.nativeId) : Number.NaN;
  if (Number.isFinite(na) && Number.isFinite(nb) && na !== nb) return na - nb;
  if (a.inferredK !== null && b.inferredK !== null && a.inferredK !== b.inferredK) return a.inferredK - b.inferredK;
  return a.key < b.key ? -1 : a.key > b.key ? 1 : 0;
}

/** Build the view model of `world` at wall time `now` (or of a replayed moment). */
export function buildVm(world: WorldState, now: number, opts: VmOptions): FlVm {
  const S = opts.structure ?? null;
  const seats = new Map<string, FlSeat>();
  const seatOfUid = new Map<SessionUid, string>();

  // Members by team, tasks by team, messages by team (one pass each).
  const membersBy = new Map<TeamKey, Member[]>();
  for (const m of world.members.values()) {
    if (S && !S.members.has(m.key)) continue;
    const list = membersBy.get(m.teamKey);
    if (list) list.push(m);
    else membersBy.set(m.teamKey, [m]);
  }
  const tasksBy = new Map<TeamKey, Task[]>();
  for (const t of world.tasks.values()) {
    if (t.status === 'deleted') continue;
    if (S && !S.tasks.has(t.key)) continue;
    const list = tasksBy.get(t.teamKey);
    if (list) list.push(t);
    else tasksBy.set(t.teamKey, [t]);
  }

  const teams: FlTeam[] = [];
  const teamKeys = new Set<TeamKey>();
  for (const team of world.teams.values()) {
    const members = lineOrder(team, membersBy.get(team.key) ?? []);
    if (members.length === 0) continue;
    teamKeys.add(team.key);
    const memberSet = new Set(members.map((m) => m.key));
    let alive = false;
    for (const m of members) {
      const s = m.sessionUid ? world.sessions.get(m.sessionUid) : undefined;
      let seat: FlSeat;
      if (s && (!S || S.sessions.has(s.uid))) {
        seat = seatFromSession(world, s, m.key, opts);
        seat.name = oneLine(m.name) || seat.name;
        seat.colour = m.colour ?? seat.colour;
        seat.vendor = m.vendor;
        seat.teamKey = team.key;
        seat.role = m.role;
        if (m.presence.kind === 'gone' && !seat.ghost) {
          seat.alive = false;
        }
        seatOfUid.set(s.uid, m.key);
      } else {
        // No session to read (an in-process member without a transcript yet): Status unknown (G2).
        seat = {
          id: m.key,
          uid: null,
          name: oneLine(m.name) || m.key,
          vendor: m.vendor,
          colour: m.colour,
          state: m.presence.kind === 'gone' ? 'ended' : 'statusUnknown',
          since: m.joinedAt ?? now,
          likely: false,
          watch: false,
          ownerApp: null,
          origin: null,
          alive: m.presence.kind === 'present',
          ghost: m.presence.kind === 'gone',
          projectId: team.projectId,
          teamKey: team.key,
          role: m.role,
          quotaLeft: null,
          errorSub: null,
          startedAt: m.joinedAt,
        };
      }
      if (seat.alive) alive = true;
      seats.set(m.key, seat);
    }

    const tasks = (tasksBy.get(team.key) ?? []).slice().sort(taskOrder);
    const stationOfTask = (t: Task): Station => S?.tasks.get(t.key)?.station ?? t.station;
    const ownerOfTask = (t: Task): MemberKey | null => {
      const o = S ? (S.tasks.get(t.key)?.owner ?? null) : t.owner;
      return o && memberSet.has(o) ? o : null;
    };
    const byKey = new Map(tasks.map((t) => [t.key, t] as const));
    const items: FlItem[] = tasks.map((t) => {
      const raw = stationOfTask(t);
      const station = STATION_OF[raw];
      const owner = ownerOfTask(t);
      let blockedBy: TaskKey | null = null;
      if (!t.inferred) {
        for (const b of t.blockedBy) {
          const bt = byKey.get(b);
          if (bt && STATION_OF[stationOfTask(bt)] !== 'done') {
            blockedBy = b;
            break;
          }
        }
      }
      const started = t.firstSeenInProgressAt ?? t.startedAt;
      const ownerSeat = owner ? seats.get(owner) : undefined;
      const noReport =
        t.inferred &&
        (station === 'assigned' || station === 'doing') &&
        !!ownerSeat &&
        ownerSeat.state === 'ready' &&
        now - ownerSeat.since >= 10 * 60_000;
      return {
        key: t.key,
        title: t.title !== null ? oneLine(t.title) : opts.request(t.inferredK ?? 0),
        inferred: t.inferred,
        station,
        rawStation: raw,
        owner,
        blockedBy,
        started: station === 'doing' || station === 'you' || station === 'done' ? started : null,
        approx: t.firstSeenInProgressAt !== null,
        completedAt: t.completedAt,
        took: t.completedAt !== null && t.startedAt !== null ? Math.max(0, t.completedAt - t.startedAt) : null,
        noReport,
      };
    });

    const inFlightSignals = team.progress.inFlight !== null;
    const msgs: FlMsg[] = [];
    for (const m of world.messages.values()) {
      if (m.teamKey !== team.key || !HANDOFF_KINDS.has(m.kind)) continue;
      if (m.from.kind !== 'member' || m.to.kind !== 'member' || m.from.member === m.to.member) continue;
      if (!memberSet.has(m.from.member) || !memberSet.has(m.to.member)) continue;
      if (S && !S.msgs.has(m.key)) continue;
      const status = S ? (S.msgs.get(m.key) ?? m.status) : m.status;
      msgs.push({ key: m.key, from: m.from.member, to: m.to.member, at: m.at, status, inFlight: inFlightSignals && status === 'queued' });
    }
    msgs.sort((a, b) => a.at - b.at || (a.key < b.key ? -1 : 1));

    const inferred = team.progress.inferred || team.caps.observeTasks === 'inferred' || tasks.some((t) => t.inferred);
    const noTaskList = !inferred && items.length === 0 && (team.caps.observeTasks === 'no' || team.caps.observeTasks === 'unknown');
    const done = items.filter((i) => i.station === 'done').length;
    teams.push({
      key: team.key,
      name: oneLine(team.name) || team.rawId,
      projectId: team.projectId,
      source: team.source,
      inferred,
      approx: team.progress.approx,
      noTaskList,
      lead: team.lead,
      line: members.map((m) => m.key),
      items,
      msgs,
      done: S ? done : team.progress.done,
      total: S ? items.length : team.progress.total,
      inFlight: S ? (inFlightSignals ? msgs.filter((m) => m.inFlight).length : null) : team.progress.inFlight,
      bottlenecks: team.progress.bottlenecks.filter((k) => memberSet.has(k)),
      startedAt: team.startedAt,
      alive,
    });
  }
  teams.sort((a, b) => (a.startedAt ?? 0) - (b.startedAt ?? 0) || (a.key < b.key ? -1 : 1));

  // Standalone sessions: alive, crashed (its band-3 ticket stays), or a ghost within the grace.
  const soloSessions: Session[] = [];
  for (const s of world.sessions.values()) {
    if (seatOfUid.has(s.uid)) continue;
    if (s.team && teamKeys.has(s.team.teamKey)) continue;
    if (S && !S.sessions.has(s.uid)) continue;
    const st = S?.sessions.get(s.uid);
    const kind = st ? st.kind : s.activity.state.kind;
    const alive = st ? st.alive : s.control.kind !== 'notRunning';
    const ghostAge = s.endedAt !== null ? now - s.endedAt : now - (st ? st.since : s.activity.since);
    if (!alive && kind !== 'crashed' && !((kind === 'ended' || kind === 'stopped') && ghostAge < GHOST_GRACE_MS)) continue;
    soloSessions.push(s);
  }
  soloSessions.sort((a, b) => (a.startedAt ?? 0) - (b.startedAt ?? 0) || (a.uid < b.uid ? -1 : 1));
  const solo: string[] = [];
  for (const s of soloSessions) {
    const seat = seatFromSession(world, s, s.uid, opts);
    seats.set(s.uid, seat);
    seatOfUid.set(s.uid, s.uid);
    solo.push(s.uid);
  }

  const projects = [...world.projects.values()]
    .sort((a, b) => a.order - b.order || (a.name < b.name ? -1 : a.name > b.name ? 1 : 0))
    .map((p) => ({ id: p.id, name: oneLine(p.name) || p.id }));

  const queue = world.queue.items.filter((item) => item.laterUntil === null || item.laterUntil <= now);
  const waits = S
    ? S.waits.slice()
    : queue.filter((i) => i.band <= 2 && i.subject.kind === 'session').map((i) => (i.subject as { uid: SessionUid }).uid);

  return { now, seats, seatOfUid, teams, projects, solo, queue, needYou: world.queue.needYou, also: world.queue.also, waits };
}

// ---------------------------------------------------------------------------
// Derived facts the Floor labels (pure; Board and Floor agree because the
// rules that decide them run in Rust — here only the words are chosen)
// ---------------------------------------------------------------------------

export function itemsOf(team: FlTeam, owner: MemberKey, station: FlStation): FlItem[] {
  return team.items.filter((i) => i.owner === owner && i.station === station);
}

/** The member's current card (in progress, or waiting on you). */
export function currentItem(team: FlTeam, owner: MemberKey): FlItem | null {
  return team.items.find((i) => i.owner === owner && (i.station === 'doing' || i.station === 'you')) ?? null;
}

/** Tasks that wait on `item`, directly or through a chain of declared dependencies. */
export function downstream(team: FlTeam, item: FlItem): number {
  const seen = new Set<TaskKey>([item.key]);
  let n = 0;
  let grew = true;
  while (grew) {
    grew = false;
    for (const k of team.items) {
      if (!seen.has(k.key) && k.blockedBy && seen.has(k.blockedBy) && k.station !== 'done') {
        seen.add(k.key);
        n++;
        grew = true;
      }
    }
  }
  return n;
}

export interface BottleneckWhy {
  n: number;
  why: 'blocks' | 'tray' | 'slow';
}

/**
 * Why a member Rust marked as a bottleneck is one, and the count its label
 * shows (§2.3): blocks ≥ 2, else tray ≥ 2, else the 3× median rule (n = 1).
 * `null` when Rust did not mark the member.
 */
export function bottleneckOf(team: FlTeam, member: MemberKey): BottleneckWhy | null {
  if (!team.bottlenecks.includes(member)) return null;
  const cur = currentItem(team, member);
  const blocks = cur && !team.inferred ? downstream(team, cur) : 0;
  if (blocks >= 2) return { n: blocks, why: 'blocks' };
  const tray = itemsOf(team, member, 'assigned').length;
  if (tray >= 2) return { n: tray, why: 'tray' };
  return { n: 1, why: 'slow' };
}

/** The counter's bottleneck (§2.3 row 4): ≥ 2 items in bands 1–2 and the oldest waited ≥ 5 min. */
export function counterBottleneck(vm: FlVm): { n: number; age: number } | null {
  const w = vm.queue.filter((i) => i.band <= 2 && i.subject.kind === 'session');
  if (w.length < 2) return null;
  const age = Math.max(...w.map((i) => vm.now - i.since));
  return age >= 5 * 60_000 ? { n: w.length, age } : null;
}

/** The card a session's ticket carries (its task waiting on you). */
export function cardAtCounter(vm: FlVm, uid: SessionUid): FlItem | null {
  const seatId = vm.seatOfUid.get(uid);
  if (!seatId) return null;
  for (const team of vm.teams) {
    const it = team.items.find((i) => i.owner === seatId && i.station === 'you');
    if (it) return it;
  }
  return null;
}

/** Items in the order the twin and the task table list them. */
export function orderedItems(team: FlTeam): FlItem[] {
  const o: Record<FlStation, number> = { you: 0, doing: 1, assigned: 2, blocked: 3, todo: 4, done: 5 };
  return team.items.slice().sort((a, b) => o[a.station] - o[b.station]);
}

/** Messages in flight to a member (envelopes at its desk). */
export function mailTo(team: FlTeam, member: MemberKey): number {
  let n = 0;
  for (const m of team.msgs) if (m.to === member && m.inFlight) n++;
  return n;
}

export function teamOf(vm: FlVm, key: TeamKey | null): FlTeam | null {
  if (!key) return null;
  return vm.teams.find((t) => t.key === key) ?? null;
}

/** State counts for a team without a task list ("◉ 2 · ▲ 1 · ○ 1", §2.9). */
export function stateCounts(vm: FlVm, team: FlTeam): string {
  const counts = new Map<string, number>();
  for (const id of team.line) {
    const s = vm.seats.get(id);
    if (!s || s.ghost) continue;
    const g = GLYPH_CH[s.state];
    counts.set(g, (counts.get(g) ?? 0) + 1);
  }
  return [...counts.entries()].map(([g, n]) => `${g} ${n}`).join(' · ');
}
