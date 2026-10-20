/**
 * Typed `invoke()` wrappers.
 *
 * Every signature comes from `GildeCommands` in `lib/ipc.ts` (single owner: T04), so
 * a renamed command or a changed argument shape is a compile error at each call
 * site instead of a runtime surprise. Nothing here declares a new shape.
 *
 * Commands land task by task through M1; until the owning task registers its
 * handler, Tauri rejects the call with "Command … not found". `isUnavailable()`
 * recognises that so the shell can degrade to an empty panel instead of showing
 * a scary error.
 */

import { invoke } from '@tauri-apps/api/core';
import type { Channel } from '@tauri-apps/api/core';

import { showcaseMode } from '../features';

import type {
  AccountSlot,
  AdapterInfo,
  CcoTeamListResult,
  CcoTeamView,
  RepoIdentity,
  CcoWorkersAnswer,
  HealthSnapshot,
  AgentBinary,
  AliasProvider,
  AliasOutcome,
  GildeErrorPayload,
  AppConfig,
  AuthStatus,
  BootstrapInfo,
  BudgetState,
  CommandArgs,
  CommandName,
  CommandResult,
  CreateWorktreeReq,
  Diagnostics,
  DispatchScope,
  DispatchSpec,
  DispatchTask,
  DispatchTaskDetail,
  FocusResult,
  GraphSnapshot,
  ImportReport,
  KillOpts,
  KillOutcome,
  KillReport,
  KillTarget,
  LaunchTemplate,
  ModelInfo,
  OpenSessionReq,
  PathReport,
  Project,
  ProjectId,
  PruneReport,
  PtyChunk,
  PtySessionInfo,
  PtySignal,
  QuotaSnapshot,
  RecoveredProc,
  RecoveryDecision,
  RegistryRow,
  RemoveMode,
  RemoveOutcome,
  RemovePreflight,
  RunId,
  RunOutcome,
  ScanFilter,
  ScanResult,
  SendPromptOutcome,
  SessionId,
  SessionStatusReport,
  SidecarStatus,
  SignaturesReport,
  SpawnId,
  TaskId,
  Vendor,
  Worktree,
  WorktreeId,
  WorktreeStats,
  CloseMode,
  AccountId,
} from './ipc';

// ---------------------------------------------------------------------------
// Showcase boundary (src/features.ts `SHOWCASE`)
// ---------------------------------------------------------------------------

/**
 * What answers commands in showcase mode instead of Tauri: the demo backend of
 * `src/showcase/`, installed by the showcase itself (it is never imported from
 * here, so a normal build does not carry it).
 */
export type ShowcaseBackend = (name: CommandName, args: unknown) => Promise<unknown>;

let showcaseBackend: ShowcaseBackend | null = null;

/** Install (or, with `null`, remove) the showcase's backend. Only `src/showcase/` and its tests call this. */
export function installShowcaseBackend(backend: ShowcaseBackend | null): void {
  showcaseBackend = backend;
}

/**
 * The refusal of a command in showcase mode before a backend is installed —
 * shaped like Tauri's "not found" so every caller degrades as it does against
 * a backend that lacks the command (`isUnavailable`).
 */
function showcaseRefusal(name: string): GildeErrorPayload {
  return {
    code: 'INTERNAL',
    key: 'error.unknown',
    params: {},
    detail: `showcase: command ${name} not found`,
    retryable: false,
  };
}

/**
 * The one place `invoke` is called with a command name. In showcase mode the
 * command never reaches Tauri: the showcase's backend answers it, or it is
 * refused as unavailable.
 */
export async function call<K extends CommandName>(
  name: K,
  args: CommandArgs<K>,
): Promise<CommandResult<K>> {
  if (showcaseMode()) {
    if (showcaseBackend === null) throw showcaseRefusal(name);
    return (await showcaseBackend(name, args)) as CommandResult<K>;
  }
  return invoke<CommandResult<K>>(name, args as Record<string, unknown>);
}

// ---------------------------------------------------------------------------
// Error handling
// ---------------------------------------------------------------------------

/** Rust rejects with `{ code, key, params, detail, retryable }` — never prose. */
export function isGildeErrorPayload(e: unknown): e is GildeErrorPayload {
  return (
    typeof e === 'object' &&
    e !== null &&
    typeof (e as GildeErrorPayload).code === 'string' &&
    typeof (e as GildeErrorPayload).key === 'string'
  );
}

/** Normalize anything a rejected `invoke()` can throw into an `GildeErrorPayload`. */
export function asGildeError(e: unknown): GildeErrorPayload {
  if (isGildeErrorPayload(e)) return e;
  return {
    code: 'INTERNAL',
    key: 'error.unknown',
    params: {},
    detail: typeof e === 'string' ? e : e instanceof Error ? e.message : null,
    retryable: false,
  };
}

/**
 * True when the backend has no handler registered for the command yet. During
 * M1 the Rust command surface fills in wave by wave; the shell must stay usable
 * against a partially wired backend.
 */
export function isUnavailable(e: unknown): boolean {
  const detail = isGildeErrorPayload(e) ? (e.detail ?? '') : typeof e === 'string' ? e : '';
  return /not found|not allowed|unknown command/i.test(detail);
}

/** Run a command, returning `fallback` when the handler is not registered yet. */
export async function callOptional<K extends CommandName>(
  name: K,
  args: CommandArgs<K>,
  fallback: CommandResult<K>,
): Promise<CommandResult<K>> {
  try {
    return await call(name, args);
  } catch (e) {
    if (isUnavailable(e)) return fallback;
    throw e;
  }
}

// ---------------------------------------------------------------------------
// The command surface (DESIGN §3), one method per command.
// ---------------------------------------------------------------------------

const NO_ARGS = {} as Record<string, never>;

export const cmd = {
  // -- app ------------------------------------------------------------------
  appBootstrap: (): Promise<BootstrapInfo> => call('app_bootstrap', NO_ARGS),
  appSettingsGet: (): Promise<AppConfig> => call('app_settings_get', NO_ARGS),
  appSettingsSet: (patch: Record<string, unknown>): Promise<AppConfig> =>
    call('app_settings_set', { patch }),
  appDiagnostics: (): Promise<Diagnostics> => call('app_diagnostics', NO_ARGS),
  appReveal: (path: string): Promise<void> => call('app_reveal', { path }),

  // -- agents ---------------------------------------------------------------
  agentsDetect: (refresh: boolean): Promise<AgentBinary[]> => call('agents_detect', { refresh }),
  agentsPathReport: (): Promise<PathReport> => call('agents_path_report', NO_ARGS),
  agentsLaunchTemplates: (): Promise<LaunchTemplate[]> => call('agents_launch_templates', NO_ARGS),
  agentsReloadSignatures: (): Promise<SignaturesReport> => call('agents_reload_signatures', NO_ARGS),

  // -- projects / worktree --------------------------------------------------
  projectAdd: (path: string): Promise<Project> => call('project_add', { path }),
  projectList: (): Promise<Project[]> => call('project_list', NO_ARGS),
  projectRemove: (id: ProjectId): Promise<void> => call('project_remove', { id }),
  worktreeList: (projectId: ProjectId): Promise<Worktree[]> =>
    call('worktree_list', { projectId }),
  worktreeCreate: (req: CreateWorktreeReq): Promise<Worktree> => call('worktree_create', { req }),
  worktreeStats: (id: WorktreeId): Promise<WorktreeStats> => call('worktree_stats', { id }),
  worktreeRemovePreflight: (id: WorktreeId): Promise<RemovePreflight> =>
    call('worktree_remove_preflight', { id }),
  worktreeRemove: (id: WorktreeId, mode: RemoveMode): Promise<RemoveOutcome> =>
    call('worktree_remove', { id, mode }),
  worktreePrune: (projectId: ProjectId): Promise<PruneReport> =>
    call('worktree_prune', { projectId }),
  worktreeAliasEnsure: (id: WorktreeId, provider: AliasProvider): Promise<AliasOutcome> =>
    call('worktree_alias_ensure', { id, provider }),
  worktreeLock: (id: WorktreeId, locked: boolean): Promise<void> =>
    call('worktree_lock', { id, locked }),

  // -- session (PTY) --------------------------------------------------------
  sessionOpen: (req: OpenSessionReq, onChunk: Channel<PtyChunk>): Promise<PtySessionInfo> =>
    call('session_open', { req, onChunk }),
  sessionAttach: (id: SessionId, onChunk: Channel<PtyChunk>): Promise<void> =>
    call('session_attach', { id, onChunk }),
  sessionSnapshot: (id: SessionId): Promise<ArrayBuffer> => call('session_snapshot', { id }),
  sessionWrite: (id: SessionId, b64: string): Promise<void> => call('session_write', { id, b64 }),
  sessionResize: (id: SessionId, cols: number, rows: number): Promise<void> =>
    call('session_resize', { id, cols, rows }),
  sessionSignal: (id: SessionId, sig: PtySignal): Promise<void> =>
    call('session_signal', { id, sig }),
  sessionSendPrompt: (id: SessionId, text: string): Promise<SendPromptOutcome> =>
    call('session_send_prompt', { id, text }),
  sessionClose: (id: SessionId, mode: CloseMode): Promise<KillReport> =>
    call('session_close', { id, mode }),
  sessionList: (): Promise<PtySessionInfo[]> => call('session_list', NO_ARGS),
  sessionStatus: (id: SessionId): Promise<SessionStatusReport> => call('session_status', { id }),

  // -- dispatch / mesh ------------------------------------------------------
  dispatchCreate: (spec: DispatchSpec): Promise<DispatchTask> => call('dispatch_create', { spec }),
  dispatchList: (scope: DispatchScope, limit: number): Promise<DispatchTask[]> =>
    call('dispatch_list', { scope, limit }),
  dispatchGet: (taskId: TaskId): Promise<DispatchTaskDetail> => call('dispatch_get', { taskId }),
  dispatchCancel: (taskId: TaskId): Promise<KillReport[]> => call('dispatch_cancel', { taskId }),
  dispatchCancelRun: (runId: RunId): Promise<KillReport> => call('dispatch_cancel_run', { runId }),
  dispatchResume: (runId: RunId, message: string): Promise<RunOutcome> =>
    call('dispatch_resume', { runId, message }),
  dispatchOpenTerminal: (runId: RunId, onChunk: Channel<PtyChunk>): Promise<PtySessionInfo> =>
    call('dispatch_open_terminal', { runId, onChunk }),
  meshAdapters: (): Promise<AdapterInfo[]> => call('mesh_adapters', NO_ARGS),
  meshModels: (vendor: Vendor, accountId?: AccountId): Promise<ModelInfo[]> =>
    call('mesh_models', { vendor, accountId }),
  meshQuota: (vendor: Vendor, accountId?: AccountId): Promise<QuotaSnapshot | null> =>
    call('mesh_quota', { vendor, accountId }),
  meshAuth: (vendor: Vendor, accountId?: AccountId): Promise<AuthStatus> =>
    call('mesh_auth', { vendor, accountId }),
  sidecarStatus: (): Promise<SidecarStatus> => call('sidecar_status', NO_ARGS),
  sidecarRestart: (): Promise<SidecarStatus> => call('sidecar_restart', NO_ARGS),

  // -- monitor / recovery ---------------------------------------------------
  procmonScan: (filter: ScanFilter, force: boolean): Promise<ScanResult> =>
    call('procmon_scan', { filter, force }),
  procmonCwd: (pid: number): Promise<string | null> => call('procmon_cwd', { pid }),
  procmonRegistry: (): Promise<RegistryRow[]> => call('procmon_registry', NO_ARGS),
  procmonKill: (targets: KillTarget[], opts: KillOpts): Promise<KillOutcome[]> =>
    call('procmon_kill', { targets, opts }),
  procmonKeepalive: (spawnId: SpawnId, keep: boolean): Promise<void> =>
    call('procmon_keepalive', { spawnId, keep }),
  budgetGet: (): Promise<BudgetState> => call('budget_get', NO_ARGS),
  healthGet: (): Promise<HealthSnapshot> => call('health_get', NO_ARGS),
  healthDismiss: (alertId: string): Promise<HealthSnapshot> => call('health_dismiss', { alertId }),
  recoveryPending: (): Promise<RecoveredProc[]> => call('recovery_pending', NO_ARGS),
  recoveryResolve: (decisions: RecoveryDecision[]): Promise<KillOutcome[]> =>
    call('recovery_resolve', { decisions }),
  undoCancel: (token: string): Promise<boolean> => call('undo_cancel', { token }),

  // -- accounts / graph -----------------------------------------------------
  accountsList: (): Promise<AccountSlot[]> => call('accounts_list', NO_ARGS),
  accountsImportCco: (): Promise<ImportReport> => call('accounts_import_cco', NO_ARGS),
  graphSnapshot: (): Promise<GraphSnapshot> => call('graph_snapshot', NO_ARGS),
  graphFocus: (nodeId: string): Promise<FocusResult> => call('graph_focus', { nodeId }),

  // -- team mode, read-only preview (team contract v2 §8.2) ------------------
  ccoTeamList: (): Promise<CcoTeamListResult> => call('cco_team_list', NO_ARGS),
  ccoTeamGet: (teamId: string): Promise<CcoTeamView | null> => call('cco_team_get', { teamId }),
  repoIdentity: (paths: string[]): Promise<RepoIdentity[]> => call('repo_identity', { paths }),

  // -- worker accounts, read-only (plan #3c) ---------------------------------
  ccoWorkers: (refresh: boolean): Promise<CcoWorkersAnswer> => call('cco_workers', { refresh }),
};

export type Cmd = typeof cmd;

/** Re-exported so callers do not have to reach into `ipc.ts` for the error type. */
export type { GildeErrorPayload };
