/**
 * SINGLE OWNER (T04). The IPC contract.
 *
 * This file is the only place the shapes that cross the Rust ⇄ WebView boundary
 * are declared:
 *
 *   1. a 1:1 mirror of `src-tauri/src/model/graph.rs`, `model/status.rs` and
 *      `events.rs` (all `#[serde(rename_all = "camelCase")]`);
 *   2. every command in DESIGN §3 — including commands that are not implemented
 *      yet, so the shape is fixed before the implementing task starts;
 *   3. every `gilde://` event name from DESIGN §4 and its payload type.
 *
 * Rules that keep this file honest:
 *
 * - Nothing here has a runtime cost beyond the two frozen name tables. It is
 *   types plus string constants — no invoke wrapper, no listener. `lib/commands.ts`
 *   and `lib/events.ts` (T05) build those on top of `GildeCommands` / `GildeEvents`.
 * - **Do not redeclare any of these shapes elsewhere.** If a new event, node kind
 *   or command is needed, change this file first and build on that commit.
 * - Rust never sends prose. Errors and toasts arrive as `{ key, params }` and are
 *   rendered by `t()` (DESIGN §9-9).
 */

import type { Channel } from '@tauri-apps/api/core';

// Plan #3c: the engine's worker-account shapes, type-only, from the byte-for-byte vendored
// copy of Claude Code Orchestrator's `uiTypes.ts` — one definition, not a third mirror.
import type {
  LoginState as CcoLoginState,
  UiAccount,
  UiTokenStats,
  UiUnsupportedAccount,
  UiUsageWindow,
} from '../../src-tauri/sidecar/src/vendored/cco/uiTypes';

// ===========================================================================
// 1. Identifiers
// ===========================================================================

/** UUID v7 — lexicographic order equals creation order. */
export type Uuid = string;

export type ProjectId = Uuid;
export type WorktreeId = Uuid;
export type SessionId = Uuid;
/** A dispatch task (one prompt fanned out to N agents). */
export type TaskId = Uuid;
/** One headless run inside a dispatch task. */
export type RunId = Uuid;
export type AccountId = Uuid;
/** A row in the durable PID registry. */
export type SpawnId = Uuid;

/** Milliseconds since the Unix epoch. Every `at`/`*At` field uses this. */
export type UnixMillis = number;

// ===========================================================================
// 2. Errors — `src-tauri/src/error.rs`
// ===========================================================================

export type GildeErrorCode =
  | 'IO'
  | 'DB'
  | 'CONFIG'
  | 'GIT'
  | 'SPAWN'
  | 'SIDECAR_DOWN'
  | 'SIDECAR_RUNTIME_UNUSABLE'
  | 'NOT_FOUND'
  | 'REFUSED'
  | 'WORKTREE'
  | 'PTY'
  | 'INTERNAL';

/** What every rejected `invoke()` resolves to. Contains no localized prose. */
export interface GildeErrorPayload {
  /** Stable machine code. Branch on this, not on `key`. */
  code: GildeErrorCode | string;
  /** i18n key, present in both `i18n/ko.ts` and `i18n/en.ts`. */
  key: string;
  /** Interpolation parameters for `key`. */
  params: Record<string, unknown>;
  /** Untranslated technical detail (raw stderr, OS message); shown verbatim. */
  detail: string | null;
  retryable: boolean;
}

// ===========================================================================
// 3. Status model — `src-tauri/src/model/status.rs`
// ===========================================================================

export type SessionStatus =
  | { kind: 'starting' }
  | { kind: 'idle' }
  | { kind: 'working'; since: UnixMillis; hint?: string }
  | { kind: 'waitingForInput'; prompt: string }
  | { kind: 'stalled'; quietMs: number }
  | { kind: 'exited'; code?: number; signal?: number }
  | { kind: 'killed' }
  | { kind: 'detached' }
  | { kind: 'failed'; reason: string };

export type SessionStatusKind = SessionStatus['kind'];

/** Which bucket of the hot-reloadable signature file a pattern came from. */
export type PatternClass = 'working' | 'waiting' | 'idle';

/**
 * One observation behind a status decision. The evidence popover renders each
 * of these as a human sentence — status is never reported without its reasons.
 */
export type StatusEvidence =
  | { kind: 'outputBurst'; bytesPerS: number }
  | { kind: 'outputQuiet'; ms: number }
  | { kind: 'patternMatch'; signatureId: string; class: PatternClass; line: string }
  | { kind: 'foregroundGroup'; pgid: number; cpuPct: number; rssBytes: number }
  | { kind: 'altScreen'; on: boolean }
  /** The vte grid has held the same cells for `ms` — DESIGN §6-4, BACKLOG 24. */
  | { kind: 'screenStable'; ms: number }
  | { kind: 'bell' }
  | { kind: 'childExited'; code?: number };

export interface SessionStatusReport {
  status: SessionStatus;
  /** 0..1. Below 0.5 the pill is drawn hollow and dashed. */
  confidence: number;
  evidence: StatusEvidence[];
  since: UnixMillis;
}

/** Orthogonal to status: a node can be `working` *and* `dirty`. */
export type Badge =
  | 'dirty'
  | 'unmerged'
  | 'orphan'
  | 'attention'
  | 'over-budget'
  | 'quota-low'
  | 'stale'
  | 'unverified'
  | 'detached'
  | 'locked';

/** Every field optional; `patchNode { metrics }` merges field-wise. */
export interface NodeMetrics {
  rssBytes?: number;
  cpuPct?: number;
  insertions?: number;
  deletions?: number;
  dirtyFiles?: number;
  ahead?: number;
  behind?: number;
  inputTokens?: number;
  outputTokens?: number;
  cachedTokens?: number;
  costUsd?: number;
  elapsedMs?: number;
  lastActivityAt?: UnixMillis;
  quotaPercent?: number;
  /** Last 60 output-rate samples; drives the session node sparkline. */
  outputRate?: number[];
}

// ===========================================================================
// 4. Graph model — mirrors `src-tauri/src/model/graph.rs`
// ===========================================================================

export type NodeKind =
  | 'project'
  | 'worktree'
  | 'session'
  | 'dispatch'
  | 'run'
  | 'account'
  | 'orphan';

export type NodeStatus =
  | 'idle'
  | 'working'
  | 'waiting'
  | 'stalled'
  | 'error'
  | 'done'
  | 'exited'
  | 'orphan'
  | 'detached'
  | 'clean'
  | 'dirty'
  | 'ok';

export type EdgeKind =
  | 'contains'
  | 'runs-in'
  | 'dispatched-to'
  | 'resumes'
  | 'aliases'
  | 'uses-account';

/** Supervision facts the Inspector reads straight off the node. */
export interface NodeMeta {
  vendor?: string;
  accountId?: AccountId;
  branch?: string;
  pid?: number;
  pgid?: number;
  worktreeId?: WorktreeId;
  cwd?: string;
}

export interface GraphNode {
  /** `"session:0192f…"` — kind prefix plus entity id. */
  id: string;
  kind: NodeKind;
  parentId: string | null;
  label: string;
  sublabel?: string;
  status: NodeStatus;
  /** 0..1 — below 0.5 the ring is hollow. */
  confidence?: number;
  evidence?: StatusEvidence[];
  badges: Badge[];
  metrics: NodeMetrics;
  meta: NodeMeta;
  updatedAt: UnixMillis;
}

export interface GraphEdge {
  id: string;
  source: string;
  target: string;
  kind: EdgeKind;
  /** Only active edges animate. */
  active: boolean;
}

export type PressureLevel = 'unknown' | 'normal' | 'warn' | 'critical';

export type SidecarLifecycle = 'starting' | 'ready' | 'degraded' | 'down' | 'restarting';

/** Aggregate counters for the status bar and the graph header. */
export interface Totals {
  projects: number;
  worktrees: number;
  sessions: number;
  sessionsWorking: number;
  sessionsWaiting: number;
  sessionsIdle: number;
  runsActive: number;
  orphans: number;
  rssBytes: number;
  rssBudgetPercent: number;
  pressureLevel: PressureLevel;
  sidecarState: SidecarLifecycle;
  at: UnixMillis;
}

export interface GraphSnapshot {
  nodes: GraphNode[];
  edges: GraphEdge[];
  /**
   * sha256 over the sorted node ids and edge ids. Changes only when the node or
   * edge *set* changes — this is the gate on re-running dagre.
   */
  topologyHash: string;
  totals: Totals;
  rev: number;
  at: UnixMillis;
}

export type GraphOp =
  | { op: 'upsertNode'; node: GraphNode }
  | {
      op: 'patchNode';
      id: string;
      status?: NodeStatus;
      confidence?: number;
      evidence?: StatusEvidence[];
      badges?: Badge[];
      /** Field-wise merge, not replacement. */
      metrics?: NodeMetrics;
    }
  | { op: 'removeNode'; id: string }
  | { op: 'upsertEdge'; edge: GraphEdge }
  | { op: 'patchEdge'; id: string; active: boolean }
  | { op: 'removeEdge'; id: string }
  | { op: 'totals'; totals: Totals };

export interface GraphPatch {
  /** Monotonic, one per published patch. A gap means "call graph_snapshot()". */
  rev: number;
  ops: GraphOp[];
}

/**
 * T25 — one node the user dragged. `pinned` is what makes the position survive
 * later topology changes; mirrors `state::graph::NodePosition`.
 */
export interface NodePosition {
  x: number;
  y: number;
  pinned: boolean;
}

/** T25 — the manual positions for one canvas; mirrors `state::graph::GraphLayout`. */
export interface GraphLayout {
  positions: Record<string, NodePosition>;
  updatedAt: UnixMillis;
}

/** What `graph_focus` resolved a node to. */
export interface FocusResult {
  kind: NodeKind;
  sessionId?: SessionId;
  worktreeId?: WorktreeId;
  taskId?: TaskId;
}

/** Build `"<kind>:<entityId>"`. Mirrors `model::graph::node_id`. */
export function nodeId(kind: NodeKind, entityId: string): string {
  return `${kind}:${entityId}`;
}

/** Split `"session:0192f…"`. Mirrors `model::graph::parse_node_id`. */
export function parseNodeId(id: string): { kind: NodeKind; entityId: string } | null {
  const at = id.indexOf(':');
  if (at <= 0 || at === id.length - 1) return null;
  const kind = id.slice(0, at) as NodeKind;
  const known: readonly NodeKind[] = [
    'project',
    'worktree',
    'session',
    'dispatch',
    'run',
    'account',
    'orphan',
  ];
  if (!known.includes(kind)) return null;
  return { kind, entityId: id.slice(at + 1) };
}

// ===========================================================================
// 5. app — DESIGN §3 `app`, §10-1, §10-3, §10-6
// ===========================================================================

/** Every path `AppPaths` resolves (DESIGN §10-1). */
export interface PathsInfo {
  appDataDir: string;
  configFile: string;
  stateDb: string;
  signaturesFile: string;
  pathCache: string;
  instanceLock: string;
  logsDir: string;
  scrollbackDir: string;
  runsDir: string;
  worktreesRoot: string;
  archiveRoot: string;
  dispatchRoot: string;
  aliasesFile: string;
  ccoRegistry: string;
}

export interface BootstrapInfo {
  appInstanceId: string;
  bootId: string;
  paths: PathsInfo;
  schemaVersion: number;
  /** The saved preference; `system` follows the OS language (F1). */
  locale: LocalePreference;
  /** Rows the startup sweep left for the user to decide on. */
  recoveryPending: number;
}

export type Locale = 'ko' | 'en';
/** What `config.json` stores: a language, or `system` (the default) to follow the OS. */
export type LocalePreference = Locale | 'system';
export type ThemePreference = 'system' | 'light' | 'dark';
export type PermissionMode = 'default' | 'acceptEdits' | 'bypassPermissions' | 'plan' | string;
export type PromptMode = 'argv' | 'typeAfterReady' | 'none';

export interface ProjectConfig {
  id: ProjectId;
  path: string;
  name: string;
  color: string;
  defaultBaseRef: string;
}

export interface AgentsConfig {
  pathOverride: string | null;
  envPassthrough: string[];
  binaries: Partial<Record<AgentKindKey, string | null>>;
  launchOverrides: Partial<Record<AgentKindKey, Partial<LaunchOverride>>>;
}

export interface LaunchOverride {
  promptMode: PromptMode;
  args: string[];
  promptArgv: string[] | null;
  resumeArgs: string[] | null;
  binary: string | null;
}

export interface SidecarConfig {
  enabled: boolean;
  nodePath: string | null;
  pingIntervalSeconds: number;
}

export interface DispatchConfig {
  defaultTimeoutMinutes: number;
  permissionMode: PermissionMode;
  maxConcurrentRuns: number;
}

export interface WorktreeConfig {
  root: string;
  archiveRoot: string;
  branchPrefix: string;
  aliasProviders: AliasProviderKey[];
  lockWhileLive: boolean;
}

export interface SupervisionConfig {
  killGraceMs: number;
  maxHeadlessLifetimeMinutes: number;
  /** Default 0 = disabled (DECISIONS §M1 세부 기본값). */
  idleTimeoutMinutes: number;
  /** Default false — warn only; macOS cannot hard-cap process memory. */
  autoKillOverHardCeiling: boolean;
  softRssMiB: number;
  hardRssMiB: number;
  globalWarnPct: number;
  globalCriticalPct: number;
  scanIntervalSecondsActive: number;
  scanIntervalSecondsBackground: number;
  minAgeSecondsForBulk: number;
  /** Verbatim PTY byte log. Default false. */
  rawPtyLog: boolean;
}

export interface StatusConfig {
  idleAfterMs: number;
  stallAfterMs: number;
}

export interface TerminalConfig {
  fontSize: number;
  fontFamily: string;
  scrollbackBytes: number;
  webgl: boolean;
  shell: string | null;
}

export interface LayoutConfig {
  sidebarWidth: number;
  inspectorOpen: boolean;
  inspectorWidth: number;
  panes: Record<string, unknown>;
  tabs: unknown[];
}

export interface AccountsConfig {
  registryMode: 'import-once' | 'follow';
}

/** What the app may tell the user outside its window. */
export interface NotificationsConfig {
  /** Health alerts that need the user also as macOS notifications while in the background. Default true. */
  system: boolean;
}

/** `config.json` (DESIGN §10-3). Unknown keys round-trip untouched. */
export interface AppConfig {
  schemaVersion: number;
  locale: LocalePreference;
  theme: ThemePreference;
  projects: ProjectConfig[];
  agents: AgentsConfig;
  sidecar: SidecarConfig;
  dispatch: DispatchConfig;
  worktree: WorktreeConfig;
  supervision: SupervisionConfig;
  status: StatusConfig;
  terminal: TerminalConfig;
  layout: LayoutConfig;
  accounts: AccountsConfig;
  /** Optional only so a payload from an older backend still type-checks; absent = defaults. */
  notifications?: NotificationsConfig;
  [extra: string]: unknown;
}

export type MemoryPressureSource =
  | 'kern.memorystatus_vm_pressure_level'
  | 'vm.memory_pressure'
  | 'vm_stat'
  | 'unavailable';

/** `app_diagnostics()` (DESIGN §10-6). One click, everything a bug report needs. */
export interface Diagnostics {
  resolvedPath: string[];
  pathSource: string;
  pathFingerprint: string;
  agents: AgentBinary[];
  sidecar: {
    nodePath: string | null;
    /**
     * T28: widened from `'vendored' | 'system'` to the three rungs
     * `SidecarRuntime::resolve` actually reports — `config.sidecar.nodePath`
     * produces `'configured'`, which the original pair could not express.
     */
    nodeSource: 'vendored' | 'configured' | 'system';
    scriptPath: string | null;
    state: SidecarLifecycle;
    generation: number;
    restarts: number;
    lastError: string | null;
    /** T28: why the runtime landed on a fallback rung. Empty when vendored. */
    warnings?: string[];
  };
  db: { path: string; schemaVersion: number; integrityOk: boolean };
  memoryPressureSource: MemoryPressureSource;
  appInstanceId: string;
  bootId: string;
  appDataDir: string;
  worktreeRoot: string;
  dispatchRoot: string;
}

// ===========================================================================
// 6. agents — DESIGN §3 `agents`, §6-6
// ===========================================================================

export type AgentKindKey = 'claude' | 'codex' | 'cursorAgent' | 'agy' | 'gemini' | 'shell';

/** Which rung of the resolution ladder produced the binary's path. */
export type BinarySource = 'configured' | 'detected' | 'loginShell' | 'notFound';

export interface AgentBinary {
  kind: AgentKindKey;
  absPath: string | null;
  version: string | null;
  detectedAt: UnixMillis | null;
  /** An executable exists at `absPath`. A missing CLI is this row, not an error. */
  installed: boolean;
  /** It answered `--version` inside the probe cap. */
  ok: boolean;
  /** `false` ⇒ the UI marks it "unverified on this machine". */
  verified: boolean;
  /** The range `verified` was judged against, when the kind declares one. */
  verifiedRange: string | null;
  warning?: string;
  source: BinarySource;
}

/**
 * Which rung of the `PATH` ladder answered (DESIGN §10-6). Narrowed to the five
 * `agents::path::PathSource` variants, so a `switch` over it is exhaustive and
 * the diagnostics sheet can label every one of them.
 */
export type PathSource = 'override' | 'cache' | 'loginShellInteractive' | 'loginShell' | 'static';

export interface PathReport {
  resolved: string[];
  source: PathSource;
  fingerprint: string;
  probedAt: UnixMillis;
  /** Non-fatal problems collected on the way down the ladder. Never thrown. */
  warnings: string[];
}

/** DESIGN §6-6. Every field is overridable from `config.agents.launchOverrides`. */
export interface LaunchTemplate {
  agent: AgentKindKey;
  /** Absolute path resolved by `agents::detect`. Never a bare name. */
  binary: string;
  /** Interactive TUI argv — never `-p`. */
  args: string[];
  accountEnvKey: string | null;
  promptMode: PromptMode;
  /** Template carrying a `{PROMPT}` placeholder. */
  promptArgv: string[] | null;
  /** M2 slot, populated now so no migration is needed later. */
  resumeArgs: string[] | null;
  verified: boolean;
}

export interface SignaturesReport {
  agents: number;
  warnings: string[];
}

// ===========================================================================
// 7. projects / worktree — DESIGN §3, §7
// ===========================================================================

export interface Project {
  id: ProjectId;
  path: string;
  canonicalPath: string;
  repoSlug: string;
  name: string;
  color: string;
  defaultBaseRef: string;
  addedAt: UnixMillis;
  lastOpenedAt: UnixMillis | null;
  /**
   * The folder is still on disk. A project whose folder is gone stays listed
   * (marked "Folder not found") until the user removes it. Absent from an older
   * backend ⇒ treat as present.
   */
  pathExists?: boolean;
}

export type WorktreeOrigin = 'created' | 'adopted';

export interface Worktree {
  id: WorktreeId;
  projectId: ProjectId;
  slug: string;
  branch: string;
  path: string;
  baseSha: string;
  origin: WorktreeOrigin;
  createdAt: UnixMillis;
  removedAt: UnixMillis | null;
  locked: boolean;
  verifiedAt: UnixMillis | null;
  postCreateCmd: string | null;
}

export type AliasProviderKey = 'claude-w' | 'cursor-worktree' | 'generic' | 'custom';

export type AliasProvider =
  | { provider: 'claude-w' }
  | { provider: 'cursor-worktree' }
  | { provider: 'generic' }
  | { provider: 'custom'; linkPath: string };

export interface LaunchSpec {
  agent: AgentKindKey;
  accountId?: AccountId;
  prompt?: string;
  cols?: number;
  rows?: number;
}

export interface CreateWorktreeReq {
  projectId: ProjectId;
  taskName: string;
  baseRef?: string;
  aliasProviders: AliasProvider[];
  launch?: LaunchSpec;
  /** DECISIONS §M1 세부 기본값: shipped in M1, per-project default persisted. */
  postCreateCmd?: string;
}

export interface WorktreeStats {
  dirtyFiles: number;
  staged: number;
  untracked: number;
  conflicted: number;
  added: number;
  removed: number;
  ahead: number;
  behind: number;
  lastCommitAt: UnixMillis | null;
  lastCommitSubject: string | null;
}

/** `Force` deliberately does not exist (DESIGN §0 invariant 5, §7-7). */
export type RemoveMode = 'keepBranch' | 'archiveThenRemove' | 'commitThenRemove';

export interface RemovePreflight {
  dirtyFiles: number;
  untracked: number;
  unmergedCommits: number;
  liveSessions: SessionId[];
  runningRuns: RunId[];
  aliasLinks: string[];
  foreignPids: number[];
  allowedModes: RemoveMode[];
}

export interface RemoveOutcome {
  mode: RemoveMode;
  removed: boolean;
  archivePath?: string;
  commitSha?: string;
  /** The branch is never deleted by the app. */
  branchKept: string;
  warnings: string[];
}

export interface PruneReport {
  gitOnly: string[];
  diskOnly: string[];
  dbOnly: string[];
  healthy: string[];
}

/**
 * Why an alias path was left untouched. Every one means "something that is not
 * ours is already there" — the union is closed so the UI can offer the right
 * next step per reason instead of printing a raw token.
 */
export type SkipReason =
  | 'existingDirectory'
  | 'existingFile'
  | 'foreignSymlink'
  | 'raceLost'
  | 'parentMissing'
  | 'sameAsTarget';

export type AliasOutcome =
  /** `notIgnored` is a warning, not a failure: the link exists, but
   *  `git check-ignore` says the repository would show it as untracked. */
  | { outcome: 'created'; aliasPath: string; targetPath: string; notIgnored: boolean }
  | { outcome: 'existing'; aliasPath: string }
  | { outcome: 'skipped'; aliasPath: string; reason: SkipReason }
  /** A real git worktree already lived at the convention path and was
   *  registered with `origin: 'adopted'`. Rust does not know the new
   *  `worktreeId` at this point, so the field is absent. */
  | { outcome: 'adopted'; aliasPath: string; worktreeId?: WorktreeId };

/** Why an owned-alias removal did nothing. */
export type RemovalSkip =
  | 'missing'
  | 'notASymlink'
  | 'targetMismatch'
  | 'adopted'
  | 'ioError';

/** One line of the alias teardown report (DESIGN §7-6 rule 5). */
export interface AliasRemoval {
  provider: AliasProviderKey;
  aliasPath: string;
  targetPath: string;
  /** True only when the symlink was actually unlinked. */
  removed: boolean;
  reason?: RemovalSkip | null;
}

/** A real worktree found sitting at a convention path (DESIGN §7-6 rule 6). */
export interface AdoptionCandidate {
  provider: AliasProviderKey;
  /** Where it was found — also the worktree's real path. */
  path: string;
  /** Short branch name, when it is not detached. */
  branch: string | null;
  head: string | null;
  /** The slug it would be registered under. */
  slug: string;
}

export interface PostCreateResult {
  cmd: string;
  exitCode: number | null;
  tail: string;
}

// ===========================================================================
// 8. session (PTY) — DESIGN §3 `session`, §6
// ===========================================================================

/** Base64 envelope; the bytes are decoded exactly once, by xterm.js. */
export interface PtyChunk {
  seq: number;
  b64: string;
}

export type PtySignal = 'int' | 'hup' | 'term';

export type CloseMode = 'gentle' | 'standard' | 'detach';

export interface OpenSessionReq {
  projectId: ProjectId;
  worktreeId: WorktreeId;
  agent: AgentKindKey;
  accountId?: AccountId;
  cols: number;
  rows: number;
  title?: string;
  prompt?: string;
  /** Overrides the resolved launch template argv when present. */
  args?: string[];
}

export interface PtySessionInfo {
  id: SessionId;
  spawnId: SpawnId;
  projectId: ProjectId;
  worktreeId: WorktreeId;
  agent: AgentKindKey;
  accountId: AccountId | null;
  title: string;
  cwd: string;
  cols: number;
  rows: number;
  pid: number;
  pgid: number;
  openedAt: UnixMillis;
  detached: boolean;
  /** First sequence number still held in the ring. */
  firstRetainedSeq: number;
  nextSeq: number;
}

export interface SendPromptOutcome {
  sent: boolean;
  /** Present when the gate refused (status not idle, confidence too low). */
  refusedReason?: string;
  bracketedPaste: boolean;
}

/** Never a bare "ok": a kill reports exactly what it did and what survived. */
export interface KillReport {
  spawnId: SpawnId | null;
  requested: number[];
  excluded: Array<{ pid: number; reason: string }>;
  survivors: number[];
  refusedReason: string | null;
  dryRun: boolean;
  /** Pids that ignored SIGTERM and had to be escalated to SIGKILL. */
  escalated: number[];
  /**
   * The identity verdict that authorised the kill: `'verified'`, `'gone'`, a
   * mismatch reason, or `'unfenced'` for a bare pid with no registry row.
   * DESIGN §8-4 — a kill is never reported without the fence that allowed it.
   */
  fence: string;
  at: UnixMillis;
  /** What each pid in `requested` is (argv[0] basename), as the plan's scan saw it. */
  names?: Array<{ pid: number; name: string }>;
}

// ===========================================================================
// 9. dispatch / mesh — DESIGN §3, §5
// ===========================================================================

export type Vendor = 'claude' | 'codex' | 'cursor' | 'gemini';

/**
 * A dispatch target's vendor. `'auto'` is the one value the host interprets
 * itself: it asks `fleet.route` and runs whatever comes back, persisting the
 * decision's note on the run (DESIGN §9-7). Added by T22.
 */
export type DispatchVendor = Vendor | 'auto';

export interface DispatchTarget {
  vendor: DispatchVendor;
  model?: string;
  accountId?: AccountId;
  /** T22: a routing note already shown to the user, persisted with the run. */
  routeNote?: string;
}

export type WorktreeStrategy =
  | { strategy: 'perTarget' }
  | { strategy: 'shared' }
  | { strategy: 'existing'; worktreeId: WorktreeId };

export interface DispatchSpec {
  prompt: string;
  projectId: ProjectId;
  baseRef?: string;
  taskSlug: string;
  targets: DispatchTarget[];
  worktreeStrategy: WorktreeStrategy;
  timeoutMs: number;
  permissionMode: PermissionMode;
  /** T22: card title. Defaults to the first line of the prompt. */
  title?: string;
}

export type DispatchScope = 'active' | 'recent' | 'all';

export type DispatchTaskStatus = 'queued' | 'running' | 'done' | 'partial' | 'cancelled' | 'error';

export interface DispatchTask {
  id: TaskId;
  projectId: ProjectId;
  title: string;
  prompt: string;
  taskSlug: string;
  baseRef: string | null;
  worktreeStrategy: WorktreeStrategy;
  permissionMode: PermissionMode;
  timeoutMs: number;
  createdAt: UnixMillis;
  finishedAt: UnixMillis | null;
  status: DispatchTaskStatus;
}

export type RunStatus =
  | 'queued'
  | 'spawning'
  | 'running'
  | 'done'
  | 'error'
  | 'stopped'
  | 'incomplete'
  /** The app failed, not the agent — visually distinct from `error`. */
  | 'interrupted'
  | 'cancelled';

export interface TokenUsage {
  inputTokens?: number;
  outputTokens?: number;
  cachedInputTokens?: number;
  totalTokens?: number;
  costUsd?: number;
  [extra: string]: unknown;
}

export interface DispatchRun {
  id: RunId;
  taskId: TaskId;
  vendor: Vendor;
  modelRequested: string | null;
  /** What actually ran — may differ from what was requested. */
  modelActual: string | null;
  accountId: AccountId | null;
  worktreeId: WorktreeId | null;
  spawnId: SpawnId | null;
  conversationId: string | null;
  status: RunStatus;
  note: string | null;
  routeNote: string | null;
  /**
   * The tail of the result, capped at 256 KiB. The verbatim answer is always
   * in `result.json`, and in `textPath` too once it passed 4 MiB (DESIGN §10-2).
   */
  text: string;
  /** T22: absolute path of the spilled result, when there is one. */
  textPath: string | null;
  usage: TokenUsage | null;
  costUsd: number | null;
  startedAt: UnixMillis | null;
  finishedAt: UnixMillis | null;
  error: string | null;
}

export interface DispatchTaskDetail {
  task: DispatchTask;
  runs: DispatchRun[];
}

/** T22: what `fleet.route` decided. `note` is set when every candidate was gated. */
export interface RouteDecision {
  vendor: Vendor;
  note?: string;
}

export interface RunOutcome {
  runId: RunId;
  status: RunStatus;
  conversationId: string | null;
  note: string | null;
}

/** agent-mesh's own shape, passed through verbatim. */
export interface RunEvent {
  type: string;
  [extra: string]: unknown;
}

/**
 * How a vendor accepts guidance after a run has started. `'none'` is the value
 * a legacy `steer: false` decodes to — Rust normalises it in
 * `sidecar::types::AdapterCapabilities`, so the front end never sees a boolean.
 */
export type SteerMode = 'mid-run' | 'turn' | 'none';

export interface AdapterCapabilities {
  /** Can revise an existing conversation by id. */
  resume: boolean;
  steer: SteerMode;
  /** Native peer-to-peer session messaging (Claude mesh only, today). */
  peerMesh: boolean;
  quota: boolean;
  stream: boolean;
  [extra: string]: unknown;
}

export interface AdapterInfo {
  vendor: Vendor;
  capabilities: AdapterCapabilities;
  binary: string;
  binaryPath: string | null;
  installed: boolean;
}

export interface ModelInfo {
  id: string;
  label?: string;
  /** `'fast'` | `'slow'`, open-ended upstream. */
  latencyClass: string;
  /** Auth modes under which this model is selectable; absent = any login. */
  authModes?: string[];
  /** True for the adapter's default pick. */
  default?: boolean;
  [extra: string]: unknown;
}

export interface QuotaWindow {
  label: string;
  /** 0..100. */
  usedPercent: number;
  /** ISO 8601 instant, when the vendor reports one — not epoch millis. */
  resetAt?: string;
  [extra: string]: unknown;
}

export interface QuotaSnapshot {
  vendor: Vendor;
  windows: QuotaWindow[];
  /**
   * Whether the vendor currently permits new work. Absent means "unknown" and
   * decodes to `true`: defaulting to `false` would stop dispatching to a
   * healthy account because one adapter omitted a field.
   */
  allowed: boolean;
  /** ISO 8601 capture time, stamped by the sidecar (core has no clock). */
  capturedAt?: string;
  raw?: unknown;
  [extra: string]: unknown;
}

export interface AuthStatus {
  loggedIn: boolean;
  /** How it is logged in (`'subscription'`, `'apiKey'`, …). Open vocabulary. */
  mode?: string;
  /** Account label or email when resolvable. Never a token. */
  detail?: string;
  [extra: string]: unknown;
}

export interface SidecarStatus {
  state: SidecarLifecycle;
  generation: number;
  restarts: number;
  lastError: string | null;
  nodePath: string | null;
  scriptPath: string | null;
}

// ===========================================================================
// 10. monitor / recovery — DESIGN §3, §8-7, §8-8
// ===========================================================================

export type ProcGroupKind =
  | 'oursLive'
  | 'oursOrphan'
  | 'agentUnmanaged'
  | 'externalResidue'
  | 'mcpServer'
  | 'other';

export type ProcTag =
  | 'orphan'
  | 'inWorktree'
  | 'ours'
  | 'external'
  | 'mcpServer'
  | 'appBundleMain'
  | 'protected'
  | 'detached';

export interface OwnerRef {
  sessionId?: SessionId;
  runId?: RunId;
  worktreeId?: WorktreeId;
}

export interface ProcRow {
  pid: number;
  ppid: number;
  pgid: number;
  name: string;
  /** Masked and truncated in Rust, before the row leaves the process. */
  argvDisplay: string;
  rssBytes: number;
  cpuPct: number;
  elapsedSecs: number;
  startTime: UnixMillis;
  /** Resolved lazily, only for rows the panel actually renders. */
  cwd: string | null;
  userOwned: boolean;
  group: ProcGroupKind;
  tags: ProcTag[];
  owner: OwnerRef | null;
  spawnId: SpawnId | null;
}

export interface ProcTotals {
  processes: number;
  rssBytes: number;
  physicalRamBytes: number;
  rssPercent: number;
  pressureLevel: PressureLevel;
  byGroup: Partial<Record<ProcGroupKind, { count: number; rssBytes: number; cpuPct: number }>>;
}

export interface ScanFilter {
  groups?: ProcGroupKind[];
  query?: string;
  minRssBytes?: number;
  includeCwd?: boolean;
}

export interface ScanResult {
  rows: ProcRow[];
  totals: ProcTotals;
  at: UnixMillis;
}

export type ProcRegistryState =
  | 'planned'
  | 'running'
  | 'exited'
  | 'killed'
  | 'kill_incomplete'
  | 'lost'
  | 'adopted';

export type ProcKind = 'pty' | 'headless' | 'sidecar' | 'gateway' | 'probe' | 'git';

export interface RegistryRow {
  spawnId: SpawnId;
  appInstance: string;
  generation: number;
  kind: ProcKind;
  vendor: Vendor | null;
  sessionId: SessionId | null;
  runId: RunId | null;
  state: ProcRegistryState;
  pid: number | null;
  pgid: number | null;
  argv: string[];
  cwd: string;
  configDir: string | null;
  worktreeId: WorktreeId | null;
  keepAlive: boolean;
  plannedAt: UnixMillis;
  startedAt: UnixMillis | null;
  exitedAt: UnixMillis | null;
  exitCode: number | null;
  exitSignal: number | null;
  note: string | null;
}

export type KillTarget =
  | { target: 'spawn'; spawnId: SpawnId }
  | { target: 'pid'; pid: number };

export interface KillOpts {
  dryRun: boolean;
  allowBundleMain: boolean;
  /** How long a target may take to honour SIGTERM. Absent ⇒ the config default. */
  graceMs?: number;
  /**
   * Health "Clean up": every target must be a pid in the latest health
   * snapshot's `orphans` with the same kernel start time, or it is refused
   * (`notLeftover`). Absent ⇒ false.
   */
  onlyHealthOrphans?: boolean;
}

export interface KillOutcome {
  target: KillTarget;
  report: KillReport;
}

export interface BudgetState {
  rssBytes: number;
  physicalRamBytes: number;
  percent: number;
  warnPct: number;
  criticalPct: number;
  pressureLevel: PressureLevel;
  pressureSource: MemoryPressureSource;
}

export interface RecoveredProc {
  spawnId: SpawnId;
  kind: ProcKind;
  vendor: Vendor | null;
  pid: number | null;
  argvDisplay: string;
  cwd: string;
  worktreeId: WorktreeId | null;
  rssBytes: number;
  ageSecs: number;
  keepAlive: boolean;
  /** Pre-selected action in the sheet. Always `terminate` (DESIGN §8-5). T11. */
  suggested: RecoveryAction;
}

export type RecoveryAction = 'terminate' | 'keep' | 'adopt';

export interface RecoveryDecision {
  spawnId: SpawnId;
  action: RecoveryAction;
}

// ===========================================================================
// 11. accounts — DESIGN §10-5
// ===========================================================================

export type AccountSource = 'cco' | 'manual' | 'ambient';

export interface AccountSlot {
  id: AccountId;
  vendor: Vendor;
  label: string;
  configDir: string | null;
  modelHint: string | null;
  source: AccountSource;
  enabled: boolean;
  isDefault: boolean;
  loggedIn: boolean;
  lastSeenAt: UnixMillis | null;
  lastQuotaAt: UnixMillis | null;
}

/**
 * The non-account half of `registry.json` (DESIGN §10-5) — T26.
 *
 * Mirrors `CcoSettings` in `src-tauri/src/config/cco_import.rs`. Read-only:
 * claude-code-orchestrator owns these values, this app only displays them and
 * never acts on the billing guards.
 */
export interface CcoSettings {
  claudePath: string | null;
  permissionMode: string | null;
  workerTimeoutMinutes: number | null;
  cooldownMinutes: number | null;
  frontierWorkerDispatch: string | null;
  overageWorkerDispatch: string | null;
}

export interface ImportReport {
  imported: number;
  skipped: number;
  accounts: AccountSlot[];
  /** T26: billing guards for read-only display; `null` when no registry was read. */
  guards: CcoSettings | null;
}

// ===========================================================================
// 11a. team mode, read-only preview — Claude Code Orchestrator team contract v2
//      §8.2. Mirrors the `Cco*` types of `src-tauri/src/sidecar/types.rs`, which
//      mirror what `src-tauri/sidecar/src/methods/cco.ts` composes from a dispatch
//      host's `TeamState` (host-protocol §2.4). Every string here is DATA from
//      another process: render it flattened and capped, never as markup.
// ===========================================================================

export type CcoTeamStatus = 'active' | 'suspended' | 'ended' | 'readonly' | 'damaged';

export type CcoMemberStateName =
  | 'starting'
  | 'idle'
  | 'running'
  | 'pausing'
  | 'paused'
  | 'attach_pending'
  | 'attached'
  | 'blocked'
  | 'stopped'
  | 'ended';

export type CcoBoardState = 'open' | 'claimed' | 'blocked' | 'review' | 'done' | 'cancelled';

export interface CcoOwner {
  hostPid: number;
  hostStartedAt: UnixMillis;
  session: { pid: number; startedAt: UnixMillis };
}

export interface CcoMemberState {
  name: string;
  index: number;
  role: string;
  /** The ENGINE's state — a newer engine may send one this build does not know. */
  state: CcoMemberStateName | string;
  /** A blocked code while `blocked`; `budget` / `quota-wait` / `cancelled` otherwise. */
  reason?: { code: string; text: string; until?: UnixMillis };
  provider: string;
  worker: string;
  accountLabel: string;
  machineLogin: boolean;
  sharesLeadPlan: boolean;
  billableOverage: boolean;
  runsCommands: boolean;
  elevated?: string;
  model: string;
  modelUsed?: string;
  incarnation: number;
  conversationShort?: string;
  /** `tools` | `turns-only` | `unverified`. */
  mailbox: string;
  mailboxReason?: string;
  portBase: number;
  worktree: {
    path: string;
    branch: string;
    baseCommit: string;
    ready: boolean;
    headSha?: string;
    ahead?: number;
    filesChanged?: number;
    dirty?: boolean;
    merged?: boolean;
  };
  turn?: { id: string; n: number; startedAt: UnixMillis; pid?: number; lastOutputAt?: UnixMillis };
  turns: number;
  tokens: { in: number; out: number };
  costUsd: number | null;
  lastInputTokens?: number;
  lastTurn?: { id: string; n: number; outcome: string; endedAt: UnixMillis; head: string };
  pending: number;
  tasks: string[];
  peerWakes: number;
  rssMb?: number;
  attach?: { lease: string; pid?: number; client: string; since: UnixMillis };
  lastActivity?: { ts: UnixMillis; text: string };
}

export interface CcoBoardTask {
  id: string;
  title: string;
  /** At most a short head of the body. */
  body?: string;
  bodyFile?: string;
  state: CcoBoardState | string;
  assignee?: string;
  paths: string[];
  allowOverlap: boolean;
  createdBy: string;
  createdAt: UnixMillis;
  note?: string;
  reports: { at: UnixMillis; by: string; file: string }[];
  commits: string[];
  turns: string[];
  updatedAt: UnixMillis;
  updatedBy: string;
}

export interface CcoTeamState {
  v: number;
  id: string;
  name: string;
  title: string;
  status: CcoTeamStatus | string;
  damagedReason?: string;
  goal?: string;
  workspace: string;
  repoRoot: string;
  leadRel: string;
  baseCommit: string;
  createdAt: UnixMillis;
  endedAt?: UnixMillis;
  seq: number;
  owner: CcoOwner | null;
  lead: { unread: number; readSeq: number; waiter: boolean; lastCallAt: UnixMillis | null; wakesToday: number };
  user: { unread: number; readSeq: number };
  members: CcoMemberState[];
  board: CcoBoardTask[];
  budgets: { turnsToday: number; turnsPerDay: number; running: number; maxRunning: number; pingPongTripped: boolean };
  totals: { turns: number; inputTokens: number; outputTokens: number; costUsd: number | null };
  warnings: { kind: string; text: string }[];
}

export interface CcoTeamMessage {
  seq: number;
  /** `m<seq>`. */
  id: string;
  ts: UnixMillis;
  from: string;
  to: string;
  kind: string;
  urgent: boolean;
  re?: string;
  task?: string;
  notice?: string;
  body?: string;
  /** The body is longer than what was kept, or could not be read. */
  cut: boolean;
  bytes: number;
}

export interface CcoActivityLine {
  ts: UnixMillis;
  turn: string;
  /** `turn` | `text` | `tool` | `usage` | `error` | `done` | `notice`. */
  k: string;
  text: string;
}

export interface CcoMemberActivity {
  member: string;
  lines: CcoActivityLine[];
}

/** One team's whole view — idempotent; keep the greatest `(generation, rev)`. */
export interface CcoTeamView {
  teamId: string;
  rev: number;
  source: 'host' | 'disk';
  live: boolean;
  hostKey?: string;
  paneSessionPid?: number;
  state: CcoTeamState;
  /** Newest first. */
  messages: CcoTeamMessage[];
  activity: CcoMemberActivity[];
  truncated: boolean;
  composedAt: UnixMillis;
  /** The sidecar generation that composed it (Rust's). */
  generation: number;
  /** L7 (Rust's): `active` and running members are current. False = host gone. */
  believed: boolean;
}

export interface CcoHostInfo {
  key: string;
  sessionPid: number;
  /** The lead session's start time (`run/s<pid>.json`), unix millis. */
  sessionStartedAt?: number;
  hostPid: number;
  workspace: string;
  pane: boolean;
  state: 'idle' | 'connecting' | 'connected' | 'no-teams' | 'failed' | string;
  reason?: string;
  teamId?: string;
}

export interface CcoTeamSummary {
  teamId: string;
  name: string;
  title: string;
  status: CcoTeamStatus | string;
  source: 'host' | 'disk';
  live: boolean;
  members: number;
  running: number;
  blocked: number;
  userUnread: number;
  leadUnread: number;
  createdAt: UnixMillis;
  endedAt?: UnixMillis;
  hostKey?: string;
  paneSessionPid?: number;
  owner: CcoOwner | null;
  /** The newest view's rev; greater than the held one ⇒ re-read with `cco_team_get`. */
  rev: number;
  /**
   * Why a connected dispatch host lists the team as it does (CCO `TeamListEntry.reason`:
   * `host gone`, or a damaged team's reason). One line, ≤ 300 chars, host text — flatten
   * and render as text. Absent when no host gave one.
   */
  reason?: string;
  believed: boolean;
}

export interface CcoTeamListResult {
  /** False while the sidecar is down. */
  available: boolean;
  /** False where team mode does not exist. */
  supported: boolean;
  dataRoot: string;
  teamMode: 'on' | 'off' | 'unknown' | string;
  hosts: CcoHostInfo[];
  teams: CcoTeamSummary[];
  generation: number;
}

/**
 * Which repository a folder belongs to, worktree-aware — `repo_identity`
 * (`src-tauri/src/commands/repo_identity.rs`). Two paths are the same repository
 * when their `key`s are equal: the normalized `origin` URL when there is one
 * (so separate clones collapse), else the canonical `git --git-common-dir` (so
 * the main checkout, its subfolders and every linked worktree of it collapse).
 * `unknown` = git did not answer in time; ask again later.
 */
export type RepoKind = 'repo' | 'notRepo' | 'gone' | 'unknown';

export interface RepoIdentity {
  /** The path as asked. */
  path: string;
  kind: RepoKind | string;
  /** `origin:<url>` | `dir:<common dir>` | `path:<path>` (not a repository / not known). */
  key: string;
  commonDir: string | null;
  /** The main checkout (the label's source). */
  root: string | null;
  /** This path's own checkout root (a linked worktree's root). */
  toplevel: string | null;
  linkedWorktree: boolean;
  /** HEAD's short branch; null when detached. */
  branch: string | null;
  origin: string | null;
}

// ===========================================================================
// 11b. worker accounts, read-only (Gilde plan #3c) — what the Claude Code
//      Orchestrator ENGINE knows about its worker accounts, from `tools/call
//      list_workers`' `structuredContent` (host-protocol `WorkersSnapshot`,
//      schema `cco.workers/1`). Composed by `src-tauri/sidecar/src/methods/
//      cco-workers.ts` (`cco.workers`); Rust passes it through and adds
//      `available` / `generation` like `cco_team_list`. Gilde DISPLAYS these
//      values; it computes no quota, selection or cooldown of its own. Every
//      string is DATA from another process: render it as text, flattened, capped.
// ===========================================================================

export type { CcoLoginState };
export type CcoWorkerAccount = UiAccount;
export type CcoUsageWindow = UiUsageWindow;
export type CcoTokenStats = UiTokenStats;
export type CcoUnsupportedAccount = UiUnsupportedAccount;

/** The only `schema` this build reads. */
export const CCO_WORKERS_SCHEMA = 'cco.workers/1';

export interface CcoWorkersSnapshot {
  schema: typeof CCO_WORKERS_SCHEMA;
  /** Epoch ms the ENGINE composed it — the "as of" time. */
  generatedAt: UnixMillis;
  /** `main` first (when known), then the registered workers in registry order. */
  accounts: CcoWorkerAccount[];
  unsupportedAccounts: CcoUnsupportedAccount[];
  /**
   * True when the HOST answered from its own observer cache (its 60 s floor) — not
   * Gilde's own `fromCache`. Absent from a host that predates it (CCO 0b7b1b8).
   */
  servedFromCache?: boolean;
  guards: { overageWorkerDispatch: 'allow' | 'block'; frontierWorkerDispatch: 'allow' | 'block' };
}

export type CcoWorkersUnavailableReason =
  | 'no-host'
  | 'host-gone'
  | 'host-too-old'
  | 'schema-unknown'
  | 'host-error';

export interface CcoWorkersUnavailable {
  reason: CcoWorkersUnavailableReason | string;
  /** One line from the host (an error text, an unknown schema's name), or null. */
  detail: string | null;
  at: UnixMillis;
}

export interface CcoWorkersAnswer {
  /** False while the sidecar is down (stamped by Rust). */
  available: boolean;
  /** False where the dispatch host does not exist. */
  supported: boolean;
  /** The newest snapshot held; null when none has arrived — never an empty stand-in. */
  snapshot: CcoWorkersSnapshot | null;
  hostKey: string | null;
  receivedAt: UnixMillis | null;
  /** The snapshot is not what a connected host answers now. */
  stale: boolean;
  /** Served inside the 60 s floor without asking a host. */
  fromCache: boolean;
  unavailable: CcoWorkersUnavailable | null;
  /** When an automatic request would next reach a host. */
  nextPollAt: UnixMillis;
  /** The engine's poll floor (WORKERS_SNAPSHOT_MIN_POLL_MS, 60 000). */
  minPollMs: number;
  generation: number;
}

// ===========================================================================
// 11b. Health — "is everything running well" (gilde://health, health_get,
//      health_dismiss). Mirrors `src-tauri/src/supervise/health.rs`. Rust
//      decides every alert; the frontend only renders (keys + params, never prose).
// ===========================================================================

export type HealthLevel = 'ok' | 'attention' | 'action';
export type HealthSeverity = 'attention' | 'action';
export type AgentKind = 'claude' | 'codex' | 'cursor-agent' | 'agy' | 'gemini';

export type HealthAttribution =
  | { kind: 'teamMember'; teamId: string | null; team: string; member: string; worktree: string }
  | { kind: 'gildeSession'; sessionId: string }
  | { kind: 'other' };

export interface HealthAgent {
  pid: number;
  pgid: number;
  ppid: number;
  agent: AgentKind;
  /** Summed over the pgid subtree (pid ∪ descendants ∪ group); nested agents are folded in. */
  rssBytes: number;
  cpuPct: number;
  elapsedSecs: number;
  /** Kernel start time, unix millis (same as `ProcRow.startTime`). */
  startTime: UnixMillis;
  attribution: HealthAttribution;
  /** A confirmed leftover (held for at least two consecutive ticks). */
  orphan: boolean;
  /** Every pid of the subtree — details and tooltips only. */
  pids: number[];
  /** The leader's monitor row, argv masked — what the kill sheet renders. */
  row: ProcRow;
}

export interface HealthHost {
  pid: number;
  rssBytes: number;
  elapsedSecs: number;
  /** The lead Claude Code session pid, when a host record or a team owner names it. */
  sessionPid: number | null;
  /** `null`: unknown (nothing names this host's session). */
  leadAlive: boolean | null;
  teamIds: string[];
}

export type HealthMemberState = 'running' | 'idle' | 'leftover';

export interface HealthMember {
  name: string;
  state: HealthMemberState;
  rssBytes: number;
  pids: number[];
}

export interface HealthTeam {
  teamId: string | null;
  team: string;
  status: string;
  hostAlive: boolean;
  rssBytes: number;
  members: HealthMember[];
}

export interface HealthMemory {
  pressure: PressureLevel;
  /** Agents plus dispatch hosts, each pid counted once. */
  agentsRssBytes: number;
  physicalRamBytes: number;
}

export type HealthAlertKind =
  | 'orphans'
  | 'memAgent'
  | 'memLow'
  | 'hostLost'
  | 'turnStuck'
  | 'hostGone'
  | 'hostNoLead';

export type HealthActionKind = 'cleanUp' | 'details' | 'copyCommand' | 'showAgents';

export interface HealthAction {
  kind: HealthActionKind;
  /** `copyCommand`: the exact `cco …` line. */
  command?: string;
  /** `cleanUp`: the leftover leader pids (exactly `HealthSnapshot.orphans` of this alert). */
  pids?: number[];
}

export interface HealthAlert {
  /** Stable: kind + subject (`orphans:<team>`, `mem-agent:<pid>-<startTime>`, `mem-low`, …). */
  id: string;
  kind: HealthAlertKind;
  severity: HealthSeverity;
  titleKey: string;
  bodyKey: string;
  /** Raw values (bytes, names, lists); the frontend formats them and fills the keys. */
  params: Record<string, unknown>;
  actions: HealthAction[];
  /** When the condition started; kept while it holds. */
  since: UnixMillis;
  /** The last raise, re-raise or escalation — a toast is shown once per `(id, raisedAt)`. */
  raisedAt: UnixMillis;
  teamId: string | null;
  dismissed: boolean;
  /**
   * When `dismissed`: the end of its quiet period (dismissal time + the 10 min
   * cooldown) — the popover's "back at". `null` while active. Rust always sends
   * it; optional only so a payload from an older backend still type-checks.
   */
  dismissedUntil?: UnixMillis | null;
}

export interface HealthSnapshot {
  at: UnixMillis;
  /** The max severity of the alerts that are not dismissed; `ok` when none. */
  overall: HealthLevel;
  agents: HealthAgent[];
  hosts: HealthHost[];
  /** Pids of the agents with `orphan: true`. */
  orphans: number[];
  teams: HealthTeam[];
  memory: HealthMemory;
  alerts: HealthAlert[];
  /** False when no team facts were available this tick: no orphan or team alert was decided. */
  teamsKnown: boolean;
}

// ===========================================================================
// 12. Events — DESIGN §4. Mirrors `src-tauri/src/events.rs` exactly.
// ===========================================================================

export const GRAPH_PATCH = 'gilde://graph/patch';
export const GRAPH_RESYNC = 'gilde://graph/resync';
export const SESSION_STATUS = 'gilde://session/status';
export const SESSION_EXIT = 'gilde://session/exit';
export const PTY_RESYNC = 'gilde://pty/resync';
export const DISPATCH_EVENT = 'gilde://dispatch/event';
export const WORKTREE_CHANGED = 'gilde://worktree/changed';
export const PROC_SCAN = 'gilde://proc/scan';
export const PROC_REGISTRY = 'gilde://proc/registry';
export const BUDGET_ALERT = 'gilde://budget/alert';
export const AGENTS_DETECTED = 'gilde://agents/detected';
export const SIDECAR_STATE = 'gilde://sidecar/state';
export const RECOVERY_PENDING = 'gilde://recovery/pending';
export const TOAST = 'gilde://toast';
export const LOG = 'gilde://log';
export const TEAM_SNAPSHOT = 'gilde://team/snapshot';
export const HEALTH = 'gilde://health';

/** Every event name, in DESIGN §4 order. */
export const GILDE_EVENTS = [
  GRAPH_PATCH,
  GRAPH_RESYNC,
  SESSION_STATUS,
  SESSION_EXIT,
  PTY_RESYNC,
  DISPATCH_EVENT,
  WORKTREE_CHANGED,
  PROC_SCAN,
  PROC_REGISTRY,
  BUDGET_ALERT,
  AGENTS_DETECTED,
  SIDECAR_STATE,
  RECOVERY_PENDING,
  TOAST,
  LOG,
  TEAM_SNAPSHOT,
  HEALTH,
] as const;

export interface SessionStatusEvent {
  sessionId: SessionId;
  status: SessionStatus;
  confidence: number;
  evidence: StatusEvidence[];
  since: UnixMillis;
  at: UnixMillis;
}

export interface SessionExitEvent {
  sessionId: SessionId;
  pid: number;
  exitCode: number | null;
  signal: number | null;
  at: UnixMillis;
}

export interface PtyResyncEvent {
  sessionId: SessionId;
  nextSeq: number;
}

export type DispatchEventKind =
  | 'routed'
  | 'spawned'
  | 'text'
  | 'tool'
  | 'usage'
  | 'done'
  | 'error'
  | 'interrupted'
  | 'cancelled';

export interface DispatchEventPayload {
  taskId: TaskId;
  runId: RunId;
  vendor: Vendor;
  kind: DispatchEventKind;
  /** The adapter's own `RunEvent`, passed through verbatim. */
  payload: RunEvent | null;
}

export interface WorktreeChangedEvent {
  worktreeId: WorktreeId;
  dirty: number;
  added: number;
  removed: number;
  ahead: number;
  behind: number;
  at: UnixMillis;
}

export interface ProcScanEvent {
  at: UnixMillis;
  rows: ProcRow[];
  totals: ProcTotals;
}

export interface ProcRegistryEvent {
  changedSpawnIds: SpawnId[];
}

export type BudgetLevel = 'soft' | 'hard' | 'system';

export interface BudgetAlertEvent {
  level: BudgetLevel;
  sessionId: SessionId | null;
  rssBytes: number;
  budgetPercent: number;
  pressureLevel: PressureLevel;
  /** Present when the alert opened a 10 s undo window. */
  undoToken: string | null;
}

export interface AgentsDetectedEvent {
  agents: AgentBinary[];
}

export interface SidecarStateEvent {
  state: SidecarLifecycle;
  generation: number;
  restarts: number;
  lastError: string | null;
}

export interface RecoveryPendingEvent {
  count: number;
}

export type ToastLevel = 'info' | 'success' | 'warn' | 'error';

/** Rust never sends prose: `key` indexes `i18n/ko.ts`, `params` fills it in. */
export interface ToastEvent {
  level: ToastLevel;
  key: string;
  params: Record<string, unknown>;
  actionId: string | null;
  undoToken: string | null;
}

export interface LogEvent {
  level: string;
  target: string;
  msg: string;
  at: UnixMillis;
}

/** `gilde://team/snapshot` — `view: null` means the team left sight. */
export interface TeamSnapshotEvent {
  generation: number;
  rev: number;
  teamId: string;
  view: CcoTeamView | null;
}

/** Event name → payload. `lib/events.ts` (T05) types `listen()` off this. */
export interface GildeEvents {
  'gilde://graph/patch': GraphPatch;
  'gilde://graph/resync': GraphSnapshot;
  'gilde://session/status': SessionStatusEvent;
  'gilde://session/exit': SessionExitEvent;
  'gilde://pty/resync': PtyResyncEvent;
  'gilde://dispatch/event': DispatchEventPayload;
  'gilde://worktree/changed': WorktreeChangedEvent;
  'gilde://proc/scan': ProcScanEvent;
  'gilde://proc/registry': ProcRegistryEvent;
  'gilde://budget/alert': BudgetAlertEvent;
  'gilde://agents/detected': AgentsDetectedEvent;
  'gilde://sidecar/state': SidecarStateEvent;
  'gilde://recovery/pending': RecoveryPendingEvent;
  'gilde://toast': ToastEvent;
  'gilde://log': LogEvent;
  'gilde://team/snapshot': TeamSnapshotEvent;
  'gilde://health': HealthSnapshot;
}

export type GildeEventName = keyof GildeEvents;
export type GildeEventPayload<K extends GildeEventName> = GildeEvents[K];

// ===========================================================================
// 13. Commands — DESIGN §3. Every command, including the not-yet-implemented.
// ===========================================================================

/** Commands that take no arguments. */
export type NoArgs = Record<string, never>;

/**
 * Command name → `{ args, result }`.
 *
 * `lib/commands.ts` (T05) wraps `invoke()` with these, so a typo or a shape
 * change is a compile error at every call site instead of a runtime surprise.
 */
export interface GildeCommands {
  // -- app ------------------------------------------------------------------
  app_bootstrap: { args: NoArgs; result: BootstrapInfo };
  app_settings_get: { args: NoArgs; result: AppConfig };
  /** Deep-merged patch. */
  app_settings_set: { args: { patch: Record<string, unknown> }; result: AppConfig };
  app_diagnostics: { args: NoArgs; result: Diagnostics };
  app_reveal: { args: { path: string }; result: void };

  // -- agents ---------------------------------------------------------------
  agents_detect: { args: { refresh: boolean }; result: AgentBinary[] };
  agents_path_report: { args: NoArgs; result: PathReport };
  agents_launch_templates: { args: NoArgs; result: LaunchTemplate[] };
  agents_reload_signatures: { args: NoArgs; result: SignaturesReport };

  // -- projects / worktree --------------------------------------------------
  project_add: { args: { path: string }; result: Project };
  project_list: { args: NoArgs; result: Project[] };
  project_remove: { args: { id: ProjectId }; result: void };
  worktree_list: { args: { projectId: ProjectId }; result: Worktree[] };
  worktree_create: { args: { req: CreateWorktreeReq }; result: Worktree };
  worktree_stats: { args: { id: WorktreeId }; result: WorktreeStats };
  worktree_remove_preflight: { args: { id: WorktreeId }; result: RemovePreflight };
  worktree_remove: { args: { id: WorktreeId; mode: RemoveMode }; result: RemoveOutcome };
  worktree_prune: { args: { projectId: ProjectId }; result: PruneReport };
  worktree_alias_ensure: {
    args: { id: WorktreeId; provider: AliasProvider };
    result: AliasOutcome;
  };
  worktree_lock: { args: { id: WorktreeId; locked: boolean }; result: void };

  // -- session (PTY) --------------------------------------------------------
  session_open: {
    args: { req: OpenSessionReq; onChunk: Channel<PtyChunk> };
    result: PtySessionInfo;
  };
  /** Attach a fresh channel on remount — **before** taking the snapshot. */
  session_attach: { args: { id: SessionId; onChunk: Channel<PtyChunk> }; result: void };
  /** 16-byte header (`GLDP`, version, nextSeq u64 LE) followed by raw ring bytes. */
  session_snapshot: { args: { id: SessionId }; result: ArrayBuffer };
  session_write: { args: { id: SessionId; b64: string }; result: void };
  session_resize: { args: { id: SessionId; cols: number; rows: number }; result: void };
  session_signal: { args: { id: SessionId; sig: PtySignal }; result: void };
  session_send_prompt: { args: { id: SessionId; text: string }; result: SendPromptOutcome };
  session_close: { args: { id: SessionId; mode: CloseMode }; result: KillReport };
  session_list: { args: NoArgs; result: PtySessionInfo[] };
  session_status: { args: { id: SessionId }; result: SessionStatusReport };
  /**
   * T16: acknowledge a BEL.
   *
   * A `0x07` from the agent raises a sticky per-session attention flag in the
   * status engine (DESIGN §6-4) — sticky because the whole point of the signal
   * is that nobody was looking. This is what lowers it, and the terminal pane
   * calls it when a tab takes focus. Resolves to whether a flag was actually
   * cleared, so a caller can skip a pointless re-render.
   */
  session_attention_clear: { args: { id: SessionId }; result: boolean };

  // -- dispatch / mesh ------------------------------------------------------
  dispatch_create: { args: { spec: DispatchSpec }; result: DispatchTask };
  dispatch_list: { args: { scope: DispatchScope; limit: number }; result: DispatchTask[] };
  dispatch_get: { args: { taskId: TaskId }; result: DispatchTaskDetail };
  dispatch_cancel: { args: { taskId: TaskId }; result: KillReport[] };
  dispatch_cancel_run: { args: { runId: RunId }; result: KillReport };
  dispatch_resume: { args: { runId: RunId; message: string }; result: RunOutcome };
  dispatch_open_terminal: {
    args: { runId: RunId; onChunk: Channel<PtyChunk> };
    result: PtySessionInfo;
  };
  /** T22: the composer's `Auto` preview — the only other caller of `fleet.route`. */
  dispatch_route: { args: { prompt: string; policy?: unknown }; result: RouteDecision };
  /** T22: `~/.gilde/dispatch/<task>/<run>/`, for reveal-in-Finder. */
  dispatch_artifact_dir: { args: { runId: RunId }; result: string };
  mesh_adapters: { args: NoArgs; result: AdapterInfo[] };
  mesh_models: { args: { vendor: Vendor; accountId?: AccountId }; result: ModelInfo[] };
  mesh_quota: { args: { vendor: Vendor; accountId?: AccountId }; result: QuotaSnapshot | null };
  mesh_auth: { args: { vendor: Vendor; accountId?: AccountId }; result: AuthStatus };
  sidecar_status: { args: NoArgs; result: SidecarStatus };
  sidecar_restart: { args: NoArgs; result: SidecarStatus };

  // -- monitor / recovery ---------------------------------------------------
  procmon_scan: { args: { filter: ScanFilter; force: boolean }; result: ScanResult };
  /** Lazy: one syscall per pid, so only for rows actually rendered. */
  procmon_cwd: { args: { pid: number }; result: string | null };
  procmon_registry: { args: NoArgs; result: RegistryRow[] };
  procmon_kill: { args: { targets: KillTarget[]; opts: KillOpts }; result: KillOutcome[] };
  procmon_keepalive: { args: { spawnId: SpawnId; keep: boolean }; result: void };
  budget_get: { args: NoArgs; result: BudgetState };
  /** The latest health snapshot (an empty `at: 0` one before the first tick). */
  health_get: { args: NoArgs; result: HealthSnapshot };
  /** Dismiss one alert; it stays quiet for 10 min unless its severity rises. */
  health_dismiss: { args: { alertId: string }; result: HealthSnapshot };
  recovery_pending: { args: NoArgs; result: RecoveredProc[] };
  recovery_resolve: { args: { decisions: RecoveryDecision[] }; result: KillOutcome[] };
  /** Cancel a 10 s undo window. */
  undo_cancel: { args: { token: string }; result: boolean };

  // -- accounts / graph -----------------------------------------------------
  accounts_list: { args: NoArgs; result: AccountSlot[] };
  accounts_import_cco: { args: NoArgs; result: ImportReport };
  graph_snapshot: { args: NoArgs; result: GraphSnapshot };
  graph_focus: { args: { nodeId: string }; result: FocusResult };
  // T25 — manual node positions, persisted per project in `settings_kv`
  // (`src-tauri/src/state/graph.rs`). `projectId: null` is the "everything"
  // canvas, which is what the graph shows by default.
  graph_layout_get: { args: { projectId: ProjectId | null }; result: GraphLayout };
  graph_layout_save: {
    args: { projectId: ProjectId | null; layout: GraphLayout };
    result: GraphLayout;
  };

  // -- team mode, read-only preview (team contract v2 §8.2) ------------------
  /** Every team in sight of the open projects; also (re)states what is watched. */
  cco_team_list: { args: NoArgs; result: CcoTeamListResult };
  /** The latest view of one team, `null` when it is not in sight. */
  cco_team_get: { args: { teamId: string }; result: CcoTeamView | null };
  /**
   * The repository identity of each path (at most 256 per call), read-only git
   * queries cached a few seconds in Rust. Answers keep the order asked.
   */
  repo_identity: { args: { paths: string[] }; result: RepoIdentity[] };

  // -- worker accounts, read-only (plan #3c) ---------------------------------
  /**
   * The engine's worker accounts (sidecar `cco.workers`, scope as `cco_team_list`).
   * `refresh: true` is the user's explicit refresh — still floored by the sidecar.
   */
  cco_workers: { args: { refresh: boolean }; result: CcoWorkersAnswer };
}

export type CommandName = keyof GildeCommands;
export type CommandArgs<K extends CommandName> = GildeCommands[K]['args'];
export type CommandResult<K extends CommandName> = GildeCommands[K]['result'];
