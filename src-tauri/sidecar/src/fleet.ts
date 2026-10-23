/**
 * fleet.ts — construct the four vendor adapters (contract §4.5, protocol 2).
 *
 * Nothing here forks or reimplements a vendor: every adapter is the published
 * class, configured through its PUBLIC constructor seams (DESIGN §0 rule 1,
 * §5-6). We inject:
 *
 *   - `exec`         the registered run-to-completion spawner (all four),
 *   - `lineStream`   the registered streaming spawner (all four),
 *   - `getUsage`     the registered duplex quota probe (claude only; the CLI's
 *                    control-protocol probe needs an open stdin, which
 *                    `ExecRequest.stdin` cannot express — §5-7),
 *   - `spawn`        the registered spawner for codex's live model listing
 *                    (`codex app-server`; registry kind `probe`),
 *   - `readTextFile` the never-open guard (methods/never-open.ts): no adapter ever
 *                    opens `.claude.json`, `.credentials.json`, `auth.json` or any
 *                    other credential-bearing file (contract §9.7, C6),
 *   - `readKeychain` cursor's keychain read, refused: the keychain is never read
 *                    and `/usr/bin/security` is never spawned (C6),
 *   - `fetchFn`      codex/cursor's usage endpoint, disabled: it needs a token
 *                    Gilde never reads, so no request is ever sent.
 *
 * With that, every vendor launch the sidecar can reach goes through a registered
 * seam; `scripts/check-seam.mjs` holds the bundle to it. The host's allowlist then
 * refuses any argv[0] that is not a configured vendor binary (§4.5, §9.3).
 *
 * Two more deliberate choices:
 *   - cursor gets `trust: true`. Choosing to run a coding agent in a folder IS the
 *     trust decision; without it every turn stalls ~21s on an interactive gate.
 *   - claude gets NO `gateway`, and no adapter gets a default account: every call
 *     names its account, so a missing one can never silently fall back to another
 *     login.
 *
 * Attribution: the seams read the turn id, vendor and registry kind from
 * `runCtx` (exec-registered.ts), which the caller sets around each adapter call —
 * `turn` for `turn.start`, `probe` for `probe.*`.
 */

import type { Adapter } from '@toragonite/agent-mesh';
import { ClaudeAdapter } from '@toragonite/agent-mesh-claude';
import type { ClaudeAdapterOptions, ExecFn, LineStreamFn } from '@toragonite/agent-mesh-claude';
import { CodexAdapter } from '@toragonite/agent-mesh-codex';
import type { SpawnFn } from '@toragonite/agent-mesh-codex';
import { CursorAdapter } from '@toragonite/agent-mesh-cursor';
import { GeminiAdapter } from '@toragonite/agent-mesh-gemini';

import type { AccountEnv } from './gen/AccountEnv';
import type { Vendor } from './gen/Vendor';
import { guardedReadTextFile, noNetworkFetch, refusedKeychain } from './methods/never-open';
import { makeRegisteredExec, makeRegisteredLineStream, makeRegisteredSpawn } from './exec-registered';
import type { Rpc } from './rpc';

/** The four vendors of this build, in roster order (contract vendor names). */
export const VENDORS: readonly Vendor[] = ['claude', 'codex', 'cursor', 'agy'];

/** Host-resolved absolute paths for the vendor CLIs (`mesh.configure.binaries`). */
export type BinaryMap = Partial<Record<Vendor, string>>;

/**
 * Command names used while the host has not configured a binary. A spawn of one
 * of these is refused by the host's allowlist (only configured absolute paths
 * pass), which is exactly "not installed".
 */
export const UNCONFIGURED_BINARIES: Readonly<Record<Vendor, string>> = {
  claude: 'claude',
  codex: 'codex',
  cursor: 'cursor-agent',
  agy: 'agy',
};

export interface BuildConfig {
  rpc: Rpc;
  appInstanceId: string;
  binaries: BinaryMap;
  /**
   * The registered claude quota probe (`usage.ts`). Injected rather than
   * constructed here so this module never spawns: it is a `ClaudeAdapterOptions`
   * seam, and leaving it out would silently fall back to the vendor's own
   * unregistered probe.
   */
  getUsage?: ClaudeAdapterOptions['getUsage'];
}

export interface BuiltFleet {
  /** One instance per vendor (Cursor memoizes its model catalog on the instance). */
  adapters: Map<Vendor, Adapter>;
  /** The binaries these adapters were built with. */
  binaries: Record<Vendor, string>;
  /** Which vendors the host configured a binary for (the others are not installed). */
  configured: ReadonlySet<Vendor>;
}

/**
 * Everything a method handler needs. Held in one mutable box because
 * `mesh.configure` REBUILDS the fleet rather than mutating adapters — a handler
 * must always read the current build, never a captured one.
 */
export interface SidecarState {
  appInstanceId: string;
  built: BuiltFleet;
  /** `mesh.configure.accounts`, by account id. Empty until the first configure. */
  accounts: Map<string, AccountEnv>;
  /** Set once the host has sent `mesh.configure`. */
  configured: boolean;
}

interface Seams {
  exec: ExecFn;
  lineStream: LineStreamFn;
  spawn: SpawnFn;
}

function makeAdapters(
  seams: Seams,
  binaries: Record<Vendor, string>,
  getUsage: ClaudeAdapterOptions['getUsage'],
): Map<Vendor, Adapter> {
  // One shared seam per kind; the per-call attribution comes from `runCtx`.
  const exec = (_vendor: string): ExecFn => seams.exec;
  const lineStream = (_vendor: string): LineStreamFn => seams.lineStream;
  const spawnSeam = (_vendor: string): SpawnFn => seams.spawn;

  const claudeOpts: ClaudeAdapterOptions = {
    exec: exec('claude'),
    lineStream: lineStream('claude'),
    binary: binaries.claude,
    readTextFile: guardedReadTextFile,
    // No `gateway`: steer() stays an honest error (DESIGN §5-7).
  };
  if (getUsage !== undefined) claudeOpts.getUsage = getUsage;

  const map = new Map<Vendor, Adapter>();
  map.set('claude', new ClaudeAdapter(claudeOpts));
  map.set(
    'codex',
    new CodexAdapter({
      exec: exec('codex'),
      lineStream: lineStream('codex'),
      spawn: spawnSeam('codex'),
      binary: binaries.codex,
      readTextFile: guardedReadTextFile,
      fetchFn: noNetworkFetch,
    }),
  );
  map.set(
    'cursor',
    new CursorAdapter({
      exec: exec('cursor'),
      lineStream: lineStream('cursor'),
      readKeychain: refusedKeychain,
      readTextFile: guardedReadTextFile,
      fetchFn: noNetworkFetch,
      binary: binaries.cursor,
      trust: true,
    }),
  );
  map.set(
    'agy',
    new GeminiAdapter({
      exec: exec('gemini'),
      lineStream: lineStream('gemini'),
      binary: binaries.agy,
      readTextFile: guardedReadTextFile,
    }),
  );
  return map;
}

/**
 * Build the fleet. Called once at `mesh.hello` and again on every
 * `mesh.configure` — reconfiguration is a rebuild, never a mutation, so no adapter
 * is ever half-reconfigured.
 */
export function buildFleet(cfg: BuildConfig): BuiltFleet {
  const binaries = {} as Record<Vendor, string>;
  const configured = new Set<Vendor>();
  for (const vendor of VENDORS) {
    const configuredPath = cfg.binaries[vendor];
    if (typeof configuredPath === 'string' && configuredPath.length > 0) {
      binaries[vendor] = configuredPath;
      configured.add(vendor);
    } else {
      binaries[vendor] = UNCONFIGURED_BINARIES[vendor];
    }
  }

  const seams: Seams = {
    exec: makeRegisteredExec(cfg.rpc, cfg.appInstanceId),
    lineStream: makeRegisteredLineStream(cfg.rpc, cfg.appInstanceId),
    spawn: makeRegisteredSpawn(cfg.rpc, cfg.appInstanceId),
  };

  return { adapters: makeAdapters(seams, binaries, cfg.getUsage), binaries, configured };
}
