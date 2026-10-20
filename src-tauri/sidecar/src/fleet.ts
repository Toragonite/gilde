/**
 * fleet.ts — construct the four vendor adapters and the Fleet around them.
 *
 * Nothing here forks or reimplements a vendor: every adapter is the published
 * class, configured through its PUBLIC constructor seams (DESIGN §0 rule 1,
 * §5-6). We inject exactly four things:
 *
 *   - `exec`         the registered run-to-completion spawner (all four),
 *   - `lineStream`   the registered streaming spawner (all four; codex gained the
 *                    seam — and `capabilities.stream === true` — in agent-mesh 0.1.3),
 *   - `getUsage`     the registered duplex quota probe (claude only; the CLI's
 *                    control-protocol probe needs an open stdin, which
 *                    `ExecRequest.stdin` cannot express — §5-7),
 *   - `readKeychain` cursor's macOS keychain read (agent-mesh 0.1.3), run through
 *                    the registered `exec` instead of the vendor's own
 *                    `/usr/bin/security` spawn.
 *
 * One vendor launch is NOT injectable in agent-mesh 0.1.3: codex's live model
 * listing (`availableModels()` → `codex app-server`, also reached by
 * `Fleet.route` for a latency-class policy) spawns directly. It is bounded
 * (`modelsTimeoutMs`, own process group, memoized per CODEX_HOME) and recorded as
 * the one reachable unregistered launch in `scripts/check-seam.mjs`.
 *
 * Two more deliberate choices:
 *   - cursor gets `trust: true`. Choosing to run a coding agent in a worktree IS
 *     the trust decision; without it every task stalls ~21s on an interactive gate.
 *   - claude gets NO `gateway`. M1 does not enable `steer()`, and an absent
 *     gateway makes `steer()` return an honest error instead of a silent no-op.
 *
 * A SECOND set of adapter instances backs introspection (`mesh.quota`,
 * `mesh.availableModels`, `mesh.authStatus`, `fleet.route`). Their seams are
 * pre-wrapped in `runCtx` with `runId:'introspect'`, so those probes are
 * registered and visible in the Process Monitor while never being attributed to
 * a user's dispatch run.
 */

import { Fleet } from '@toragonite/agent-mesh';
import type { Account, Adapter, Vendor } from '@toragonite/agent-mesh';
import { ClaudeAdapter } from '@toragonite/agent-mesh-claude';
import type { ClaudeAdapterOptions, ExecFn, LineStreamFn } from '@toragonite/agent-mesh-claude';
import { CodexAdapter } from '@toragonite/agent-mesh-codex';
import { CursorAdapter } from '@toragonite/agent-mesh-cursor';
import type { CursorAdapterOptions } from '@toragonite/agent-mesh-cursor';
import { GeminiAdapter } from '@toragonite/agent-mesh-gemini';

import { makeRegisteredExec, makeRegisteredLineStream, runCtx, INTROSPECT_RUN_ID } from './exec-registered';
import type { Rpc } from './rpc';

/** The four vendors this build registers, in the order `mesh.hello` reports them. */
export const VENDORS = ['claude', 'codex', 'cursor', 'gemini'] as const;
export type KnownVendor = (typeof VENDORS)[number];

/** Host-resolved absolute paths (or bare command names) for the vendor CLIs. */
export interface BinaryMap {
  claude?: string;
  codex?: string;
  cursorAgent?: string;
  agy?: string;
}

/** Defaults the host imposes on every task (DESIGN §5-4). */
export interface FleetDefaults {
  timeoutMs?: number;
  permissionMode?: string;
}

export interface BuildConfig {
  rpc: Rpc;
  appInstanceId: string;
  binaries?: BinaryMap;
  /** Account slots; the first one per vendor becomes that adapter's default account. */
  accounts?: Account[];
  defaults?: FleetDefaults;
  /**
   * The registered claude quota probe (`usage.ts`). Injected rather than
   * constructed here so this module never spawns: it is a `ClaudeAdapterOptions`
   * seam, and leaving it out would silently fall back to the vendor's own
   * unregistered probe.
   */
  getUsage?: ClaudeAdapterOptions['getUsage'];
}

export interface BuiltFleet {
  fleet: Fleet;
  /** Run/stream instances — one per vendor, so Cursor's memoized catalog survives. */
  adapters: Map<Vendor, Adapter>;
  /** Introspection instances, pinned to `runId:'introspect'`. */
  introspect: Map<Vendor, Adapter>;
  /** The binaries these adapters were built with; `mesh.hello`/`mesh.adapters` report them. */
  binaries: Required<BinaryMap>;
}

/**
 * Everything a method handler needs. Held in one mutable box because
 * `mesh.configure` REBUILDS the fleet rather than mutating adapters — a handler
 * must always read the current build, never a captured one.
 */
export interface SidecarState {
  appInstanceId: string;
  bootId: string;
  hostVersion: string;
  defaults: FleetDefaults;
  accounts: Account[];
  built: BuiltFleet;
  /** Set once `mesh.hello` has been answered successfully. */
  ready: boolean;
}

/** Default command names when the host resolved nothing. */
const DEFAULT_BINARIES: Required<BinaryMap> = {
  claude: 'claude',
  codex: 'codex',
  cursorAgent: 'cursor-agent',
  agy: 'agy',
};

function accountFor(accounts: Account[] | undefined, vendor: Vendor): Account | undefined {
  return accounts?.find((a) => a.vendor === vendor);
}

/** Wrap a seam so every spawn it makes is attributed to the introspection run. */
function pinExec(exec: ExecFn, vendor: Vendor): ExecFn {
  return (req) => runCtx.run({ runId: INTROSPECT_RUN_ID, vendor: String(vendor) }, () => exec(req));
}

function pinLineStream(lineStream: LineStreamFn, vendor: Vendor): LineStreamFn {
  return (req, onEnd) =>
    runCtx.run({ runId: INTROSPECT_RUN_ID, vendor: String(vendor) }, () => lineStream(req, onEnd));
}

type ReadKeychain = NonNullable<CursorAdapterOptions['readKeychain']>;

/** What cursor-agent's default credential store keeps in the login keychain. */
const CURSOR_KEYCHAIN_ARGS = ['find-generic-password', '-a', 'cursor-user', '-s', 'cursor-access-token', '-w'];
/** The only variables `security` gets — the same four the vendor default passes. */
const KEYCHAIN_ENV_KEYS = ['PATH', 'HOME', 'USER', 'LANG'] as const;

/**
 * Cursor's keychain read, through the REGISTERED exec seam rather than the vendor's
 * own `spawn('/usr/bin/security')`, so the probe has a registry row like every other
 * child. Semantics mirror the vendor default exactly: exit 0 with output → the token;
 * exit 36/51 or an interaction-refusal message → `refused` (the adapter then backs
 * off for hours instead of re-prompting); anything else, a timeout or a launch
 * failure → not found. The token stays in this process: the host is told the
 * command and its arguments, never stdout.
 */
function registeredKeychain(exec: ExecFn): ReadKeychain {
  return async () => {
    const env: Record<string, string> = {};
    for (const key of KEYCHAIN_ENV_KEYS) {
      const value = process.env[key];
      if (typeof value === 'string') env[key] = value;
    }
    try {
      const r = await exec({ cmd: '/usr/bin/security', args: [...CURSOR_KEYCHAIN_ARGS], env, timeoutMs: 10_000 });
      if (r.timedOut) return { found: false, refused: false };
      const refused =
        r.exitCode === 51 || r.exitCode === 36 || /interaction is not allowed|user canceled|user cancelled/i.test(r.stderr);
      if (refused) return { found: false, refused: true };
      const token = r.stdout.trim();
      return r.exitCode === 0 && token.length > 0 ? { found: true, token } : { found: false, refused: false };
    } catch {
      return { found: false, refused: false };
    }
  };
}

interface Seams {
  exec: ExecFn;
  lineStream: LineStreamFn;
}

function makeAdapters(
  seams: Seams,
  binaries: Required<BinaryMap>,
  accounts: Account[] | undefined,
  getUsage: ClaudeAdapterOptions['getUsage'],
  pin: boolean,
): Map<Vendor, Adapter> {
  const exec = (vendor: Vendor): ExecFn => (pin ? pinExec(seams.exec, vendor) : seams.exec);
  const lineStream = (vendor: Vendor): LineStreamFn =>
    pin ? pinLineStream(seams.lineStream, vendor) : seams.lineStream;

  const claudeOpts: ClaudeAdapterOptions = {
    exec: exec('claude'),
    lineStream: lineStream('claude'),
    binary: binaries.claude,
    // No `gateway`: M1 does not enable steer() (DESIGN §5-7).
  };
  const claudeAccount = accountFor(accounts, 'claude');
  if (claudeAccount !== undefined) claudeOpts.defaultAccount = claudeAccount;
  if (getUsage !== undefined) claudeOpts.getUsage = getUsage;

  const codexAccount = accountFor(accounts, 'codex');
  const cursorAccount = accountFor(accounts, 'cursor');
  const geminiAccount = accountFor(accounts, 'gemini');

  const map = new Map<Vendor, Adapter>();
  map.set('claude', new ClaudeAdapter(claudeOpts));
  map.set(
    'codex',
    // agent-mesh 0.1.3 gave CodexAdapter a lineStream seam (and stream: true); leaving
    // it out would fall back to the vendor's own unregistered spawner.
    new CodexAdapter({
      exec: exec('codex'),
      lineStream: lineStream('codex'),
      binary: binaries.codex,
      ...(codexAccount !== undefined ? { defaultAccount: codexAccount } : {}),
    }),
  );
  map.set(
    'cursor',
    new CursorAdapter({
      exec: exec('cursor'),
      lineStream: lineStream('cursor'),
      readKeychain: registeredKeychain(exec('cursor')),
      binary: binaries.cursorAgent,
      trust: true,
      ...(cursorAccount !== undefined ? { defaultAccount: cursorAccount } : {}),
    }),
  );
  map.set(
    'gemini',
    new GeminiAdapter({
      exec: exec('gemini'),
      lineStream: lineStream('gemini'),
      binary: binaries.agy,
      ...(geminiAccount !== undefined ? { defaultAccount: geminiAccount } : {}),
    }),
  );
  return map;
}

/**
 * Build the run fleet plus its introspection twin. Called once at `mesh.hello`
 * and again on every `mesh.configure` — reconfiguration is a rebuild, never a
 * mutation, so no adapter is ever half-reconfigured.
 */
export function buildFleet(cfg: BuildConfig): BuiltFleet {
  const binaries: Required<BinaryMap> = {
    claude: cfg.binaries?.claude ?? DEFAULT_BINARIES.claude,
    codex: cfg.binaries?.codex ?? DEFAULT_BINARIES.codex,
    cursorAgent: cfg.binaries?.cursorAgent ?? DEFAULT_BINARIES.cursorAgent,
    agy: cfg.binaries?.agy ?? DEFAULT_BINARIES.agy,
  };

  const seams: Seams = {
    exec: makeRegisteredExec(cfg.rpc, cfg.appInstanceId),
    lineStream: makeRegisteredLineStream(cfg.rpc, cfg.appInstanceId),
  };

  const adapters = makeAdapters(seams, binaries, cfg.accounts, cfg.getUsage, false);
  const introspect = makeAdapters(seams, binaries, cfg.accounts, cfg.getUsage, true);

  const fleet = new Fleet();
  for (const vendor of VENDORS) {
    const adapter = adapters.get(vendor);
    if (adapter !== undefined) fleet.register(adapter);
  }

  return { fleet, adapters, introspect, binaries };
}
