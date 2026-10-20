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
 *   3. `main()` builds the fleet, registers the lifecycle methods, and starts the
 *      channel. The `mesh.*` run surface is registered on top of this (T18).
 *
 * The process never dies from an unexpected throw: an uncaught exception or a
 * rejected promise is logged to stderr and the channel keeps running, because a
 * sidecar that exits mid-run turns a recoverable error into orphaned children.
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
declare const __GILDE_SIDECAR_VERSION__: string;
declare const __GILDE_MESH_PACKAGES__: Record<string, string>;

/** Wire protocol revision this build speaks. A mismatch is a hard error (§5-3). */
const PROTOCOL = 1;
/** JSON-RPC error code for a `mesh.hello` protocol mismatch. */
const RPC_PROTOCOL_MISMATCH = -32002;

/**
 * Exit budgets (INT-C). Each is a wall-clock ceiling enforced by a timer, not a
 * hope that the event loop drains.
 *
 * The channel-end and parent-death paths get 1 s: the host is already gone, so
 * there is no one to flush to and no reason to linger. A signal gets 2 s because
 * the sender is usually the host's own `take_down`, which escalates TERM→KILL
 * after `TERMINATE_GRACE` (3 s) — exiting inside that window is what makes the
 * host record `SIGTERM` rather than `SIGKILL` for a healthy stop.
 */
const CHANNEL_EXIT_BUDGET_MS = 1_000;
const SIGNAL_EXIT_BUDGET_MS = 2_000;

interface HelloParams {
  protocol?: number;
  hostVersion?: string;
  appInstanceId?: string;
  bootId?: string;
  binaries?: { claude?: string; codex?: string; cursorAgent?: string; agy?: string };
  defaults?: { timeoutMs?: number; permissionMode?: string };
  accounts?: unknown[];
}

async function main(): Promise<void> {
  // Everything below is imported HERE, after the hardening block above.
  const { createRpc, RpcError, RPC_INVALID_PARAMS } = await import('./rpc');
  const { buildFleet } = await import('./fleet');
  const { trackSpawns, killAllChildren } = await import('./runs');
  const { watchParent, isBrokenPipe } = await import('./lifecycle');
  const { makeRegisteredGetUsage } = await import('./usage');
  const { registerMeshMethods } = await import('./methods/mesh');
  const { registerFleetMethods } = await import('./methods/fleet');
  const { registerCcoMethods } = await import('./methods/cco');
  const fs = await import('node:fs');
  const path = await import('node:path');
  const os = await import('node:os');

  type SidecarState = import('./fleet').SidecarState;
  type Account = import('@toragonite/agent-mesh').Account;
  type Adapter = import('@toragonite/agent-mesh').Adapter;
  type Vendor = import('@toragonite/agent-mesh').Vendor;

  /**
   * The registered duplex quota probe (`usage.ts`). It is a `ClaudeAdapterOptions`
   * seam and claude-only by design (§5-7): the control protocol needs an open
   * stdin, which `ExecRequest.stdin` cannot express. Left undefined,
   * `ClaudeAdapter` would fall back to its own UNREGISTERED probe, so it is wired
   * through `buildFleet` at both construction points. It is bound to the app
   * instance id, so it can only be built once `mesh.hello` has supplied one.
   */
  let usageProbe: import('@toragonite/agent-mesh-claude').ClaudeAdapterOptions['getUsage'];

  const rpc = createRpc(process.stdin, realStdoutWrite, {
    log: (level, msg, fields) => writeStderrLine(`[rpc:${level}]`, fields === undefined ? [msg] : [msg, fields]),
    onPeerClosed: (why) => {
      // EXIT PATH 1 — the channel ended. `end`, `close` and an error on stdin all
      // arrive here, and all three mean the same thing: nothing more can be asked
      // of us, and the process that would have reaped our children is gone.
      void shutdown(0, `host closed the channel (${why})`, CHANNEL_EXIT_BUDGET_MS, true);
    },
  });

  /**
   * The channel the SEAMS use. It forwards everything unchanged and additionally
   * records `host.registerSpawn`/`spawned`/`exited` into the `runs` table, which is
   * what lets `mesh.cancel` find a run's process groups (`runs.ts`). Handing the
   * seams this wrapper — rather than giving the adapters a second callback — keeps
   * the supervision contract to exactly one path.
   */
  const supervisedRpc = trackSpawns(rpc);

  let state: SidecarState | undefined;
  let shuttingDown = false;
  /** Stops the parent-death watchdog. Set once the guards are installed. */
  let stopParentWatch: (() => void) | undefined;

  const log = (level: 'debug' | 'info' | 'warn' | 'error', msg: string, fields?: Record<string, unknown>): void => {
    writeStderrLine(`[sidecar:${level}]`, fields === undefined ? [msg] : [msg, fields]);
    // Once we are on the way out, a log line is not worth a frame: the host may
    // be gone, and an outbound write is the first half of the loop INT-C closed.
    if (shuttingDown) return;
    rpc.notify('host.log', fields === undefined ? { level, msg } : { level, msg, fields });
  };

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
   *   3. **Signal our children.** When `hostGone` we escalate to `SIGKILL`
   *      ourselves, because the host's reaper — the authority per DESIGN §5-9 —
   *      died with the host. When the host is alive we send only `SIGTERM` and
   *      leave the escalation to it, which is where the identity fences live.
   *   4. **Flush, bounded.** The host is owed its pending responses, but not at
   *      the price of the exit.
   *   5. **Exit.**
   */
  async function shutdown(
    code: number,
    reason: string,
    budgetMs: number,
    hostGone: boolean,
  ): Promise<void> {
    if (shuttingDown) return;
    shuttingDown = true;
    process.exitCode = code;
    stopParentWatch?.();
    writeStderrLine('[sidecar:info]', [`shutting down: ${reason} (budget ${budgetMs}ms)`]);

    const hardExit = setTimeout(() => {
      writeStderrLine('[sidecar:warn]', [`shutdown budget elapsed; exiting hard (${reason})`]);
      try {
        killAllChildren('SIGKILL');
      } catch {
        /* the exit below is what matters */
      }
      process.exit(code);
    }, budgetMs);

    try {
      rpc.stopAccepting(reason);
    } catch {
      /* best effort */
    }

    let signalled = 0;
    try {
      signalled = killAllChildren('SIGTERM');
    } catch {
      /* best effort */
    }
    if (signalled > 0) {
      writeStderrLine('[sidecar:info]', [`signalled ${signalled} child process group(s)`]);
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

    if (hostGone && signalled > 0) {
      try {
        killAllChildren('SIGKILL');
      } catch {
        /* best effort */
      }
    }

    clearTimeout(hardExit);
    process.exit(code);
  }

  // ── binary resolution (no spawning: `mesh.hello` must stay instant) ─────────
  const isExecutable = (p: string): boolean => {
    try {
      const st = fs.statSync(p);
      if (!st.isFile()) return false;
      fs.accessSync(p, fs.constants.X_OK);
      return true;
    } catch {
      return false;
    }
  };

  const resolveOnPath = (name: string): string | null => {
    const pathEnv = process.env.PATH ?? '';
    for (const dir of pathEnv.split(path.delimiter)) {
      if (dir.length === 0) continue;
      const candidate = path.join(dir, name);
      if (isExecutable(candidate)) return candidate;
    }
    return null;
  };

  const resolveBinary = (value: string): string | null =>
    value.includes(path.sep) ? (isExecutable(value) ? path.resolve(value) : null) : resolveOnPath(value);

  interface AdapterInfo {
    vendor: Vendor;
    capabilities: Adapter['capabilities'];
    binary: string;
    binaryPath: string | null;
    installed: boolean;
  }

  type ResolvedBinaries = import('./fleet').BuiltFleet['binaries'];

  const binaryFor = (vendor: Vendor, binaries: ResolvedBinaries): string => {
    switch (vendor) {
      case 'claude':
        return binaries.claude;
      case 'codex':
        return binaries.codex;
      case 'cursor':
        return binaries.cursorAgent;
      case 'gemini':
        return binaries.agy;
      default:
        return String(vendor);
    }
  };

  const describeAdapters = (s: SidecarState): AdapterInfo[] => {
    const out: AdapterInfo[] = [];
    for (const [vendor, adapter] of s.built.adapters) {
      const binary = binaryFor(vendor, s.built.binaries);
      const binaryPath = resolveBinary(binary);
      out.push({
        vendor,
        // Read straight off the instance — the host never hardcodes a capability
        // matrix, so a future adapter version flipping `stream` moves the UI with it.
        capabilities: adapter.capabilities,
        binary,
        binaryPath,
        installed: binaryPath !== null,
      });
    }
    return out;
  };

  const asAccounts = (raw: unknown): Account[] => {
    if (!Array.isArray(raw)) return [];
    const out: Account[] = [];
    for (const item of raw) {
      if (typeof item !== 'object' || item === null) continue;
      const rec = item as Record<string, unknown>;
      if (typeof rec.vendor !== 'string') continue;
      const account: Account = { vendor: rec.vendor };
      if (typeof rec.label === 'string') account.label = rec.label;
      if (typeof rec.configDir === 'string') account.configDir = rec.configDir;
      out.push(account);
    }
    return out;
  };

  const requireState = (): SidecarState => {
    if (state === undefined) {
      throw new RpcError(RPC_INVALID_PARAMS, 'mesh.hello has not been completed yet');
    }
    return state;
  };

  // ── lifecycle methods (DESIGN §5-3, §5-4) ──────────────────────────────────
  rpc.on('mesh.hello', (params: HelloParams | undefined) => {
    const p = params ?? {};
    if (p.protocol !== PROTOCOL) {
      // Never guess. The host kills the sidecar and surfaces a hard error.
      throw new RpcError(RPC_PROTOCOL_MISMATCH, 'sidecar/host protocol mismatch', {
        expected: PROTOCOL,
        received: p.protocol ?? null,
      });
    }

    const accounts = asAccounts(p.accounts);
    const appInstanceId = typeof p.appInstanceId === 'string' ? p.appInstanceId : 'unknown-instance';
    // Built here, not at module load: the probe stamps `GILDE_APP_INSTANCE` on its
    // child, and the host only tells us that id in the handshake.
    usageProbe = makeRegisteredGetUsage(supervisedRpc, appInstanceId);
    const built = buildFleet({
      rpc: supervisedRpc,
      appInstanceId,
      ...(p.binaries !== undefined ? { binaries: p.binaries } : {}),
      accounts,
      ...(p.defaults !== undefined ? { defaults: p.defaults } : {}),
      // `getUsage` (the registered duplex quota probe, T18/`usage.ts`) is injected
      // here once that module exists; the seam is claude-only by design (§5-7).
      ...(usageProbe !== undefined ? { getUsage: usageProbe } : {}),
    });

    state = {
      appInstanceId,
      bootId: typeof p.bootId === 'string' ? p.bootId : '',
      hostVersion: typeof p.hostVersion === 'string' ? p.hostVersion : '',
      defaults: p.defaults ?? {},
      accounts,
      built,
      ready: true,
    };

    return {
      protocol: PROTOCOL,
      sidecarVersion: __GILDE_SIDECAR_VERSION__,
      node: process.version,
      packages: __GILDE_MESH_PACKAGES__,
      adapters: describeAdapters(state),
    };
  });

  rpc.on('mesh.ping', (params: { nonce?: unknown } | undefined) => ({
    nonce: params?.nonce ?? null,
    ts: Date.now(),
  }));

  rpc.on('mesh.adapters', () => describeAdapters(requireState()));

  rpc.on(
    'mesh.configure',
    (params: { binaries?: HelloParams['binaries']; accounts?: unknown[] } | undefined) => {
      const s = requireState();
      const accounts = params?.accounts === undefined ? s.accounts : asAccounts(params.accounts);
      const binaries =
        params?.binaries === undefined
          ? {
              claude: s.built.binaries.claude,
              codex: s.built.binaries.codex,
              cursorAgent: s.built.binaries.cursorAgent,
              agy: s.built.binaries.agy,
            }
          : params.binaries;

      // Reconfiguration REBUILDS: no adapter is ever left half-configured.
      s.built = buildFleet({
        rpc: supervisedRpc,
        appInstanceId: s.appInstanceId,
        binaries,
        accounts,
        defaults: s.defaults,
        ...(usageProbe !== undefined ? { getUsage: usageProbe } : {}),
      });
      s.accounts = accounts;
      return { ok: true };
    },
  );

  rpc.on('mesh.shutdown', (params: { graceMs?: number } | undefined) => {
    const graceMs = typeof params?.graceMs === 'number' && params.graceMs > 0 ? params.graceMs : 0;
    // Deliberately NOT unref'd: the shutdown must happen even if the channel is
    // the only thing keeping the loop alive. `hostGone: false` — the host asked
    // for this, so it is alive and its reaper owns the SIGKILL escalation.
    setTimeout(() => void shutdown(0, 'mesh.shutdown', SIGNAL_EXIT_BUDGET_MS, false), graceMs);
    return { ok: true };
  });

  // ── the run surface (DESIGN §5-4) ──────────────────────────────────────────
  // `mesh.authStatus` / `availableModels` / `quota` / `run` / `stream` / `resume` /
  // `cancel`, and `fleet.route` — the one method allowed to choose a
  // vendor. They are handed `requireState` rather than a captured state because
  // `mesh.configure` REBUILDS the fleet, and a captured build would go stale.
  registerMeshMethods(rpc, requireState);
  registerFleetMethods(rpc, requireState);

  // ── team mode, read-only (team contract v2 §8.2) ──────────────────────────
  // `cco.team.list` / `cco.team.get` and the `cco.team.snapshot` notification: an
  // OBSERVER of Claude Code Orchestrator dispatch hosts. It needs no fleet and no
  // `mesh.hello`, spawns nothing, and connects to nothing until the first
  // `cco.team.list` hands it the folders Gilde has open.
  registerCcoMethods(rpc, { log: (level, msg) => log(level, msg) });

  // ── self-test hooks ────────────────────────────────────────────────────────
  // `test/stub-host.mjs` drives the exec seam directly through these. They exist
  // only when GILDE_SIDECAR_SELFTEST=1, so the production channel never exposes a
  // way to spawn an arbitrary command.
  if (process.env.GILDE_SIDECAR_SELFTEST === '1') {
    const { makeRegisteredExec, makeRegisteredLineStream, runCtx } = await import('./exec-registered');
    const { registeredDuplexSpawn } = await import('./duplex');
    const selfTestInstance = process.env.GILDE_APP_INSTANCE ?? 'selftest-instance';

    interface DevExecParams {
      cmd: string;
      args?: string[];
      cwd?: string;
      env?: Record<string, string | undefined>;
      timeoutMs?: number;
      stdin?: string;
      runId?: string;
      vendor?: string;
    }

    rpc.on('dev.exec', async (params: DevExecParams) => {
      const exec = makeRegisteredExec(rpc, selfTestInstance);
      const req = {
        cmd: params.cmd,
        args: params.args ?? [],
        ...(params.cwd !== undefined ? { cwd: params.cwd } : {}),
        ...(params.env !== undefined ? { env: params.env } : {}),
        ...(params.timeoutMs !== undefined ? { timeoutMs: params.timeoutMs } : {}),
        ...(params.stdin !== undefined ? { stdin: params.stdin } : {}),
      };
      const ctx = { runId: params.runId ?? 'selftest', vendor: params.vendor ?? 'claude' };
      return runCtx.run(ctx, () => exec(req));
    });

    rpc.on('dev.lineStream', async (params: DevExecParams) => {
      const lineStream = makeRegisteredLineStream(rpc, selfTestInstance);
      const req = {
        cmd: params.cmd,
        args: params.args ?? [],
        ...(params.cwd !== undefined ? { cwd: params.cwd } : {}),
        ...(params.env !== undefined ? { env: params.env } : {}),
        ...(params.timeoutMs !== undefined ? { timeoutMs: params.timeoutMs } : {}),
      };
      const ctx = { runId: params.runId ?? 'selftest', vendor: params.vendor ?? 'claude' };
      return runCtx.run(ctx, async () => {
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
      const handle = await runCtx.run({ runId: params.runId ?? 'selftest', vendor: params.vendor ?? 'claude' }, () =>
        registeredDuplexSpawn(rpc, selfTestInstance, {
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

    rpc.on('dev.stats', () => ({ ...rpc.stats, tmpdir: os.tmpdir() }));

    // Prove the hardening: a dependency's console.log and even a direct
    // process.stdout.write must land on stderr, never on the protocol channel.
    rpc.on('dev.pollute', (params: { text?: string }) => {
      const text = params?.text ?? 'POLLUTION';
      console.log(text, { via: 'console.log' });
      console.error(text, { via: 'console.error' });
      process.stdout.write(`${text} via process.stdout.write\n`);
      return { ok: true };
    });

    // Prove the backpressure contract: with the host not reading, `mesh.event`
    // notifications are discarded at the queue cap while responses still arrive.
    rpc.on('dev.floodEvents', (params: { count?: number }) => {
      const count = typeof params?.count === 'number' ? params.count : 1000;
      for (let i = 0; i < count; i += 1) {
        rpc.notify('mesh.event', {
          runId: 'flood',
          event: { type: 'text', text: `${i}:${'x'.repeat(64)}` },
        });
      }
      return { sent: count, dropped: rpc.stats.droppedNotifications };
    });
  }

  // ── process-level guards ───────────────────────────────────────────────────
  //
  // The rule stays what the header says — a recoverable error must not kill a
  // sidecar mid-run — with the one exception INT-C carved out: a failure of our
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
    void shutdown(0, `stdio pipe closed (${why})`, CHANNEL_EXIT_BUDGET_MS, true);
  };
  if (stdioBroken) onStdioBroken('detected before the guards were installed');

  // EXIT PATH 2 — a signal. All three are the same request; the hard floor inside
  // `shutdown` is what makes them impossible to ignore.
  for (const signal of ['SIGTERM', 'SIGINT', 'SIGHUP'] as const) {
    process.on(signal, () => void shutdown(0, signal, SIGNAL_EXIT_BUDGET_MS, false));
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
      void shutdown(0, `parent died (${why})`, CHANNEL_EXIT_BUDGET_MS, true);
    },
  });

  rpc.start();
  writeStderrLine('[sidecar:info]', [
    `gilde-code-sidecar ${__GILDE_SIDECAR_VERSION__} on ${process.version}, protocol ${PROTOCOL}`,
  ]);
}

main().catch((err: unknown) => {
  writeStderrLine('[sidecar:fatal]', [err]);
  process.exit(70); // EX_SOFTWARE
});
