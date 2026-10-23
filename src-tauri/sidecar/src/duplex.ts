/**
 * duplex.ts — a registered spawner that keeps the child's stdin OPEN.
 *
 * `ExecRequest.stdin?: string` can only hand a child one fixed blob and then
 * close the pipe, so it cannot express a request/response conversation over
 * stdin. Claude's live-usage probe needs exactly that: write a `control_request`
 * and keep reading until the matching `control_response` arrives (DESIGN §5-7).
 *
 * Rather than let that probe escape supervision, it runs on top of this spawner:
 * same `host.registerSpawn` → `host.spawned` → `host.exited` lifecycle, same
 * `GILDE_SPAWN_ID` marker, same own-process-group kill path — so the probe shows up
 * in the Process Monitor like every other child. (A cold `claude` quota probe has
 * been measured at over 20 seconds; an unexplained `claude` process for that long
 * is exactly what the monitor exists to explain.)
 *
 * The probe itself lives in `usage.ts`; this module only owns the process.
 *
 * This file and `exec-registered.ts` are the only modules of ours allowed to call
 * `spawn(` — `scripts/check-seam.mjs` fails the build otherwise.
 */

import { spawn } from 'node:child_process';
import type { ChildProcess } from 'node:child_process';
import type { Readable, Writable } from 'node:stream';

import {
  KILL_GRACE_MS,
  STDERR_TAIL_CAP,
  killProcessGroup,
  registerSpawn,
  reportExited,
  reportSpawned,
} from './exec-registered';
import type { Rpc } from './rpc';

export interface DuplexRequest {
  cmd: string;
  args: string[];
  cwd?: string;
  env?: Record<string, string | undefined>;
  /** Wall-clock cap. On expiry the group gets SIGTERM, then a second SIGTERM after 5s (never SIGKILL, C5). */
  timeoutMs?: number;
}

export interface DuplexExit {
  exitCode: number | null;
  signal: NodeJS.Signals | null;
  timedOut: boolean;
}

export interface DuplexHandle {
  /** Registry id of this child (`GILDE_SPAWN_ID` is derived from the host's answer). */
  readonly spawnId: string;
  /** Always defined: a failed spawn rejects instead of returning a handle. */
  readonly pid: number;
  readonly stdin: Writable;
  readonly stdout: Readable;
  readonly stderr: Readable;
  /** The last ≤4 KiB of stderr, UTF-8 decoded. */
  stderrTail(): string;
  /** True once the wall-clock cap tripped. */
  timedOut(): boolean;
  /**
   * Signal the child's process group: SIGTERM, repeated once after the grace.
   * A vendor CLI is never sent SIGKILL (contract §9.8, C5) — asking for it sends
   * SIGTERM.
   */
  kill(signal?: NodeJS.Signals): void;
  /** Resolves when the child has closed. Never rejects. */
  readonly closed: Promise<DuplexExit>;
}

/**
 * Spawn a registered child with all three stdio pipes and stdin left OPEN.
 *
 * Rejects if the spawn itself fails (ENOENT); in that case the planned registry
 * row is settled first so it cannot dangle.
 */
export async function registeredDuplexSpawn(
  rpc: Rpc,
  appInstanceId: string,
  req: DuplexRequest,
): Promise<DuplexHandle> {
  const { spawnId, env } = await registerSpawn(
    rpc,
    appInstanceId,
    {
      cmd: req.cmd,
      args: req.args,
      cwd: req.cwd,
      env: req.env,
      timeoutMs: req.timeoutMs,
    },
    'probe',
  );

  let child: ChildProcess;
  try {
    child = spawn(req.cmd, req.args, {
      cwd: req.cwd,
      env,
      shell: false,
      // Own process group so the escalation below can never reach the sidecar.
      detached: true,
      stdio: ['pipe', 'pipe', 'pipe'],
    });
  } catch (err) {
    await reportExited(rpc, spawnId, null, null, false);
    throw err;
  }

  const stdin = child.stdin;
  const stdout = child.stdout;
  const stderr = child.stderr;
  if (stdin === null || stdout === null || stderr === null) {
    killProcessGroup(child, 'SIGTERM');
    await reportExited(rpc, spawnId, null, null, false);
    throw new Error(`duplex spawn of ${req.cmd} did not yield all three pipes`);
  }

  let timedOut = false;
  let stderrTail = '';
  let repeatTimer: NodeJS.Timeout | undefined;
  let timeoutTimer: NodeJS.Timeout | undefined;

  stdin.on('error', () => {
    /* the child may exit mid-write; the closed promise carries the real outcome */
  });
  stderr.setEncoding('utf8');
  stderr.on('data', (d: string) => {
    stderrTail = (stderrTail + d).slice(-STDERR_TAIL_CAP);
  });
  stdout.setEncoding('utf8');

  const spawnError = new Promise<Error>((resolve) => {
    child.once('error', resolve);
  });

  const closed = new Promise<DuplexExit>((resolve) => {
    child.once('close', (code, signal) => {
      if (timeoutTimer !== undefined) clearTimeout(timeoutTimer);
      if (repeatTimer !== undefined) clearTimeout(repeatTimer);
      void reportExited(rpc, spawnId, code, signal, timedOut).then(() =>
        resolve({ exitCode: code, signal, timedOut }),
      );
    });
  });

  // SIGTERM, then one more SIGTERM after the grace — never SIGKILL (C5).
  let escalated = false;
  const escalate = (): void => {
    killProcessGroup(child, 'SIGTERM');
    if (escalated) return;
    escalated = true;
    repeatTimer = setTimeout(() => killProcessGroup(child, 'SIGTERM'), KILL_GRACE_MS);
    repeatTimer.unref?.();
  };

  if (req.timeoutMs !== undefined && req.timeoutMs > 0) {
    timeoutTimer = setTimeout(() => {
      timedOut = true;
      escalate();
    }, req.timeoutMs);
    timeoutTimer.unref?.();
  }

  if (child.pid === undefined) {
    // spawn() returned without a pid: the 'error' event is already queued.
    const err = await spawnError;
    if (timeoutTimer !== undefined) clearTimeout(timeoutTimer);
    await reportExited(rpc, spawnId, null, null, false);
    throw err;
  }

  const pid = child.pid;
  await reportSpawned(rpc, spawnId, pid);

  return {
    spawnId,
    pid,
    stdin,
    stdout,
    stderr,
    stderrTail: () => stderrTail,
    timedOut: () => timedOut,
    kill(signal: NodeJS.Signals = 'SIGTERM') {
      if (signal === 'SIGTERM' || signal === 'SIGKILL') {
        escalate();
        return;
      }
      killProcessGroup(child, signal);
    },
    closed,
  };
}
