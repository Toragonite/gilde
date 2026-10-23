/**
 * methods/turns.ts — `turn.start` / `turn.cancel` and the `turn.event`
 * notification (contract §4.5, §6.3). Replaces the old `mesh.run/stream/resume`.
 *
 *   - **Every field is forwarded** (the `normalizeTask` fix, R1 §4 `mesh.ts:185-217`):
 *     prompt, cwd, model, permission mode, account, wall clock AND `resumeId` reach
 *     the adapter. Codex refuses a resumed turn without a model, so that case is
 *     refused here, before anything spawns, rather than failing inside the CLI.
 *   - **Exactly one terminal event.** A turn ends with one `done` or one `error`;
 *     a source that throws, ends early or is cancelled gets one synthesized here
 *     (`done{stopped}` for a cancel). `seq` is strictly increasing per turn, and
 *     `turn.event` is never dropped by the channel (rpc.ts gives it rank 0).
 *   - **Text is coalesced** over 50 ms, ≤ 16 KiB per event, cut on a character
 *     boundary.
 *   - **Typed errors.** `error.class` is `auth` / `quota` / `exec` / `unknown`:
 *     from the thrown error's name when the adapter typed it, else from a fixed,
 *     conservative pattern list over the message (the stream path only carries a
 *     message), else `unknown`.
 *   - **No credential is read**: the adapters run with `skipAuthCheck` (codex's
 *     model-vs-login gate reads `auth.json`) and the never-open seams (fleet.ts).
 */

import { canStream } from '@toragonite/agent-mesh';
import type { Account, Adapter, RunEvent, RunOptions, RunResult, RunTask, TokenUsage } from '@toragonite/agent-mesh';

import { runCtx } from '../exec-registered';
import type { SidecarState } from '../fleet';
import type { PermissionMode } from '../gen/PermissionMode';
import type { TurnEvent } from '../gen/TurnEvent';
import type { TurnEventParams } from '../gen/TurnEventParams';
import type { TurnStartParams } from '../gen/TurnStartParams';
import type { TurnUsage } from '../gen/TurnUsage';
import type { Vendor } from '../gen/Vendor';
import type { VendorErrorClass } from '../gen/VendorErrorClass';
import { RpcError, RPC_UNSUPPORTED } from '../rpc';
import type { Rpc } from '../rpc';
import { beginRun, cancelRun, endRun } from '../runs';
import type { RunEntry } from '../runs';
import {
  RPC_NOT_READY,
  invalid,
  meshVendorOf,
  requireDirectory,
  turnCancelParamsOf,
  turnStartParamsOf,
} from './wire';

/** Coalescing window for `text` events (contract §4.5). */
export const TEXT_MERGE_MS = 50;
/** Upper bound on one merged `text` event's payload (contract §4.5). */
export const TEXT_MAX_BYTES = 16 * 1024;
/**
 * Wall clock when the host names none (`timeoutMs: null`). A turn without a
 * clock is a scheduled leak.
 */
export const FALLBACK_TIMEOUT_MS = 45 * 60_000;
/** Cap of a tool name and of a tool detail preview (memory-only content). */
export const TOOL_NAME_MAX = 128;
export const TOOL_DETAIL_MAX = 2048;
/** Cap of an error message (content, never logged). */
export const ERROR_MESSAGE_MAX = 4096;

/**
 * Contract permission modes (Claude's vocabulary, R4 §1.9) → each turn-based
 * vendor's own flag value. agent-mesh passes `permissionMode` verbatim to the
 * vendor flag (codex `--sandbox`, cursor `--sandbox`, agy `--mode`), so a Claude
 * mode must never reach those CLIs unmapped. Unmapped = the vendor's default
 * (no flag) — the conservative choice for every mode not listed.
 */
export const TURN_PERMISSION_MAP: Readonly<Record<Vendor, Partial<Record<PermissionMode, string>>>> = {
  claude: {
    default: 'default',
    acceptEdits: 'acceptEdits',
    plan: 'plan',
    dontAsk: 'dontAsk',
    auto: 'auto',
    bypassPermissions: 'bypassPermissions',
  },
  codex: { plan: 'read-only', acceptEdits: 'workspace-write', auto: 'workspace-write' },
  cursor: {},
  agy: {},
};

// ── error classes ────────────────────────────────────────────────────────────

const AUTH_PATTERNS = [
  /not (?:logged|signed) in/i,
  /\blog ?in required\b/i,
  /please (?:run )?\/?log ?in/i,
  /\bunauthori[sz]ed\b/i,
  /\bauthentication (?:failed|error|required)\b/i,
  /\binvalid (?:api[ _-]?key|token|credentials?)\b/i,
  /\b(?:token|session|login) (?:has )?expired\b/i,
  /\b401\b/,
];
const QUOTA_PATTERNS = [
  /\brate[ _-]?limit/i,
  /\bquota\b/i,
  /\busage limit\b/i,
  /\btoo many requests\b/i,
  /\blimit (?:reached|exceeded)\b/i,
  /\bout of (?:credits|quota)\b/i,
  /\bcredit balance\b/i,
  /\b429\b/,
];
const EXEC_PATTERNS = [
  /\bENOENT\b/,
  /\bEACCES\b/,
  /\bspawn\b/i,
  /\bexited with (?:code|signal)\b/i,
  /\btimed out\b/i,
  /\bended without a (?:result|terminal)/i,
  /registerSpawn/,
  /\bnot installed\b/i,
];

/** The vendor error class of a message (contract §4.4 `VENDOR_ERROR.class`). */
export function classifyMessage(message: string): VendorErrorClass {
  if (AUTH_PATTERNS.some((re) => re.test(message))) return 'auth';
  if (QUOTA_PATTERNS.some((re) => re.test(message))) return 'quota';
  if (EXEC_PATTERNS.some((re) => re.test(message))) return 'exec';
  return 'unknown';
}

/** The class of a thrown value: the adapter's typed name first, then the message. */
export function classifyError(err: unknown): VendorErrorClass {
  const name = err instanceof Error ? err.name : '';
  if (name === 'AuthError') return 'auth';
  if (name === 'QuotaExceededError') return 'quota';
  if (name === 'AdapterExecError') {
    const byMessage = classifyMessage(err instanceof Error ? err.message : '');
    return byMessage === 'unknown' ? 'exec' : byMessage;
  }
  return classifyMessage(err instanceof Error ? err.message : String(err));
}

// ── value cleaning ───────────────────────────────────────────────────────────

/**
 * The largest prefix of `s` that fits in `capBytes` UTF-8 bytes, plus the rest.
 * Cuts on a character boundary and never splits a surrogate pair.
 */
export function takeChunk(s: string, capBytes: number): [string, string] {
  if (Buffer.byteLength(s, 'utf8') <= capBytes) return [s, ''];
  let lo = 0;
  let hi = s.length;
  while (lo < hi) {
    const mid = (lo + hi + 1) >> 1;
    if (Buffer.byteLength(s.slice(0, mid), 'utf8') <= capBytes) lo = mid;
    else hi = mid - 1;
  }
  const code = lo > 0 ? s.charCodeAt(lo - 1) : 0;
  if (lo > 0 && code >= 0xd800 && code <= 0xdbff) lo -= 1;
  if (lo === 0) lo = Math.min(s.length, 1); // pathological cap: make progress anyway
  return [s.slice(0, lo), s.slice(lo)];
}

/** Well-formed UTF-16 (a lone surrogate would make the host reject the whole line). */
function wellFormed(s: string): string {
  let out = '';
  for (let i = 0; i < s.length; i += 1) {
    const c = s.charCodeAt(i);
    if (c >= 0xd800 && c <= 0xdbff) {
      const next = s.charCodeAt(i + 1);
      if (next >= 0xdc00 && next <= 0xdfff) {
        out += s[i]! + s[i + 1]!;
        i += 1;
      } else {
        out += '�';
      }
    } else if (c >= 0xdc00 && c <= 0xdfff) {
      out += '�';
    } else {
      out += s[i]!;
    }
  }
  return out;
}

function capText(s: string, maxBytes: number): string {
  return takeChunk(wellFormed(s), maxBytes)[0];
}

/** A tool name: one line, no control characters, ≤ {@link TOOL_NAME_MAX} bytes. */
export function cleanToolName(name: unknown): string {
  const s = typeof name === 'string' ? name : String(name ?? '');
  // eslint-disable-next-line no-control-regex
  const oneLine = s.replace(/[\u0000-\u001f\u007f]+/g, ' ').trim();
  return capText(oneLine.length > 0 ? oneLine : 'tool', TOOL_NAME_MAX);
}

/** A tool's detail as a bounded preview string, or `null` when it has none. */
export function detailPreview(detail: unknown): string | null {
  if (detail === undefined || detail === null) return null;
  let s: string;
  if (typeof detail === 'string') s = detail;
  else {
    try {
      s = JSON.stringify(detail) ?? '';
    } catch {
      s = '';
    }
  }
  if (s.length === 0) return null;
  return capText(s, TOOL_DETAIL_MAX);
}

/** A token count the host can carry: a safe, non-negative integer, else absent. */
function count(value: unknown): number | null {
  return typeof value === 'number' && Number.isSafeInteger(value) && value >= 0 ? value : null;
}

/** agent-mesh `TokenUsage` → `TurnUsage` (absent = unknown, never 0). */
export function turnUsage(usage: TokenUsage | undefined): TurnUsage | null {
  if (usage === undefined || usage === null) return null;
  const out: TurnUsage = {
    inputTokens: count(usage.inputTokens),
    outputTokens: count(usage.outputTokens),
    cachedInputTokens: count(usage.cachedInputTokens),
  };
  return out.inputTokens === null && out.outputTokens === null && out.cachedInputTokens === null ? null : out;
}

/** The terminal event of a `RunResult` (`status: error` is an error, never a `done`). */
export function terminalOf(result: RunResult, lastConversationId: string): TurnEvent {
  const conversationId =
    typeof result.conversationId === 'string' && result.conversationId !== ''
      ? result.conversationId
      : lastConversationId;
  if (result.status === 'error') {
    const message = capText(String(result.note ?? result.text ?? 'the turn failed'), ERROR_MESSAGE_MAX);
    return { kind: 'error', class: classifyMessage(message), message };
  }
  const status = result.status === 'incomplete' || result.status === 'stopped' ? result.status : 'ok';
  return { kind: 'done', status, conversationId, usage: turnUsage(result.usage), costUsd: null };
}

function errorEvent(err: unknown): TurnEvent {
  const message = capText(err instanceof Error ? err.message : String(err), ERROR_MESSAGE_MAX);
  return { kind: 'error', class: classifyError(err), message };
}

// ── text coalescing ──────────────────────────────────────────────────────────

interface TextCoalescer {
  push(text: string): void;
  flush(): void;
  cancelTimer(): void;
}

function createTextCoalescer(emit: (delta: string) => void): TextCoalescer {
  let buffered = '';
  let timer: NodeJS.Timeout | undefined;

  const cancelTimer = (): void => {
    if (timer !== undefined) {
      clearTimeout(timer);
      timer = undefined;
    }
  };

  const drain = (all: boolean): void => {
    while (buffered.length > 0) {
      if (!all && Buffer.byteLength(buffered, 'utf8') < TEXT_MAX_BYTES) return;
      const [chunk, rest] = takeChunk(buffered, TEXT_MAX_BYTES);
      buffered = rest;
      emit(chunk);
    }
  };

  return {
    push(text: string): void {
      if (typeof text !== 'string' || text.length === 0) return;
      buffered += wellFormed(text);
      drain(false);
      if (buffered.length > 0 && timer === undefined) {
        timer = setTimeout(() => {
          timer = undefined;
          drain(true);
        }, TEXT_MERGE_MS);
        timer.unref?.();
      }
    },
    flush(): void {
      cancelTimer();
      drain(true);
    },
    cancelTimer,
  };
}

// ── the pump ─────────────────────────────────────────────────────────────────

/** A `run()` result as a one-shot event stream, for an adapter that cannot stream. */
async function* runAsEvents(adapter: Adapter, task: RunTask, opts: RunOptions): AsyncGenerator<RunEvent> {
  const result = await adapter.run(task, opts);
  if (typeof result.conversationId === 'string' && result.conversationId !== '') {
    yield { type: 'session', conversationId: result.conversationId };
  }
  if (typeof result.text === 'string' && result.text.length > 0) yield { type: 'text', text: result.text };
  yield { type: 'done', result };
}

const ABORTED = Symbol('aborted');

/**
 * Drive one turn to exactly one terminal event, forwarding every event as a
 * `turn.event` notification with a strictly increasing `seq`.
 *
 * The source is a factory so that opening it — where `adapter.stream()` performs
 * its eager validation and can throw — is inside the same try, and so tests can
 * drive this pump with a scripted source (`dev.turnScript`).
 */
export async function pumpTurn(
  rpc: Rpc,
  turnId: string,
  open: () => AsyncIterable<RunEvent> | Promise<AsyncIterable<RunEvent>>,
  entry: RunEntry,
): Promise<void> {
  let seq = 0;
  const emit = (event: TurnEvent): void => {
    seq += 1;
    const params: TurnEventParams = { turnId, seq, event };
    rpc.notify('turn.event', params);
  };
  const text = createTextCoalescer((delta) => emit({ kind: 'text', delta }));
  let terminal = false;
  let lastConversationId = '';

  const emitTerminal = (event: TurnEvent): void => {
    if (terminal) return;
    terminal = true;
    text.flush();
    emit(event);
  };

  const aborted = new Promise<typeof ABORTED>((resolve) => {
    if (entry.abort.signal.aborted) resolve(ABORTED);
    else entry.abort.signal.addEventListener('abort', () => resolve(ABORTED), { once: true });
  });

  try {
    const events = await open();
    const it = events[Symbol.asyncIterator]();
    for (;;) {
      const next = await Promise.race([it.next(), aborted]);
      if (next === ABORTED) {
        // The child's group already has its SIGTERM (cancelRun); unblock the
        // source so its seam settles the registry row.
        void Promise.resolve(it.return?.()).catch(() => {});
        break;
      }
      if (next.done === true) break;
      const event = next.value;
      if (terminal) continue; // nothing follows a terminal event
      switch (event.type) {
        case 'text':
          text.push(event.text);
          break;
        case 'tool':
          text.flush();
          emit({ kind: 'tool', name: cleanToolName(event.name), detailPreview: detailPreview(event.detail) });
          break;
        case 'usage': {
          const usage = turnUsage(event.usage);
          if (usage !== null) {
            text.flush();
            emit({ kind: 'usage', usage });
          }
          break;
        }
        case 'session':
          if (typeof event.conversationId === 'string' && event.conversationId !== '') {
            lastConversationId = event.conversationId;
            text.flush();
            emit({ kind: 'session', conversationId: event.conversationId });
          }
          break;
        case 'done':
          emitTerminal(terminalOf(event.result, lastConversationId));
          break;
        case 'error': {
          const message = capText(String(event.message ?? 'the turn failed'), ERROR_MESSAGE_MAX);
          emitTerminal({ kind: 'error', class: classifyMessage(message), message });
          break;
        }
        default:
          break; // an event kind this build does not know is ignored, never guessed
      }
    }
  } catch (err) {
    emitTerminal(errorEvent(err));
  } finally {
    text.cancelTimer();
    if (!terminal) {
      emitTerminal(
        entry.abort.signal.aborted
          ? { kind: 'done', status: 'stopped', conversationId: lastConversationId, usage: null, costUsd: null }
          : { kind: 'error', class: 'exec', message: 'the turn ended without a result' },
      );
    }
    endRun(turnId);
  }
}

// ── turn.start → RunTask ─────────────────────────────────────────────────────

/** The agent-mesh account of a configured account id (`undefined` = the default login). */
export function meshAccountOf(state: SidecarState, accountId: string, vendor: Vendor): Account | undefined {
  const env = state.accounts.get(accountId);
  if (env === undefined || env.vendor !== vendor) {
    throw invalid('accountId', 'unknown', 'accountId is not a configured account of this vendor');
  }
  // C8: the default login runs with the account variable UNSET — no configDir.
  // agy ignores AGY_HOME, so it never gets one either (§3.4.3).
  if (env.envKey === null || env.configDir === null || vendor === 'agy') return undefined;
  return { vendor: meshVendorOf(vendor), configDir: env.configDir };
}

/** Every `TurnStartParams` field, forwarded (the `normalizeTask` fix). */
export function runTaskOf(p: TurnStartParams, account: Account | undefined): RunTask {
  const task: RunTask = {
    prompt: p.prompt,
    cwd: p.cwd,
    timeoutMs: p.timeoutMs ?? FALLBACK_TIMEOUT_MS,
  };
  if (p.model !== null) task.model = p.model;
  if (p.permissionMode !== null) {
    const mapped = TURN_PERMISSION_MAP[p.vendor][p.permissionMode];
    if (mapped !== undefined) task.permissionMode = mapped;
  }
  if (p.resumeId !== null) task.resumeId = p.resumeId;
  if (account !== undefined) task.account = account;
  return task;
}

function requireState(get: () => SidecarState | undefined): SidecarState {
  const state = get();
  if (state === undefined || !state.configured) {
    throw new RpcError(RPC_NOT_READY, 'mesh.hello and mesh.configure must come first', { name: 'NotReady' });
  }
  return state;
}

/**
 * Register `turn.start` / `turn.cancel`. `getState` re-reads the state on every
 * call because `mesh.configure` REBUILDS the fleet.
 */
export function registerTurnMethods(rpc: Rpc, getState: () => SidecarState | undefined): void {
  rpc.on('turn.start', (raw: unknown) => {
    const p = turnStartParamsOf(raw);
    const state = requireState(getState);
    if (!state.built.configured.has(p.vendor)) {
      throw new RpcError(RPC_UNSUPPORTED, `${p.vendor} is not installed`, { cap: 'vendorNotInstalled' });
    }
    const adapter = state.built.adapters.get(p.vendor);
    if (adapter === undefined) {
      throw new RpcError(RPC_UNSUPPORTED, `${p.vendor} has no adapter`, { cap: 'vendorNotInstalled' });
    }
    const account = meshAccountOf(state, p.accountId, p.vendor);
    requireDirectory(p.cwd, 'cwd');
    // R2 C6: codex refuses `exec resume` without a model.
    if (p.vendor === 'codex' && p.resumeId !== null && p.model === null) {
      throw invalid('model', 'required', 'a codex resume needs a model');
    }
    const task = runTaskOf(p, account);

    let entry: RunEntry;
    try {
      entry = beginRun(p.turnId, p.vendor);
    } catch (err) {
      throw invalid('turnId', 'unique', err instanceof Error ? err.message : String(err));
    }

    // We never read the login state (C6): codex's model-vs-login gate is skipped.
    const opts: RunOptions = { skipAuthCheck: true };
    const ctx = { turnId: p.turnId, vendor: p.vendor, kind: 'turn' as const };
    // Detached on purpose: the response must land before the first event does.
    // `pumpTurn` owns every failure inside itself, so this never rejects.
    void runCtx.run(ctx, () =>
      pumpTurn(
        rpc,
        p.turnId,
        () => (canStream(adapter) ? adapter.stream(task, opts) : runAsEvents(adapter, task, opts)),
        entry,
      ),
    );
    return { accepted: true };
  });

  rpc.on('turn.cancel', (raw: unknown) => {
    const p = turnCancelParamsOf(raw);
    return { signalled: cancelRun(p.turnId).signalled };
  });

  registerTurnSelfTest(rpc);
}

/**
 * A scripted driver for {@link pumpTurn}, exposed ONLY under
 * `GILDE_SIDECAR_SELFTEST=1`, so the production channel never gains a way to
 * inject events into a turn. It reaches the branches no real adapter can be
 * talked into (an iterator that throws or just ends without a terminal event).
 */
function registerTurnSelfTest(rpc: Rpc): void {
  if (process.env.GILDE_SIDECAR_SELFTEST !== '1') return;

  rpc.on('dev.turnScript', (params: unknown) => {
    const p = (typeof params === 'object' && params !== null ? params : {}) as Record<string, unknown>;
    const turnId = typeof p.turnId === 'string' ? p.turnId : 'selftest';
    const scripted = Array.isArray(p.events) ? (p.events as RunEvent[]) : [];
    const end = p.end === 'throw' ? 'throw' : p.end === 'hang' ? 'hang' : 'natural';
    const throwMessage = typeof p.throwMessage === 'string' ? p.throwMessage : 'scripted failure';
    const entry = beginRun(turnId, 'selftest');
    void pumpTurn(
      rpc,
      turnId,
      () =>
        (async function* scriptedSource(): AsyncGenerator<RunEvent> {
          for (const event of scripted) yield event;
          if (end === 'throw') throw new Error(throwMessage);
          if (end === 'hang') await new Promise(() => {});
        })(),
      entry,
    );
    return { accepted: true };
  });
}
