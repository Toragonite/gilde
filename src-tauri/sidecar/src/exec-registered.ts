/**
 * exec-registered.ts — the supervision seam.
 *
 * agent-mesh's four adapters all accept an `exec` (run-to-completion) and, except
 * codex, a `lineStream` (streaming) constructor seam. We implement those public
 * contracts FROM SCRATCH — `nodeExec` / `nodeLineStream` are never copied
 * (DESIGN §0 rule 1, §5-6) — and add exactly one thing the vendor defaults cannot
 * do: the host learns about the child BEFORE it exists.
 *
 * Every spawn here therefore:
 *   1. awaits `host.registerSpawn` (a durable `state='planned'` row, §8-3),
 *   2. stamps `GILDE_SPAWN_ID` / `GILDE_APP_INSTANCE` on the child so the identity
 *      fences (§8-4) can recognize it later,
 *   3. reports `host.spawned` immediately after spawn and `host.exited` when it
 *      settles — both as REQUESTS, so a lost report is detectable.
 *
 * `ExecRequest.env` semantics are honoured exactly: when present it IS the child
 * env and is NOT merged over `process.env` (§5-6). Only the two markers are added,
 * so an adapter's account isolation (`CLAUDE_CONFIG_DIR`, `CODEX_HOME`, `AGY_HOME`)
 * survives intact.
 *
 * Env VALUES never leave this process: only `envKeys` is reported.
 *
 * This file and `duplex.ts` are the only modules of ours allowed to call
 * `spawn(` — `scripts/check-seam.mjs` fails the build otherwise.
 */

import { spawn } from 'node:child_process';
import type { ChildProcess } from 'node:child_process';
import { AsyncLocalStorage } from 'node:async_hooks';
import { randomUUID } from 'node:crypto';
import * as readline from 'node:readline';

import type { ExecFn, ExecRequest, ExecResult, LineStreamFn } from '@toragonite/agent-mesh-claude';

import type { Rpc } from './rpc';

/**
 * Upstream gap in agent-mesh 0.1.1: the claude package DECLARES `StreamEndMeta`
 * but omits it from its export list (codex/cursor/gemini all export it). Deriving
 * it from `LineStreamFn` keeps this file pinned to the exact same contract without
 * importing a type from a sibling adapter that happens to re-declare it.
 */
export type StreamEndMeta = Parameters<NonNullable<Parameters<LineStreamFn>[1]>>[0];

/** Grace between SIGTERM and SIGKILL on a wall-clock timeout. Matches the vendor default. */
export const KILL_GRACE_MS = 5_000;
/** Upper bound on the stderr we keep, so a chatty child can never fill its pipe. */
export const STDERR_TAIL_CAP = 4096;
/** `runId` used for probes that are not part of a dispatch run (§5-6). */
export const INTROSPECT_RUN_ID = 'introspect';

/**
 * Per-call context for the seam. `ExecFn` is `(req) => Promise<ExecResult>` with no
 * slot for a run id, and we want ONE adapter instance per vendor (Cursor memoizes
 * its model catalog on the instance), so the run id travels through
 * `AsyncLocalStorage` from the `adapter.run(task)` call site down to the spawn.
 *
 * Documented fallback (DESIGN §5-6): if a future adapter version breaks async
 * context propagation, fall back to per-run adapter instances — the only cost is
 * Cursor's memoized model list.
 */
export const runCtx = new AsyncLocalStorage<{ runId: string; vendor: string }>();

/** What the host returns from `host.registerSpawn`. */
export interface RegisterSpawnResult {
  ok?: boolean;
  /** The value to stamp as `GILDE_SPAWN_ID`. The host chooses it; we never invent it. */
  envMarker?: string;
}

/** Registry `kind` values this module produces (DESIGN §8-2). */
export type SpawnKind = 'headless' | 'probe';

export interface RegisteredSpawn {
  spawnId: string;
  envMarker: string;
  env: Record<string, string>;
}

function currentCtx(): { runId: string; vendor: string } {
  return runCtx.getStore() ?? { runId: INTROSPECT_RUN_ID, vendor: 'unknown' };
}

/**
 * Compose the child env. `req.env` present ⇒ it IS the whole child env; absent ⇒
 * inherit `process.env`. Either way the two markers are added and `undefined`
 * values are dropped (spawn would drop them anyway; being explicit keeps the
 * reported key list honest).
 */
function composeEnv(
  reqEnv: Record<string, string | undefined> | undefined,
  envMarker: string,
  appInstanceId: string,
): Record<string, string> {
  const base: Record<string, string | undefined> = reqEnv ?? process.env;
  const out: Record<string, string> = {};
  for (const [key, value] of Object.entries(base)) {
    if (value !== undefined) out[key] = value;
  }
  out.GILDE_SPAWN_ID = envMarker;
  out.GILDE_APP_INSTANCE = appInstanceId;
  return out;
}

/**
 * Durable plan row first, spawn second (DESIGN §0 rule 2). Awaiting this is the
 * whole point of the bidirectional channel — a rejection here means we never spawn.
 */
export async function registerSpawn(
  rpc: Rpc,
  appInstanceId: string,
  kind: SpawnKind,
  req: ExecRequest,
): Promise<RegisteredSpawn> {
  const { runId, vendor } = currentCtx();
  const spawnId = randomUUID();
  const res = await rpc.call<RegisterSpawnResult>('host.registerSpawn', {
    spawnId,
    runId,
    vendor,
    kind,
    cmd: req.cmd,
    args: req.args,
    cwd: req.cwd,
    // Keys only. An env VALUE must never reach a host log (DESIGN §5-5).
    envKeys: Object.keys(req.env ?? {}),
    plannedAt: Date.now(),
  });
  const envMarker = typeof res?.envMarker === 'string' && res.envMarker.length > 0 ? res.envMarker : spawnId;
  return { spawnId, envMarker, env: composeEnv(req.env, envMarker, appInstanceId) };
}

/** `detached:true` makes the child a group leader, so `pgid === pid`. */
export async function reportSpawned(rpc: Rpc, spawnId: string, pid: number): Promise<void> {
  await rpc.call('host.spawned', { spawnId, pid, pgid: pid });
}

export async function reportExited(
  rpc: Rpc,
  spawnId: string,
  exitCode: number | null,
  signal: NodeJS.Signals | null,
  timedOut: boolean,
): Promise<void> {
  try {
    await rpc.call('host.exited', { spawnId, exitCode, signal, timedOut });
  } catch (err) {
    // The row settles through the reaper if this never lands; losing the run's
    // result because a bookkeeping call failed would be worse.
    rpc.notify('host.log', {
      level: 'warn',
      msg: 'host.exited failed',
      fields: { spawnId, error: err instanceof Error ? err.message : String(err) },
    });
  }
}

/**
 * Signal the child's whole process group. `detached:true` guarantees a group of
 * its own, so this can never reach the sidecar or the app.
 */
export function killProcessGroup(child: ChildProcess, signal: NodeJS.Signals): void {
  if (child.pid !== undefined) {
    try {
      process.kill(-child.pid, signal);
      return;
    } catch {
      /* group already gone — fall through to the direct kill */
    }
  }
  try {
    child.kill(signal);
  } catch {
    /* already reaped */
  }
}

/** Arm the wall-clock cap. Returns a disposer that clears both timers. */
function armTimeout(
  child: ChildProcess,
  timeoutMs: number | undefined,
  onTimeout: () => void,
): () => void {
  if (timeoutMs === undefined || !(timeoutMs > 0)) return () => {};
  let killTimer: NodeJS.Timeout | undefined;
  const timer = setTimeout(() => {
    onTimeout();
    killProcessGroup(child, 'SIGTERM');
    killTimer = setTimeout(() => killProcessGroup(child, 'SIGKILL'), KILL_GRACE_MS);
    killTimer.unref?.();
  }, timeoutMs);
  timer.unref?.();
  return () => {
    clearTimeout(timer);
    if (killTimer !== undefined) clearTimeout(killTimer);
  };
}

/** Feed `req.stdin` and close it. A child left waiting on stdin would hang forever. */
function primeStdin(child: ChildProcess, stdin: string | undefined): void {
  child.stdin?.on('error', () => {
    /* the child may close stdin before we finish writing; not our failure */
  });
  try {
    if (stdin !== undefined) child.stdin?.write(stdin);
    child.stdin?.end();
  } catch {
    /* same */
  }
}

/**
 * The registered {@link ExecFn}: run a child to completion, collecting stdout and
 * stderr. Resolves an {@link ExecResult}; a spawn-level failure (ENOENT) REJECTS,
 * which the adapters convert into an `AdapterExecError`.
 */
export function makeRegisteredExec(rpc: Rpc, appInstanceId: string): ExecFn {
  return async (req: ExecRequest): Promise<ExecResult> => {
    const { spawnId, env } = await registerSpawn(rpc, appInstanceId, 'headless', req);

    let child: ChildProcess;
    try {
      child = spawn(req.cmd, req.args, {
        cwd: req.cwd,
        env,
        shell: false,
        // Own process group: `pgid === pid`, so a timeout kill reaps the whole tree
        // and the host's killpg can never touch anything but this run.
        detached: true,
        stdio: ['pipe', 'pipe', 'pipe'],
      });
    } catch (err) {
      // The planned row must not dangle even though nothing ever ran.
      await reportExited(rpc, spawnId, null, null, false);
      throw err;
    }

    let stdout = '';
    let stderr = '';
    let timedOut = false;
    let exitCode: number | null = null;
    let exitSignal: NodeJS.Signals | null = null;

    // Streaming-safe decoding: setEncoding keeps a multi-byte character that
    // straddles two chunks intact.
    child.stdout?.setEncoding('utf8');
    child.stderr?.setEncoding('utf8');
    child.stdout?.on('data', (d: string) => {
      stdout += d;
    });
    child.stderr?.on('data', (d: string) => {
      stderr = (stderr + d).slice(-STDERR_TAIL_CAP);
    });

    // Assigned immediately below; the child's events cannot fire before then.
    let disarm: () => void = () => {};

    const settled = new Promise<void>((resolve, reject) => {
      let done = false;
      child.on('error', (err) => {
        if (done) return;
        done = true;
        disarm();
        reject(err);
      });
      child.on('exit', () => {
        if (timedOut) {
          child.stdout?.destroy();
          child.stderr?.destroy();
        }
      });
      child.on('close', (code, signal) => {
        if (done) return;
        done = true;
        exitCode = code;
        exitSignal = signal;
        disarm();
        resolve();
      });
    });

    disarm = armTimeout(child, req.timeoutMs, () => {
      timedOut = true;
    });

    primeStdin(child, req.stdin);

    if (child.pid !== undefined) {
      await reportSpawned(rpc, spawnId, child.pid);
    }

    try {
      await settled;
    } catch (err) {
      await reportExited(rpc, spawnId, null, null, timedOut);
      throw err;
    }

    await reportExited(rpc, spawnId, exitCode, exitSignal, timedOut);
    return { stdout, stderr, exitCode, timedOut };
  };
}

/**
 * The registered {@link LineStreamFn}: yield the child's stdout line by line.
 *
 * `onEnd` is invoked EXACTLY ONCE after the final line — on normal completion, on
 * a timeout, on a spawn error, and when the consumer abandons the iterator early
 * (in which case the child's group is force-killed first).
 */
export function makeRegisteredLineStream(rpc: Rpc, appInstanceId: string): LineStreamFn {
  return function registeredLineStream(
    req: ExecRequest,
    onEnd?: (meta: StreamEndMeta) => void,
  ): AsyncIterable<string> {
    return (async function* () {
      const { spawnId, env } = await registerSpawn(rpc, appInstanceId, 'headless', req);

      let child: ChildProcess;
      try {
        child = spawn(req.cmd, req.args, {
          cwd: req.cwd,
          env,
          shell: false,
          detached: true,
          stdio: ['pipe', 'pipe', 'pipe'],
        });
      } catch (err) {
        await reportExited(rpc, spawnId, null, null, false);
        onEnd?.({ timedOut: false, exitCode: null, stderrTail: '' });
        throw err;
      }

      let timedOut = false;
      let exitCode: number | null = null;
      let exitSignal: NodeJS.Signals | null = null;
      let stderrTail = '';
      let exited = false;
      let resolveExit: () => void = () => {};
      const exitedPromise = new Promise<void>((resolve) => {
        resolveExit = resolve;
      });

      child.stderr?.setEncoding('utf8');
      child.stderr?.on('data', (d: string) => {
        stderrTail = (stderrTail + d).slice(-STDERR_TAIL_CAP);
      });
      child.stdout?.setEncoding('utf8');

      child.on('exit', (code, signal) => {
        if (timedOut) {
          child.stdout?.destroy();
          child.stderr?.destroy();
        }
        if (exited) return;
        exited = true;
        exitCode = code;
        exitSignal = signal;
        resolveExit();
      });
      child.on('error', (err) => {
        if (!exited) {
          exited = true;
          resolveExit();
        }
        // Route the spawn failure into the line stream so `for await` throws and a
        // streaming caller can emit exactly one error event.
        child.stdout?.destroy(err);
      });

      const disarm = armTimeout(child, req.timeoutMs, () => {
        timedOut = true;
      });

      primeStdin(child, req.stdin);

      if (child.pid !== undefined) {
        await reportSpawned(rpc, spawnId, child.pid);
      }

      const stdoutStream = child.stdout;
      let reported = false;
      const finish = async (): Promise<void> => {
        if (reported) return;
        reported = true;
        disarm();
        await reportExited(rpc, spawnId, exitCode, exitSignal, timedOut);
        onEnd?.({ timedOut, exitCode, stderrTail });
      };

      if (stdoutStream === null) {
        await finish();
        return;
      }

      const rl = readline.createInterface({ input: stdoutStream, crlfDelay: Infinity });
      try {
        for await (const line of rl) {
          yield line;
        }
      } finally {
        disarm();
        rl.close();
        if (!exited) {
          // The consumer walked away (break / throw): never leave the group behind.
          killProcessGroup(child, 'SIGKILL');
          await exitedPromise;
        }
        await finish();
      }
    })();
  };
}
