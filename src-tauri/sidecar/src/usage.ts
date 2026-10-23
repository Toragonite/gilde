/**
 * usage.ts — the claude live-usage probe, rebuilt on the REGISTERED duplex spawner.
 *
 * `ClaudeAdapter.quota()` calls its `getUsage` seam, whose default (`getLiveUsage`)
 * spawns `claude` itself — outside our supervision, so a probe that hangs shows up
 * in Activity Monitor and nowhere in ours. A cold `claude` probe has been measured
 * at over 20 seconds, which is exactly the kind of unexplained process the Process
 * Monitor exists to explain (DESIGN §5-7).
 *
 * So the seam is replaced with this function, which speaks the SAME control
 * protocol over `duplex.ts`'s registered spawner:
 *
 *     claude -p --input-format stream-json --output-format stream-json --verbose
 *     stdin  ← {"type":"control_request","request_id":"u1",
 *               "request":{"subtype":"get_usage"}}
 *     stdout → …{"type":"control_response","response":{…}}…
 *
 * Two rules keep this honest:
 *
 *   - **The parsing is not reimplemented.** The payload is handed to the adapter's
 *     own exported pure parser, {@link parseLiveUsage}. Upstream calls the
 *     `get_usage` shape experimental; re-deriving it here would be a second,
 *     silently diverging reader of an unstable contract.
 *   - **The cwd is neutral** (`os.tmpdir()` unless the caller names one). A project
 *     cwd would make `claude` load that project's `.mcp.json` and start its MCP
 *     servers on every quota poll.
 *
 * The contract of {@link getLiveUsage} is preserved exactly, because the adapter's
 * `getUsage` seam is typed as `typeof getLiveUsage`: this NEVER rejects for an
 * environmental failure (missing binary, timeout, garbage, killed process) — every
 * such path resolves a {@link LiveUsage} with `error` set. The only throw is a
 * synchronous `TypeError` for an invalid `timeoutMs`, a programmer error.
 *
 * `available:false` with no `error` stays what upstream means by it: a valid "this
 * login exposes no claude.ai plan limits" reading, not a failure.
 */

import * as os from 'node:os';

import { parseLiveUsage } from '@toragonite/agent-mesh-claude';
import type { LiveUsage, LiveUsageOptions, getLiveUsage } from '@toragonite/agent-mesh-claude';

import { registeredDuplexSpawn } from './duplex';
import type { DuplexHandle } from './duplex';
import type { Rpc } from './rpc';

/**
 * Wall-clock cap when the caller names none.
 *
 * Upstream defaults to 20s; `ClaudeAdapter` already overrides that to 30s because a
 * cold CLI boot was observed past 20s, yielding a permanent `null` quota. This
 * default matches the adapter's, so the probe behaves the same whether the adapter
 * passes `quotaTimeoutMs` or not.
 */
export const DEFAULT_USAGE_TIMEOUT_MS = 30_000;

/** How long to wait for the CLI's first output line before writing the request anyway. */
const SEND_FALLBACK_MS = 800;

/** The exact argv of the control-protocol probe. */
const PROBE_ARGS = [
  '-p',
  '--input-format',
  'stream-json',
  '--output-format',
  'stream-json',
  '--verbose',
] as const;

/** The one control request this probe issues. */
const CONTROL_REQUEST = JSON.stringify({
  type: 'control_request',
  request_id: 'u1',
  request: { subtype: 'get_usage' },
});

function isRecord(value: unknown): value is Record<string, unknown> {
  return typeof value === 'object' && value !== null && !Array.isArray(value);
}

/**
 * Validate an optional `timeoutMs` the way upstream does and return the effective
 * value.
 *
 * @throws {TypeError} when present but not a finite number greater than 0.
 */
function resolveTimeout(timeoutMs: number | undefined): number {
  if (timeoutMs === undefined) return DEFAULT_USAGE_TIMEOUT_MS;
  if (typeof timeoutMs !== 'number' || !Number.isFinite(timeoutMs) || timeoutMs <= 0) {
    throw new TypeError(`timeoutMs must be a finite number > 0, received ${String(timeoutMs)}`);
  }
  return timeoutMs;
}

/** Every failure path funnels through here, so the shape is identical and honest. */
function fail(error: string): LiveUsage {
  return {
    available: false,
    subscriptionType: null,
    windows: [],
    sessionCostUsd: null,
    extraUsage: null,
    fetchedAt: Date.now(),
    raw: null,
    error,
  };
}

/**
 * The child env for one probe.
 *
 * `duplex.ts` treats a provided `env` as the WHOLE child environment (the
 * `ExecRequest.env` semantics of §5-6), so the merge is done here and passed
 * complete. `CLAUDE_CONFIG_DIR` is forced only for an EXPLICIT `configDir`; an
 * ambient probe inherits the ambient environment, because forcing `~/.claude` onto
 * an ambient login makes the CLI behave like a fresh install and report nothing.
 */
function composeProbeEnv(options: LiveUsageOptions): Record<string, string | undefined> {
  const base: Record<string, string | undefined> = { ...process.env, ...options.env };
  if (options.configDir !== undefined) base.CLAUDE_CONFIG_DIR = options.configDir;
  return base;
}

/** Run one probe to a settled {@link LiveUsage}. Never rejects. */
async function probe(
  rpc: Rpc,
  appInstanceId: string,
  options: LiveUsageOptions,
  timeoutMs: number,
): Promise<LiveUsage> {
  const claudePath = options.claudePath ?? 'claude';
  // A neutral cwd is REQUIRED so no project .mcp.json is loaded (see the header).
  const cwd = options.cwd ?? os.tmpdir();

  let handle: DuplexHandle;
  try {
    handle = await registeredDuplexSpawn(rpc, appInstanceId, {
      cmd: claudePath,
      args: [...PROBE_ARGS],
      cwd,
      env: composeProbeEnv(options),
      // A backstop only: the settle path below owns the deadline and kills the
      // group itself. This makes the child bounded even if this event loop stalls.
      timeoutMs,
    });
  } catch (err) {
    return fail(`spawn failed: ${err instanceof Error ? err.message : String(err)}`);
  }

  return new Promise<LiveUsage>((resolve) => {
    let settled = false;
    let sent = false;
    let buffered = '';
    let stderrSeen = false;

    // Single settle path: clears every timer and ends the group exactly once —
    // with SIGTERM: the probe child is a vendor CLI, and a vendor CLI is never
    // sent SIGKILL (contract §9.8, C5). Closing its stdin first lets a CLI that
    // already answered leave on its own.
    const done = (result: LiveUsage): void => {
      if (settled) return;
      settled = true;
      clearTimeout(deadline);
      clearTimeout(sendFallback);
      try {
        handle.stdin.end();
      } catch {
        /* already closed */
      }
      handle.kill('SIGTERM');
      resolve(result);
    };

    const deadline = setTimeout(() => done(fail('timeout')), timeoutMs);
    deadline.unref?.();

    const sendRequest = (): void => {
      if (sent || settled) return;
      sent = true;
      try {
        handle.stdin.write(`${CONTROL_REQUEST}\n`);
      } catch {
        // stdin already closed; the close handler settles with the real outcome.
      }
    };
    // Write once the CLI is up: on its first output line, else after a short delay.
    const sendFallback = setTimeout(sendRequest, SEND_FALLBACK_MS);
    sendFallback.unref?.();

    const handleLine = (line: string): void => {
      if (line.trim().length === 0) return;
      sendRequest();
      let msg: unknown;
      try {
        msg = JSON.parse(line);
      } catch {
        return; // stray prose / log line from the CLI — skip
      }
      if (!isRecord(msg) || msg.type !== 'control_response') return;
      clearTimeout(sendFallback);
      const response = isRecord(msg.response) ? msg.response : {};
      if (response.subtype !== 'success') {
        done(fail(`control_response ${String(response.subtype ?? 'error')}`));
        return;
      }
      const data = response.response;
      // The adapter's own pure parser — never a second implementation of it.
      done({ ...parseLiveUsage(data), fetchedAt: Date.now(), raw: data });
    };

    // `duplex.ts` already set utf8 on both pipes (streaming-safe decoding).
    handle.stdout.on('data', (chunk: string) => {
      buffered += chunk;
      for (;;) {
        const nl = buffered.indexOf('\n');
        if (nl === -1) break;
        const line = buffered.slice(0, nl);
        buffered = buffered.slice(nl + 1);
        handleLine(line);
      }
    });
    handle.stderr.on('data', () => {
      stderrSeen = true;
    });

    void handle.closed.then((exit) => {
      if (exit.timedOut) {
        done(fail('timeout'));
        return;
      }
      const tail = handle.stderrTail().trim();
      done(fail(tail && stderrSeen ? `claude exited: ${tail.slice(0, 200)}` : 'no response'));
    });
  });
}

/**
 * Build the `ClaudeAdapterOptions.getUsage` seam bound to this channel.
 *
 * The returned function is signature-compatible with {@link getLiveUsage} — that
 * is the adapter's declared seam type — so it drops in wherever the vendor default
 * would have gone, and every child it creates is registered with the host first.
 */
export function makeRegisteredGetUsage(rpc: Rpc, appInstanceId: string): typeof getLiveUsage {
  return function registeredGetUsage(options: LiveUsageOptions = {}): Promise<LiveUsage> {
    const timeoutMs = resolveTimeout(options.timeoutMs);
    return probe(rpc, appInstanceId, options, timeoutMs);
  };
}
