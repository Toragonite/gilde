/**
 * methods/cco-workers.ts — the worker accounts the Claude Code Orchestrator ENGINE knows
 * (Gilde plan #3c), read through `tools/call list_workers` on an observer connection.
 *
 * Gilde DISPLAYS what the engine knows; it computes nothing of it. No quota, selection,
 * failover or cooldown logic lives here, and no private file of the engine is read for
 * it: the one source is `structuredContent: WorkersSnapshot` of a host's `list_workers`
 * answer (host-protocol "list_workers' structured answer", frozen 2026-10-16; vendored at CCO
 * 0b7b1b8, which adds the binding answers to Gilde's questions this file follows).
 *
 * The RULES this file keeps (they bind every reader):
 *
 *   - NO SNAPSHOT IS NOT AN EMPTY ONE. An answer without `structuredContent` (a host that
 *     predates the addition) is `host-too-old`; `isError: true` is `host-error` with the
 *     host's one-line text. The text answer is prose for a model and is NEVER parsed.
 *   - A `schema` other than `cco.workers/1` is `schema-unknown` ("this dispatch host is
 *     newer/older than this Gilde") and nothing else of that answer is read.
 *   - THE POLL FLOOR. A host is asked at most once per WORKERS_SNAPSHOT_MIN_POLL_MS (60 s),
 *     for every consumer together: a request inside the floor gets the snapshot already
 *     held, with its own `generatedAt` (`fromCache: true`). The host floors observer calls
 *     itself too (and may answer its own cached snapshot — then the snapshot says
 *     `servedFromCache: true`, passed through as the host said it); this floor is Gilde's
 *     promise not to rely on that. Two exceptions, each still at most once per
 *     WORKERS_REFRESH_MIN_MS: the user's explicit refresh (a plain call — the host may
 *     answer cached), and a snapshot whose host is gone (restarted or closed), which is
 *     asked of the host that is there now. One call is in flight at a time; concurrent
 *     requests share it.
 *   - STALE, NOT GONE. The last snapshot is kept when a later attempt fails or its host
 *     goes away; the answer then says `stale: true`, why (`unavailable`), and when it was
 *     received — the panel shows it marked, never blank.
 *   - EVERY VALUE IS CHECKED BY VALUE. The engine already caps and clamps (UiAccount's
 *     doc comments and the contract's answers: key ≤ 128, spendLabel ≤ 40, sameAccountAs
 *     ≤ 8 names of ≤ 64, windows ≤ 8, accounts ≤ 200); this re-applies the same caps,
 *     flattens every string to one line (controls and bidi overrides removed, well-formed
 *     UTF-16), clamps percents to 0–100, keeps a time only as a positive integer of epoch
 *     ms, and drops a `cooldownUntil` that is not after `generatedAt` (the contract makes
 *     the host send null then; readers guard as well). A login / window kind / severity
 *     this build does not know is additive, not a new schema: an error naming the value,
 *     'other', 'normal'.
 */

import type { HostAnswer, HostClient } from '../vendored/cco/host-client';
import {
  RPC_ERROR,
  TEAM_OBSERVER_TOOLS,
  WORKERS_SNAPSHOT_MIN_POLL_MS,
  WORKERS_SNAPSHOT_SCHEMA,
} from '../vendored/cco/host-protocol';
import type { WorkersSnapshot } from '../vendored/cco/host-protocol';
import type {
  LoginState,
  UiAccount,
  UiLoginCheck,
  UiTokenStats,
  UiUnsupportedAccount,
  UiUsageWindow,
} from '../vendored/cco/uiTypes';

/* ------------------------------------------------------------------------- *
 * Wire (sidecar ⇄ Rust)
 * ------------------------------------------------------------------------- */

/**
 * host → sidecar request: `{refresh?: boolean, roots?: string[], panePids?: number[], allWorkspaces?: boolean}` →
 * CcoWorkersResult. `roots`/`panePids` are the same scope `cco.team.list` takes (and, when
 * given, restate it); `refresh: true` is the user's explicit refresh.
 */
/** Contract §4.5: the orchestrator methods are `orch.*` (renamed from `cco.*`). */
export const CCO_WORKERS_METHOD = 'orch.workers';

/** The user's refresh (and a gone host's replacement) is asked at most this often. */
export const WORKERS_REFRESH_MIN_MS = 5_000;
/**
 * A list_workers call may wait for the engine's usage probes (5 s each, side by side, per
 * server.ts); past this it reads as `host-error`. Rust's call timeout must exceed
 * WORKERS_REQUEST_TIMEOUT_MS + WORKERS_SETTLE_MS.
 */
export const WORKERS_REQUEST_TIMEOUT_MS = 30_000;
/** A host in sight whose observer connection is still being made is waited for this long. */
export const WORKERS_SETTLE_MS = 3_000;

export type CcoWorkersUnavailableReason =
  /** No dispatch host in sight of the open folders / panes is reachable. */
  | 'no-host'
  /** The host that answered the held snapshot is gone (restarted, closed); another may answer next. */
  | 'host-gone'
  /** The host predates the structured answer (no `structuredContent`), or predates observers' list_workers. */
  | 'host-too-old'
  /** The host's `schema` is not one this Gilde knows; `detail` names it. */
  | 'schema-unknown'
  /** The host answered an error (`isError`, an RPC error, no answer in time); `detail` is its one line. */
  | 'host-error';

export interface CcoWorkersUnavailable {
  reason: CcoWorkersUnavailableReason;
  /** One display line (≤ 300 chars), or null. Host text: never markup, never parsed. */
  detail: string | null;
  /** When this was found (epoch ms). */
  at: number;
}

export interface CcoWorkersResult {
  /** False where the dispatch host does not exist (win32). */
  supported: boolean;
  /** The newest snapshot this sidecar holds, checked by value; null when none ever arrived. */
  snapshot: WorkersSnapshot | null;
  /** `s<sessionPid>` of the host that answered `snapshot`. */
  hostKey: string | null;
  /** When this sidecar received `snapshot` (epoch ms). */
  receivedAt: number | null;
  /** `snapshot` is not what a connected host answers now: the last attempt failed, or its host is gone. */
  stale: boolean;
  /** No host was asked for this answer: the 60 s floor served the held snapshot. */
  fromCache: boolean;
  /** Why the latest attempt gave no snapshot, or why the held one is stale; null when it is fresh. */
  unavailable: CcoWorkersUnavailable | null;
  /** When an automatic request would next reach a host (epoch ms). */
  nextPollAt: number;
  /** WORKERS_SNAPSHOT_MIN_POLL_MS, so the UI schedules by the engine's number. */
  minPollMs: number;
}

/* ------------------------------------------------------------------------- *
 * Where the hosts come from (implemented by CcoObserver)
 * ------------------------------------------------------------------------- */

/** A host with an open observer connection. */
export interface WorkersHost {
  key: string;
  hostPid: number;
  hostStartedAt: number;
  /** The session is one of Gilde's own PTY panes. */
  pane: boolean;
  client: HostClient;
}

export interface WorkersHostSource {
  readonly supported: boolean;
  workersHosts(): {
    connected: WorkersHost[];
    /** A host in sight is still being connected to. */
    settling: boolean;
    /** A host in sight is an earlier build (its hello lacked the team feature). */
    older: boolean;
    /** Why a host in sight could not be reached, one line, if one could not. */
    failure: string | undefined;
  };
}

/* ------------------------------------------------------------------------- *
 * Values, checked
 * ------------------------------------------------------------------------- */

function isRecord(value: unknown): value is Record<string, unknown> {
  return typeof value === 'object' && value !== null && !Array.isArray(value);
}

const LONE_SURROGATE = /[\uD800-\uDBFF](?![\uDC00-\uDFFF])|(?<![\uD800-\uDBFF])[\uDC00-\uDFFF]/g;
// eslint-disable-next-line no-control-regex
const CONTROL_OR_SPACE = /[\s\u0000-\u001f\u007f-\u009f\u2028\u2029]+/g;
/** Bidi embeddings/overrides/isolates and zero-width characters: they change what the eye reads. */
const INVISIBLE = /[\u202A-\u202E\u2066-\u2069\u200B-\u200F\u061C\uFEFF]/g;

/** One display line: well-formed, no controls, no bidi tricks, at most `max` UTF-16 units (never a split pair). */
export function displayLine(value: string, max: number): string {
  let s = value.replace(LONE_SURROGATE, '\uFFFD').replace(INVISIBLE, '').replace(CONTROL_OR_SPACE, ' ').trim();
  if (s.length > max) {
    let cut = Math.max(0, max);
    const code = s.charCodeAt(cut - 1);
    if (cut > 0 && code >= 0xd800 && code <= 0xdbff) cut -= 1;
    s = s.slice(0, cut);
  }
  return s;
}

function text(value: unknown, max: number): string | undefined {
  return typeof value === 'string' ? displayLine(value, max) : undefined;
}

function textOrNull(value: unknown, max: number): string | null {
  const s = text(value, max);
  return s === undefined || s === '' ? null : s;
}

/** A time: a positive finite number of epoch ms, as an integer; else null. */
function time(value: unknown): number | null {
  if (typeof value !== 'number' || !Number.isFinite(value) || value <= 0) return null;
  return Math.min(Math.trunc(value), Number.MAX_SAFE_INTEGER);
}

/** The login check states the vendored `UiLoginCheck` knows (orchestrator v2.0.0). */
const LOGIN_CHECK_STATES: readonly UiLoginCheck['state'][] = ['ok', 'unconfirmed', 'checking', 'loggedOut', 'probeFailed'];

/**
 * `UiAccount.loginCheck` by value, or `null`. A state this build does not know is
 * dropped (null), never guessed into another; `reason` is one display line.
 */
function loginCheckOf(raw: unknown): UiLoginCheck | null {
  if (!isRecord(raw)) return null;
  const state = LOGIN_CHECK_STATES.find((s) => s === raw.state);
  if (state === undefined) return null;
  return { state, since: time(raw.since), checkedAt: time(raw.checkedAt), reason: text(raw.reason, 200) ?? '' };
}

function count(value: unknown): number {
  if (typeof value !== 'number' || !Number.isFinite(value) || value <= 0) return 0;
  return Math.min(Math.trunc(value), Number.MAX_SAFE_INTEGER);
}

function money(value: unknown): number {
  return typeof value === 'number' && Number.isFinite(value) && value > 0 ? value : 0;
}

/** 0–100 as given (fractions kept), clamped; undefined when not a finite number. */
function percent(value: unknown): number | undefined {
  if (typeof value !== 'number' || !Number.isFinite(value)) return undefined;
  return Math.min(100, Math.max(0, value));
}

function oneOf<T extends string>(value: unknown, list: readonly T[]): T | undefined {
  return typeof value === 'string' && (list as readonly string[]).includes(value) ? (value as T) : undefined;
}

/*
 * Caps: the contract's (host-protocol "list_workers' structured answer", answers to a front
 * end's questions: key ≤ 128, spendLabel ≤ 40, sameAccountAs ≤ 8 names ≤ 64, windows ≤ 8,
 * accounts ≤ 200) and UiAccount's own (uiTypes.ts doc comments); where neither states one,
 * a layout bound. Exported so the tests pin them to the contract's numbers.
 */
export const WORKERS_CAPS = {
  accounts: 200,
  unsupported: 20,
  windows: 8,
  sameAccountAs: 8,
  keyChars: 128,
  nameChars: 64,
  spendLabelChars: 40,
} as const;
const ACCOUNTS_KEPT = WORKERS_CAPS.accounts;
const UNSUPPORTED_KEPT = WORKERS_CAPS.unsupported;
const WINDOWS_KEPT = WORKERS_CAPS.windows;
const SAME_ACCOUNT_KEPT = WORKERS_CAPS.sameAccountAs;
const KEY_CHARS = WORKERS_CAPS.keyChars;
const NAME_CHARS = WORKERS_CAPS.nameChars;
const PROVIDER_CHARS = 32;
const PROVIDER_LABEL_CHARS = 32;
const PERMISSION_CHARS = 32;
const MODEL_CHARS = 64;
const DETAIL_CHARS = 200;
const PLAN_CHARS = 24;
const PLAN_LABEL_CHARS = 40;
const WINDOW_LABEL_CHARS = 40;
const SPEND_LABEL_CHARS = WORKERS_CAPS.spendLabelChars;
const UNSUPPORTED_DETAIL_CHARS = 80;
const UNAVAILABLE_DETAIL_CHARS = 300;

const LOGIN_STATES: readonly LoginState[] = ['ok', 'expired', 'no-plan-limits', 'error', 'refreshing'];
const WINDOW_KINDS: readonly UiUsageWindow['kind'][] = ['session', 'weekly_all', 'weekly_scoped', 'other'];
const SEVERITIES: readonly UiUsageWindow['severity'][] = ['normal', 'warning', 'critical'];

function statsOf(raw: unknown): UiTokenStats {
  const r = isRecord(raw) ? raw : {};
  return { tasks: count(r.tasks), inputTokens: count(r.inputTokens), outputTokens: count(r.outputTokens), costUsd: money(r.costUsd) };
}

function windowOf(raw: unknown): UiUsageWindow | undefined {
  if (!isRecord(raw)) return undefined;
  const pct = percent(raw.percent);
  if (pct === undefined) return undefined;
  return {
    kind: oneOf(raw.kind, WINDOW_KINDS) ?? 'other',
    label: text(raw.label, WINDOW_LABEL_CHARS) ?? '',
    percent: pct,
    severity: oneOf(raw.severity, SEVERITIES) ?? 'normal',
    resetsAt: time(raw.resetsAt),
  };
}

/** One UiAccount by value; undefined when it has no key or no name (it could not be told apart). */
export function accountOf(raw: unknown): UiAccount | undefined {
  if (!isRecord(raw)) return undefined;
  const key = text(raw.key, KEY_CHARS);
  const name = text(raw.name, NAME_CHARS);
  if (key === undefined || key === '' || name === undefined || name === '') return undefined;
  let login = oneOf(raw.login, LOGIN_STATES);
  let loginDetail = textOrNull(raw.loginDetail, DETAIL_CHARS);
  if (login === undefined) {
    // A state this build does not know: shown as what it is, never guessed into another.
    login = 'error';
    const named = text(raw.login, 32);
    loginDetail = named ? `unknown login state "${named}"` : 'no login state';
  }
  const windows: UiUsageWindow[] = [];
  for (const entry of Array.isArray(raw.windows) ? raw.windows.slice(0, 64) : []) {
    if (windows.length >= WINDOWS_KEPT) break;
    const w = windowOf(entry);
    if (w !== undefined) windows.push(w);
  }
  const sameAccountAs: string[] = [];
  for (const entry of Array.isArray(raw.sameAccountAs) ? raw.sameAccountAs.slice(0, 64) : []) {
    if (sameAccountAs.length >= SAME_ACCOUNT_KEPT) break;
    const s = text(entry, NAME_CHARS);
    if (s) sameAccountAs.push(s);
  }
  const overageRaw = isRecord(raw.overage) ? raw.overage : undefined;
  const allTimeRaw = isRecord(raw.allTime) ? raw.allTime : {};
  return {
    key,
    name,
    isMain: raw.isMain === true,
    isWorker: raw.isWorker === true,
    preferred: raw.preferred === true,
    defaultModel: textOrNull(raw.defaultModel, MODEL_CHARS),
    provider: text(raw.provider, PROVIDER_CHARS) || 'claude',
    providerLabel: text(raw.providerLabel, PROVIDER_LABEL_CHARS) ?? '',
    sessionWorkers: raw.sessionWorkers === true,
    permissionMode: textOrNull(raw.permissionMode, PERMISSION_CHARS),
    modelRanAs: textOrNull(raw.modelRanAs, MODEL_CHARS),
    login,
    loginDetail,
    loginCheck: loginCheckOf(raw.loginCheck),
    plan: textOrNull(raw.plan, PLAN_CHARS),
    planLabel: textOrNull(raw.planLabel, PLAN_LABEL_CHARS),
    sameAccountAs,
    windows,
    windowsStaleSince: time(raw.windowsStaleSince),
    lastGoodWindowsAt: time(raw.lastGoodWindowsAt),
    usageFetchedAt: time(raw.usageFetchedAt),
    overage:
      overageRaw === undefined
        ? null
        : {
            enabled: overageRaw.enabled === true,
            percent: percent(overageRaw.percent) ?? null,
            spendLabel: textOrNull(overageRaw.spendLabel, SPEND_LABEL_CHARS),
          },
    cooldownUntil: time(raw.cooldownUntil),
    session5h: statsOf(raw.session5h),
    week: statsOf(raw.week),
    allTime: { ...statsOf(allTimeRaw), errors: count(allTimeRaw.errors) },
    lastError: textOrNull(raw.lastError, DETAIL_CHARS),
    running: count(raw.running),
  };
}

function unsupportedOf(raw: unknown): UiUnsupportedAccount | undefined {
  if (!isRecord(raw)) return undefined;
  const detail = text(raw.detail, UNSUPPORTED_DETAIL_CHARS);
  if (detail === undefined) return undefined;
  return { name: text(raw.name, NAME_CHARS) ?? '', detail };
}

/**
 * A `structuredContent` whose `schema` is `cco.workers/1` → a WorkersSnapshot every field
 * of which has its type and cap. Accounts without a key or a name, and the duplicates of
 * a key, are dropped one by one; a `generatedAt` that is not a time reads `receivedAt`; a
 * `cooldownUntil` not after `generatedAt` is null; `servedFromCache` is kept only as the
 * boolean the host sent (absent from a host that predates it — never guessed).
 */
export function workersSnapshotOf(raw: Record<string, unknown>, receivedAt: number): WorkersSnapshot {
  const generatedAt = time(raw.generatedAt) ?? receivedAt;
  const accounts: UiAccount[] = [];
  const keys = new Set<string>();
  for (const entry of Array.isArray(raw.accounts) ? raw.accounts.slice(0, ACCOUNTS_KEPT * 4) : []) {
    if (accounts.length >= ACCOUNTS_KEPT) break;
    const account = accountOf(entry);
    if (account === undefined || keys.has(account.key)) continue;
    keys.add(account.key);
    // Contract: null when not after generatedAt — the host sends that; this reader guards too.
    if (account.cooldownUntil !== null && account.cooldownUntil <= generatedAt) account.cooldownUntil = null;
    accounts.push(account);
  }
  const unsupportedAccounts: UiUnsupportedAccount[] = [];
  for (const entry of Array.isArray(raw.unsupportedAccounts) ? raw.unsupportedAccounts.slice(0, 100) : []) {
    if (unsupportedAccounts.length >= UNSUPPORTED_KEPT) break;
    const u = unsupportedOf(entry);
    if (u !== undefined) unsupportedAccounts.push(u);
  }
  const guards = isRecord(raw.guards) ? raw.guards : {};
  const snapshot: WorkersSnapshot = {
    schema: WORKERS_SNAPSHOT_SCHEMA,
    generatedAt,
    accounts,
    unsupportedAccounts,
    guards: {
      // Only the literal 'allow' is 'allow' (host-protocol WorkersSnapshot.guards).
      overageWorkerDispatch: guards.overageWorkerDispatch === 'allow' ? 'allow' : 'block',
      frontierWorkerDispatch: guards.frontierWorkerDispatch === 'allow' ? 'allow' : 'block',
    },
  };
  // Additive (CCO 0b7b1b8): true when the HOST answered from its observer cache. Only a
  // boolean is a value; anything else is as if absent.
  if (typeof raw.servedFromCache === 'boolean') snapshot.servedFromCache = raw.servedFromCache;
  return snapshot;
}

/** What one `tools/call list_workers` answer means. Never reads the text answer except as `isError`'s line. */
export type WorkersOutcome =
  | { ok: true; snapshot: WorkersSnapshot }
  | { ok: false; reason: CcoWorkersUnavailableReason; detail: string | null };

export function classifyWorkersAnswer(answer: HostAnswer, receivedAt: number): WorkersOutcome {
  switch (answer.kind) {
    case 'gone':
      return { ok: false, reason: 'host-gone', detail: 'the connection to the dispatch host closed' };
    case 'timeout':
      return { ok: false, reason: 'host-error', detail: `no answer within ${Math.round(WORKERS_REQUEST_TIMEOUT_MS / 1000)} s` };
    case 'error': {
      const detail = displayLine(answer.message, UNAVAILABLE_DETAIL_CHARS) || null;
      // A host whose role table does not let an observer call list_workers, or that has no
      // tools/call at all, predates this addition.
      if (answer.code === RPC_ERROR.roleForbidden || answer.code === -32601) {
        return { ok: false, reason: 'host-too-old', detail };
      }
      return { ok: false, reason: 'host-error', detail };
    }
    case 'result':
      break;
  }
  const result = answer.result;
  if (!isRecord(result)) return { ok: false, reason: 'host-error', detail: 'the answer is not a tools/call result' };
  if (result.isError === true) {
    // The registry could not be read (or another failure): the host's own line, as data.
    const first = Array.isArray(result.content) ? result.content.find((c) => isRecord(c) && typeof c.text === 'string') : undefined;
    const line = isRecord(first) && typeof first.text === 'string' ? displayLine(first.text, UNAVAILABLE_DETAIL_CHARS) : '';
    return { ok: false, reason: 'host-error', detail: line || null };
  }
  if (!('structuredContent' in result) || result.structuredContent === undefined || result.structuredContent === null) {
    // An older host: no snapshot. The text is prose for a model and is not parsed.
    return { ok: false, reason: 'host-too-old', detail: null };
  }
  const sc = result.structuredContent;
  if (!isRecord(sc)) return { ok: false, reason: 'schema-unknown', detail: '(not an object)' };
  if (sc.schema !== WORKERS_SNAPSHOT_SCHEMA) {
    const named = typeof sc.schema === 'string' ? displayLine(sc.schema, 64) : '';
    return { ok: false, reason: 'schema-unknown', detail: named || '(none)' };
  }
  return { ok: true, snapshot: workersSnapshotOf(sc, receivedAt) };
}

/* ------------------------------------------------------------------------- *
 * The poller: one per sidecar, shared by every consumer
 * ------------------------------------------------------------------------- */

const sleep = (ms: number): Promise<void> =>
  new Promise((resolve) => {
    const timer = setTimeout(resolve, ms);
    timer.unref?.();
  });

/** The host asked: Gilde's own pane first (its session's `main`), then the newest host process, then the key. */
export function pickWorkersHost(hosts: WorkersHost[]): WorkersHost | undefined {
  return [...hosts].sort(
    (a, b) => Number(b.pane) - Number(a.pane) || b.hostStartedAt - a.hostStartedAt || a.key.localeCompare(b.key),
  )[0];
}

export class WorkersPoller {
  private held: { snapshot: WorkersSnapshot; hostKey: string; hostPid: number; receivedAt: number } | undefined;
  /** The latest attempt's failure; cleared by a snapshot. */
  private failure: CcoWorkersUnavailable | undefined;
  /** When a list_workers call was last sent, and to which host process. */
  private lastCallAt = Number.NEGATIVE_INFINITY;
  private lastCallHost: { key: string; hostPid: number } | undefined;
  private inFlight: Promise<void> | undefined;

  constructor(
    private readonly source: WorkersHostSource,
    private readonly log: (level: 'debug' | 'info' | 'warn' | 'error', msg: string) => void = () => undefined,
    private readonly now: () => number = Date.now,
  ) {}

  async answer(opts: { refresh: boolean }): Promise<CcoWorkersResult> {
    if (!this.source.supported) return this.compose(false);
    if (this.inFlight !== undefined) {
      await this.inFlight;
      return this.compose(false);
    }
    if (!this.mayPoll(this.now(), opts.refresh)) return this.compose(true);
    this.inFlight = this.poll().finally(() => {
      this.inFlight = undefined;
    });
    await this.inFlight;
    return this.compose(false);
  }

  /** Is the host the last call went to (and the held snapshot's) still connected? */
  private connected(host: { key: string; hostPid: number } | undefined): boolean {
    if (host === undefined) return false;
    return this.source.workersHosts().connected.some((h) => h.key === host.key && h.hostPid === host.hostPid && !h.client.closed);
  }

  private mayPoll(now: number, refresh: boolean): boolean {
    const since = now - this.lastCallAt;
    if (since >= WORKERS_SNAPSHOT_MIN_POLL_MS) return true;
    const sourceGone = !this.connected(this.lastCallHost);
    return (refresh || sourceGone) && since >= WORKERS_REFRESH_MIN_MS;
  }

  private nextPollAt(now: number): number {
    if (this.mayPoll(now, false)) return now;
    if (!this.connected(this.lastCallHost)) return this.lastCallAt + WORKERS_REFRESH_MIN_MS;
    return this.lastCallAt + WORKERS_SNAPSHOT_MIN_POLL_MS;
  }

  private async poll(): Promise<void> {
    let hosts = this.source.workersHosts();
    if (hosts.connected.length === 0 && hosts.settling) {
      const deadline = this.now() + WORKERS_SETTLE_MS;
      while (this.now() < deadline) {
        await sleep(50);
        hosts = this.source.workersHosts();
        if (hosts.connected.length > 0 || !hosts.settling) break;
      }
    }
    const host = pickWorkersHost(hosts.connected);
    if (host === undefined) {
      this.failure = {
        reason: hosts.older ? 'host-too-old' : 'no-host',
        detail: hosts.older || hosts.failure === undefined ? null : displayLine(hosts.failure, UNAVAILABLE_DETAIL_CHARS),
        at: this.now(),
      };
      return;
    }
    this.lastCallAt = this.now();
    this.lastCallHost = { key: host.key, hostPid: host.hostPid };
    let answer: HostAnswer;
    try {
      answer = await host.client.request('tools/call', { name: TEAM_OBSERVER_TOOLS[0], arguments: {} }, WORKERS_REQUEST_TIMEOUT_MS);
    } catch (err) {
      answer = { kind: 'error', code: undefined, message: err instanceof Error ? err.message : String(err) };
    }
    const receivedAt = this.now();
    let outcome: WorkersOutcome;
    try {
      outcome = classifyWorkersAnswer(answer, receivedAt);
    } catch (err) {
      outcome = { ok: false, reason: 'host-error', detail: displayLine(err instanceof Error ? err.message : String(err), UNAVAILABLE_DETAIL_CHARS) };
    }
    if (outcome.ok) {
      this.held = { snapshot: outcome.snapshot, hostKey: host.key, hostPid: host.hostPid, receivedAt };
      this.failure = undefined;
      return;
    }
    this.failure = { reason: outcome.reason, detail: outcome.detail, at: receivedAt };
    this.log('info', `[cco] list_workers on host pid ${host.hostPid}: ${outcome.reason}${outcome.detail ? ` (${outcome.detail})` : ''}`);
  }

  private compose(fromCache: boolean): CcoWorkersResult {
    const now = this.now();
    const supported = this.source.supported;
    const held = this.held;
    let unavailable: CcoWorkersUnavailable | null = this.failure ?? null;
    const heldConnected = held !== undefined && this.connected({ key: held.hostKey, hostPid: held.hostPid });
    if (supported && unavailable === null && held !== undefined && !heldConnected) {
      const hosts = this.source.workersHosts();
      unavailable = {
        reason: hosts.connected.length > 0 ? 'host-gone' : hosts.older ? 'host-too-old' : 'no-host',
        detail: null,
        at: now,
      };
    }
    return {
      supported,
      snapshot: held?.snapshot ?? null,
      hostKey: held?.hostKey ?? null,
      receivedAt: held?.receivedAt ?? null,
      stale: held !== undefined && (unavailable !== null || !heldConnected),
      fromCache,
      unavailable,
      nextPollAt: supported ? this.nextPollAt(now) : now + WORKERS_SNAPSHOT_MIN_POLL_MS,
      minPollMs: WORKERS_SNAPSHOT_MIN_POLL_MS,
    };
  }
}
