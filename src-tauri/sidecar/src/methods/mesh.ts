/**
 * methods/mesh.ts — the `mesh.*` run and introspection surface (DESIGN §5-4).
 *
 * Everything here is a thin, honest shell around an agent-mesh adapter:
 *
 *   - **No vendor knowledge.** Every method takes a `vendor`, looks the adapter up
 *     in the current build, and calls the contract method. Nothing branches on who
 *     the vendor is; capability questions are answered by `adapter.capabilities`.
 *   - **No routing.** `mesh.run`/`mesh.stream`/`mesh.resume` require an explicit
 *     vendor. Choosing one is `fleet.route`'s job and nothing else's (§5-4).
 *   - **Results pass through verbatim.** A `RunResult` is returned exactly as the
 *     adapter produced it, `conversationId: ''` included — that empty string is the
 *     contract's way of saying "not resumable", and normalising it away would
 *     silently promise the user a resume that cannot happen.
 *   - **Every spawn is attributed.** The adapter call runs inside
 *     `runCtx.run({runId, vendor})`, which is how the supervision seam learns the
 *     run id it stamps on the durable registry row (§5-6).
 *
 * Two host-imposed rules on `RunTask` are enforced here rather than trusted (§5-4):
 * `cwd` must be an absolute path to an existing directory, and `timeoutMs` is
 * always set — a run with no wall clock is a scheduled leak.
 *
 * Errors become JSON-RPC errors carrying the AgentMesh error NAME in `data`, so the
 * host can tell an auth failure from a quota wall from a crashed CLI without
 * pattern-matching on prose. Nothing here is allowed to kill the process: handler
 * throws are turned into responses by `rpc.ts`, and the detached streaming pump
 * wraps its whole body.
 */

import { canStream } from '@toragonite/agent-mesh';
import type {
  Account,
  Adapter,
  QuotaSnapshot,
  ResumeOptions,
  RunEvent,
  RunTask,
} from '@toragonite/agent-mesh';
import * as fs from 'node:fs';
import * as path from 'node:path';

import { INTROSPECT_RUN_ID, runCtx } from '../exec-registered';
import type { SidecarState } from '../fleet';
import { RpcError, RPC_INVALID_PARAMS, RPC_UNSUPPORTED } from '../rpc';
import type { Rpc } from '../rpc';
import { beginRun, cancelRun, endRun } from '../runs';
import type { RunEntry } from '../runs';

/**
 * Application error code for "the adapter (or the agent it drives) failed".
 * Distinct from the -326xx transport codes: the channel is healthy, the work is not.
 */
export const RPC_ADAPTER_ERROR = -32001;

/** Coalescing window for `text` events (DESIGN §5-8). */
export const TEXT_MERGE_MS = 50;
/** Upper bound on one merged `text` event's payload (DESIGN §5-8). */
export const TEXT_MAX_BYTES = 16 * 1024;

/**
 * Fallback wall clock when neither the task nor the handshake defaults name one.
 * Matches the cco registry's `workerTimeoutMinutes: 45` import default (§5-4).
 */
export const FALLBACK_TIMEOUT_MS = 45 * 60_000;

/** Bytes of a failing child's stderr attached to an error's `data`. */
const STDERR_CAP = 2048;

function isRecord(x: unknown): x is Record<string, unknown> {
  return typeof x === 'object' && x !== null && !Array.isArray(x);
}

function invalid(message: string, data?: unknown): RpcError {
  return new RpcError(RPC_INVALID_PARAMS, message, data);
}

function requireString(value: unknown, field: string): string {
  if (typeof value !== 'string' || value.trim() === '') {
    throw invalid(`${field} must be a non-empty string`);
  }
  return value;
}

/**
 * Map any thrown value to a JSON-RPC error whose `data.name` is the AgentMesh error
 * name (`AuthError`, `QuotaExceededError`, `AdapterExecError`, `NoRouteError`, …).
 * An `RpcError` we raised ourselves passes through unchanged.
 */
export function toRpcFailure(err: unknown, vendor?: string): RpcError {
  if (err instanceof RpcError) return err;
  if (err instanceof Error) {
    const rec = err as unknown as Record<string, unknown>;
    const data: Record<string, unknown> = { name: err.name };
    const errVendor = typeof rec.vendor === 'string' ? rec.vendor : vendor;
    if (errVendor !== undefined) data.vendor = errVendor;
    if (typeof rec.exitCode === 'number') data.exitCode = rec.exitCode;
    if (typeof rec.stderr === 'string' && rec.stderr.length > 0) {
      data.stderr = rec.stderr.slice(-STDERR_CAP);
    }
    return new RpcError(RPC_ADAPTER_ERROR, err.message, data);
  }
  return new RpcError(RPC_ADAPTER_ERROR, String(err), { name: 'UnknownError' });
}

/** The run/stream adapter for a vendor, or `-32602` when nothing is registered. */
function runAdapter(state: SidecarState, vendor: string): Adapter {
  const adapter = state.built.adapters.get(vendor);
  if (adapter === undefined) {
    throw invalid(`no adapter registered for vendor '${vendor}'`, {
      registered: [...state.built.adapters.keys()],
    });
  }
  return adapter;
}

/**
 * The introspection twin of {@link runAdapter}. Its seams are pinned to
 * `runId:'introspect'`, so a quota or model probe shows up in the Process Monitor
 * without ever being charged to a user's dispatch run (§5-6).
 */
function introspectAdapter(state: SidecarState, vendor: string): Adapter {
  const adapter = state.built.introspect.get(vendor);
  if (adapter === undefined) {
    throw invalid(`no adapter registered for vendor '${vendor}'`, {
      registered: [...state.built.introspect.keys()],
    });
  }
  return adapter;
}

/** Read an optional `Account` param, rejecting a present-but-malformed one. */
function toAccount(raw: unknown, vendor: string): Account | undefined {
  if (raw === undefined || raw === null) return undefined;
  if (!isRecord(raw)) throw invalid('account must be an object');
  const account: Account = { vendor: typeof raw.vendor === 'string' ? raw.vendor : vendor };
  if (typeof raw.label === 'string') account.label = raw.label;
  if (typeof raw.configDir === 'string') account.configDir = raw.configDir;
  return account;
}

/**
 * Validate a `cwd` and return it resolved.
 *
 * The sidecar refuses to let an adapter inherit ITS cwd (§5-4): a relative path or
 * a directory that does not exist is `-32602`, never a run that quietly edits
 * whatever directory this process happens to be sitting in.
 */
function requireCwd(value: unknown, field: string): string {
  const cwd = requireString(value, field);
  if (!path.isAbsolute(cwd)) {
    throw invalid(`${field} must be an absolute path`, { cwd });
  }
  const resolved = path.resolve(cwd);
  let stat: fs.Stats;
  try {
    stat = fs.statSync(resolved);
  } catch {
    throw invalid(`${field} does not exist`, { cwd: resolved });
  }
  if (!stat.isDirectory()) {
    throw invalid(`${field} is not a directory`, { cwd: resolved });
  }
  return resolved;
}

/** The effective wall clock: task value, else the handshake default, else the fallback. */
function effectiveTimeout(state: SidecarState, requested: unknown): number {
  if (typeof requested === 'number' && Number.isFinite(requested) && requested > 0) {
    return requested;
  }
  if (requested !== undefined && requested !== null) {
    throw invalid('timeoutMs must be a finite number > 0');
  }
  const fromDefaults = state.defaults.timeoutMs;
  return typeof fromDefaults === 'number' && Number.isFinite(fromDefaults) && fromDefaults > 0
    ? fromDefaults
    : FALLBACK_TIMEOUT_MS;
}

/**
 * Validate and complete a `RunTask`. Unknown fields are dropped rather than
 * forwarded: the adapters have an explicit `extra` escape hatch, and passing
 * anything else through would make a typo look like a feature.
 */
function normalizeTask(state: SidecarState, raw: unknown, vendor: string): RunTask {
  if (!isRecord(raw)) throw invalid('task must be an object');
  const task: RunTask = {
    prompt: requireString(raw.prompt, 'task.prompt'),
    cwd: requireCwd(raw.cwd, 'task.cwd'),
    timeoutMs: effectiveTimeout(state, raw.timeoutMs),
  };
  if (typeof raw.model === 'string' && raw.model.trim() !== '') task.model = raw.model;

  const permissionMode =
    typeof raw.permissionMode === 'string' && raw.permissionMode.trim() !== ''
      ? raw.permissionMode
      : state.defaults.permissionMode;
  if (typeof permissionMode === 'string' && permissionMode.trim() !== '') {
    task.permissionMode = permissionMode;
  }

  if (raw.allowedTools !== undefined && raw.allowedTools !== null) {
    if (!Array.isArray(raw.allowedTools) || raw.allowedTools.some((t) => typeof t !== 'string')) {
      throw invalid('task.allowedTools must be an array of strings');
    }
    task.allowedTools = raw.allowedTools as string[];
  }

  const account = toAccount(raw.account, vendor);
  if (account !== undefined) task.account = account;

  if (raw.extra !== undefined && raw.extra !== null) {
    if (!isRecord(raw.extra)) throw invalid('task.extra must be an object');
    task.extra = raw.extra;
  }
  return task;
}

/** `ResumeOptions` for `mesh.resume`. `extra.cwd` is validated the same way `task.cwd` is. */
function normalizeResumeOptions(state: SidecarState, raw: unknown, vendor: string): ResumeOptions {
  const rec = isRecord(raw) ? raw : {};
  const opts: ResumeOptions = { timeoutMs: effectiveTimeout(state, rec.timeoutMs) };
  if (typeof rec.model === 'string' && rec.model.trim() !== '') opts.model = rec.model;
  const account = toAccount(rec.account, vendor);
  if (account !== undefined) opts.account = account;
  if (rec.extra !== undefined && rec.extra !== null) {
    if (!isRecord(rec.extra)) throw invalid('opts.extra must be an object');
    const extra: Record<string, unknown> = { ...rec.extra };
    if (extra.cwd !== undefined) extra.cwd = requireCwd(extra.cwd, 'opts.extra.cwd');
    opts.extra = extra;
  }
  return opts;
}

/** Run an introspection probe with its spawns attributed to the introspect run. */
function withIntrospect<T>(vendor: string, fn: () => Promise<T>): Promise<T> {
  return runCtx.run({ runId: INTROSPECT_RUN_ID, vendor }, fn);
}

/**
 * The largest prefix of `s` that fits in `capBytes` UTF-8 bytes, plus the rest.
 * Cuts on a character boundary and never splits a surrogate pair, so a merged
 * `text` event can never carry half of an emoji.
 */
function takeChunk(s: string, capBytes: number): [string, string] {
  if (Buffer.byteLength(s, 'utf8') <= capBytes) return [s, ''];
  let lo = 0;
  let hi = s.length;
  while (lo < hi) {
    const mid = (lo + hi + 1) >> 1;
    if (Buffer.byteLength(s.slice(0, mid), 'utf8') <= capBytes) lo = mid;
    else hi = mid - 1;
  }
  // Never end on a lone high surrogate.
  const code = lo > 0 ? s.charCodeAt(lo - 1) : 0;
  if (lo > 0 && code >= 0xd800 && code <= 0xdbff) lo -= 1;
  if (lo === 0) lo = Math.min(s.length, 1); // pathological cap: make progress anyway
  return [s.slice(0, lo), s.slice(lo)];
}

interface TextCoalescer {
  push(text: string): void;
  flush(): void;
  cancelTimer(): void;
}

/**
 * Merge `text` events over a {@link TEXT_MERGE_MS} window, capped at
 * {@link TEXT_MAX_BYTES} per emitted message (DESIGN §5-8). A token-level stream
 * would otherwise turn one run into thousands of IPC frames.
 */
function createTextCoalescer(emit: (event: RunEvent) => void): TextCoalescer {
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
      emit({ type: 'text', text: chunk });
    }
  };

  return {
    push(text: string): void {
      if (typeof text !== 'string' || text.length === 0) return;
      buffered += text;
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

/**
 * Drive one `RunEvent` iterator to completion, forwarding every event as a
 * `mesh.event` notification.
 *
 * The invariant this function exists to guarantee: **exactly one terminal event**.
 * A `done` or `error` from the source is forwarded once and any later terminal is
 * suppressed; a source that throws, or that ends without one, gets a synthesized
 * `error` from the `finally`. The host settles the run row off that event, so a
 * missing terminal would leave a run `running` forever and a second one would
 * settle it twice with different outcomes.
 *
 * The source is a factory rather than an iterable so that opening it — where
 * `adapter.stream()` performs its eager validation and can throw — is inside the
 * same try, and so tests can drive this pump with a scripted iterator.
 */
export async function pumpRunEvents(
  rpc: Rpc,
  runId: string,
  open: () => AsyncIterable<RunEvent> | Promise<AsyncIterable<RunEvent>>,
  entry: RunEntry,
  vendor?: string,
): Promise<void> {
  const emit = (event: RunEvent): void => {
    rpc.notify('mesh.event', { runId, event });
  };
  const text = createTextCoalescer(emit);
  let terminal = false;

  const emitTerminal = (event: RunEvent): void => {
    if (terminal) return;
    terminal = true;
    text.flush();
    emit(event);
  };

  try {
    const events = await open();
    for await (const event of events) {
      if (entry.abort.signal.aborted) break;
      if (event.type === 'text') {
        text.push(event.text);
        continue;
      }
      if (event.type === 'done' || event.type === 'error') {
        emitTerminal(event);
        continue;
      }
      text.flush();
      emit(event);
    }
  } catch (err) {
    const failure = toRpcFailure(err, vendor);
    emitTerminal({ type: 'error', message: failure.message });
  } finally {
    text.cancelTimer();
    if (!terminal) {
      emitTerminal({
        type: 'error',
        message: entry.abort.signal.aborted
          ? 'run cancelled'
          : 'stream ended without a terminal event',
      });
    }
    endRun(runId);
  }
}

/**
 * Register the `mesh.*` run and introspection methods.
 *
 * `requireState` is the handshake gate: every method below is meaningless before
 * `mesh.hello`, and it re-reads the state on every call because `mesh.configure`
 * REBUILDS the fleet rather than mutating it.
 */
export function registerMeshMethods(rpc: Rpc, requireState: () => SidecarState): void {
  // ── introspection ──────────────────────────────────────────────────────────
  rpc.on('mesh.authStatus', async (params: unknown) => {
    const state = requireState();
    const p = isRecord(params) ? params : {};
    const vendor = requireString(p.vendor, 'vendor');
    const adapter = introspectAdapter(state, vendor);
    const account = toAccount(p.account, vendor);
    try {
      return await withIntrospect(vendor, () => adapter.authStatus(account));
    } catch (err) {
      throw toRpcFailure(err, vendor);
    }
  });

  rpc.on('mesh.availableModels', async (params: unknown) => {
    const state = requireState();
    const p = isRecord(params) ? params : {};
    const vendor = requireString(p.vendor, 'vendor');
    const adapter = introspectAdapter(state, vendor);
    const account = toAccount(p.account, vendor);
    try {
      return await withIntrospect(vendor, () => adapter.availableModels(account));
    } catch (err) {
      throw toRpcFailure(err, vendor);
    }
  });

  rpc.on('mesh.quota', async (params: unknown) => {
    const state = requireState();
    const p = isRecord(params) ? params : {};
    const vendor = requireString(p.vendor, 'vendor');
    const adapter = introspectAdapter(state, vendor);
    const account = toAccount(p.account, vendor);
    let snapshot: QuotaSnapshot | null;
    try {
      snapshot = await withIntrospect(vendor, () => adapter.quota(account));
    } catch (err) {
      throw toRpcFailure(err, vendor);
    }
    if (snapshot === null || snapshot === undefined) return null;
    // `capturedAt` is documented as "stamped by the caller — core has no clock".
    // We are that caller; without it the host cannot age a cached snapshot.
    return snapshot.capturedAt === undefined
      ? { ...snapshot, capturedAt: new Date().toISOString() }
      : snapshot;
  });

  // ── worker lifecycle ───────────────────────────────────────────────────────
  rpc.on('mesh.run', async (params: unknown) => {
    const state = requireState();
    const p = isRecord(params) ? params : {};
    const runId = requireString(p.runId, 'runId');
    const vendor = requireString(p.vendor, 'vendor');
    const adapter = runAdapter(state, vendor);
    const task = normalizeTask(state, p.task, vendor);

    try {
      beginRun(runId, adapter.vendor);
    } catch (err) {
      throw invalid(err instanceof Error ? err.message : String(err));
    }
    try {
      // The RunResult is returned verbatim — including `conversationId: ''`.
      return await runCtx.run({ runId, vendor }, () => adapter.run(task));
    } catch (err) {
      throw toRpcFailure(err, vendor);
    } finally {
      endRun(runId);
    }
  });

  rpc.on('mesh.resume', async (params: unknown) => {
    const state = requireState();
    const p = isRecord(params) ? params : {};
    const runId = requireString(p.runId, 'runId');
    const vendor = requireString(p.vendor, 'vendor');
    const adapter = runAdapter(state, vendor);
    const conversationId = requireString(p.conversationId, 'conversationId');
    const message = requireString(p.message, 'message');
    const opts = normalizeResumeOptions(state, p.opts, vendor);

    try {
      beginRun(runId, adapter.vendor);
    } catch (err) {
      throw invalid(err instanceof Error ? err.message : String(err));
    }
    try {
      return await runCtx.run({ runId, vendor }, () =>
        adapter.resume(conversationId, message, opts),
      );
    } catch (err) {
      throw toRpcFailure(err, vendor);
    } finally {
      endRun(runId);
    }
  });

  rpc.on('mesh.stream', (params: unknown) => {
    const state = requireState();
    const p = isRecord(params) ? params : {};
    const runId = requireString(p.runId, 'runId');
    const vendor = requireString(p.vendor, 'vendor');
    const adapter = runAdapter(state, vendor);
    if (!canStream(adapter)) {
      // Read off the instance, never a hardcoded matrix: the host is expected to
      // fall back to `mesh.run` for a vendor whose `capabilities.stream` is false.
      throw new RpcError(RPC_UNSUPPORTED, `${vendor} does not support streaming`, {
        name: 'StreamUnsupported',
        capabilities: adapter.capabilities,
      });
    }
    const task = normalizeTask(state, p.task, vendor);

    let entry: RunEntry;
    try {
      entry = beginRun(runId, adapter.vendor);
    } catch (err) {
      throw invalid(err instanceof Error ? err.message : String(err));
    }

    // Detached on purpose: the response must land before the first event does.
    // `pumpRunEvents` owns every failure inside itself, so this can never reject.
    void pumpRunEvents(
      rpc,
      runId,
      () =>
        runCtx.run({ runId, vendor }, async () => {
          if (!canStream(adapter)) {
            // Defensive: checked above, but `mesh.configure` could have raced.
            throw new RpcError(RPC_UNSUPPORTED, `${vendor} does not implement stream()`);
          }
          return adapter.stream(task);
        }),
      entry,
      vendor,
    );
    return { accepted: true };
  });

  // ── cancellation: convenience only; the host is the authority (§5-9) ───────
  rpc.on('mesh.cancel', (params: unknown) => {
    const p = isRecord(params) ? params : {};
    const runId = requireString(p.runId, 'runId');
    return cancelRun(runId);
  });

  registerStreamSelfTest(rpc);
}

/**
 * A scripted driver for {@link pumpRunEvents}, exposed ONLY under
 * `GILDE_SIDECAR_SELFTEST=1` so the production channel never gains a way to inject
 * events into a run.
 *
 * It exists because the terminal-event invariant has branches no real adapter can
 * be talked into taking: all four of agent-mesh's adapters catch their own
 * failures and always yield a terminal event, so "the iterator throws with no
 * terminal" and "the iterator just ends" are unreachable through them — yet those
 * are exactly the cases where a missing terminal would strand a run as `running`
 * forever. A scripted source reaches them directly.
 */
function registerStreamSelfTest(rpc: Rpc): void {
  if (process.env.GILDE_SIDECAR_SELFTEST !== '1') return;

  interface ScriptParams {
    runId?: unknown;
    /** `RunEvent`s to yield, in order. */
    events?: unknown;
    /** How the iterator finishes: return normally, or throw. */
    end?: unknown;
    /** The message of the thrown error when `end === 'throw'`. */
    throwMessage?: unknown;
  }

  rpc.on('dev.streamScript', (params: ScriptParams | undefined) => {
    const p = params ?? {};
    const runId = requireString(p.runId, 'runId');
    const scripted = Array.isArray(p.events) ? (p.events as RunEvent[]) : [];
    const end = p.end === 'throw' ? 'throw' : 'natural';
    const throwMessage = typeof p.throwMessage === 'string' ? p.throwMessage : 'scripted failure';

    let entry: RunEntry;
    try {
      entry = beginRun(runId, 'selftest');
    } catch (err) {
      throw invalid(err instanceof Error ? err.message : String(err));
    }

    void pumpRunEvents(
      rpc,
      runId,
      () =>
        (async function* scriptedSource(): AsyncGenerator<RunEvent> {
          for (const event of scripted) yield event;
          if (end === 'throw') throw new Error(throwMessage);
        })(),
      entry,
    );
    return { accepted: true };
  });
}
