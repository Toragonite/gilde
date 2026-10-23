/**
 * methods/fleet.ts — `fleet.route`, the ONLY routing surface (DESIGN §5-4).
 *
 * NOT PART OF PROTOCOL 2 (contract §2.2.3: "KEEP, routing unused in v1"): this
 * module is kept for v1.x and is not registered on the channel (`index.ts`), so a
 * `fleet.route` request is `-32601 unknown method`. It still type-checks against
 * the protocol-2 fleet: the agent-mesh `Fleet` is assembled from the current
 * adapters on each call.
 *
 * Routing is deliberately isolated in one method. `mesh.run`, `mesh.stream` and
 * `mesh.resume` all demand an explicit vendor and will never pick one for you:
 * a dispatch that silently re-targets itself would send a user's prompt to a
 * different agent — and a different worktree — than the board says it did.
 *
 * So the host asks first (`fleet.route`), shows the answer, and then runs the
 * vendor it was given. `Fleet.route` itself is agent-mesh's, unmodified: it reads
 * quota and model catalogs through the Adapter contract and applies the caller's
 * `RoutePolicy`. This module only validates the params, pins the probes it triggers
 * to the introspection run, and flattens the decision to `{vendor, note?}` — the
 * `Adapter` instance inside a `RouteDecision` is not serialisable and must not
 * cross the channel.
 *
 * `note` matters: `Fleet.route` sets it when EVERY candidate was gated (over the
 * quota ceiling or rate-limited) and it fell back best-effort. Dropping it would
 * turn "everything is exhausted, this is a guess" into a confident answer.
 */

import { Fleet } from '@toragonite/agent-mesh';
import type { RoutePolicy, RunTask } from '@toragonite/agent-mesh';
import * as fs from 'node:fs';
import * as path from 'node:path';

import { runCtx } from '../exec-registered';
import type { SidecarState } from '../fleet';
import { RpcError, RPC_INVALID_PARAMS } from '../rpc';
import type { Rpc } from '../rpc';
import { classifyError } from './turns';
import { RPC_ADAPTER_ERROR } from './wire';

/** Strategies `Fleet.route` understands; anything else is a caller mistake. */
const STRATEGIES = new Set(['prefer-order', 'most-quota', 'lowest-latency']);
const LATENCY_CLASSES = new Set(['fast', 'slow']);

function isRecord(x: unknown): x is Record<string, unknown> {
  return typeof x === 'object' && x !== null && !Array.isArray(x);
}

function invalid(message: string, data?: unknown): RpcError {
  return new RpcError(RPC_INVALID_PARAMS, message, data);
}

/**
 * The routing view of a `RunTask`.
 *
 * Routing never spawns the task itself — it reads quota and model catalogs — so
 * `cwd` is optional here, unlike in `mesh.run`. When one IS supplied it is held to
 * the same rule (absolute, existing) so a task cannot pass routing and then fail
 * validation on the very next call with the same object.
 */
function normalizeRouteTask(raw: unknown): RunTask {
  if (!isRecord(raw)) throw invalid('task must be an object');
  if (typeof raw.prompt !== 'string') throw invalid('task.prompt must be a string');

  const task: RunTask = { prompt: raw.prompt };

  if (raw.cwd !== undefined && raw.cwd !== null) {
    if (typeof raw.cwd !== 'string' || !path.isAbsolute(raw.cwd)) {
      throw invalid('task.cwd must be an absolute path');
    }
    const resolved = path.resolve(raw.cwd);
    let isDir = false;
    try {
      isDir = fs.statSync(resolved).isDirectory();
    } catch {
      isDir = false;
    }
    if (!isDir) throw invalid('task.cwd does not exist', { cwd: resolved });
    task.cwd = resolved;
  }

  if (typeof raw.model === 'string' && raw.model.trim() !== '') task.model = raw.model;
  if (typeof raw.timeoutMs === 'number' && raw.timeoutMs > 0) task.timeoutMs = raw.timeoutMs;

  // The account is what makes routing account-aware: `Fleet.route` passes it to
  // every `quota()` / `availableModels()` probe it runs.
  if (raw.account !== undefined && raw.account !== null) {
    if (!isRecord(raw.account)) throw invalid('task.account must be an object');
    const vendor = raw.account.vendor;
    if (typeof vendor !== 'string' || vendor.trim() === '') {
      throw invalid('task.account.vendor must be a non-empty string');
    }
    task.account = {
      vendor,
      ...(typeof raw.account.label === 'string' ? { label: raw.account.label } : {}),
      ...(typeof raw.account.configDir === 'string' ? { configDir: raw.account.configDir } : {}),
    };
  }
  return task;
}

/** Validate a `RoutePolicy`. An unknown strategy is rejected, never silently defaulted. */
function normalizePolicy(raw: unknown): RoutePolicy {
  if (raw === undefined || raw === null) return {};
  if (!isRecord(raw)) throw invalid('policy must be an object');
  const policy: RoutePolicy = {};

  if (raw.prefer !== undefined && raw.prefer !== null) {
    if (!Array.isArray(raw.prefer) || raw.prefer.some((v) => typeof v !== 'string')) {
      throw invalid('policy.prefer must be an array of vendor strings');
    }
    policy.prefer = raw.prefer as string[];
  }
  if (raw.quotaCeiling !== undefined && raw.quotaCeiling !== null) {
    const ceiling = raw.quotaCeiling;
    if (typeof ceiling !== 'number' || !Number.isFinite(ceiling) || ceiling < 0 || ceiling > 100) {
      throw invalid('policy.quotaCeiling must be a number in 0..100');
    }
    policy.quotaCeiling = ceiling;
  }
  if (raw.strategy !== undefined && raw.strategy !== null) {
    if (typeof raw.strategy !== 'string' || !STRATEGIES.has(raw.strategy)) {
      throw invalid('policy.strategy must be one of prefer-order, most-quota, lowest-latency');
    }
    policy.strategy = raw.strategy as RoutePolicy['strategy'];
  }
  if (raw.latency !== undefined && raw.latency !== null) {
    if (typeof raw.latency !== 'string' || !LATENCY_CLASSES.has(raw.latency)) {
      throw invalid("policy.latency must be 'fast' or 'slow'");
    }
    policy.latency = raw.latency as RoutePolicy['latency'];
  }
  return policy;
}

/** Register `fleet.route`. */
export function registerFleetMethods(rpc: Rpc, requireState: () => SidecarState): void {
  rpc.on('fleet.route', async (params: unknown) => {
    const state = requireState();
    const p = isRecord(params) ? params : {};
    const task = normalizeRouteTask(p.task);
    const policy = normalizePolicy(p.policy);

    try {
      // `Fleet.route` probes quota and model catalogs; those children belong to the
      // introspection run, not to whatever dispatch is about to be created.
      const fleet = new Fleet();
      for (const adapter of state.built.adapters.values()) fleet.register(adapter);
      const decision = await runCtx.run({ turnId: null, vendor: null, kind: 'probe' }, () =>
        fleet.route(task, policy),
      );
      return decision.note === undefined
        ? { vendor: decision.adapter.vendor }
        : { vendor: decision.adapter.vendor, note: decision.note };
    } catch (err) {
      // NoRouteError arrives here as `data.name = 'NoRouteError'`.
      const name = err instanceof Error ? err.name : 'UnknownError';
      throw new RpcError(RPC_ADAPTER_ERROR, err instanceof Error ? err.message : String(err), {
        name,
        class: classifyError(err),
      });
    }
  });
}
