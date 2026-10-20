/**
 * runs.ts — the sidecar's table of live runs, and the convenience cancel path.
 *
 * Two things live here:
 *
 *   1. {@link runs} — one {@link RunEntry} per in-flight `mesh.run` / `mesh.stream`
 *      / `mesh.resume`, carrying the ids of every child that run has created and
 *      the process groups those children lead.
 *   2. {@link trackSpawns} — a pass-through {@link Rpc} wrapper that fills that
 *      table by OBSERVING the supervision seam. The seam
 *      (`exec-registered.ts` / `duplex.ts`) already tells the host
 *      `host.registerSpawn{runId, spawnId}` → `host.spawned{spawnId, pid, pgid}` →
 *      `host.exited{spawnId}`; wrapping the channel lets us learn the same facts
 *      without touching the seam or giving the adapters a second callback to honour.
 *
 * ─────────────────────────────────────────────────────────────────────────────
 * CANCELLATION AUTHORITY LIVES IN THE HOST, NOT HERE (DESIGN §5-9).
 *
 * No agent-mesh adapter implements `Adapter.stop()`, so cancellation cannot travel
 * through the Adapter contract at all. The real cancel is the host running
 * `supervise::kill::kill_tree` over every `proc_registry` row carrying that
 * `run_id` — that path works even when this process is dead, hung, or SIGSTOPped.
 *
 * {@link cancelRun} is the CONVENIENCE half of that pair: it aborts the streaming
 * loop so the adapter promise settles cleanly and signals the process groups we
 * happen to know about. It is never the authority, it is allowed to know less than
 * the host, and its `{stopped:false}` must never be read as "the run survived".
 * ─────────────────────────────────────────────────────────────────────────────
 */

import type { Vendor } from '@toragonite/agent-mesh';

import type { Rpc } from './rpc';

/** One in-flight run. `spawnIds`/`pgids` grow as the seam reports children. */
export interface RunEntry {
  runId: string;
  vendor: Vendor;
  /** Registry ids of every child this run has created (including settled ones). */
  spawnIds: Set<string>;
  /** Process-group ids of this run's children that have not reported an exit. */
  pgids: Set<number>;
  /** Aborted by {@link cancelRun}; the streaming pump polls it and stops yielding. */
  abort: AbortController;
}

/** Every live run, keyed by the host-chosen `runId`. */
export const runs = new Map<string, RunEntry>();

/** Grace between the convenience SIGTERM and its SIGKILL escalation (DESIGN §5-9). */
export const CANCEL_GRACE_MS = 5_000;

/** spawnId → runId, learned from `host.registerSpawn`. */
const spawnOwner = new Map<string, string>();
/** spawnId → pgid, learned from `host.spawned`; dropped again on `host.exited`. */
const spawnPgid = new Map<string, number>();

function isRecord(x: unknown): x is Record<string, unknown> {
  return typeof x === 'object' && x !== null && !Array.isArray(x);
}

/**
 * Open a run's table entry. Called BEFORE the adapter is invoked so that a
 * `mesh.cancel` racing the very first spawn still finds a row to abort.
 *
 * A `runId` that is already live is rejected: two runs sharing an id would share
 * a kill list, and cancelling one would reach into the other.
 */
export function beginRun(runId: string, vendor: Vendor): RunEntry {
  const existing = runs.get(runId);
  if (existing !== undefined) {
    throw new Error(`runId '${runId}' is already active`);
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

/** Close a run's entry and forget its spawn bookkeeping. Idempotent. */
export function endRun(runId: string): void {
  const entry = runs.get(runId);
  runs.delete(runId);
  if (entry === undefined) return;
  for (const spawnId of entry.spawnIds) {
    spawnOwner.delete(spawnId);
    spawnPgid.delete(spawnId);
  }
}

/** The entry a spawn belongs to, or `undefined` for an introspection probe. */
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
    const runId = typeof params.runId === 'string' ? params.runId : undefined;
    if (runId === undefined) return;
    spawnOwner.set(spawnId, runId);
    runs.get(runId)?.spawnIds.add(spawnId);
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
    start(): void {
      rpc.start();
    },
  };
}

/** How a cancel was actually delivered. `none` = nothing of that run was known here. */
export type CancelVia = 'killpg' | 'abort' | 'none';

export interface CancelOutcome {
  stopped: boolean;
  via: CancelVia;
}

/** Signal one process group, reporting whether the signal was actually delivered. */
function signalGroup(pgid: number, signal: NodeJS.Signals): boolean {
  try {
    process.kill(-pgid, signal);
    return true;
  } catch {
    // ESRCH (already gone) or EPERM (not ours any more) — both mean "not our problem".
    return false;
  }
}

/**
 * The convenience half of cancellation (see the module header): abort the
 * streaming loop, then SIGTERM every process group this run is known to lead and
 * escalate the survivors to SIGKILL after {@link CANCEL_GRACE_MS}.
 *
 * Returns as soon as the TERM wave is out — the escalation is scheduled, not
 * awaited, so a hung child can never make `mesh.cancel` itself hang.
 *
 * The AUTHORITATIVE cancel is the host's `kill_tree` over the registry rows for
 * this `run_id`; that path is what the product guarantees, and it keeps working
 * when this process does not.
 */
export function cancelRun(runId: string, graceMs: number = CANCEL_GRACE_MS): CancelOutcome {
  const entry = runs.get(runId);
  if (entry === undefined) return { stopped: false, via: 'none' };

  entry.abort.abort();

  const targets = [...entry.pgids];
  let delivered = 0;
  for (const pgid of targets) {
    if (signalGroup(pgid, 'SIGTERM')) delivered += 1;
  }

  if (delivered > 0) {
    const escalation = setTimeout(() => {
      // Groups that already went away simply fail with ESRCH. Identity fencing
      // against a recycled pgid is the HOST's job (DESIGN §8-4) — this path is
      // best-effort convenience and deliberately does not pretend otherwise.
      for (const pgid of targets) signalGroup(pgid, 'SIGKILL');
    }, graceMs);
    escalation.unref?.();
  }

  return { stopped: true, via: delivered > 0 ? 'killpg' : 'abort' };
}

/**
 * Every process group the seam has reported as spawned and not yet exited.
 *
 * Wider than the union of {@link RunEntry.pgids} on purpose: an introspection
 * probe (`runId: 'introspect'`) has no run entry, but its `claude` child is just
 * as real and has been measured running for over 20 seconds.
 */
export function livePgids(): number[] {
  return [...new Set(spawnPgid.values())];
}

/**
 * Signal every live child group. Used by the shutdown path (INT-C).
 *
 * When this process is going away because the HOST is gone, nothing else is left
 * to supervise these children: the registry rows exist, but the reaper that acts
 * on them lives in the process that just died. So the shutdown escalates here
 * itself — `SIGTERM`, then `SIGKILL` from the caller a moment later — rather than
 * exiting and leaving a `claude -p` behind. That population is exactly what this
 * app exists to prevent.
 *
 * Returns how many groups actually took the signal.
 */
export function killAllChildren(signal: NodeJS.Signals): number {
  let delivered = 0;
  for (const entry of runs.values()) entry.abort.abort();
  for (const pgid of livePgids()) {
    if (signalGroup(pgid, signal)) delivered += 1;
  }
  return delivered;
}
