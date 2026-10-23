/**
 * methods/cco.ts — Gilde's READ-ONLY window onto Claude Code Orchestrator teams.
 *
 * The team engine lives in another product: each Claude Code session that runs the
 * orchestrator has its own dispatch host (a Unix socket under
 * `~/.claude-code-orchestrator/run/`), and a host that leads a team serves it to
 * OBSERVERS over that socket (team contract v2 §7.6). This module is such an
 * observer, and nothing more (S1-2, §8.2):
 *
 *   - it FINDS hosts through their run-directory records `run/s<P>.json`, and only a
 *     record whose `socket` is exactly what `socketPathFor` names for its session, whose
 *     host process is alive, and whose `workspace` is related to a folder Gilde has open
 *     (or whose session pid is one of Gilde's own PTY panes) is ever connected to — unless
 *     the scope says `allWorkspaces: true` (what Gilde's preview sends, so the Team view
 *     works with no project added): then a record of ANY workspace of this user passes the
 *     workspace test, and disk teams of any workspace are read. Every other check (socket
 *     path, uid, live host pid, caps, `endedLongAgo`, header checks) holds either way, and
 *     the observer stays read-only;
 *   - it says `cco/hello {role:'observer', client:'gilde'}` and then uses exactly three
 *     host methods — `cco/team.list`, `cco/team.get`, `cco/team.subscribe {activity:true}`;
 *   - it reads, read-only and capped, what the contract lets a front end read under the
 *     data root: `teams/<id>/team.json` + `state.json` (teams with no live host),
 *     `members/<name>/feed.jsonl` (the activity backlog) and `bodies/m<seq>.md` (a message
 *     body past the inline cap). It never reads `events.jsonl` or `lease.json`, and it
 *     never writes anything anywhere;
 *   - it spawns nothing and signals nothing (DESIGN §5-6: `scripts/check-seam.mjs`).
 *
 * T0 AMENDMENT A14 — the rule this file is built around. No reducer of the team log is
 * frozen for front ends, so every `cco/team.event` is only a CHANGE SIGNAL: it schedules
 * a fresh `cco/team.get`, at most two per second per team, one in flight at a time, and
 * the state shown is always the engine's own projection. What the projection does not
 * hold is kept from the stream itself: the last messages (`msg.posted`) and the member
 * activity lines (`cco/team.activity`).
 *
 * WHAT CROSSES TO RUST. Events never do (§8.2, L17). This module composes a VIEW of a
 * team — the projection plus the kept messages and activity lines, every value checked
 * and capped — and forwards it as the `cco.team.snapshot` notification, coalesced over
 * FORWARD_COALESCE_MS per team, latest wins. Every view carries a `rev` from one
 * monotonic counter, so a view that was dropped under backpressure (`rpc.ts` gives this
 * notification `mesh.event`'s lossy rank) is noticed by the next `cco.team.list`, whose
 * summaries carry each team's latest rev, and re-read with `cco.team.get`. A view of
 * `null` means the team left this sidecar's sight.
 *
 * L7 (believing `running`) is NOT decided here: it needs the owner host's process START
 * TIME, which Node cannot read without spawning `ps`. Each view says where it came from
 * (`source`, `live`, `state.owner`) and Rust — which reads start times through libproc
 * — decides `believed` before anything reaches the UI.
 *
 * EVERYTHING HERE IS DATA FROM ANOTHER PROCESS: a host record, a hello answer, a
 * snapshot, an event, a file. Each is checked BY VALUE; strings are made well-formed
 * (a lone UTF-16 surrogate would make the Rust reader reject the whole line) and capped
 * on a code-point boundary; numbers are made finite integers where Rust reads integers.
 * Nothing here throws into the event loop: every socket and timer callback is guarded.
 */

import * as fs from 'node:fs';
import * as os from 'node:os';
import * as path from 'node:path';

import { RpcError, RPC_INVALID_PARAMS } from '../rpc';
import type { Rpc } from '../rpc';
import { CCO_WORKERS_METHOD, WorkersPoller } from './cco-workers';
import type { WorkersHost, WorkersHostSource } from './cco-workers';
import { connectHost } from '../vendored/cco/host-client';
import type { HostAnswer, HostClient, HostConnectionEnd } from '../vendored/cco/host-client';
import {
  BOARD_ID,
  FEED_KINDS,
  MEMBER_MAILBOX,
  MEMBER_NAME,
  MEMBER_STATES,
  MESSAGE_ID,
  MESSAGE_KINDS,
  METHOD_TEAM,
  NOTICE_KINDS,
  PROTOCOL_VERSION,
  TASK_ID,
  TASK_STATES,
  TEAM_DIR_ENTRIES,
  TEAM_DIR_SCAN_MAX,
  TEAM_FEATURE,
  TEAM_FEATURE_MISSING_SENTENCE,
  TEAM_ID,
  TEAM_LIST_MAX,
  TEAM_PROVIDERS,
  TEAM_STATUSES,
  TEAM_SUBSCRIBE_REPLAY_MAX,
  TEAM_WARNING_KINDS,
  memberFeedFileOf,
  messageBodyFileOf,
  runDirOf,
  socketPathFor,
  teamDirOf,
  teamsDirOf,
} from '../vendored/cco/host-protocol';
import type {
  BoardTask,
  HostRecord,
  MemberState,
  ObserverHelloParams,
  TeamState,
} from '../vendored/cco/host-protocol';

/* ------------------------------------------------------------------------- *
 * Wire names (sidecar ⇄ Rust). Mirrored in src-tauri/src/sidecar/protocol.rs.
 * ------------------------------------------------------------------------- */

/**
 * host → sidecar requests (contract §4.5: the existing `cco.*` methods, renamed
 * `orch.*`, registered only while the orchestrator is present — methods/orch.ts).
 */
export const CCO_METHOD = {
  /** `{roots, panePids, allWorkspaces?}` → CcoTeamListResult. Also sets what is watched. */
  list: 'orch.team.list',
  /** `{teamId}` → CcoTeamView | null. */
  get: 'orch.team.get',
  /** Same params and answer as `list`; the name the contract gives starting the observer. */
  subscribe: 'orch.team.subscribe',
} as const;

/**
 * sidecar → host notification (contract §4.5 `OrchSnapshotParams`):
 * `{generation, rev, team: {teamId, view: CcoTeamView | null}}`. Lossy, latest
 * wins by `(generation, rev)`; `generation` is this observer's epoch (a new
 * observer — the orchestrator came back — restarts `rev`).
 */
export const CCO_SNAPSHOT_NOTIFICATION = 'orch.snapshot';

/* ------------------------------------------------------------------------- *
 * Bounds
 * ------------------------------------------------------------------------- */

/** Discovery tick (§8.1 "every 5 s"): run directory, team directories, list polls. */
export const DISCOVERY_INTERVAL_MS = 5_000;
/**
 * A change in the run directory asks for a tick this soon — debounced, because the
 * directory also holds every host's LOG (`s<P>.log`), which is appended all the time:
 * a tick per watch event would be a tick (and a list request) per log line.
 */
const WATCH_TICK_DEBOUNCE_MS = 500;
/** `cco/team.list` is asked at most this often per host, whatever triggers the tick. */
const LIST_POLL_MIN_MS = DISCOVERY_INTERVAL_MS - 500;
/** A `team.json` that could not be read is tried again after this long (it may have been mid-write). */
const HEADER_RETRY_MS = 30_000;
/** §8.1 reconnect backoff, by consecutive failures of one host. */
export const RECONNECT_BACKOFF_MS = [1_000, 2_000, 5_000, 10_000] as const;
const CONNECT_TIMEOUT_MS = 5_000;
const HELLO_TIMEOUT_MS = 10_000;
/** A non-held request is answered within this or read as unreachable. */
const REQUEST_TIMEOUT_MS = 30_000;
/** A14: at most two snapshot re-reads per second per team. */
export const SNAPSHOT_MIN_INTERVAL_MS = 500;
/** §8.2: views to Rust coalesced over this window per team, latest wins. */
export const FORWARD_COALESCE_MS = 250;
/** The message timeline keeps the newest this many per team. */
export const MESSAGES_KEPT = 50;
/** Events of history replayed at subscribe time to fill the timeline (≤ TEAM_SUBSCRIBE_REPLAY_MAX). */
const MESSAGE_REPLAY_EVENTS = Math.min(2_000, TEAM_SUBSCRIBE_REPLAY_MAX);
/** Characters of a message body kept. */
const MESSAGE_BODY_KEPT = 2_000;
/** Bytes read from the head of `bodies/m<seq>.md`. */
const MESSAGE_BODY_FILE_READ = 8 * 1024;
/** Activity lines kept per member, and the characters of one. */
export const ACTIVITY_LINES_KEPT = 30;
const ACTIVITY_LINE_CHARS = 300;
/** Bytes read from the tail of `members/<name>/feed.jsonl` for the backlog. */
const FEED_BACKLOG_BYTES = 64 * 1024;
/** Run-directory entries examined, and the sizes a record / header / state is read up to. */
const RUN_DIR_SCAN_MAX = 200;
const RUN_RECORD_MAX_BYTES = 64 * 1024;
const TEAM_HEADER_MAX_BYTES = 64 * 1024;
/** Team contract §12 STATE_MAX: `state.json` is at most this. */
const STATE_MAX_BYTES = 4 * 1024 * 1024;
const REGISTRY_MAX_BYTES = 1024 * 1024;
/** An `ended` team stays in view this long after it ended. */
const ENDED_SHOWN_MS = 24 * 60 * 60_000;
/**
 * What was kept of a team that left sight (messages, activity, its last seq) is forgotten
 * after this long — not at once, so a dropped connection resumes from where it was.
 */
const EXTRAS_FORGET_MS = 10 * 60_000;
/**
 * The most one serialized view may weigh. Rust's reader refuses lines over 4 MiB and
 * restarts the sidecar on one (sidecar/protocol.rs MAX_LINE_BYTES), so this budget is
 * an invariant, not a preference: an over-budget view is cut down until it fits.
 */
export const VIEW_MAX_BYTES = 1024 * 1024;
/** What `cco.team.list` accepts from Rust. */
const ROOTS_MAX = 256;
const PANE_PIDS_MAX = 256;
const ROOT_MAX_CHARS = 4_096;
/** What is kept of host strings and lists (the UI cuts again to its own, smaller caps). */
const TEXT_KEPT = 4_096;
const NAME_KEPT = 256;
const MEMBERS_KEPT = 32;
const BOARD_KEPT = 200;
const WARNINGS_KEPT = 30;
const GOAL_KEPT = 4_000;
const NOTE_KEPT = 1_000;
const BOARD_BODY_HEAD = 300;
const TASK_LIST_KEPT = 20;
const PATHS_KEPT = 16;
/** TeamListEntry.reason: "one line, ≤ 300 chars as rendered" (host-protocol, CCO 0b7b1b8). */
export const TEAM_LIST_REASON_CHARS = 300;

/* ------------------------------------------------------------------------- *
 * Shapes forwarded to Rust (mirrored by src-tauri/src/sidecar/types.rs)
 * ------------------------------------------------------------------------- */

/** One message as this sidecar saw it (`msg.posted`). */
export interface CcoMessage {
  seq: number;
  /** `m<seq>`. */
  id: string;
  ts: number;
  from: string;
  to: string;
  kind: string;
  urgent: boolean;
  re?: string;
  task?: string;
  notice?: string;
  /** The first MESSAGE_BODY_KEPT characters, when known. */
  body?: string;
  /** The body is longer than what is kept, or could not be read. */
  cut: boolean;
  bytes: number;
}

/** One activity line (a FeedItem, flattened to text). */
export interface CcoActivityLine {
  ts: number;
  turn: string;
  k: string;
  text: string;
}

/** One member's kept activity, oldest first. */
export interface CcoMemberActivity {
  member: string;
  lines: CcoActivityLine[];
}

/** What crosses to Rust for one team. */
export interface CcoTeamView {
  teamId: string;
  rev: number;
  /** `host`: the engine's projection through a live subscription; `disk`: `state.json`. */
  source: 'host' | 'disk';
  live: boolean;
  /** `s<sessionPid>` of the host it came from. */
  hostKey?: string;
  /** The Gilde PTY pane whose Claude Code session leads it (§8.2 direct match). */
  paneSessionPid?: number;
  state: TeamState;
  /** Newest first, at most MESSAGES_KEPT. */
  messages: CcoMessage[];
  activity: CcoMemberActivity[];
  /** A cap of VIEW_MAX_BYTES cut something out of this view. */
  truncated: boolean;
  composedAt: number;
}

/** One team in `cco.team.list`. */
export interface CcoTeamSummary {
  teamId: string;
  name: string;
  title: string;
  status: string;
  source: 'host' | 'disk';
  live: boolean;
  members: number;
  running: number;
  blocked: number;
  userUnread: number;
  leadUnread: number;
  createdAt: number;
  endedAt?: number;
  hostKey?: string;
  paneSessionPid?: number;
  owner: TeamState['owner'];
  /** The rev of the newest view composed for this team; 0 = none yet. */
  rev: number;
  /**
   * Why a connected host's `cco/team.list` lists the team as it does (TeamListEntry.reason,
   * CCO 0b7b1b8: `host gone` for a stale lease, or a damaged team's reason) — one line,
   * ≤ TEAM_LIST_REASON_CHARS, host text (never markup, never parsed). Absent when no host
   * gave one.
   */
  reason?: string;
}

export type CcoHostState = 'idle' | 'connecting' | 'connected' | 'no-teams' | 'failed';

/** One host this sidecar found. */
export interface CcoHostInfo {
  key: string;
  sessionPid: number;
  /** The lead session's start time (`run/s<pid>.json` session.startedAt) — tells a live lead from a recycled pid. */
  sessionStartedAt: number;
  hostPid: number;
  workspace: string;
  pane: boolean;
  state: CcoHostState;
  reason?: string;
  teamId?: string;
}

export interface CcoTeamListResult {
  /** False on win32, where team mode does not exist (contract I12). */
  supported: boolean;
  dataRoot: string;
  /** `registry.json` `teamMode`: `on`, `off` (any other value), or `unknown` (no readable registry). */
  teamMode: 'on' | 'off' | 'unknown';
  hosts: CcoHostInfo[];
  teams: CcoTeamSummary[];
}

/* ------------------------------------------------------------------------- *
 * Small total helpers
 * ------------------------------------------------------------------------- */

function isRecord(value: unknown): value is Record<string, unknown> {
  return typeof value === 'object' && value !== null && !Array.isArray(value);
}

const LONE_SURROGATE = /[\uD800-\uDBFF](?![\uDC00-\uDFFF])|(?<![\uD800-\uDBFF])[\uDC00-\uDFFF]/g;

/**
 * A string Rust can read: lone surrogates become U+FFFD (serde_json rejects a lone
 * `\uD800` escape, and with it the whole line), cut to `max` UTF-16 units without
 * splitting a pair.
 */
export function cleanText(value: string, max: number): string {
  let s = value.replace(LONE_SURROGATE, '\uFFFD');
  if (s.length > max) {
    let cut = Math.max(0, max);
    const code = s.charCodeAt(cut - 1);
    if (cut > 0 && code >= 0xd800 && code <= 0xdbff) cut -= 1;
    s = s.slice(0, cut);
  }
  return s;
}

function str(value: unknown, max: number = TEXT_KEPT): string | undefined {
  return typeof value === 'string' ? cleanText(value, max) : undefined;
}

/** Bidi embeddings/overrides/isolates and zero-width characters: they change what the eye reads. */
const BIDI_OR_INVISIBLE = /[\u202A-\u202E\u2066-\u2069\u200B-\u200F\u061C\uFEFF]/g;

/** One line: every run of whitespace or control characters becomes one space. */
function oneLine(value: string, max: number): string {
  // eslint-disable-next-line no-control-regex
  return cleanText(value.replace(/[\s\u0000-\u001f\u007f-\u009f]+/g, ' ').trim(), max);
}

/** A finite integer in the safe range, or undefined. Fractions are truncated. */
function int(value: unknown): number | undefined {
  if (typeof value !== 'number' || !Number.isFinite(value)) return undefined;
  const t = Math.trunc(value);
  if (t > Number.MAX_SAFE_INTEGER) return Number.MAX_SAFE_INTEGER;
  if (t < -Number.MAX_SAFE_INTEGER) return -Number.MAX_SAFE_INTEGER;
  return t;
}

/** A count: a non-negative integer, 0 for anything else. */
function count(value: unknown): number {
  const n = int(value);
  return n !== undefined && n > 0 ? n : 0;
}

function positiveInt(value: unknown): number | undefined {
  const n = int(value);
  return n !== undefined && n > 0 ? n : undefined;
}

function finite(value: unknown): number | undefined {
  return typeof value === 'number' && Number.isFinite(value) ? value : undefined;
}

function oneOf<T extends string>(value: unknown, list: readonly T[]): T | undefined {
  return typeof value === 'string' && (list as readonly string[]).includes(value) ? (value as T) : undefined;
}

function strings(value: unknown, max: number, each: number = NAME_KEPT): string[] {
  if (!Array.isArray(value)) return [];
  const out: string[] = [];
  for (const entry of value) {
    if (out.length >= max) break;
    const s = str(entry, each);
    if (s !== undefined) out.push(s);
  }
  return out;
}

function errnoOf(err: unknown): string | undefined {
  const code = (err as { code?: unknown } | null)?.code;
  return typeof code === 'string' ? code : undefined;
}

function describe(err: unknown): string {
  return err instanceof Error ? err.message : String(err);
}

function isPidAlive(pid: number): boolean {
  if (!Number.isSafeInteger(pid) || pid <= 0) return false;
  try {
    process.kill(pid, 0);
    return true;
  } catch (err) {
    // EPERM: it exists and is not ours — still alive.
    return errnoOf(err) === 'EPERM';
  }
}

const fold = (p: string): string => {
  const resolved = path.resolve(p);
  return process.platform === 'darwin' ? resolved.toLowerCase() : resolved;
};

/** Is one path the other or inside it (the §7.6 host-record rule)? Case as the filesystem folds it. */
export function related(a: string, b: string): boolean {
  const x = fold(a);
  const y = fold(b);
  const within = (child: string, root: string): boolean =>
    child === root || child.startsWith(root.endsWith(path.sep) ? root : root + path.sep);
  return within(x, y) || within(y, x);
}

function realDir(dir: string): string {
  try {
    return fs.realpathSync.native(dir);
  } catch {
    return path.resolve(dir);
  }
}

function uid(): number {
  return typeof process.getuid === 'function' ? process.getuid() : -1;
}

type ReadOutcome = { ok: true; text: string; size: number } | { ok: false; why: string };

/**
 * Read a small file under the data root, read-only: no symlink at the last component, a
 * regular file of this user. `whole` (the default) refuses a file over `max` bytes;
 * `head` reads its first `max` bytes and `tail` its last. Never throws.
 */
function readCapped(file: string, max: number, opts: { mode?: 'whole' | 'head' | 'tail' } = {}): ReadOutcome {
  const mode = opts.mode ?? 'whole';
  let fd: number | undefined;
  try {
    fd = fs.openSync(file, fs.constants.O_RDONLY | (fs.constants.O_NOFOLLOW ?? 0));
    const st = fs.fstatSync(fd);
    if (!st.isFile()) return { ok: false, why: 'not a file' };
    const me = uid();
    if (me >= 0 && st.uid !== me) return { ok: false, why: 'not owned by this user' };
    if (st.size > max && mode === 'whole') return { ok: false, why: 'too large' };
    const length = Math.min(st.size, max);
    const start = mode === 'tail' ? Math.max(0, st.size - length) : 0;
    const buffer = Buffer.alloc(length);
    let read = 0;
    while (read < length) {
      const n = fs.readSync(fd, buffer, read, length - read, start + read);
      if (n <= 0) break;
      read += n;
    }
    return { ok: true, text: buffer.subarray(0, read).toString('utf8'), size: st.size };
  } catch (err) {
    return { ok: false, why: errnoOf(err) ?? describe(err) };
  } finally {
    if (fd !== undefined) {
      try {
        fs.closeSync(fd);
      } catch {
        /* closed */
      }
    }
  }
}

/** Never throws: `~/.claude-code-orchestrator`, or undefined when there is no home. */
function defaultDataRoot(): string | undefined {
  try {
    const home = os.homedir();
    return home !== '' && path.isAbsolute(home) ? path.join(home, '.claude-code-orchestrator') : undefined;
  } catch {
    return undefined;
  }
}

/* ------------------------------------------------------------------------- *
 * Records from another process, checked by value
 * ------------------------------------------------------------------------- */

/** `run/s<P>.json` (§15.1) → the record, when it has its shape and protocol. */
export function hostRecordOf(raw: unknown): HostRecord | undefined {
  if (!isRecord(raw) || !isRecord(raw.session)) return undefined;
  const hostPid = positiveInt(raw.hostPid);
  const hostStartedAt = finite(raw.hostStartedAt);
  const sessionPid = positiveInt(raw.session.pid);
  const sessionStartedAt = finite(raw.session.startedAt);
  const workspace = typeof raw.workspace === 'string' ? raw.workspace : undefined;
  const socket = typeof raw.socket === 'string' ? raw.socket : undefined;
  const serverHash = typeof raw.serverHash === 'string' ? raw.serverHash : undefined;
  if (
    raw.protocol !== PROTOCOL_VERSION ||
    hostPid === undefined ||
    hostStartedAt === undefined ||
    sessionPid === undefined ||
    sessionStartedAt === undefined ||
    workspace === undefined ||
    workspace.length > ROOT_MAX_CHARS ||
    !path.isAbsolute(workspace) ||
    workspace.includes('\u0000') ||
    socket === undefined ||
    socket.length > ROOT_MAX_CHARS ||
    serverHash === undefined
  ) {
    return undefined;
  }
  return {
    protocol: PROTOCOL_VERSION,
    hostPid,
    hostStartedAt,
    session: { pid: sessionPid, startedAt: sessionStartedAt },
    workspace,
    socket,
    serverHash: cleanText(serverHash, 64),
  };
}

function memberStateOf(raw: unknown): MemberState | undefined {
  if (!isRecord(raw)) return undefined;
  const name = typeof raw.name === 'string' && MEMBER_NAME.test(raw.name) ? raw.name : undefined;
  const state = oneOf(raw.state, MEMBER_STATES);
  const provider = oneOf(raw.provider, TEAM_PROVIDERS);
  if (name === undefined || state === undefined || provider === undefined) return undefined;
  const reasonRaw = isRecord(raw.reason) ? raw.reason : undefined;
  const reasonCode = str(reasonRaw?.code, 32);
  const worktreeRaw = isRecord(raw.worktree) ? raw.worktree : {};
  const turnRaw = isRecord(raw.turn) ? raw.turn : undefined;
  const turnId = typeof turnRaw?.id === 'string' && TASK_ID.test(turnRaw.id) ? turnRaw.id : undefined;
  const lastTurnRaw = isRecord(raw.lastTurn) ? raw.lastTurn : undefined;
  const attachRaw = isRecord(raw.attach) ? raw.attach : undefined;
  const activityRaw = isRecord(raw.lastActivity) ? raw.lastActivity : undefined;
  const tokensRaw = isRecord(raw.tokens) ? raw.tokens : {};
  const member: MemberState = {
    name,
    index: count(raw.index),
    role: str(raw.role, NAME_KEPT) ?? '',
    state,
    provider,
    worker: str(raw.worker, NAME_KEPT) ?? '',
    accountLabel: str(raw.accountLabel, NAME_KEPT) ?? '',
    machineLogin: raw.machineLogin === true,
    sharesLeadPlan: raw.sharesLeadPlan === true,
    billableOverage: raw.billableOverage === true,
    runsCommands: raw.runsCommands === true,
    model: str(raw.model, NAME_KEPT) ?? '',
    incarnation: count(raw.incarnation),
    mailbox: oneOf(raw.mailbox, MEMBER_MAILBOX) ?? 'unverified',
    portBase: count(raw.portBase),
    worktree: {
      path: str(worktreeRaw.path, 1_024) ?? '',
      branch: str(worktreeRaw.branch, NAME_KEPT) ?? '',
      baseCommit: str(worktreeRaw.baseCommit, 64) ?? '',
      ready: worktreeRaw.ready === true,
    },
    turns: count(raw.turns),
    tokens: { in: count(tokensRaw.in), out: count(tokensRaw.out) },
    costUsd: finite(raw.costUsd) ?? null,
    pending: count(raw.pending),
    tasks: strings(raw.tasks, TASK_LIST_KEPT, 8).filter((id) => BOARD_ID.test(id)),
    peerWakes: count(raw.peerWakes),
  };
  const headSha = str(worktreeRaw.headSha, 64);
  if (headSha !== undefined) member.worktree.headSha = headSha;
  const ahead = int(worktreeRaw.ahead);
  if (ahead !== undefined) member.worktree.ahead = ahead;
  const filesChanged = int(worktreeRaw.filesChanged);
  if (filesChanged !== undefined) member.worktree.filesChanged = filesChanged;
  if (typeof worktreeRaw.dirty === 'boolean') member.worktree.dirty = worktreeRaw.dirty;
  if (typeof worktreeRaw.merged === 'boolean') member.worktree.merged = worktreeRaw.merged;
  if (reasonRaw !== undefined && reasonCode !== undefined) {
    member.reason = {
      code: reasonCode as NonNullable<MemberState['reason']>['code'],
      text: str(reasonRaw.text, 600) ?? '',
    };
    const until = int(reasonRaw.until);
    if (until !== undefined) member.reason.until = until;
  }
  if (raw.elevated === 'cursor-force' || raw.elevated === 'agy-skip-permissions') member.elevated = raw.elevated;
  const modelUsed = str(raw.modelUsed, NAME_KEPT);
  if (modelUsed !== undefined) member.modelUsed = modelUsed;
  const conversationShort = str(raw.conversationShort, 16);
  if (conversationShort !== undefined) member.conversationShort = conversationShort;
  const mailboxReason = str(raw.mailboxReason, NAME_KEPT);
  if (mailboxReason !== undefined) member.mailboxReason = mailboxReason;
  if (turnRaw !== undefined && turnId !== undefined) {
    member.turn = { id: turnId, n: count(turnRaw.n), startedAt: int(turnRaw.startedAt) ?? 0 };
    const pid = positiveInt(turnRaw.pid);
    if (pid !== undefined) member.turn.pid = pid;
    const lastOutputAt = int(turnRaw.lastOutputAt);
    if (lastOutputAt !== undefined) member.turn.lastOutputAt = lastOutputAt;
  }
  const lastInputTokens = int(raw.lastInputTokens);
  if (lastInputTokens !== undefined) member.lastInputTokens = lastInputTokens;
  if (lastTurnRaw !== undefined) {
    member.lastTurn = {
      id: str(lastTurnRaw.id, 160) ?? '',
      n: count(lastTurnRaw.n),
      outcome: str(lastTurnRaw.outcome, 32) ?? '',
      endedAt: int(lastTurnRaw.endedAt) ?? 0,
      head: str(lastTurnRaw.head, 400) ?? '',
    };
  }
  const rssMb = finite(raw.rssMb);
  if (rssMb !== undefined) member.rssMb = rssMb;
  if (attachRaw !== undefined && typeof attachRaw.lease === 'string') {
    member.attach = {
      lease: str(attachRaw.lease, 32) ?? '',
      client: str(attachRaw.client, 32) ?? '',
      since: int(attachRaw.since) ?? 0,
    };
    const pid = positiveInt(attachRaw.pid);
    if (pid !== undefined) member.attach.pid = pid;
  }
  if (activityRaw !== undefined) {
    member.lastActivity = { ts: int(activityRaw.ts) ?? 0, text: str(activityRaw.text, 400) ?? '' };
  }
  return member;
}

function boardTaskOf(raw: unknown): BoardTask | undefined {
  if (!isRecord(raw)) return undefined;
  const id = typeof raw.id === 'string' && BOARD_ID.test(raw.id) ? raw.id : undefined;
  const state = oneOf(raw.state, TASK_STATES);
  if (id === undefined || state === undefined) return undefined;
  const reports: BoardTask['reports'] = [];
  for (const entry of Array.isArray(raw.reports) ? raw.reports.slice(-TASK_LIST_KEPT) : []) {
    if (!isRecord(entry)) continue;
    reports.push({ at: int(entry.at) ?? 0, by: str(entry.by, NAME_KEPT) ?? '', file: str(entry.file, NAME_KEPT) ?? '' });
  }
  // Only the head of a body is kept (BOARD_BODY_HEAD): the preview shows titles, and a
  // whole body (≤ 64 KiB) would weigh down every view. `bodyFile` says a long one exists.
  const task: BoardTask = {
    id,
    title: str(raw.title, NAME_KEPT) ?? '',
    state,
    paths: strings(raw.paths, PATHS_KEPT, 300),
    allowOverlap: raw.allowOverlap === true,
    createdBy: raw.createdBy === 'lead' ? 'lead' : 'user',
    createdAt: int(raw.createdAt) ?? 0,
    reports,
    commits: strings(raw.commits, TASK_LIST_KEPT, 64),
    turns: strings(raw.turns, TASK_LIST_KEPT, 160),
    updatedAt: int(raw.updatedAt) ?? 0,
    updatedBy: str(raw.updatedBy, NAME_KEPT) ?? '',
  };
  const assignee = typeof raw.assignee === 'string' && MEMBER_NAME.test(raw.assignee) ? raw.assignee : undefined;
  if (assignee !== undefined) task.assignee = assignee;
  const note = str(raw.note, NOTE_KEPT);
  if (note !== undefined) task.note = note;
  const body = str(raw.body, BOARD_BODY_HEAD);
  if (body !== undefined && body !== '') task.body = body;
  const bodyFile = str(raw.bodyFile, NAME_KEPT);
  if (bodyFile !== undefined) task.bodyFile = bodyFile;
  return task;
}

/**
 * A projection (a `cco/team.get` answer, a subscription's snapshot, or `state.json`) →
 * a TeamState every field of which has its type, or undefined when its identity is not
 * one (§2.4). Members, tasks and warnings of another shape are dropped one by one; a
 * missing count reads 0. Lists are capped (MEMBERS_KEPT, BOARD_KEPT, WARNINGS_KEPT).
 */
export function teamStateOf(raw: unknown): TeamState | undefined {
  if (!isRecord(raw)) return undefined;
  const id = typeof raw.id === 'string' && TEAM_ID.test(raw.id) ? raw.id : undefined;
  const status = oneOf(raw.status, TEAM_STATUSES);
  const name = str(raw.name, 64);
  if (raw.v !== 1 || id === undefined || status === undefined || name === undefined) return undefined;
  const ownerRaw = isRecord(raw.owner) ? raw.owner : undefined;
  const ownerSession = isRecord(ownerRaw?.session) ? ownerRaw.session : undefined;
  const leadRaw = isRecord(raw.lead) ? raw.lead : {};
  const userRaw = isRecord(raw.user) ? raw.user : {};
  const budgetsRaw = isRecord(raw.budgets) ? raw.budgets : {};
  const totalsRaw = isRecord(raw.totals) ? raw.totals : {};
  const members: MemberState[] = [];
  for (const entry of Array.isArray(raw.members) ? raw.members.slice(0, 64) : []) {
    if (members.length >= MEMBERS_KEPT) break;
    const member = memberStateOf(entry);
    if (member !== undefined && !members.some((m) => m.name === member.name)) members.push(member);
  }
  const board: BoardTask[] = [];
  const seenTasks = new Set<string>();
  for (const entry of Array.isArray(raw.board) ? raw.board.slice(0, 1_000) : []) {
    if (board.length >= BOARD_KEPT) break;
    const task = boardTaskOf(entry);
    if (task !== undefined && !seenTasks.has(task.id)) {
      seenTasks.add(task.id);
      board.push(task);
    }
  }
  const warnings: TeamState['warnings'] = [];
  for (const entry of Array.isArray(raw.warnings) ? raw.warnings.slice(0, 100) : []) {
    if (warnings.length >= WARNINGS_KEPT) break;
    if (!isRecord(entry)) continue;
    const kind = oneOf(entry.kind, TEAM_WARNING_KINDS);
    const text = str(entry.text, 600);
    if (kind !== undefined && text !== undefined) warnings.push({ kind, text });
  }
  const state: TeamState = {
    v: 1,
    id,
    name,
    title: str(raw.title, NAME_KEPT) ?? '',
    status,
    workspace: str(raw.workspace, 1_024) ?? '',
    repoRoot: str(raw.repoRoot, 1_024) ?? '',
    leadRel: str(raw.leadRel, 1_024) ?? '',
    baseCommit: str(raw.baseCommit, 64) ?? '',
    createdAt: int(raw.createdAt) ?? 0,
    seq: count(raw.seq),
    owner:
      ownerRaw !== undefined && ownerSession !== undefined
        ? {
            hostPid: positiveInt(ownerRaw.hostPid) ?? 0,
            hostStartedAt: int(ownerRaw.hostStartedAt) ?? 0,
            session: { pid: positiveInt(ownerSession.pid) ?? 0, startedAt: int(ownerSession.startedAt) ?? 0 },
          }
        : null,
    lead: {
      unread: count(leadRaw.unread),
      readSeq: count(leadRaw.readSeq),
      waiter: leadRaw.waiter === true,
      lastCallAt: int(leadRaw.lastCallAt) ?? null,
      wakesToday: count(leadRaw.wakesToday),
    },
    user: { unread: count(userRaw.unread), readSeq: count(userRaw.readSeq) },
    members,
    board,
    budgets: {
      turnsToday: count(budgetsRaw.turnsToday),
      turnsPerDay: count(budgetsRaw.turnsPerDay),
      running: count(budgetsRaw.running),
      maxRunning: count(budgetsRaw.maxRunning),
      pingPongTripped: budgetsRaw.pingPongTripped === true,
    },
    totals: {
      turns: count(totalsRaw.turns),
      inputTokens: count(totalsRaw.inputTokens),
      outputTokens: count(totalsRaw.outputTokens),
      costUsd: finite(totalsRaw.costUsd) ?? null,
    },
    warnings,
  };
  const damagedReason = str(raw.damagedReason, 600);
  if (damagedReason !== undefined) state.damagedReason = damagedReason;
  const goal = str(raw.goal, GOAL_KEPT);
  if (goal !== undefined) state.goal = goal;
  const endedAt = int(raw.endedAt);
  if (endedAt !== undefined) state.endedAt = endedAt;
  return state;
}

/** A `msg.posted` event's data → a message, or undefined. `seq` is the event's. */
function messageOf(seq: number, ts: number, data: Record<string, unknown>): CcoMessage | undefined {
  const kind = oneOf(data.kind, MESSAGE_KINDS);
  const from = str(data.from, NAME_KEPT);
  const to = str(data.to, NAME_KEPT);
  if (kind === undefined || from === undefined || to === undefined) return undefined;
  const message: CcoMessage = {
    seq,
    id: `m${seq}`,
    ts,
    from,
    to,
    kind,
    urgent: data.urgent === true,
    cut: false,
    bytes: count(data.bytes),
  };
  if (typeof data.re === 'string' && MESSAGE_ID.test(data.re)) message.re = data.re;
  if (typeof data.task === 'string' && BOARD_ID.test(data.task)) message.task = data.task;
  const notice = oneOf(data.notice, NOTICE_KINDS);
  if (notice !== undefined) message.notice = notice;
  if (typeof data.body === 'string') {
    message.body = cleanText(data.body, MESSAGE_BODY_KEPT);
    message.cut = data.body.length > MESSAGE_BODY_KEPT;
  } else {
    // In `bodies/m<seq>.md` (over the 16 KiB inline cap): read lazily at composition.
    message.cut = true;
  }
  return message;
}

/** Activity items (a notification's, or feed lines) → kept lines, by value (§3.8). */
export function activityLinesOf(raw: unknown): CcoActivityLine[] {
  const out: CcoActivityLine[] = [];
  for (const entry of Array.isArray(raw) ? raw.slice(-500) : []) {
    if (!isRecord(entry)) continue;
    const k = oneOf(entry.k, FEED_KINDS);
    if (k === undefined) continue;
    const s = typeof entry.s === 'string' ? entry.s : undefined;
    const tool = typeof entry.tool === 'string' ? oneLine(entry.tool, 64) : undefined;
    const d = typeof entry.d === 'string' ? entry.d : undefined;
    let text: string;
    if (k === 'tool') text = [tool, d].filter((v) => v !== undefined && v !== '').join(' · ');
    else text = s ?? d ?? '';
    const line = oneLine(text, ACTIVITY_LINE_CHARS);
    if (line === '' && k === 'text') continue;
    out.push({ ts: int(entry.ts) ?? 0, turn: str(entry.turn, 160) ?? '', k, text: line });
  }
  return out;
}

/**
 * The `reason` of each entry of a `cco/team.list` answer (TeamListEntry.reason, additive in
 * CCO 0b7b1b8), by team id: only a well-formed team id with a string reason that is not
 * blank once flattened to one line (controls and bidi overrides removed) and capped at
 * TEAM_LIST_REASON_CHARS. Anything else of an entry is ignored here; nothing is parsed.
 */
export function listReasonsOf(entries: unknown[]): Map<string, string> {
  const out = new Map<string, string>();
  for (const entry of entries) {
    if (!isRecord(entry) || typeof entry.teamId !== 'string' || !TEAM_ID.test(entry.teamId)) continue;
    if (typeof entry.reason !== 'string' || out.has(entry.teamId)) continue;
    const line = oneLine(entry.reason.replace(BIDI_OR_INVISIBLE, ''), TEAM_LIST_REASON_CHARS);
    if (line !== '') out.set(entry.teamId, line);
  }
  return out;
}

/* ------------------------------------------------------------------------- *
 * The observer
 * ------------------------------------------------------------------------- */

/** One host found through its run-directory record. */
interface HostEntry {
  /** `s<sessionPid>`. */
  key: string;
  record: HostRecord;
  /** Matched through a Gilde PTY pane's pid. */
  pane: boolean;
  client: HostClient | undefined;
  connecting: boolean;
  failures: number;
  retryAt: number;
  /** The hostPid whose hello lacked TEAM_FEATURE: skipped until the record names another process. */
  noTeamsPid: number | undefined;
  listing: boolean;
  /** When `cco/team.list` was last asked (LIST_POLL_MIN_MS). */
  lastListAt: number;
  /**
   * TeamListEntry.reason of each team the last `cco/team.list` answer named (one line,
   * capped). Replaced by every answer; emptied when the connection ends — a reason is
   * shown only while a host that says it is connected.
   */
  listReasons: Map<string, string>;
  state: CcoHostState;
  reason: string | undefined;
  /** The one held subscription on this connection (§7.6: a second replaces the first). */
  sub: { teamId: string } | undefined;
  /** Bumped at every resubscribe, so an answer to a replaced subscribe is told apart. */
  subGeneration: number;
}

/** The live side of a team this sidecar is subscribed to. */
interface LiveTeam {
  hostKey: string;
  state: TeamState | undefined;
  lastGetAt: number;
  getting: boolean;
  dirty: boolean;
  timer: NodeJS.Timeout | undefined;
}

/** What the projection does not hold, kept from the stream — survives a live → disk switch. */
interface TeamExtras {
  messages: Map<number, CcoMessage>;
  activity: Map<string, CcoActivityLine[]>;
  backlogRead: Set<string>;
  /** Message seqs whose body file was tried (read once). */
  bodyTried: Set<number>;
  /**
   * The last log seq this sidecar saw for the team. Kept across a dropped connection, so
   * the next subscription resumes FROM it (§7.6: "the observer resubscribes from its last
   * seq") instead of replaying history it already holds.
   */
  lastEventSeq: number;
}

interface DiskTeam {
  state: TeamState;
  stamp: string;
}

export interface CcoObserverOptions {
  /** Where each view goes (the `cco.team.snapshot` notification). */
  forward: (params: { rev: number; teamId: string; view: CcoTeamView | null }) => void;
  log: (level: 'debug' | 'info' | 'warn' | 'error', msg: string) => void;
  /** The data root; defaults to `~/.claude-code-orchestrator` (HOME). */
  dataRoot?: string;
}

export class CcoObserver implements WorkersHostSource {
  private readonly hosts = new Map<string, HostEntry>();
  private readonly live = new Map<string, LiveTeam>();
  private readonly disk = new Map<string, DiskTeam>();
  private readonly extras = new Map<string, TeamExtras>();
  /** team.json headers by team id; immutable once written (§2.2). */
  private readonly headers = new Map<string, { workspace: string; repoRoot: string }>();
  /** Team ids whose header could not be read, and when (retried after HEADER_RETRY_MS). */
  private readonly headerFailedAt = new Map<string, number>();
  private readonly latestRev = new Map<string, number>();
  /** Teams whose last forward carried a view (so a removal is told once). */
  private readonly forwarded = new Set<string>();
  private readonly dirtyForward = new Set<string>();
  /** Teams withdrawn (forwarded `null`) and when — their extras are forgotten after EXTRAS_FORGET_MS. */
  private readonly withdrawnAt = new Map<string, number>();
  private forwardTimer: NodeJS.Timeout | undefined;
  private tickTimer: NodeJS.Timeout | undefined;
  private soonTimer: NodeJS.Timeout | undefined;
  private watcher: fs.FSWatcher | undefined;
  private roots: string[] = [];
  private panePids = new Set<number>();
  /** Watch every workspace of this user, not only those related to `roots` (Gilde preview). */
  private allWorkspaces = false;
  private rev = 0;
  private disposed = false;
  readonly dataRoot: string | undefined;
  private readonly runDir: string | undefined;

  constructor(private readonly opts: CcoObserverOptions) {
    this.dataRoot = opts.dataRoot ?? defaultDataRoot();
    this.runDir = this.dataRoot === undefined ? undefined : runDirOf(this.dataRoot);
  }

  get supported(): boolean {
    return process.platform !== 'win32';
  }

  /** Set what is watched (absolute roots + Gilde PTY pane pids) and run a tick now. */
  setScope(roots: string[], panePids: number[], allWorkspaces = false): void {
    if (this.disposed || !this.supported) return;
    const real = [...new Set(roots.map(realDir))].sort();
    const pids = new Set(panePids);
    this.roots = real;
    this.panePids = pids;
    this.allWorkspaces = allWorkspaces;
    this.start();
    this.tick();
  }

  dispose(): void {
    this.disposed = true;
    if (this.tickTimer !== undefined) clearInterval(this.tickTimer);
    if (this.soonTimer !== undefined) clearTimeout(this.soonTimer);
    if (this.forwardTimer !== undefined) clearTimeout(this.forwardTimer);
    for (const team of this.live.values()) if (team.timer !== undefined) clearTimeout(team.timer);
    try {
      this.watcher?.close();
    } catch {
      /* closed */
    }
    for (const host of this.hosts.values()) host.client?.close();
    this.hosts.clear();
    this.live.clear();
  }

  /* ---- answers ---- */

  list(): CcoTeamListResult {
    const result: CcoTeamListResult = {
      supported: this.supported,
      dataRoot: this.dataRoot ?? '',
      teamMode: this.readTeamMode(),
      hosts: [],
      teams: [],
    };
    if (!this.supported) return result;
    for (const host of this.hosts.values()) {
      const info: CcoHostInfo = {
        key: host.key,
        sessionPid: host.record.session.pid,
        sessionStartedAt: host.record.session.startedAt,
        hostPid: host.record.hostPid,
        workspace: cleanText(host.record.workspace, ROOT_MAX_CHARS),
        pane: host.pane,
        state: host.state,
      };
      if (host.reason !== undefined) info.reason = cleanText(host.reason, 400);
      if (host.sub !== undefined) info.teamId = host.sub.teamId;
      result.hosts.push(info);
    }
    const now = Date.now();
    for (const teamId of this.teamIds()) {
      const picked = this.pick(teamId, now);
      if (picked === undefined) continue;
      const { state } = picked;
      const summary: CcoTeamSummary = {
        teamId,
        name: state.name,
        title: state.title,
        status: state.status,
        source: picked.source,
        live: picked.live,
        members: state.members.length,
        running: state.members.filter((m) => m.state === 'running').length,
        blocked: state.members.filter((m) => m.state === 'blocked').length,
        userUnread: state.user.unread,
        leadUnread: state.lead.unread,
        createdAt: state.createdAt,
        owner: state.owner,
        rev: this.latestRev.get(teamId) ?? 0,
      };
      if (state.endedAt !== undefined) summary.endedAt = state.endedAt;
      if (picked.hostKey !== undefined) summary.hostKey = picked.hostKey;
      const pane = picked.hostKey === undefined ? undefined : this.hosts.get(picked.hostKey);
      if (pane?.pane === true) summary.paneSessionPid = pane.record.session.pid;
      const reason = this.listReasonOf(teamId, picked.hostKey);
      if (reason !== undefined) summary.reason = reason;
      result.teams.push(summary);
    }
    result.teams.sort((a, b) => Number(b.live) - Number(a.live) || b.createdAt - a.createdAt);
    result.teams = result.teams.slice(0, TEAM_LIST_MAX);
    return result;
  }

  /**
   * TeamListEntry.reason for a team, from a CONNECTED host's last list answer: the team's
   * own host first, then the others in key order (one reason per team, deterministic).
   */
  private listReasonOf(teamId: string, ownHostKey: string | undefined): string | undefined {
    const hosts = [...this.hosts.values()]
      .filter((h) => h.client !== undefined && !h.client.closed)
      .sort((a, b) => Number(b.key === ownHostKey) - Number(a.key === ownHostKey) || a.key.localeCompare(b.key));
    for (const host of hosts) {
      const reason = host.listReasons.get(teamId);
      if (reason !== undefined) return reason;
    }
    return undefined;
  }

  /**
   * The hosts `cco.workers` may ask `list_workers` through (cco-workers.ts): the ones in
   * sight with an open observer connection, and what the others are doing. The same
   * connections the team preview uses — list_workers is the one `tools/call` an observer
   * may make (TEAM_OBSERVER_TOOLS).
   */
  workersHosts(): ReturnType<WorkersHostSource['workersHosts']> {
    const connected: WorkersHost[] = [];
    let settling = false;
    let older = false;
    let failure: string | undefined;
    for (const host of this.hosts.values()) {
      if (host.client !== undefined && !host.client.closed && host.state === 'connected') {
        connected.push({
          key: host.key,
          hostPid: host.record.hostPid,
          hostStartedAt: host.record.hostStartedAt,
          pane: host.pane,
          client: host.client,
        });
      } else if (host.state === 'idle' || host.state === 'connecting') settling = true;
      else if (host.state === 'no-teams') older = true;
      else if (host.state === 'failed' && failure === undefined) failure = host.reason;
    }
    return { connected, settling, older, failure };
  }

  /** The view of one team now (a new rev), or null when it is not in sight. */
  get(teamId: string): CcoTeamView | null {
    const view = this.compose(teamId);
    if (view === undefined) return null;
    // Whoever holds this view must hear when the team leaves sight.
    this.forwarded.add(teamId);
    return view;
  }

  /* ---- discovery ---- */

  private start(): void {
    if (this.tickTimer !== undefined || this.disposed) return;
    this.tickTimer = setInterval(() => this.tick(), DISCOVERY_INTERVAL_MS);
    this.tickTimer.unref?.();
    this.watchRunDir();
  }

  private watchRunDir(): void {
    if (this.runDir === undefined || this.watcher !== undefined) return;
    try {
      this.watcher = fs.watch(this.runDir, { persistent: false }, (_event, filename) => {
        // Only a host RECORD matters; a name the platform does not give is a maybe.
        const name = filename === null || filename === undefined ? undefined : String(filename);
        if (name === undefined || /^s[0-9]+\.json$/.test(name)) this.tickSoon();
      });
      this.watcher.on('error', () => {
        try {
          this.watcher?.close();
        } catch {
          /* closed */
        }
        this.watcher = undefined;
      });
    } catch {
      // no run directory yet: the 5 s tick finds it (and retries the watch)
      this.watcher = undefined;
    }
  }

  /** A debounced tick (the run-directory watch). */
  private tickSoon(): void {
    if (this.soonTimer !== undefined || this.disposed) return;
    this.soonTimer = setTimeout(() => {
      this.soonTimer = undefined;
      this.tick();
    }, WATCH_TICK_DEBOUNCE_MS);
    this.soonTimer.unref?.();
  }

  private tick(): void {
    if (this.disposed || !this.supported) return;
    try {
      if (this.watcher === undefined) this.watchRunDir();
      const wanted = this.allWorkspaces || this.roots.length > 0 || this.panePids.size > 0;
      if (wanted) this.scanHosts();
      else this.dropHosts(new Set());
      this.scanTeams();
      this.forgetWithdrawn();
    } catch (err) {
      this.opts.log('warn', `[cco] discovery failed: ${describe(err)}`);
    }
  }

  /** The run directory's host records (§7.6 "Front ends find hosts", §8.2 direct match). */
  private scanHosts(): void {
    if (this.runDir === undefined) return;
    let names: string[];
    try {
      names = fs.readdirSync(this.runDir).slice(0, RUN_DIR_SCAN_MAX);
    } catch {
      names = [];
    }
    const me = uid();
    const found = new Set<string>();
    for (const name of names) {
      const match = /^s([1-9][0-9]{0,9})\.json$/.exec(name);
      if (match === null) continue;
      const sessionPid = Number(match[1]);
      const read = readCapped(path.join(this.runDir, name), RUN_RECORD_MAX_BYTES);
      if (!read.ok) continue;
      let record: HostRecord | undefined;
      try {
        record = hostRecordOf(JSON.parse(read.text));
      } catch {
        record = undefined;
      }
      if (record === undefined || record.session.pid !== sessionPid) continue;
      const pane = this.panePids.has(sessionPid);
      if (!pane && !this.allWorkspaces && !this.roots.some((root) => related(record.workspace, root))) continue;
      if (!this.isHostSocket(record, me) || !isPidAlive(record.hostPid)) continue;
      const key = `s${sessionPid}`;
      found.add(key);
      const known = this.hosts.get(key);
      if (known === undefined || known.record.hostPid !== record.hostPid || known.record.socket !== record.socket) {
        if (known !== undefined) this.closeHost(known);
        this.hosts.set(key, {
          key,
          record,
          pane,
          client: undefined,
          connecting: false,
          failures: 0,
          retryAt: 0,
          noTeamsPid: undefined,
          listing: false,
          lastListAt: 0,
          listReasons: new Map(),
          state: 'idle',
          reason: undefined,
          sub: undefined,
          subGeneration: 0,
        });
      } else {
        known.pane = pane;
      }
    }
    this.dropHosts(found);
    const now = Date.now();
    for (const host of this.hosts.values()) {
      if (host.noTeamsPid === host.record.hostPid) continue;
      if (host.client !== undefined && !host.client.closed) {
        if (now - host.lastListAt >= LIST_POLL_MIN_MS) void this.pollList(host);
      } else if (!host.connecting && now >= host.retryAt) void this.connect(host);
    }
  }

  /**
   * Is the record's socket THE socket of its session (§15.1: socketPathFor)? A record
   * cannot point this observer at an arbitrary socket. A fallback socket under the temp
   * directory is used only when that directory is this user's, not a link, and private
   * — the check `cco` makes before it uses one.
   */
  private isHostSocket(record: HostRecord, me: number): boolean {
    if (this.runDir === undefined) return false;
    const expected = socketPathFor(this.runDir, record.session.pid, me);
    if (expected === undefined || expected.socket !== record.socket) return false;
    if (expected.fallbackDir === undefined) return true;
    try {
      const st = fs.lstatSync(expected.fallbackDir);
      return st.isDirectory() && !st.isSymbolicLink() && st.uid === me && (st.mode & 0o077) === 0;
    } catch {
      return false;
    }
  }

  private dropHosts(keep: Set<string>): void {
    for (const [key, host] of this.hosts) {
      if (keep.has(key)) continue;
      this.hosts.delete(key);
      this.closeHost(host);
    }
  }

  private closeHost(host: HostEntry): void {
    const client = host.client;
    host.client = undefined;
    host.sub = undefined;
    host.listReasons = new Map();
    client?.close();
    this.dropLiveOf(host.key);
  }

  private dropLiveOf(hostKey: string): void {
    for (const [teamId, team] of this.live) {
      if (team.hostKey !== hostKey) continue;
      if (team.timer !== undefined) clearTimeout(team.timer);
      this.live.delete(teamId);
      this.changed(teamId);
      this.refollow(teamId, hostKey);
    }
  }

  /**
   * Whether the host behind `hostKey` follows `teamId` now: connected, and its
   * subscription is to that team.
   */
  private following(hostKey: string, teamId: string): boolean {
    const host = this.hosts.get(hostKey);
    return host?.client !== undefined && !host.client.closed && host.sub?.teamId === teamId;
  }

  /**
   * The live side of `teamId` (followed through `goneKey`) just went. Another connected
   * host may claim the same team (a lease handoff window, or a second host in sight with
   * `allWorkspaces`): its subscription was left in place when the team was taken over
   * through `goneKey`, so ask it again now — its list poll follows the team afresh.
   */
  private refollow(teamId: string, goneKey: string): void {
    for (const other of this.hosts.values()) {
      if (other.key === goneKey || other.sub?.teamId !== teamId) continue;
      if (other.client !== undefined && !other.client.closed) void this.pollList(other);
    }
  }

  private async connect(host: HostEntry): Promise<void> {
    host.connecting = true;
    host.state = 'connecting';
    const hello: ObserverHelloParams = {
      protocol: PROTOCOL_VERSION,
      role: 'observer',
      client: 'gilde',
      clientPid: process.pid,
    };
    let greeted: Awaited<ReturnType<typeof connectHost>>;
    try {
      greeted = await connectHost(host.record.socket, hello, {
        connectTimeoutMs: CONNECT_TIMEOUT_MS,
        helloTimeoutMs: HELLO_TIMEOUT_MS,
      });
    } catch (err) {
      greeted = { kind: 'unreachable', code: errnoOf(err) ?? describe(err) };
    }
    host.connecting = false;
    if (this.disposed || this.hosts.get(host.key) !== host) {
      if (greeted.kind === 'ok') greeted.client.close();
      return;
    }
    switch (greeted.kind) {
      case 'ok':
        break;
      case 'unreachable':
        this.failed(host, `could not connect (${greeted.code})`);
        return;
      case 'refused':
        this.failed(host, `hello refused (${greeted.code ?? '?'}): ${oneLine(greeted.message, 200)}`);
        return;
      default:
        this.failed(host, `hello ${greeted.kind}`);
        return;
    }
    const { client, hello: answer } = greeted;
    if (answer.protocol !== PROTOCOL_VERSION || answer.hostPid !== host.record.hostPid) {
      client.close();
      this.failed(host, 'the host answered for another process or protocol');
      return;
    }
    if (!(answer.features ?? []).includes(TEAM_FEATURE)) {
      // §8.1: skipped with ONE log line, until the record names another host process.
      client.close();
      host.noTeamsPid = host.record.hostPid;
      host.state = 'no-teams';
      host.reason = TEAM_FEATURE_MISSING_SENTENCE;
      this.opts.log('info', `[cco] host pid ${host.record.hostPid}: ${TEAM_FEATURE_MISSING_SENTENCE}`);
      return;
    }
    host.client = client;
    host.failures = 0;
    host.state = 'connected';
    host.reason = undefined;
    client.listen(
      (method, params) => this.guard(() => this.onNotification(host, client, method, params)),
      (end) => this.guard(() => this.onEnd(host, client, end)),
    );
    await this.pollList(host);
  }

  private failed(host: HostEntry, why: string): void {
    const delay = RECONNECT_BACKOFF_MS[Math.min(host.failures, RECONNECT_BACKOFF_MS.length - 1)] ?? 10_000;
    host.failures += 1;
    host.retryAt = Date.now() + delay;
    host.state = 'failed';
    host.reason = why;
    if (host.failures === 1) this.opts.log('info', `[cco] host pid ${host.record.hostPid}: ${why}; retrying`);
    const timer = setTimeout(() => this.tick(), delay + 50);
    timer.unref?.();
  }

  private onEnd(host: HostEntry, client: HostClient, end: HostConnectionEnd): void {
    if (host.client !== client) return;
    host.client = undefined;
    host.sub = undefined;
    host.listReasons = new Map();
    // An overflow close (MD-55) or the host going away: the view falls back to the disk
    // until the next connection resubscribes from the last seq seen.
    this.dropLiveOf(host.key);
    if (!this.disposed && this.hosts.get(host.key) === host) {
      this.failed(host, end.kind === 'error' ? `the connection failed (${end.code ?? 'error'})` : `the connection ${end.kind === 'closed' ? 'closed' : 'sent a line that is too long'}`);
    }
  }

  /** `cco/team.list`: follow the team this host owns (one active team per host, MD-10). */
  private async pollList(host: HostEntry): Promise<void> {
    const client = host.client;
    if (client === undefined || client.closed || host.listing) return;
    host.listing = true;
    host.lastListAt = Date.now();
    try {
      const listed = resultOf(await client.request(METHOD_TEAM.list, {}, REQUEST_TIMEOUT_MS));
      if (listed === undefined || host.client !== client) return;
      const teams = Array.isArray(listed.teams) ? listed.teams.slice(0, TEAM_LIST_MAX) : [];
      host.listReasons = listReasonsOf(teams);
      let owned: string | undefined;
      for (const entry of teams) {
        if (
          isRecord(entry) &&
          entry.ownedHere === true &&
          typeof entry.teamId === 'string' &&
          TEAM_ID.test(entry.teamId) &&
          entry.state !== 'ended'
        ) {
          owned = entry.teamId;
          break;
        }
      }
      if (owned === undefined) {
        if (host.sub !== undefined) {
          const teamId = host.sub.teamId;
          host.sub = undefined;
          host.subGeneration += 1;
          void client.request(METHOD_TEAM.unsubscribe, { teamId }, REQUEST_TIMEOUT_MS);
          this.dropLiveOf(host.key);
        }
        return;
      }
      // Follow it unless this host already does. "Already" means the live side is this
      // host's own: a subscription left in place while another host took the team over is
      // not following it once that other host is gone (a team is followed through ONE
      // connection; the most recent claimant wins and keeps it while it follows it).
      const team = this.live.get(owned);
      const ownLive = team !== undefined && team.hostKey === host.key;
      const heldElsewhere = team !== undefined && !ownLive && this.following(team.hostKey, owned);
      if (host.sub?.teamId !== owned || (!ownLive && !heldElsewhere)) await this.startTeam(host, client, owned);
    } finally {
      host.listing = false;
    }
  }

  /** get, then subscribe from far enough back to fill the timeline (A14, §8.2). */
  private async startTeam(host: HostEntry, client: HostClient, teamId: string): Promise<void> {
    const got = resultOf(await client.request(METHOD_TEAM.get, { teamId }, REQUEST_TIMEOUT_MS));
    const state = got === undefined ? undefined : teamStateOf(got.state);
    if (state === undefined || state.id !== teamId || host.client !== client) return;
    let team = this.live.get(teamId);
    if (team === undefined || team.hostKey !== host.key) {
      if (team?.timer !== undefined) clearTimeout(team.timer);
      team = {
        hostKey: host.key,
        state,
        lastGetAt: Date.now(),
        getting: false,
        dirty: false,
        timer: undefined,
      };
      this.live.set(teamId, team);
    } else {
      team.state = state;
    }
    this.readBacklog(teamId, state);
    const seen = this.extrasOf(teamId).lastEventSeq;
    const fromSeq = seen > 0 ? seen : Math.max(0, state.seq - MESSAGE_REPLAY_EVENTS);
    this.subscribe(host, client, teamId, fromSeq);
    this.changed(teamId);
  }

  /** The held subscribe; its ANSWER is the end of the subscription (§7.6, L8). */
  private subscribe(host: HostEntry, client: HostClient, teamId: string, fromSeq: number | undefined): void {
    const params: Record<string, unknown> = { teamId, activity: true };
    // `fromSeq: 0` replays the whole log (≤ TEAM_SUBSCRIBE_REPLAY_MAX); an ABSENT
    // fromSeq replays nothing — so 0 is sent, never dropped.
    if (fromSeq !== undefined) params.fromSeq = fromSeq;
    host.subGeneration += 1;
    const generation = host.subGeneration;
    host.sub = { teamId };
    void client.request(METHOD_TEAM.subscribe, params).then((answer) =>
      this.guard(() => {
        if (host.client !== client || host.subGeneration !== generation) return; // replaced (L8) or gone
        const result = answer.kind === 'result' && isRecord(answer.result) ? answer.result : undefined;
        if (result?.ok === false && result.code === 'resnapshot_required') {
          this.subscribe(host, client, teamId, undefined);
          return;
        }
        // Ended by the host (an unsubscribe, the team ended or moved): the next list poll decides.
        host.sub = undefined;
        this.dropLiveOf(host.key);
        if (result?.ok === false) {
          this.opts.log('info', `[cco] subscription to ${teamId} ended: ${oneLine(String(result.code), 64)}`);
        }
      }),
    );
  }

  /* ---- notifications ---- */

  private onNotification(host: HostEntry, client: HostClient, method: string, params: unknown): void {
    if (host.client !== client || !isRecord(params)) return;
    if (method === METHOD_TEAM.host) {
      const state = str(params.state, 32);
      const reason = str(params.reason, 64);
      this.opts.log('debug', `[cco] host pid ${host.record.hostPid}: ${state ?? '?'}${reason ? ` (${reason})` : ''}`);
      if (reason === 'lease-lost' || state === 'ending') void this.pollList(host);
      return;
    }
    // One subscription per connection: the team id is what ties a notification to it.
    // (A notification of a subscription this connection replaced is at worst one more
    // change signal for the same team — A14 makes that harmless.)
    const sub = host.sub;
    if (sub === undefined || params.teamId !== sub.teamId) return;
    const team = this.live.get(sub.teamId);
    if (team === undefined || team.hostKey !== host.key) return;
    if (method === METHOD_TEAM.activity) {
      const member = typeof params.member === 'string' && MEMBER_NAME.test(params.member) ? params.member : undefined;
      if (member === undefined) return;
      this.addActivity(sub.teamId, member, activityLinesOf(params.items));
      this.changed(sub.teamId);
      return;
    }
    if (method !== METHOD_TEAM.event) return;
    const kind = str(params.kind, 64);
    if (kind === undefined) return;
    if (kind === 'snapshot') {
      const state = teamStateOf(params.state);
      if (state !== undefined && state.id === sub.teamId) {
        team.state = state;
        const extras = this.extrasOf(sub.teamId);
        extras.lastEventSeq = Math.max(extras.lastEventSeq, state.seq);
        this.readBacklog(sub.teamId, state);
        this.changed(sub.teamId);
      }
      return;
    }
    const seq = count(params.seq);
    const extras = this.extrasOf(sub.teamId);
    extras.lastEventSeq = Math.max(extras.lastEventSeq, seq);
    if (kind === 'msg.posted' && seq > 0) {
      const data = isRecord(params.data) ? params.data : {};
      const message = messageOf(seq, int(params.ts) ?? Date.now(), data);
      if (message !== undefined) this.addMessage(sub.teamId, message);
    }
    // A14: the event is a change signal; the state shown is the engine's own projection.
    this.scheduleSnapshot(sub.teamId);
  }

  /** A14: re-read the snapshot, at most twice a second, one read in flight at a time. */
  private scheduleSnapshot(teamId: string): void {
    const team = this.live.get(teamId);
    if (team === undefined) return;
    if (team.getting || team.timer !== undefined) {
      team.dirty = true;
      return;
    }
    const wait = SNAPSHOT_MIN_INTERVAL_MS - (Date.now() - team.lastGetAt);
    if (wait > 0) {
      team.timer = setTimeout(() => {
        team.timer = undefined;
        void this.readSnapshot(teamId);
      }, wait);
      team.timer.unref?.();
      return;
    }
    void this.readSnapshot(teamId);
  }

  private async readSnapshot(teamId: string): Promise<void> {
    const team = this.live.get(teamId);
    const host = team === undefined ? undefined : this.hosts.get(team.hostKey);
    const client = host?.client;
    if (team === undefined || client === undefined || client.closed) return;
    team.getting = true;
    team.dirty = false;
    team.lastGetAt = Date.now();
    let answer: HostAnswer;
    try {
      answer = await client.request(METHOD_TEAM.get, { teamId }, REQUEST_TIMEOUT_MS);
    } catch {
      answer = { kind: 'timeout' };
    }
    team.getting = false;
    this.guard(() => {
      const got = resultOf(answer);
      const state = got === undefined ? undefined : teamStateOf(got.state);
      if (state !== undefined && state.id === teamId && this.live.get(teamId) === team) {
        team.state = state;
        this.readBacklog(teamId, state);
        this.changed(teamId);
      }
      if (team.dirty && this.live.get(teamId) === team) this.scheduleSnapshot(teamId);
    });
  }

  /* ---- what the projection does not hold ---- */

  private extrasOf(teamId: string): TeamExtras {
    let extras = this.extras.get(teamId);
    if (extras === undefined) {
      extras = { messages: new Map(), activity: new Map(), backlogRead: new Set(), bodyTried: new Set(), lastEventSeq: 0 };
      this.extras.set(teamId, extras);
    }
    return extras;
  }

  private addMessage(teamId: string, message: CcoMessage): void {
    const extras = this.extrasOf(teamId);
    if (extras.messages.has(message.seq)) return;
    extras.messages.set(message.seq, message);
    if (extras.messages.size > MESSAGES_KEPT * 2) {
      const old = [...extras.messages.keys()].sort((a, b) => a - b).slice(0, extras.messages.size - MESSAGES_KEPT);
      for (const seq of old) {
        extras.messages.delete(seq);
        extras.bodyTried.delete(seq);
      }
    }
  }

  private addActivity(teamId: string, member: string, lines: CcoActivityLine[]): void {
    if (lines.length === 0) return;
    const extras = this.extrasOf(teamId);
    const kept = extras.activity.get(member) ?? [];
    for (const line of lines) {
      const recent = kept.slice(-5);
      if (recent.some((r) => r.ts === line.ts && r.k === line.k && r.turn === line.turn && r.text === line.text)) continue;
      kept.push(line);
    }
    extras.activity.set(member, kept.slice(-ACTIVITY_LINES_KEPT));
  }

  /**
   * §8.2: the activity BACKLOG of each member is the tail of its `feed.jsonl` — read
   * once per member, at most FEED_BACKLOG_BYTES, before the live items append to it.
   */
  private readBacklog(teamId: string, state: TeamState): void {
    if (this.dataRoot === undefined) return;
    const extras = this.extrasOf(teamId);
    let teamDir: string;
    try {
      teamDir = teamDirOf(this.dataRoot, teamId);
    } catch {
      return;
    }
    for (const member of state.members) {
      if (extras.backlogRead.has(member.name)) continue;
      extras.backlogRead.add(member.name);
      let file: string;
      try {
        file = memberFeedFileOf(teamDir, member.name);
      } catch {
        continue;
      }
      const read = readCapped(file, FEED_BACKLOG_BYTES, { mode: 'tail' });
      if (!read.ok) continue;
      let lines = read.text.split('\n');
      if (read.size > FEED_BACKLOG_BYTES) lines = lines.slice(1); // the first one is cut
      const items: unknown[] = [];
      for (const line of lines.slice(-ACTIVITY_LINES_KEPT * 4)) {
        if (line.trim() === '') continue;
        try {
          items.push(JSON.parse(line));
        } catch {
          /* a torn or foreign line */
        }
      }
      const backlog = activityLinesOf(items).slice(-ACTIVITY_LINES_KEPT);
      const live = extras.activity.get(member.name) ?? [];
      extras.activity.set(member.name, [...backlog, ...live].slice(-ACTIVITY_LINES_KEPT));
    }
  }

  /** §8.2: a body over the inline cap is in `bodies/m<seq>.md` — read its head once. */
  private fillBody(teamId: string, extras: TeamExtras, message: CcoMessage): void {
    if (message.body !== undefined || extras.bodyTried.has(message.seq) || this.dataRoot === undefined) return;
    extras.bodyTried.add(message.seq);
    try {
      const file = messageBodyFileOf(teamDirOf(this.dataRoot, teamId), message.id);
      const head = readCapped(file, MESSAGE_BODY_FILE_READ, { mode: 'head' });
      if (head.ok) {
        message.body = cleanText(head.text, MESSAGE_BODY_KEPT);
        message.cut = head.text.length > MESSAGE_BODY_KEPT || head.size > MESSAGE_BODY_FILE_READ;
      }
    } catch {
      /* stays cut */
    }
  }

  /* ---- teams on disk (§2.4, §7.6) ---- */

  private scanTeams(): void {
    const roots = [...this.roots];
    for (const host of this.hosts.values()) if (host.pane) roots.push(host.record.workspace);
    if (this.dataRoot === undefined || (roots.length === 0 && !this.allWorkspaces)) {
      for (const teamId of [...this.disk.keys()]) {
        this.disk.delete(teamId);
        this.changed(teamId);
      }
      return;
    }
    let names: string[];
    try {
      names = fs.readdirSync(teamsDirOf(this.dataRoot)).slice(0, TEAM_DIR_SCAN_MAX);
    } catch {
      names = [];
    }
    const found = new Set<string>();
    const now = Date.now();
    for (const teamId of names) {
      if (!TEAM_ID.test(teamId)) continue;
      const dir = path.join(teamsDirOf(this.dataRoot), teamId);
      let header = this.headers.get(teamId);
      if (header === undefined) {
        const failedAt = this.headerFailedAt.get(teamId);
        if (failedAt !== undefined && now - failedAt < HEADER_RETRY_MS) continue;
        const read = this.readHeader(dir, teamId);
        if (read === null) {
          this.headerFailedAt.set(teamId, now);
          continue;
        }
        this.headerFailedAt.delete(teamId);
        this.headers.set(teamId, read);
        header = read;
      }
      const { workspace, repoRoot } = header;
      if (!this.allWorkspaces && !roots.some((root) => related(workspace, root) || related(repoRoot, root))) continue;
      const file = path.join(dir, TEAM_DIR_ENTRIES.state);
      let stamp: string;
      try {
        const st = fs.lstatSync(file);
        if (!st.isFile()) continue;
        stamp = `${st.size}:${st.mtimeMs}:${st.ino}`;
      } catch {
        continue;
      }
      const known = this.disk.get(teamId);
      if (known !== undefined && known.stamp === stamp) {
        if (!this.endedLongAgo(known.state, now)) found.add(teamId);
        continue;
      }
      const read = readCapped(file, STATE_MAX_BYTES);
      let state: TeamState | undefined;
      if (read.ok) {
        try {
          state = teamStateOf(JSON.parse(read.text));
        } catch {
          state = undefined;
        }
      }
      if (state === undefined || state.id !== teamId) {
        // A state.json being replaced (temp + rename) or of another shape: keep what was known.
        if (known !== undefined) found.add(teamId);
        continue;
      }
      if (this.endedLongAgo(state, now)) continue;
      found.add(teamId);
      this.disk.set(teamId, { state, stamp });
      this.readBacklog(teamId, state);
      // The engine rewrites state.json after every batch; while the team is followed live
      // that changes nothing shown (the live projection wins), so it forwards nothing.
      if (!this.live.has(teamId)) this.changed(teamId);
    }
    for (const teamId of [...this.disk.keys()]) {
      if (found.has(teamId)) continue;
      this.disk.delete(teamId);
      this.changed(teamId);
    }
  }

  private endedLongAgo(state: TeamState, now: number): boolean {
    return state.status === 'ended' && (state.endedAt === undefined || now - state.endedAt > ENDED_SHOWN_MS);
  }

  private readHeader(dir: string, teamId: string): { workspace: string; repoRoot: string } | null {
    const read = readCapped(path.join(dir, TEAM_DIR_ENTRIES.header), TEAM_HEADER_MAX_BYTES);
    if (!read.ok) return null;
    try {
      const raw: unknown = JSON.parse(read.text);
      if (!isRecord(raw) || raw.id !== teamId) return null;
      const { workspace, repoRoot } = raw;
      return typeof workspace === 'string' &&
        typeof repoRoot === 'string' &&
        path.isAbsolute(workspace) &&
        path.isAbsolute(repoRoot)
        ? { workspace, repoRoot }
        : null;
    } catch {
      return null;
    }
  }

  private readTeamMode(): CcoTeamListResult['teamMode'] {
    if (this.dataRoot === undefined) return 'unknown';
    const read = readCapped(path.join(this.dataRoot, 'registry.json'), REGISTRY_MAX_BYTES);
    if (!read.ok) return 'unknown';
    try {
      const raw: unknown = JSON.parse(read.text);
      if (!isRecord(raw)) return 'unknown';
      // §7.7: any value but the literal `on` reads `off`.
      return raw.teamMode === 'on' ? 'on' : 'off';
    } catch {
      return 'unknown';
    }
  }

  /* ---- views ---- */

  private teamIds(): string[] {
    return [...new Set([...this.live.keys(), ...this.disk.keys()])];
  }

  /** The state to show for a team: the live projection when subscribed, else `state.json`. */
  private pick(
    teamId: string,
    now: number,
  ): { state: TeamState; source: 'host' | 'disk'; live: boolean; hostKey: string | undefined } | undefined {
    const team = this.live.get(teamId);
    const host = team === undefined ? undefined : this.hosts.get(team.hostKey);
    if (team?.state !== undefined && host?.client !== undefined && !host.client.closed && host.sub?.teamId === teamId) {
      return { state: team.state, source: 'host', live: true, hostKey: host.key };
    }
    const disk = this.disk.get(teamId);
    if (disk === undefined || this.endedLongAgo(disk.state, now)) return undefined;
    // A disk team led by a host this sidecar knows (but is not subscribed to) is still that host's.
    let hostKey: string | undefined;
    for (const candidate of this.hosts.values()) {
      if (disk.state.owner !== null && candidate.record.hostPid === disk.state.owner.hostPid) hostKey = candidate.key;
    }
    return { state: disk.state, source: 'disk', live: false, hostKey };
  }

  /** Compose the view of a team now (a new rev), cut to VIEW_MAX_BYTES; undefined when not in sight. */
  private compose(teamId: string): CcoTeamView | undefined {
    const now = Date.now();
    const picked = this.pick(teamId, now);
    if (picked === undefined) return undefined;
    const extras = this.extrasOf(teamId);
    const messages = [...extras.messages.values()].sort((a, b) => b.seq - a.seq).slice(0, MESSAGES_KEPT);
    for (const message of messages) this.fillBody(teamId, extras, message);
    const names = new Set(picked.state.members.map((m) => m.name));
    const activity: CcoMemberActivity[] = [];
    for (const [member, lines] of extras.activity) {
      if (names.has(member) && lines.length > 0) activity.push({ member, lines: [...lines] });
    }
    this.rev += 1;
    const view: CcoTeamView = {
      teamId,
      rev: this.rev,
      source: picked.source,
      live: picked.live,
      state: picked.state,
      messages: messages.map((m) => ({ ...m })),
      activity,
      truncated: false,
      composedAt: now,
    };
    if (picked.hostKey !== undefined) {
      view.hostKey = picked.hostKey;
      const host = this.hosts.get(picked.hostKey);
      if (host?.pane === true) view.paneSessionPid = host.record.session.pid;
    }
    this.latestRev.set(teamId, this.rev);
    return fitView(view);
  }

  /** Something about `teamId` changed: forward its view within FORWARD_COALESCE_MS. */
  private changed(teamId: string): void {
    if (this.disposed) return;
    this.dirtyForward.add(teamId);
    if (this.forwardTimer !== undefined) return;
    this.forwardTimer = setTimeout(() => {
      this.forwardTimer = undefined;
      this.guard(() => this.flushForward());
    }, FORWARD_COALESCE_MS);
    this.forwardTimer.unref?.();
  }

  private flushForward(): void {
    const ids = [...this.dirtyForward];
    this.dirtyForward.clear();
    for (const teamId of ids) {
      const view = this.compose(teamId);
      if (view !== undefined) {
        this.forwarded.add(teamId);
        this.withdrawnAt.delete(teamId);
        this.opts.forward({ rev: view.rev, teamId, view });
      } else if (this.forwarded.delete(teamId)) {
        this.rev += 1;
        this.latestRev.set(teamId, this.rev);
        this.withdrawnAt.set(teamId, Date.now());
        this.opts.forward({ rev: this.rev, teamId, view: null });
      }
    }
  }

  private forgetWithdrawn(): void {
    const now = Date.now();
    for (const [teamId, at] of this.withdrawnAt) {
      if (now - at < EXTRAS_FORGET_MS || this.live.has(teamId) || this.disk.has(teamId)) continue;
      this.withdrawnAt.delete(teamId);
      this.extras.delete(teamId);
      this.latestRev.delete(teamId);
    }
  }

  private guard(fn: () => void): void {
    try {
      fn();
    } catch (err) {
      this.opts.log('error', `[cco] handler failed: ${describe(err)}`);
    }
  }
}

/** A request's answer as `{ok:true, …}`, or undefined (a refusal, an error, no answer). */
function resultOf(answer: HostAnswer): Record<string, unknown> | undefined {
  if (answer.kind !== 'result' || !isRecord(answer.result) || answer.result.ok !== true) return undefined;
  return answer.result;
}

function viewBytes(view: CcoTeamView): number {
  try {
    return Buffer.byteLength(JSON.stringify(view), 'utf8');
  } catch {
    return Number.POSITIVE_INFINITY;
  }
}

/**
 * Cut a view down to VIEW_MAX_BYTES, in the order that loses least: activity and
 * message bodies, then finished board tasks, then the board, the goal and the
 * warnings, and at last everything but the roster. `truncated` says it happened.
 */
export function fitView(view: CcoTeamView): CcoTeamView {
  if (viewBytes(view) <= VIEW_MAX_BYTES) return view;
  view.truncated = true;
  const steps: Array<(v: CcoTeamView) => void> = [
    (v) => {
      v.activity = v.activity.map((a) => ({ member: a.member, lines: a.lines.slice(-10) }));
      v.messages = v.messages.slice(0, 20).map((m) => {
        const copy = { ...m };
        if (copy.body !== undefined && copy.body.length > 300) {
          copy.body = cleanText(copy.body, 300);
          copy.cut = true;
        }
        return copy;
      });
    },
    (v) => {
      v.state = { ...v.state, board: v.state.board.filter((t) => t.state !== 'done' && t.state !== 'cancelled').slice(0, 100) };
    },
    (v) => {
      const { goal: _goal, ...rest } = v.state;
      v.state = { ...rest, board: rest.board.slice(0, 30), warnings: rest.warnings.slice(0, 5) };
    },
    (v) => {
      v.activity = [];
      v.messages = [];
      v.state = { ...v.state, board: [], warnings: [] };
    },
    (v) => {
      v.state = { ...v.state, members: [] };
    },
  ];
  for (const step of steps) {
    step(view);
    if (viewBytes(view) <= VIEW_MAX_BYTES) return view;
  }
  return view;
}

/* ------------------------------------------------------------------------- *
 * Registration
 * ------------------------------------------------------------------------- */

function invalid(message: string): RpcError {
  return new RpcError(RPC_INVALID_PARAMS, message);
}

/**
 * `{roots, panePids, allWorkspaces?}` from Rust, checked: absolute paths and positive pids,
 * capped; `allWorkspaces` a boolean when present (absent = false, the folder-scoped default).
 */
export function scopeOf(params: unknown): { roots: string[]; panePids: number[]; allWorkspaces: boolean } {
  const p = isRecord(params) ? params : {};
  const rawRoots = p.roots ?? [];
  const rawPids = p.panePids ?? [];
  if (!Array.isArray(rawRoots)) throw invalid('roots must be an array of absolute paths');
  if (!Array.isArray(rawPids)) throw invalid('panePids must be an array of pids');
  if (p.allWorkspaces !== undefined && typeof p.allWorkspaces !== 'boolean') {
    throw invalid('allWorkspaces must be a boolean');
  }
  const roots: string[] = [];
  for (const root of rawRoots.slice(0, ROOTS_MAX)) {
    if (typeof root !== 'string' || root.length === 0 || root.length > ROOT_MAX_CHARS || root.includes('\u0000')) continue;
    if (!path.isAbsolute(root)) continue;
    // A filesystem root would relate every workspace on the machine: never a scope.
    if (path.resolve(root) === path.parse(path.resolve(root)).root) continue;
    roots.push(root);
  }
  const panePids: number[] = [];
  for (const pid of rawPids.slice(0, PANE_PIDS_MAX)) {
    if (typeof pid === 'number' && Number.isSafeInteger(pid) && pid > 1) panePids.push(pid);
  }
  return { roots, panePids, allWorkspaces: p.allWorkspaces === true };
}

/**
 * Register `cco.team.list` / `cco.team.get` on the channel. The observer starts lazily —
 * at the first `cco.team.list`, which carries the scope — so a Gilde that never opens
 * the Team view never connects to anything.
 */
export function registerCcoMethods(
  rpc: Rpc,
  opts: { log: CcoObserverOptions['log']; dataRoot?: string; generation?: number },
): CcoObserver {
  const generation = opts.generation ?? 1;
  const observer = new CcoObserver({
    forward: (params) =>
      rpc.notify(CCO_SNAPSHOT_NOTIFICATION, {
        generation,
        rev: params.rev,
        team: { teamId: params.teamId, view: params.view },
      }),
    log: opts.log,
    ...(opts.dataRoot !== undefined ? { dataRoot: opts.dataRoot } : {}),
  });
  const listHandler = (params: unknown): ReturnType<CcoObserver['list']> => {
    const scope = scopeOf(params);
    observer.setScope(scope.roots, scope.panePids, scope.allWorkspaces);
    return observer.list();
  };
  rpc.on(CCO_METHOD.list, listHandler);
  rpc.on(CCO_METHOD.subscribe, listHandler);
  rpc.on(CCO_METHOD.get, (params: unknown) => {
    const teamId = isRecord(params) ? params.teamId : undefined;
    if (typeof teamId !== 'string' || !TEAM_ID.test(teamId)) throw invalid('teamId must be a team id (tm…)');
    return observer.get(teamId);
  });
  // Plan #3c: the engine's worker accounts (methods/cco-workers.ts), through the same
  // observer connections. `roots`/`panePids`, when given, restate the scope exactly as
  // `cco.team.list` does — so the Accounts view works without the Team view open.
  const workers = new WorkersPoller(observer, opts.log);
  rpc.on(CCO_WORKERS_METHOD, async (params: unknown) => {
    const p = isRecord(params) ? params : {};
    if (p.refresh !== undefined && typeof p.refresh !== 'boolean') throw invalid('refresh must be a boolean');
    if (p.roots !== undefined || p.panePids !== undefined || p.allWorkspaces !== undefined) {
      const scope = scopeOf(p);
      observer.setScope(scope.roots, scope.panePids, scope.allWorkspaces);
    }
    return workers.answer({ refresh: p.refresh === true });
  });
  return observer;
}
