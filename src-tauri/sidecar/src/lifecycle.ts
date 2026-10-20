/**
 * lifecycle.ts — the three ways this process learns it must stop (INT-C).
 *
 * The sidecar is a child that owns children. When the host dies the OS does not
 * tell us: on macOS we are reparented to `launchd` (ppid 1) and keep running,
 * still holding the `claude -p` process groups the run seam spawned. Left alone
 * that is a zombie population with a CPU cost — the defect this module closes was
 * measured at 6–7 hours of ~100% CPU per orphan.
 *
 * So there are exactly three exit paths, and this module owns the two the OS does
 * not hand us for free:
 *
 *   1. **channel end** — `rpc.ts` reports `end`/`close`/`error` on stdin.
 *   2. **signal** — SIGTERM / SIGINT / SIGHUP, wired in `index.ts`.
 *   3. **parent death** — {@link watchParent}, below.
 *
 * Nothing here spawns; `scripts/check-seam.mjs` would fail the build if it did.
 */

/** How often the parent-death watchdog looks. */
export const PARENT_POLL_MS = 2_000;

/** `init`. A ppid of 1 on macOS means our parent was reaped and we were adopted. */
const INIT_PID = 1;

export interface ParentWatchOptions {
  /**
   * The host pid, when the host told us (`GILDE_HOST_PID`). Checked with signal 0,
   * which is strictly stronger than the ppid test: it also catches the case where
   * a process between us and the host was the one that actually went away.
   */
  hostPid?: number | undefined;
  /** Poll cadence. */
  intervalMs?: number;
  /** Called once, with a human reason, the first time the parent is gone. */
  onParentGone: (why: string) => void;
}

/**
 * Poll for the death of whoever started us.
 *
 * Two independent signals, because neither alone is complete:
 *
 *   - `process.ppid` changing (to 1, or to anything other than the pid we were
 *     born under) means our direct parent was reaped. This needs no cooperation
 *     from the host and works in every launch mode.
 *   - `process.kill(hostPid, 0)` throwing `ESRCH` means the host process itself
 *     is gone. `EPERM` is deliberately NOT death — it means the pid exists and
 *     belongs to someone else, which is a recycled pid, and treating a live
 *     stranger as our dead host would be a self-inflicted outage.
 *
 * The timer is `unref`'d: a watchdog that keeps the process alive would be a new
 * instance of the bug it exists to fix.
 *
 * Returns a function that stops the watch.
 */
export function watchParent(opts: ParentWatchOptions): () => void {
  const initialPpid = process.ppid;
  const intervalMs = opts.intervalMs ?? PARENT_POLL_MS;
  const hostPid =
    typeof opts.hostPid === 'number' && Number.isInteger(opts.hostPid) && opts.hostPid > 1
      ? opts.hostPid
      : undefined;

  let fired = false;
  const fire = (why: string): void => {
    if (fired) return;
    fired = true;
    stop();
    opts.onParentGone(why);
  };

  const tick = (): void => {
    const ppid = process.ppid;
    if (ppid === INIT_PID || (initialPpid > INIT_PID && ppid !== initialPpid)) {
      fire(`parent ${initialPpid} is gone (ppid is now ${ppid})`);
      return;
    }
    if (hostPid !== undefined) {
      try {
        process.kill(hostPid, 0);
      } catch (err) {
        // ESRCH: no such process. Anything else (notably EPERM on a recycled
        // pid) is not evidence of death and must not end a healthy session.
        if ((err as NodeJS.ErrnoException).code === 'ESRCH') {
          fire(`host pid ${hostPid} is gone (ESRCH)`);
        }
      }
    }
  };

  const timer = setInterval(tick, intervalMs);
  timer.unref?.();

  function stop(): void {
    clearInterval(timer);
  }

  return stop;
}

/**
 * True for the failures that mean "our own stdio pipe has no reader any more".
 *
 * This is the load-bearing predicate of INT-C. When the host is SIGKILLed, the
 * sidecar's stderr write fails asynchronously with `EPIPE`; with no listener that
 * becomes an `uncaughtException`, and an `uncaughtException` handler that logs
 * produces the next `EPIPE` — a self-feeding storm that pegged a core and starved
 * the event loop so thoroughly that the SIGTERM handler could never run. Such an
 * error is not something to report; it is the shutdown signal itself.
 */
export function isBrokenPipe(err: unknown): boolean {
  const code = (err as NodeJS.ErrnoException | undefined)?.code;
  return (
    code === 'EPIPE' ||
    code === 'ERR_STREAM_DESTROYED' ||
    code === 'ERR_STREAM_WRITE_AFTER_END' ||
    code === 'ECONNRESET'
  );
}
