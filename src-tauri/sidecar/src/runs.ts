/**
 * runs.ts — the sidecar's table of live turns, and the convenience cancel path.
 *
 * Two things live here:
 *
 *   1. {@link runs} — one {@link RunEntry} per in-flight `turn.start`, carrying the
 *      ids of every child that turn has created and the process groups those
 *      children lead.
 *   2. {@link trackSpawns} — a pass-through {@link Rpc} wrapper that fills that
 *      table by OBSERVING the supervision seam. The seam
 *      (`exec-registered.ts` / `duplex.ts`) already tells the host
 *      `host.registerSpawn{turnId, spawnId}` → `host.spawned{spawnId, pid, pgid}` →
 *      `host.exited{spawnId}`; wrapping the channel lets us learn the same facts
 *      without touching the seam or giving the adapters a second callback to honour.
 *
 * ─────────────────────────────────────────────────────────────────────────────
 * CANCELLATION AUTHORITY LIVES IN THE HOST, NOT HERE (contract §4.5 `turn.cancel`).
 *
 * No agent-mesh adapter implements `Adapter.stop()`, so cancellation cannot travel
 * through the Adapter contract at all. `turn.cancel` is a hard stop of the turn's
 * registered process groups with SIGTERM; the core's fenced kill over the registry
 * rows is authoritative and works even when this process is dead or hung.
 *
 * SIGNALS: every group here leads a vendor CLI, and a vendor CLI is never sent
 * SIGKILL (contract §9.8, C5) — not by cancel, not by shutdown. A group that
 * survives its SIGTERM gets one more after {@link CANCEL_GRACE_MS} and is then a
 * survivor for the host to report.
 * ─────────────────────────────────────────────────────────────────────────────
 */

import type { Rpc } from './rpc';

/** One in-flight turn. `spawnIds`/`pgids` grow as the seam reports children. */
export interface RunEntry {
  /** The host's `turnId`. */
  runId: string;
  vendor: string;
  /** Registry ids of every child this turn has created (including settled ones). */
  spawnIds: Set<string>;
  /** Process-group ids of this turn's children that have not reported an exit. */
  pgids: Set<number>;
  /** Aborted by {@link cancelRun}; the streaming pump polls it and stops yielding. */
  abort: AbortController;
}

/** Every live turn, keyed by the host-chosen `turnId`. */
export const runs = new Map<string, RunEntry>();

/** Grace between the cancel SIGTERM and the repeated one. */
export const CANCEL_GRACE_MS = 5_000;

/** spawnId → turnId, learned from `host.registerSpawn`. */
const spawnOwner = new Map<string, string>();
/** spawnId → pgid, learned from `host.spawned`; dropped again on `host.exited`. */
const spawnPgid = new Map<string, number>();

function isRecord(x: unknown): x is Record<string, unknown> {
  return typeof x === 'object' && x !== null && !Array.isArray(x);
}

/**
 * Open a turn's table entry. Called BEFORE the adapter is invoked so that a
 * `turn.cancel` racing the very first spawn still finds a row to abort.
 *
 * A `turnId` that is already live is rejected: two turns sharing an id would share
 * a kill list, and cancelling one would reach into the other.
 */
export function beginRun(runId: string, vendor: string): RunEntry {
  const existing = runs.get(runId);
  if (existing !== undefined) {
    throw new Error(`turnId '${runId}' is already active`);
  }
  const entry: RunEntry = {
    runId,
    vendor,
    spawnIds: new Set<string>(),
    pgids: new Set<number>(),
    abort: new AbortController(),
  };
  runs.set(runId, entry);
  return entry;
}

/** Close a turn's entry and forget its spawn bookkeeping. Idempotent. */
export function endRun(runId: string): void {
  const entry = runs.get(runId);
  runs.delete(runId);
  if (entry === undefined) return;
  for (const spawnId of entry.spawnIds) {
    spawnOwner.delete(spawnId);
    spawnPgid.delete(spawnId);
  }
}

/** The entry a spawn belongs to, or `undefined` for a probe. */
function entryForSpawn(spawnId: string): RunEntry | undefined {
  const runId = spawnOwner.get(spawnId);
  return runId === undefined ? undefined : runs.get(runId);
}

/**
 * Record what one outbound supervision message tells us. Deliberately total: an
 * unexpected shape is ignored rather than thrown, because this observer sits on
 * the path that spawns children and must never be the reason one fails to start.
 */
function observe(method: string, params: unknown): void {
  if (!isRecord(params)) return;
  const spawnId = typeof params.spawnId === 'string' ? params.spawnId : undefined;
  if (spawnId === undefined) return;

  if (method === 'host.registerSpawn') {
    const turnId = typeof params.turnId === 'string' ? params.turnId : undefined;
    if (turnId === undefined) return;
    spawnOwner.set(spawnId, turnId);
    runs.get(turnId)?.spawnIds.add(spawnId);
    return;
  }

  if (method === 'host.spawned') {
    const pgid = typeof params.pgid === 'number' ? params.pgid : undefined;
    if (pgid === undefined || !Number.isInteger(pgid) || pgid <= 1) return;
    spawnPgid.set(spawnId, pgid);
    entryForSpawn(spawnId)?.pgids.add(pgid);
    return;
  }

  if (method === 'host.exited') {
    const pgid = spawnPgid.get(spawnId);
    spawnPgid.delete(spawnId);
    if (pgid !== undefined) entryForSpawn(spawnId)?.pgids.delete(pgid);
  }
}

/**
 * Wrap an {@link Rpc} so every supervision request it carries also updates
 * {@link runs}. Pure observation: the wrapper forwards everything unchanged and
 * adds no failure mode of its own.
 */
export function trackSpawns(rpc: Rpc): Rpc {
  return {
    call<T>(method: string, params: unknown, timeoutMs?: number): Promise<T> {
      observe(method, params);
      return rpc.call<T>(method, params, timeoutMs);
    },
    notify(method: string, params: unknown): void {
      rpc.notify(method, params);
    },
    on(method: string, h: (params: any) => Promise<any> | any): void {
      rpc.on(method, h);
    },
    off(method: string): void {
      rpc.off(method);
    },
    start(): void {
      rpc.start();
    },
  };
}

export interface CancelOutcome {
  /** A SIGTERM reached at least one of the turn's process groups. */
  signalled: boolean;
  /** The turn was known here (its pump was aborted). */
  known: boolean;
}

/** Signal one process group, reporting whether the signal was actually delivered. */
function signalGroup(pgid: number, signal: 'SIGTERM'): boolean {
  try {
    process.kill(-pgid, signal);
    return true;
  } catch {
    // ESRCH (already gone) or EPERM (not ours any more) — both mean "not our problem".
    return false;
  }
}

/**
 * `turn.cancel`: abort the streaming pump, SIGTERM every process group this turn
 * is known to lead, and repeat the SIGTERM for the survivors after
 * {@link CANCEL_GRACE_MS}. Never SIGKILL (C5).
 *
 * Returns as soon as the first wave is out — the repeat is scheduled, not
 * awaited, so a hung child can never make `turn.cancel` itself hang.
 */
export function cancelRun(runId: string, graceMs: number = CANCEL_GRACE_MS): CancelOutcome {
  const entry = runs.get(runId);
  if (entry === undefined) return { signalled: false, known: false };

  entry.abort.abort();

  const targets = [...entry.pgids];
  let delivered = 0;
  for (const pgid of targets) {
    if (signalGroup(pgid, 'SIGTERM')) delivered += 1;
  }

  if (delivered > 0) {
    const repeat = setTimeout(() => {
      // Groups that already went away simply fail with ESRCH. Identity fencing
      // against a recycled pgid is the HOST's job — this path is best-effort and
      // deliberately does not pretend otherwise.
      for (const pgid of targets) signalGroup(pgid, 'SIGTERM');
    }, graceMs);
    repeat.unref?.();
  }

  return { signalled: delivered > 0, known: true };
}

/**
 * Every process group the seam has reported as spawned and not yet exited —
 * turns and probes alike (a cold `claude` quota probe has been measured running
 * for over 20 seconds).
 */
export function livePgids(): number[] {
  return [...new Set(spawnPgid.values())];
}

/**
 * SIGTERM every live child group. Used by the shutdown path (INT-C). Never
 * SIGKILL: the children are vendor CLIs (C5); whatever outlives this is in the
 * host's registry for its sweep to report.
 *
 * Returns how many groups actually took the signal.
 */
export function killAllChildren(): number {
  let delivered = 0;
  for (const entry of runs.values()) entry.abort.abort();
  for (const pgid of livePgids()) {
    if (signalGroup(pgid, 'SIGTERM')) delivered += 1;
  }
  return delivered;
}
