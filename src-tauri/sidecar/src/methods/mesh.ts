/**
 * methods/mesh.ts — the lifecycle half of protocol 2 (contract §4.5):
 * `mesh.hello`, `mesh.configure`, `mesh.ping`, `mesh.shutdown`.
 *
 *   - `mesh.hello {protocol: 2, buildId, appInstanceId, providers}` →
 *     `{protocol, buildId, adapters}`. A peer that does not speak protocol 2 gets
 *     `-32002` (the host stops it). A peer from another BUILD gets `-32005` and
 *     then this process exits with code 78 (`EXIT_BUILD_MISMATCH`): the host
 *     reports `VERSION_MISMATCH` ("Restart Gilde to finish updating.") instead of
 *     talking to a sidecar whose types may differ from its own (§4.7).
 *   - `mesh.configure {binaries, accounts, providers}` → `{}`: the resolved
 *     absolute vendor binaries, the accounts (env KEY names and config dirs only —
 *     never a value from the user's environment, never a secret), and the
 *     providers. Reconfiguration rebuilds the fleet. Until the first configure no
 *     vendor is installed and no account is known, so `turn.*` / `probe.*` answer
 *     `-32006`.
 *   - `providers.orchestrator` (in both) registers or removes the `orch.*`
 *     methods (methods/orch.ts).
 *
 * Every params object is read strictly (methods/wire.ts): an unknown field, a
 * missing one or a value outside its rule is `-32602` with `data.field`.
 */

import * as fs from 'node:fs';

import type { ClaudeAdapterOptions } from '@toragonite/agent-mesh-claude';

import { buildFleet, VENDORS } from '../fleet';
import type { SidecarState } from '../fleet';
import type { AdapterInfo } from '../gen/AdapterInfo';
import type { MeshHelloResult } from '../gen/MeshHelloResult';
import { RpcError } from '../rpc';
import type { Rpc } from '../rpc';
import type { OrchestratorGate } from './orch';
import {
  PROTOCOL,
  RPC_BUILD_MISMATCH,
  RPC_NOT_READY,
  RPC_PROTOCOL_MISMATCH,
  configureParamsOf,
  exactObject,
  helloParamsOf,
  isRecord,
  invalid,
} from './wire';

export interface MeshLifecycleOptions {
  /** The raw channel the methods are registered on. */
  channel: Rpc;
  /** The supervised channel the fleet's spawn seams use (runs.ts `trackSpawns`). */
  seamRpc: Rpc;
  /** This build's id (`__GILDE_BUILD_ID__`). */
  buildId: string;
  /** Builds the registered claude quota probe for an app instance (usage.ts). */
  makeGetUsage: (appInstanceId: string) => ClaudeAdapterOptions['getUsage'];
  orch: OrchestratorGate;
  /** The hello carried another build id: answer, then exit 78. */
  onBuildMismatch: (received: string) => void;
  /** `mesh.shutdown` was asked for, with this grace. */
  onShutdown: (graceMs: number) => void;
  log: (level: 'debug' | 'info' | 'warn' | 'error', msg: string, fields?: Record<string, string | number | boolean>) => void;
}

function isExecutable(p: string): boolean {
  try {
    const st = fs.statSync(p);
    if (!st.isFile()) return false;
    fs.accessSync(p, fs.constants.X_OK);
    return true;
  } catch {
    return false;
  }
}

/** The roster `mesh.hello` reports: capabilities read off the live adapters, never a hardcoded matrix. */
export function describeAdapters(state: SidecarState): AdapterInfo[] {
  const out: AdapterInfo[] = [];
  for (const vendor of VENDORS) {
    const adapter = state.built.adapters.get(vendor);
    if (adapter === undefined) continue;
    const caps = adapter.capabilities;
    out.push({
      vendor,
      resume: caps.resume === true,
      stream: caps.stream === true,
      quota: caps.quota === true,
      installed: state.built.configured.has(vendor) && isExecutable(state.built.binaries[vendor]),
    });
  }
  return out;
}

/** Register the lifecycle methods; returns the state accessor the other methods read. */
export function registerMeshMethods(opts: MeshLifecycleOptions): () => SidecarState | undefined {
  let state: SidecarState | undefined;

  opts.channel.on('mesh.hello', (raw: unknown): MeshHelloResult => {
    // The protocol first and alone: a protocol-1 peer gets ITS error, not "unknown field".
    const protocol = isRecord(raw) ? raw.protocol : undefined;
    if (protocol !== PROTOCOL) {
      throw new RpcError(RPC_PROTOCOL_MISMATCH, 'sidecar/host protocol mismatch', {
        expected: PROTOCOL,
        received: typeof protocol === 'number' ? protocol : null,
      });
    }
    const p = helloParamsOf(raw);
    if (p.buildId !== opts.buildId) {
      opts.log('error', 'mesh.hello from another build; exiting 78', { expected: opts.buildId, received: p.buildId });
      opts.onBuildMismatch(p.buildId);
      throw new RpcError(RPC_BUILD_MISMATCH, 'sidecar/host build mismatch', {
        expected: opts.buildId,
        received: p.buildId,
      });
    }
    state = {
      appInstanceId: p.appInstanceId,
      built: buildFleet({
        rpc: opts.seamRpc,
        appInstanceId: p.appInstanceId,
        binaries: {},
        getUsage: opts.makeGetUsage(p.appInstanceId),
      }),
      accounts: new Map(),
      configured: false,
    };
    opts.orch.set(p.providers.orchestrator);
    return { protocol: PROTOCOL, buildId: opts.buildId, adapters: describeAdapters(state) };
  });

  opts.channel.on('mesh.configure', (raw: unknown): Record<string, never> => {
    const c = configureParamsOf(raw);
    if (state === undefined) {
      throw new RpcError(RPC_NOT_READY, 'mesh.hello has not been completed yet', { name: 'NotReady' });
    }
    // Reconfiguration REBUILDS: no adapter is ever left half-configured.
    state.built = buildFleet({
      rpc: opts.seamRpc,
      appInstanceId: state.appInstanceId,
      binaries: c.binaries,
      getUsage: opts.makeGetUsage(state.appInstanceId),
    });
    state.accounts = new Map(c.accounts.map((a) => [a.accountId, a]));
    state.configured = true;
    opts.orch.set(c.providers.orchestrator);
    opts.log('info', 'fleet configured', {
      binaries: Object.keys(c.binaries).length,
      accounts: c.accounts.length,
      orchestrator: c.providers.orchestrator,
    });
    return {};
  });

  opts.channel.on('mesh.ping', (raw: unknown) => {
    const p = raw === undefined || raw === null ? {} : exactObject(raw, '$', ['nonce'], []);
    const nonce = p.nonce;
    if (nonce !== undefined && (typeof nonce !== 'number' || !Number.isSafeInteger(nonce))) {
      throw invalid('nonce', 'type', 'nonce must be an integer');
    }
    return { nonce: nonce ?? null, ts: Date.now() };
  });

  opts.channel.on('mesh.shutdown', (raw: unknown) => {
    const p = raw === undefined || raw === null ? {} : exactObject(raw, '$', ['graceMs'], []);
    const grace = p.graceMs;
    if (grace !== undefined && (typeof grace !== 'number' || !Number.isFinite(grace) || grace < 0 || grace > 60_000)) {
      throw invalid('graceMs', 'range', 'graceMs must be within 0..=60000');
    }
    opts.onShutdown(typeof grace === 'number' ? grace : 0);
    return { ok: true };
  });

  return () => state;
}
