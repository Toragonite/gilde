/**
 * index.ts — gilde-code-sidecar entry point.
 *
 * Layout is deliberate and load-bearing (DESIGN §5-1):
 *
 *   1. The stream-hardening block below is the FIRST executed code in the bundle.
 *      It captures the real stdout writer and then makes `process.stdout.write`
 *      an alias of stderr, so that a stray `console.log` from any dependency —
 *      loaded later, through the dynamic `import()` in `main()` — physically
 *      cannot interleave with the NDJSON frames.
 *   2. Every module (ours and agent-mesh's) is pulled in AFTER that, through
 *      dynamic import, which esbuild keeps deferred in the CJS bundle. From then
 *      on the only path to the real stdout is the writer handed to `rpc.ts`.
 *   3. `main()` registers protocol 2 (contract §4.5) — the lifecycle methods
 *      (`mesh.*`), `turn.*`, `probe.*`, and the presence-gated `orch.*` — and
 *      starts the channel.
 *
 * The process never dies from an unexpected throw: an uncaught exception or a
 * rejected promise is logged to stderr and the channel keeps running, because a
 * sidecar that exits mid-turn turns a recoverable error into orphaned children.
 * The one deliberate exit besides the three below is `mesh.hello` from another
 * build: exit code 78 (contract §4.5).
 *
 * ONE EXCEPTION, and it is the whole of INT-C: a failure of our OWN stdio is not
 * a recoverable error, it is the news that the host is gone. Treating it as
 * loggable was the defect — `writeStderrLine` on a reader-less pipe fails
 * asynchronously with `EPIPE`, the `uncaughtException` handler logged that, the
 * log produced the next `EPIPE`, and the resulting storm pegged a core for hours
 * while starving the event loop so completely that the SIGTERM handler could
 * never run. Hence the `error` listeners installed below, before anything else
 * can write a byte, and the three exit paths at the bottom of `main()`.
 */

// ─────────────────────────────────────────────────────────────────────────────
// STREAM HARDENING — must run before any import. Nothing above this line.
// ─────────────────────────────────────────────────────────────────────────────

type StdoutWriter = (chunk: string) => boolean;

/** The one and only handle on the real stdout. Handed to `rpc.ts`, nowhere else. */
const realStdoutWrite: StdoutWriter = process.stdout.write.bind(process.stdout) as StdoutWriter;

/**
 * Set the first time one of our own stdio pipes loses its reader. From then on
 * every write here is a no-op: there is nowhere left to say anything, and trying
 * is what produced the storm this flag exists to stop.
 */
let stdioBroken = false;

/**
 * Installed by `main()` once a shutdown path exists. Until then a broken pipe
 * only raises the flag — the process is still in its first milliseconds and has
 * neither children nor a channel to wind down.
 */
let onStdioBroken: ((why: string) => void) | undefined;

function markStdioBroken(why: string): void {
  if (stdioBroken) return;
  stdioBroken = true;
  onStdioBroken?.(why);
}

// These two listeners are the reason an EPIPE can no longer become an
// `uncaughtException`. They are installed before any import, before any log
// line, and before the RPC channel exists, because the window they close starts
// at the first byte anyone writes.
process.stdout.on('error', (err: NodeJS.ErrnoException) => {
  markStdioBroken(`stdout ${err.code ?? err.message}`);
});
process.stderr.on('error', (err: NodeJS.ErrnoException) => {
  markStdioBroken(`stderr ${err.code ?? err.message}`);
});

/** Render an arbitrary console argument without ever throwing. */
function renderArg(value: unknown): string {
  if (typeof value === 'string') return value;
  if (value instanceof Error) return value.stack ?? `${value.name}: ${value.message}`;
  try {
    return JSON.stringify(value) ?? String(value);
  } catch {
    return String(value);
  }
}

function writeStderrLine(prefix: string, args: unknown[]): void {
  if (stdioBroken) return;
  try {
    process.stderr.write(`${prefix} ${args.map(renderArg).join(' ')}\n`);
  } catch (err) {
    // A synchronous failure (Linux pipes, a closed fd) lands here; the
    // asynchronous one (macOS pipes) lands on the `error` listener above. Both
    // mean the same thing and must set the same flag.
    markStdioBroken(`stderr ${(err as NodeJS.ErrnoException).code ?? 'write failed'}`);
  }
}

// stdout is now stderr for everyone but rpc.ts.
(process.stdout as unknown as { write: unknown }).write = function redirectedStdoutWrite(
  chunk: unknown,
  encoding?: unknown,
  cb?: unknown,
): boolean {
  const callback = typeof encoding === 'function' ? encoding : cb;
  const enc = typeof encoding === 'function' ? undefined : encoding;
  return (process.stderr.write as (c: unknown, e?: unknown, f?: unknown) => boolean)(
    chunk,
    enc,
    callback,
  );
};

console.log = (...args: unknown[]): void => writeStderrLine('[log]', args);
console.info = (...args: unknown[]): void => writeStderrLine('[info]', args);
console.warn = (...args: unknown[]): void => writeStderrLine('[warn]', args);
console.error = (...args: unknown[]): void => writeStderrLine('[error]', args);
console.debug = (...args: unknown[]): void => writeStderrLine('[debug]', args);

// ─────────────────────────────────────────────────────────────────────────────
// Build-time constants, injected by esbuild.mjs (`define`).
// ─────────────────────────────────────────────────────────────────────────────
declare const __GILDE_BUILD_ID__: string;
declare const __GILDE_SIDECAR_VERSION__: string;
declare const __GILDE_MESH_PACKAGES__: Record<string, string>;

/**
 * Exit budgets (INT-C). Each is a wall-clock ceiling enforced by a timer, not a
 * hope that the event loop drains.
 *
 * The channel-end and parent-death paths get 1 s: the host is already gone, so
 * there is no one to flush to and no reason to linger. A signal gets 2 s because
 * the sender is usually the host's own `take_down`, which escalates TERM→KILL
 * on THIS node process after `TERMINATE_GRACE` (3 s) — exiting inside that window
 * is what makes the host record `SIGTERM` rather than `SIGKILL` for a healthy stop.
 */
const CHANNEL_EXIT_BUDGET_MS = 1_000;
const SIGNAL_EXIT_BUDGET_MS = 2_000;

type LogFields = Record<string, string | number | boolean>;

/** `host.log` fields are primitives only (contract `HostLogParams`); anything else is dropped. */
function primitiveFields(fields: Record<string, unknown> | undefined): LogFields {
  const out: LogFields = {};
  if (fields === undefined) return out;
  for (const [key, value] of Object.entries(fields)) {
    if (typeof value === 'string') out[key] = value.slice(0, 512);
    else if (typeof value === 'number' && Number.isFinite(value)) out[key] = value;
    else if (typeof value === 'boolean') out[key] = value;
  }
  return out;
}

async function main(): Promise<void> {
  // Everything below is imported HERE, after the hardening block above.
  const { createRpc } = await import('./rpc');
  const { trackSpawns, killAllChildren } = await import('./runs');
  const { watchParent, isBrokenPipe } = await import('./lifecycle');
  const { makeRegisteredGetUsage } = await import('./usage');
  const { registerMeshMethods } = await import('./methods/mesh');
  const { registerTurnMethods } = await import('./methods/turns');
  const { registerProbeMethods } = await import('./methods/probes');
  const { OrchestratorGate } = await import('./methods/orch');
  const { EXIT_BUILD_MISMATCH, PROTOCOL } = await import('./methods/wire');
  const os = await import('node:os');

  const rpc = createRpc(process.stdin, realStdoutWrite, {
    log: (level, msg, fields) => writeStderrLine(`[rpc:${level}]`, fields === undefined ? [msg] : [msg, fields]),
    onPeerClosed: (why) => {
      // EXIT PATH 1 — the channel ended. `end`, `close` and an error on stdin all
      // arrive here, and all three mean the same thing: nothing more can be asked
      // of us, and the process that would have reaped our children is gone.
      void shutdown(0, `host closed the channel (${why})`, CHANNEL_EXIT_BUDGET_MS);
    },
  });

  /**
   * The channel the SEAMS use. It forwards everything unchanged and additionally
   * records `host.registerSpawn`/`spawned`/`exited` into the `runs` table, which is
   * what lets `turn.cancel` find a turn's process groups (`runs.ts`). Handing the
   * seams this wrapper — rather than giving the adapters a second callback — keeps
   * the supervision contract to exactly one path.
   */
  const supervisedRpc = trackSpawns(rpc);

  let shuttingDown = false;
  /** Stops the parent-death watchdog. Set once the guards are installed. */
  let stopParentWatch: (() => void) | undefined;

  const log = (level: 'debug' | 'info' | 'warn' | 'error', msg: string, fields?: Record<string, unknown>): void => {
    writeStderrLine(`[sidecar:${level}]`, fields === undefined ? [msg] : [msg, fields]);
    // Once we are on the way out, a log line is not worth a frame: the host may
    // be gone, and an outbound write is the first half of the loop INT-C closed.
    if (shuttingDown) return;
    rpc.notify('host.log', { level, msg, fields: primitiveFields(fields) });
  };

  const orch = new OrchestratorGate(rpc, { log: (level, msg) => log(level, msg) });

  /**
   * Wind down and exit, within `budgetMs`, no matter what.
   *
   * The order is not negotiable (INT-C):
   *
   *   1. **Arm the hard floor first.** Everything after it is best-effort; the
   *      timer is what makes the budget a guarantee rather than a hope, and it is
   *      deliberately not `unref`'d — a shutdown that lets the loop empty first is
   *      a shutdown that does not happen when the loop is busy.
   *   2. **Stop accepting.** No inbound request may start work we are about to
   *      abandon; a request in flight when we exit is a child nobody owns.
   *   3. **Signal our children** — SIGTERM only. They are vendor CLIs, and a
   *      vendor CLI is never sent SIGKILL (contract §9.8, C5); whatever outlives
   *      this is in the host's registry for its sweep to report as a survivor.
   *   4. **Flush, bounded.** The host is owed its pending responses, but not at
   *      the price of the exit.
   *   5. **Exit.**
   */
  async function shutdown(code: number, reason: string, budgetMs: number): Promise<void> {
    if (shuttingDown) return;
    shuttingDown = true;
    process.exitCode = code;
    stopParentWatch?.();
    writeStderrLine('[sidecar:info]', [`shutting down: ${reason} (budget ${budgetMs}ms)`]);

    const hardExit = setTimeout(() => {
      writeStderrLine('[sidecar:warn]', [`shutdown budget elapsed; exiting hard (${reason})`]);
      process.exit(code);
    }, budgetMs);

    try {
      rpc.stopAccepting(reason);
    } catch {
      /* best effort */
    }
    try {
      orch.dispose();
    } catch {
      /* best effort */
    }

    let signalled = 0;
    try {
      signalled = killAllChildren();
    } catch {
      /* best effort */
    }
    if (signalled > 0) {
      writeStderrLine('[sidecar:info]', [`sent SIGTERM to ${signalled} child process group(s)`]);
    }

    try {
      await rpc.flush(Math.max(0, Math.floor(budgetMs / 3)));
    } catch {
      /* nothing else to try */
    }
    try {
      rpc.close(reason);
    } catch {
      /* best effort */
    }

    clearTimeout(hardExit);
    process.exit(code);
  }

  // ── protocol 2 (contract §4.5) ─────────────────────────────────────────────
  const getState = registerMeshMethods({
    channel: rpc,
    seamRpc: supervisedRpc,
    buildId: __GILDE_BUILD_ID__,
    // Built per app instance: the probe stamps `GILDE_APP_INSTANCE` on its child,
    // and the host only tells us that id in the handshake.
    makeGetUsage: (appInstanceId) => makeRegisteredGetUsage(supervisedRpc, appInstanceId),
    orch,
    onBuildMismatch: (received) => {
      // The error response is queued by the time this runs (the handler throws
      // right after); the shutdown flushes it, then exits 78 (§4.5, §4.7).
      setImmediate(() => {
        void shutdown(EXIT_BUILD_MISMATCH, `build mismatch (host ${received}, sidecar ${__GILDE_BUILD_ID__})`, CHANNEL_EXIT_BUDGET_MS);
      });
    },
    onShutdown: (graceMs) => {
      // Deliberately NOT unref'd: the shutdown must happen even if the channel is
      // the only thing keeping the loop alive.
      setTimeout(() => void shutdown(0, 'mesh.shutdown', SIGNAL_EXIT_BUDGET_MS), graceMs);
    },
    log,
  });
  registerTurnMethods(rpc, getState);
  registerProbeMethods(rpc, getState);

  // ── self-test hooks ────────────────────────────────────────────────────────
  // `test/stub-host.mjs` drives the exec seam directly through these. They exist
  // only when GILDE_SIDECAR_SELFTEST=1, so the production channel never exposes a
  // way to spawn an arbitrary command.
  if (process.env.GILDE_SIDECAR_SELFTEST === '1') {
    const { makeRegisteredExec, makeRegisteredLineStream, runCtx } = await import('./exec-registered');
    const { registeredDuplexSpawn } = await import('./duplex');
    const selfTestInstance = process.env.GILDE_APP_INSTANCE ?? 'selftest-instance';
    type SelfTestVendor = import('./gen/Vendor').Vendor;

    interface DevExecParams {
      cmd: string;
      args?: string[];
      cwd?: string;
      env?: Record<string, string | undefined>;
      timeoutMs?: number;
      stdin?: string;
      turnId?: string;
      vendor?: SelfTestVendor;
    }
    const ctxOf = (params: DevExecParams) =>
      ({
        turnId: params.turnId ?? null,
        vendor: params.vendor ?? 'claude',
        kind: params.turnId === undefined ? 'probe' : 'turn',
      }) as const;

    rpc.on('dev.exec', async (params: DevExecParams) => {
      const exec = makeRegisteredExec(supervisedRpc, selfTestInstance);
      const req = {
        cmd: params.cmd,
        args: params.args ?? [],
        ...(params.cwd !== undefined ? { cwd: params.cwd } : {}),
        ...(params.env !== undefined ? { env: params.env } : {}),
        ...(params.timeoutMs !== undefined ? { timeoutMs: params.timeoutMs } : {}),
        ...(params.stdin !== undefined ? { stdin: params.stdin } : {}),
      };
      return runCtx.run(ctxOf(params), () => exec(req));
    });

    rpc.on('dev.lineStream', async (params: DevExecParams) => {
      const lineStream = makeRegisteredLineStream(supervisedRpc, selfTestInstance);
      const req = {
        cmd: params.cmd,
        args: params.args ?? [],
        ...(params.cwd !== undefined ? { cwd: params.cwd } : {}),
        ...(params.env !== undefined ? { env: params.env } : {}),
        ...(params.timeoutMs !== undefined ? { timeoutMs: params.timeoutMs } : {}),
      };
      return runCtx.run(ctxOf(params), async () => {
        const lines: string[] = [];
        let meta: unknown = null;
        for await (const line of lineStream(req, (m) => {
          meta = m;
        })) {
          lines.push(line);
        }
        return { lines, meta };
      });
    });

    rpc.on('dev.duplex', async (params: DevExecParams & { write?: string }) => {
      const handle = await runCtx.run(ctxOf(params), () =>
        registeredDuplexSpawn(supervisedRpc, selfTestInstance, {
          cmd: params.cmd,
          args: params.args ?? [],
          ...(params.cwd !== undefined ? { cwd: params.cwd } : {}),
          ...(params.env !== undefined ? { env: params.env } : {}),
          ...(params.timeoutMs !== undefined ? { timeoutMs: params.timeoutMs } : {}),
        }),
      );
      let stdout = '';
      handle.stdout.on('data', (d: string) => {
        stdout += d;
      });
      if (params.write !== undefined) handle.stdin.write(params.write);
      const exit = await handle.closed;
      return { pid: handle.pid, spawnId: handle.spawnId, stdout, stderrTail: handle.stderrTail(), ...exit };
    });

    const { neverOpenStats } = await import('./methods/never-open');
    rpc.on('dev.stats', () => ({
      ...rpc.stats,
      tmpdir: os.tmpdir(),
      orchestrator: orch.present,
      neverOpenRefused: neverOpenStats.refused,
    }));

    // Prove the hardening: a dependency's console.log and even a direct
    // process.stdout.write must land on stderr, never on the protocol channel.
    rpc.on('dev.pollute', (params: { text?: string }) => {
      const text = params?.text ?? 'POLLUTION';
      console.log(text, { via: 'console.log' });
      console.error(text, { via: 'console.error' });
      process.stdout.write(`${text} via process.stdout.write\n`);
      return { ok: true };
    });

    // Prove the backpressure contract: with the host not reading, lossy
    // `orch.snapshot` notifications are discarded at the queue cap, while every
    // `turn.event` and every response still arrives.
    rpc.on('dev.floodEvents', (params: { count?: number; turnEvents?: number }) => {
      const count = typeof params?.count === 'number' ? params.count : 1000;
      for (let i = 0; i < count; i += 1) {
        rpc.notify('orch.snapshot', {
          generation: 1,
          rev: i,
          team: { teamId: 'tmflood00001', view: null, pad: 'x'.repeat(64) },
        });
      }
      const turnEvents = typeof params?.turnEvents === 'number' ? params.turnEvents : 0;
      for (let i = 0; i < turnEvents; i += 1) {
        rpc.notify('turn.event', { turnId: 'flood', seq: i + 1, event: { kind: 'text', delta: `${i}` } });
      }
      return { sent: count, turnEvents, dropped: rpc.stats.droppedNotifications };
    });
  }

  // ── process-level guards ───────────────────────────────────────────────────
  //
  // The rule stays what the header says — a recoverable error must not kill a
  // sidecar mid-turn — with the one exception INT-C carved out: a failure of our
  // own stdio is not an error to report, it is the host's death certificate.
  // Reporting it wrote the next one, forever.
  process.on('uncaughtException', (err) => {
    if (isBrokenPipe(err) || stdioBroken) {
      markStdioBroken(`uncaughtException ${(err as NodeJS.ErrnoException).code ?? 'EPIPE'}`);
      return;
    }
    log('error', 'uncaught exception', { error: err instanceof Error ? err.stack ?? err.message : String(err) });
  });
  process.on('unhandledRejection', (reason) => {
    if (isBrokenPipe(reason) || stdioBroken) return;
    log('error', 'unhandled rejection', {
      error: reason instanceof Error ? reason.stack ?? reason.message : String(reason),
    });
  });

  // EXIT PATH 1b — our own stdio lost its reader. Same fact as a closed channel,
  // learned from the other direction.
  onStdioBroken = (why) => {
    void shutdown(0, `stdio pipe closed (${why})`, CHANNEL_EXIT_BUDGET_MS);
  };
  if (stdioBroken) onStdioBroken('detected before the guards were installed');

  // EXIT PATH 2 — a signal. All three are the same request; the hard floor inside
  // `shutdown` is what makes them impossible to ignore.
  for (const signal of ['SIGTERM', 'SIGINT', 'SIGHUP'] as const) {
    process.on(signal, () => void shutdown(0, signal, SIGNAL_EXIT_BUDGET_MS));
  }

  // EXIT PATH 3 — the host died without closing anything (SIGKILL, a panic, a
  // crashed test runner). Nothing arrives on any stream in that case: on macOS we
  // are simply reparented to launchd and keep holding our children. `GILDE_HOST_PID`
  // is what `SidecarManager` stamps on us; `process.ppid` is the fallback that
  // needs no cooperation at all.
  const hostPidEnv = Number.parseInt(process.env.GILDE_HOST_PID ?? '', 10);
  stopParentWatch = watchParent({
    hostPid: Number.isFinite(hostPidEnv) ? hostPidEnv : undefined,
    onParentGone: (why) => {
      void shutdown(0, `parent died (${why})`, CHANNEL_EXIT_BUDGET_MS);
    },
  });

  rpc.start();
  writeStderrLine('[sidecar:info]', [
    `gilde-code-sidecar ${__GILDE_SIDECAR_VERSION__} (build ${__GILDE_BUILD_ID__}) on ${process.version}, protocol ${PROTOCOL}`,
    { packages: __GILDE_MESH_PACKAGES__ },
  ]);
}

main().catch((err: unknown) => {
  writeStderrLine('[sidecar:fatal]', [err]);
  process.exit(70); // EX_SOFTWARE
});
