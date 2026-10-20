/**
 * rpc.ts — bidirectional NDJSON JSON-RPC 2.0 codec for the sidecar channel.
 *
 * One JSON object per line, UTF-8, `\n` terminated (DESIGN §5-1). The channel is
 * bidirectional by necessity, not by taste: the exec seam must *await* the host
 * before it spawns (§5-4/§5-5), so the sidecar issues requests of its own.
 *
 * Hardening rules implemented here:
 *   1. Unparseable / non-`"jsonrpc":"2.0"` lines are warned about and SKIPPED.
 *      A stray line never crashes the process and never desynchronizes framing.
 *   2. A line exceeding {@link MAX_LINE_BYTES} is a protocol violation: the
 *      partial buffer is dropped and every following byte is discarded until the
 *      next newline, so a runaway peer cannot make us allocate without bound.
 *   3. Writes are drain-aware. The outbound queue is capped at
 *      {@link MAX_OUTBOUND_QUEUE}; when it overflows, notifications whose method
 *      is contractually lossy (`mesh.event`) are dropped oldest-first.
 *      **Responses and requests are never dropped.**
 *   4. Every wait is bounded and every loop has a terminal exit (INT-C). A host
 *      that died leaves a pipe that accepts no byte and emits no `drain`; the
 *      pump declares the writer dead rather than retrying, and `flush()` carries
 *      its own deadline. Nothing here may spin, because the shutdown that has to
 *      kill this process runs *after* it.
 *
 * This module is the ONLY code in the bundle that reaches the real stdout
 * writer — `index.ts` captures it before any import and hands it in here.
 */

import type { Readable } from 'node:stream';

/** Maximum accepted inbound line length. Beyond this the peer is violating the protocol. */
export const MAX_LINE_BYTES = 8 * 1024 * 1024;

/** Outbound backlog cap before lossy notifications start being discarded. */
export const MAX_OUTBOUND_QUEUE = 4096;

/** Default wall-clock deadline for an outbound request. */
export const DEFAULT_CALL_TIMEOUT_MS = 30_000;

/**
 * Default cap on {@link RpcChannel.flush} (INT-C).
 *
 * The flush is the only place a shutdown waits for the peer, so it is the only
 * place that needs a clock. The backpressure wait deliberately has none: a host
 * that has merely stopped reading for a moment is exercising the contract in
 * §5-1, not dying, and a timeout there would throw away the very responses that
 * contract promises to keep.
 */
export const FLUSH_TIMEOUT_MS = 500;

/**
 * Methods whose delivery is lossy by contract (DESIGN §5-1, §5-8). Only these are
 * ever discarded under backpressure; the terminal state of a run always also
 * reaches the host through the `mesh.run`/`mesh.stream` response and the disk
 * artifacts, so a dropped `mesh.event` costs a UI refresh, never a result.
 */
export const LOSSY_METHODS: ReadonlySet<string> = new Set(['mesh.event', 'host.log', 'cco.team.snapshot']);

/**
 * Drop order under backpressure, lowest rank first. `mesh.event` goes first
 * (DESIGN §5-1 names it explicitly), then `host.log`. Everything else — every
 * response, and every request such as `host.registerSpawn` — is rank 0 and is
 * never dropped.
 */
const DROP_RANK: Readonly<Record<string, number>> = {
  'mesh.event': 1,
  // Team contract v2 §8.2: a team view is a whole snapshot, latest wins, so it takes
  // `mesh.event`'s rank — a dropped one only delays the UI, and the next
  // `cco.team.list` (which carries each team's latest rev) re-reads what was lost.
  'cco.team.snapshot': 1,
  'host.log': 2,
};

/** JSON-RPC error codes used by the sidecar. */
export const RPC_PARSE_ERROR = -32700;
export const RPC_INVALID_REQUEST = -32600;
export const RPC_METHOD_NOT_FOUND = -32601;
export const RPC_INVALID_PARAMS = -32602;
export const RPC_INTERNAL_ERROR = -32603;
/** Application range: the peer went away / the call deadline elapsed. */
export const RPC_CHANNEL_CLOSED = -32000;
export const RPC_TIMEOUT = -32003;
/** A capability the adapter (or this build) does not have — e.g. `stream()`. */
export const RPC_UNSUPPORTED = -32004;

export type LogLevel = 'debug' | 'info' | 'warn' | 'error';
export type RpcLogger = (level: LogLevel, msg: string, fields?: Record<string, unknown>) => void;

/** A typed JSON-RPC failure. Thrown by handlers, or rejected out of {@link Rpc.call}. */
export class RpcError extends Error {
  readonly code: number;
  readonly data: unknown;

  constructor(code: number, message: string, data?: unknown) {
    super(message);
    this.name = 'RpcError';
    this.code = code;
    this.data = data;
  }
}

export interface Rpc {
  /** Issue a request to the host and await its response. */
  call<T>(method: string, params: unknown, timeoutMs?: number): Promise<T>;
  /** Fire-and-forget notification. Subject to lossy dropping for {@link LOSSY_METHODS}. */
  notify(method: string, params: unknown): void;
  /** Register a handler for an inbound method (request or notification). */
  on(method: string, h: (params: any) => Promise<any> | any): void;
  /** Begin consuming the input stream. Idempotent. */
  start(): void;
}

/** Diagnostics + lifecycle beyond the pinned {@link Rpc} contract. */
export interface RpcChannel extends Rpc {
  /**
   * Stop serving the peer without tearing the channel down: inbound requests are
   * answered with {@link RPC_CHANNEL_CLOSED} instead of reaching a handler, and
   * the input stream is paused. The outbound queue can still be flushed, so a
   * shutdown can drain what it already owes the host (INT-C).
   */
  stopAccepting(reason: string): void;
  /** Reject every in-flight call and stop writing. */
  close(reason: string): void;
  /**
   * Hand the outbound queue to the writer, giving up after `timeoutMs`.
   *
   * **Always terminates.** A dead peer never drains and never acknowledges, so
   * an unbounded flush is an unbounded shutdown; the caller's `process.exit` is
   * more important than the last `host.log` frame.
   */
  flush(timeoutMs?: number): Promise<void>;
  readonly stats: RpcStats;
}

export interface RpcStats {
  /** Inbound lines that failed to parse or were not JSON-RPC 2.0. */
  skippedLines: number;
  /** Inbound lines discarded for exceeding {@link MAX_LINE_BYTES}. */
  oversizeLines: number;
  /** Lossy notifications discarded under backpressure. */
  droppedNotifications: number;
  /** Requests still awaiting a response. */
  pending: number;
  /** True once a write failed or a drain never came: the peer cannot be reached. */
  writerDead: boolean;
}

export interface RpcOptions {
  /** Where protocol-level warnings go. Defaults to a stderr writer. */
  log?: RpcLogger;
  maxLineBytes?: number;
  maxOutboundQueue?: number;
  defaultTimeoutMs?: number;
  /**
   * Awaited when the writer signals backpressure. Resolves `true` on a real
   * `drain` and `false` when the writer is gone or the wait timed out — the
   * pump then stops rather than looping on a stream that will never accept
   * another byte. Defaults to a bounded wait on `process.stdout`.
   */
  waitForDrain?: () => Promise<boolean>;
  /**
   * Called at most once when the input side is finished — `end` (the host closed
   * our stdin), `close`, or an error on the stream. All three mean the same
   * thing: nothing more will ever be asked of us.
   */
  onPeerClosed?: (why: string) => void;
}

interface Pending {
  resolve: (value: any) => void;
  reject: (err: Error) => void;
  timer: NodeJS.Timeout | undefined;
  method: string;
}

interface Outbound {
  line: string;
  /** 0 = never dropped; higher ranks are discarded first under backpressure. */
  dropRank: number;
}

const defaultLogger: RpcLogger = (level, msg, fields) => {
  const suffix = fields === undefined ? '' : ` ${safeStringify(fields)}`;
  process.stderr.write(`[rpc:${level}] ${msg}${suffix}\n`);
};

function safeStringify(value: unknown): string {
  try {
    return JSON.stringify(value) ?? String(value);
  } catch {
    return '[unserializable]';
  }
}

function isRecord(x: unknown): x is Record<string, unknown> {
  return typeof x === 'object' && x !== null && !Array.isArray(x);
}

/**
 * Build a bidirectional NDJSON JSON-RPC channel.
 *
 * @param stdin        the byte stream carrying inbound lines (the process stdin).
 * @param stdoutWrite  the REAL stdout writer, captured before any import so no
 *                     dependency's `console.log` can interleave with the frames.
 */
export function createRpc(
  stdin: Readable,
  stdoutWrite: (s: string) => boolean,
  opts: RpcOptions = {},
): RpcChannel {
  const log = opts.log ?? defaultLogger;
  const maxLineBytes = opts.maxLineBytes ?? MAX_LINE_BYTES;
  const maxOutboundQueue = opts.maxOutboundQueue ?? MAX_OUTBOUND_QUEUE;
  const defaultTimeoutMs = opts.defaultTimeoutMs ?? DEFAULT_CALL_TIMEOUT_MS;
  /**
   * Wait out backpressure, and settle on the peer's death as well as on `drain`.
   *
   * Before INT-C this waited on `drain` ALONE. When the reader is gone that event
   * never comes — the pipe emits `error` (EPIPE) or `close` instead — so the pump
   * parked forever while the shutdown behind it queued more and more work.
   *
   * There is deliberately no timeout: a host that has merely stopped reading is
   * the documented backpressure case (§5-1), and calling it dead would discard
   * responses the contract says are never dropped. Death is an event, not a
   * delay. Returning `false` is how the pump learns to stop rather than retry
   * into a pipe that will never take another byte.
   */
  const waitForDrain =
    opts.waitForDrain ??
    (() =>
      new Promise<boolean>((resolve) => {
        const out = process.stdout;
        let settled = false;
        const finish = (drained: boolean): void => {
          if (settled) return;
          settled = true;
          out.removeListener('drain', onDrain);
          out.removeListener('error', onGone);
          out.removeListener('close', onGone);
          resolve(drained);
        };
        const onDrain = (): void => finish(true);
        const onGone = (): void => finish(false);
        out.once('drain', onDrain);
        out.once('error', onGone);
        out.once('close', onGone);
      }));

  const handlers = new Map<string, (params: any) => Promise<any> | any>();
  const pending = new Map<number, Pending>();
  const queue: Outbound[] = [];
  const stats: RpcStats = {
    skippedLines: 0,
    oversizeLines: 0,
    droppedNotifications: 0,
    pending: 0,
    writerDead: false,
  };

  let nextId = 0;
  let started = false;
  let closed = false;
  /** False once {@link RpcChannel.stopAccepting} ran: inbound work is refused. */
  let accepting = true;
  let pumping = false;
  /** Resolves when the current pump run finishes. Never rejects. */
  let pumpIdle: Promise<void> = Promise.resolve();
  let peerClosed = false;

  // ── inbound framing ────────────────────────────────────────────────────────
  let parts: Buffer[] = [];
  let partsLen = 0;
  /** True while discarding the remainder of an oversize line. */
  let discarding = false;

  function resetLineBuffer(): void {
    parts = [];
    partsLen = 0;
  }

  function appendSegment(seg: Buffer): void {
    if (discarding) return;
    if (partsLen + seg.length > maxLineBytes) {
      // Protocol violation: drop what we have and skip to the next newline.
      stats.oversizeLines += 1;
      discarding = true;
      resetLineBuffer();
      log('error', 'inbound line exceeds MAX_LINE_BYTES; discarding until next newline', {
        maxLineBytes,
      });
      return;
    }
    parts.push(seg);
    partsLen += seg.length;
  }

  function completeLine(): void {
    if (discarding) {
      discarding = false;
      resetLineBuffer();
      return;
    }
    if (partsLen === 0) {
      resetLineBuffer();
      return;
    }
    const line = (parts.length === 1 ? parts[0]! : Buffer.concat(parts, partsLen)).toString('utf8');
    resetLineBuffer();
    handleLine(line);
  }

  function onData(chunk: Buffer): void {
    let start = 0;
    for (;;) {
      const nl = chunk.indexOf(0x0a, start);
      if (nl === -1) {
        if (start < chunk.length) appendSegment(chunk.subarray(start));
        return;
      }
      if (nl > start) appendSegment(chunk.subarray(start, nl));
      completeLine();
      start = nl + 1;
    }
  }

  function handleLine(raw: string): void {
    const text = raw.trim();
    if (text.length === 0) return;

    let msg: unknown;
    try {
      msg = JSON.parse(text);
    } catch {
      stats.skippedLines += 1;
      log('warn', 'skipping unparseable inbound line', { bytes: text.length });
      return;
    }
    if (!isRecord(msg) || msg.jsonrpc !== '2.0') {
      stats.skippedLines += 1;
      log('warn', 'skipping line without jsonrpc:"2.0"', { bytes: text.length });
      return;
    }

    const hasMethod = typeof msg.method === 'string';
    const hasId = msg.id !== undefined && msg.id !== null;

    if (hasMethod) {
      void dispatchInbound(msg as Record<string, unknown>, hasId);
      return;
    }
    if (hasId) {
      settleResponse(msg as Record<string, unknown>);
      return;
    }
    stats.skippedLines += 1;
    log('warn', 'skipping line that is neither request, notification nor response');
  }

  async function dispatchInbound(msg: Record<string, unknown>, isRequest: boolean): Promise<void> {
    const method = msg.method as string;
    const id = msg.id as number | string | undefined;

    if (!accepting) {
      // Shutting down. Answering is better than a silent drop — the host learns
      // the call will not happen instead of waiting out its own deadline — but
      // nothing may reach a handler that could start new work (INT-C).
      if (isRequest) {
        writeMessage(
          { jsonrpc: '2.0', id, error: { code: RPC_CHANNEL_CLOSED, message: `sidecar is shutting down; ${method} refused` } },
          0,
        );
      }
      return;
    }

    const handler = handlers.get(method);

    if (handler === undefined) {
      if (isRequest) {
        writeMessage(
          { jsonrpc: '2.0', id, error: { code: RPC_METHOD_NOT_FOUND, message: `unknown method: ${method}` } },
          0,
        );
      } else {
        log('warn', 'notification for unknown method', { method });
      }
      return;
    }

    try {
      const result = await handler(msg.params);
      if (isRequest) {
        writeMessage({ jsonrpc: '2.0', id, result: result === undefined ? null : result }, 0);
      }
    } catch (err) {
      const rpcErr = toRpcError(err);
      if (isRequest) {
        writeMessage(
          {
            jsonrpc: '2.0',
            id,
            error:
              rpcErr.data === undefined
                ? { code: rpcErr.code, message: rpcErr.message }
                : { code: rpcErr.code, message: rpcErr.message, data: rpcErr.data },
          },
          0,
        );
      } else {
        log('error', 'notification handler threw', { method, error: rpcErr.message });
      }
    }
  }

  function settleResponse(msg: Record<string, unknown>): void {
    const id = msg.id;
    if (typeof id !== 'number') {
      stats.skippedLines += 1;
      log('warn', 'response with a non-numeric id we never issued', { id: String(id) });
      return;
    }
    const entry = pending.get(id);
    if (entry === undefined) {
      log('warn', 'response for an unknown or already-settled id', { id });
      return;
    }
    pending.delete(id);
    stats.pending = pending.size;
    if (entry.timer !== undefined) clearTimeout(entry.timer);

    if (isRecord(msg.error)) {
      const e = msg.error;
      const code = typeof e.code === 'number' ? e.code : RPC_INTERNAL_ERROR;
      const message = typeof e.message === 'string' ? e.message : 'host error';
      entry.reject(new RpcError(code, message, e.data));
      return;
    }
    entry.resolve(msg.result);
  }

  function toRpcError(err: unknown): RpcError {
    if (err instanceof RpcError) return err;
    if (err instanceof Error) {
      return new RpcError(RPC_INTERNAL_ERROR, err.message, { name: err.name });
    }
    return new RpcError(RPC_INTERNAL_ERROR, String(err));
  }

  // ── outbound ───────────────────────────────────────────────────────────────
  function writeMessage(msg: unknown, dropRank: number): void {
    // `writerDead` matters as much as `closed`: serializing a frame nobody can
    // read is pure work, and the log line it would produce feeds the next one.
    if (closed || stats.writerDead) return;
    let line: string;
    try {
      line = JSON.stringify(msg) + '\n';
    } catch (err) {
      log('error', 'refusing to write an unserializable message', {
        error: err instanceof Error ? err.message : String(err),
      });
      return;
    }
    enqueue({ line, dropRank });
  }

  /** Discard the oldest queued entries of exactly `rank`. Returns how many went. */
  function dropRankFromQueue(rank: number, need: number): number {
    let dropped = 0;
    for (let i = 0; i < queue.length && dropped < need; ) {
      if (queue[i]!.dropRank === rank) {
        queue.splice(i, 1);
        dropped += 1;
      } else {
        i += 1;
      }
    }
    return dropped;
  }

  function enqueue(item: Outbound): void {
    if (queue.length >= maxOutboundQueue) {
      // Drop the OLDEST `mesh.event` entries first, then `host.log`. Newer events
      // describe more current state; responses and requests are structurally exempt.
      const need = queue.length - maxOutboundQueue + 1;
      let dropped = dropRankFromQueue(1, need);
      if (dropped < need) dropped += dropRankFromQueue(2, need - dropped);

      if (dropped > 0) {
        stats.droppedNotifications += dropped;
        log('warn', 'outbound queue full; dropped lossy notifications', {
          dropped,
          cap: maxOutboundQueue,
        });
      }
      if (dropped < need) {
        if (item.dropRank > 0) {
          // Nothing droppable but this very item: drop it rather than grow.
          stats.droppedNotifications += 1;
          return;
        }
        log('warn', 'outbound queue over cap with no droppable entries; growing', {
          depth: queue.length,
        });
      }
    }
    queue.push(item);
    pump();
  }

  /**
   * Drain the outbound queue.
   *
   * Every exit from this loop is terminal for a reason the caller can see: the
   * queue emptied, the channel closed, or the writer died. It NEVER returns with
   * work still queued and the writer still believed good — that combination is
   * what let `flush()` spin at 100% CPU before INT-C.
   */
  async function pumpLoop(): Promise<void> {
    try {
      while (queue.length > 0 && !closed && !stats.writerDead) {
        const item = queue.shift()!;
        let ok: boolean;
        try {
          ok = stdoutWrite(item.line);
        } catch (err) {
          stats.writerDead = true;
          log('error', 'stdout write failed; the host is not reachable', {
            error: err instanceof Error ? err.message : String(err),
          });
          break;
        }
        if (!ok) {
          const drained = await waitForDrain();
          if (drained === false) {
            stats.writerDead = true;
            log('warn', 'the host stopped reading and its pipe is gone', { queued: queue.length });
            break;
          }
        }
      }
      if (stats.writerDead && queue.length > 0) {
        // Hold nothing: a queue we can never write is only memory and a reason
        // to keep looping over it.
        stats.droppedNotifications += queue.length;
        queue.length = 0;
      }
    } finally {
      // SYNCHRONOUSLY, inside the async body — not in a `.finally()` on the
      // promise, which would land a microtask later. `enqueue` is called in tight
      // synchronous bursts (`mesh.stream` fan-out, 20k events at a time); a pump
      // that is still flagged busy after it stopped swallows every one of them
      // and the channel silently stalls with a full queue.
      pumping = false;
    }
  }

  function pump(): void {
    if (pumping) return;
    pumping = true;
    pumpIdle = pumpLoop().catch((err: unknown) => {
      pumping = false;
      stats.writerDead = true;
      log('error', 'outbound pump failed', {
        error: err instanceof Error ? err.message : String(err),
      });
    });
  }

  // ── public surface ─────────────────────────────────────────────────────────
  const channel: RpcChannel = {
    stats,

    call<T>(method: string, params: unknown, timeoutMs?: number): Promise<T> {
      if (closed || stats.writerDead) {
        // Fail now, not in 30 s. This is the seam's path: `host.registerSpawn`
        // awaits the host BEFORE spawning (§5-5), so a fast rejection here is a
        // child that never gets created rather than one created with nobody left
        // to supervise it.
        return Promise.reject(new RpcError(RPC_CHANNEL_CLOSED, `channel closed; ${method} not sent`));
      }
      const id = ++nextId;
      const deadline = timeoutMs ?? defaultTimeoutMs;
      return new Promise<T>((resolve, reject) => {
        let timer: NodeJS.Timeout | undefined;
        if (deadline > 0 && Number.isFinite(deadline)) {
          timer = setTimeout(() => {
            if (pending.delete(id)) {
              stats.pending = pending.size;
              reject(new RpcError(RPC_TIMEOUT, `host did not answer ${method} within ${deadline}ms`));
            }
          }, deadline);
          timer.unref?.();
        }
        pending.set(id, { resolve, reject, timer, method });
        stats.pending = pending.size;
        writeMessage({ jsonrpc: '2.0', id, method, params }, 0);
      });
    },

    notify(method: string, params: unknown): void {
      writeMessage({ jsonrpc: '2.0', method, params }, DROP_RANK[method] ?? 0);
    },

    on(method: string, h: (params: any) => Promise<any> | any): void {
      handlers.set(method, h);
    },

    start(): void {
      if (started) return;
      started = true;
      stdin.on('data', (chunk: Buffer | string) => {
        onData(typeof chunk === 'string' ? Buffer.from(chunk, 'utf8') : chunk);
      });
      // All three input-side endings mean the same thing and must be handled the
      // same way. `end` is the clean host close; `close` is what a pipe whose
      // writer vanished gives us when `end` never fires; an `error` (EPIPE,
      // ECONNRESET) is the same fact arriving as a failure. Before INT-C only
      // `end` was wired, so a host that died hard left this process reading a
      // stream that would never speak again.
      stdin.on('error', (err: Error) => {
        log('warn', 'stdin error; treating the host as gone', { error: err.message });
        notifyPeerClosed(`stdin error: ${err.message}`);
      });
      stdin.on('end', () => {
        log('info', 'host closed the channel (stdin end)');
        notifyPeerClosed('stdin end');
      });
      stdin.on('close', () => {
        notifyPeerClosed('stdin close');
      });
      stdin.resume();
    },

    stopAccepting(reason: string): void {
      if (!accepting) return;
      accepting = false;
      log('info', 'no longer accepting inbound work', { reason });
      try {
        stdin.pause();
      } catch {
        /* already torn down */
      }
    },

    close(reason: string): void {
      if (closed) return;
      closed = true;
      accepting = false;
      for (const [id, entry] of pending) {
        if (entry.timer !== undefined) clearTimeout(entry.timer);
        entry.reject(new RpcError(RPC_CHANNEL_CLOSED, `${entry.method} aborted: ${reason}`));
        pending.delete(id);
      }
      pending.clear();
      queue.length = 0;
      stats.pending = 0;
    },

    async flush(timeoutMs: number = FLUSH_TIMEOUT_MS): Promise<void> {
      // Two independent bounds, because a shutdown must never be hostage to a
      // peer: `pumpLoop` itself terminates on a dead writer, and this deadline
      // covers everything else (a slow but living reader, a pathological queue).
      const deadline = Date.now() + Math.max(0, timeoutMs);
      while (queue.length > 0 && !closed && !stats.writerDead) {
        const left = deadline - Date.now();
        if (left <= 0) break;
        pump();
        await Promise.race([pumpIdle, sleep(left)]);
      }
    },
  };

  /** Fire {@link RpcOptions.onPeerClosed} exactly once, whichever ending won. */
  function notifyPeerClosed(why: string): void {
    if (peerClosed) return;
    peerClosed = true;
    opts.onPeerClosed?.(why);
  }

  return channel;
}

/** A timer that can never by itself hold the process open. */
function sleep(ms: number): Promise<void> {
  return new Promise<void>((resolve) => {
    const t = setTimeout(resolve, ms);
    t.unref?.();
  });
}
