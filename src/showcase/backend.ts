/**
 * The showcase's stand-in backend: what answers `lib/commands.ts` `call()` in
 * showcase mode instead of Tauri.
 *
 * It answers the read commands the preview asks (settings, sidecar status,
 * health, team list and views, repository identities, worker accounts, project
 * folders) from the demo world, keeps the settings writes in memory (so a
 * language or layout change sticks for the session, as in the app), and makes
 * the few local actions harmless no-ops. Every other command is refused the
 * way a backend without that command refuses it ("… not found"), which the
 * callers already treat as "unavailable" and degrade from quietly.
 *
 * Answers go through a JSON round trip, as they would over IPC: the store never
 * holds a reference into the world.
 */

import type { ShowcaseBackend } from '../lib/commands';
import type {
  AppConfig,
  CommandArgs,
  CommandName,
  CommandResult,
  GildeErrorPayload,
  HealthSnapshot,
  LayoutConfig,
  Locale,
  RepoIdentity,
  ThemePreference,
} from '../lib/ipc';
import { translate } from '../i18n';
import { VIEW_PANE } from '../store/uiSlice';
import type { PersistedLayout, ViewKey } from '../store/uiSlice';
import type { SceneSpec } from './scenes';
import type { ShowcaseWorld } from './world';

/**
 * A string only the showcase carries. `scripts/check-showcase-tree-shaking.mjs`
 * asserts it is absent from a normal build's `dist/` and present in a showcase
 * build's; it is also the detail of every refusal, so it is used at runtime and
 * survives minification.
 */
export const SHOWCASE_MARKER = 'gilde-showcase-demo-backend';

type Handlers = { [K in CommandName]?: (args: CommandArgs<K>) => CommandResult<K> };

export interface ShowcaseBackendHandle {
  backend: ShowcaseBackend;
  /** Every command asked, in order (the tests read it). */
  asked: CommandName[];
}

/** The `config.json` the showcase "restores": the scene's language, theme and layout. */
export function sceneConfig(locale: Locale, theme: ThemePreference, spec: SceneSpec): AppConfig {
  const view: ViewKey = spec.view;
  const pane = VIEW_PANE[view] ?? { descriptor: { view: 'team' }, title: 'view.team' };
  const persisted: PersistedLayout = {
    sidebarWidth: 300,
    inspectorOpen: false,
    inspectorWidth: 360,
    panes: {
      sidebarVisible: true,
      drawerOpen: false,
      drawerHeight: 220,
      drawerTab: 'monitor',
      activeView: view,
      split: 1,
      tabs: [{ id: 'tab-1', title: translate(locale, pane.title), descriptor: pane.descriptor, closable: true }],
      slots: ['tab-1'],
      activePaneId: 'pane-1',
      teamScope: spec.scope,
      teamGroupBy: spec.groupBy,
      teamFolderId: null,
    },
  };
  const layout: LayoutConfig = { ...persisted, panes: { ...persisted.panes }, tabs: [] };
  return {
    schemaVersion: 1,
    locale,
    theme,
    projects: [],
    agents: { pathOverride: null, envPassthrough: [], binaries: {}, launchOverrides: {} },
    sidecar: { enabled: true, nodePath: null, pingIntervalSeconds: 15 },
    dispatch: { defaultTimeoutMinutes: 60, permissionMode: 'default', maxConcurrentRuns: 4 },
    worktree: {
      root: '~/code/.gilde-worktrees',
      archiveRoot: '~/code/.gilde-archive',
      branchPrefix: 'gilde/',
      aliasProviders: ['generic'],
      lockWhileLive: true,
    },
    supervision: {
      killGraceMs: 3000,
      maxHeadlessLifetimeMinutes: 240,
      idleTimeoutMinutes: 0,
      autoKillOverHardCeiling: false,
      softRssMiB: 4096,
      hardRssMiB: 8192,
      globalWarnPct: 70,
      globalCriticalPct: 85,
      scanIntervalSecondsActive: 3,
      scanIntervalSecondsBackground: 15,
      minAgeSecondsForBulk: 60,
      rawPtyLog: false,
    },
    status: { idleAfterMs: 4000, stallAfterMs: 120000 },
    terminal: { fontSize: 13, fontFamily: 'ui-monospace', scrollbackBytes: 4194304, webgl: true, shell: null },
    layout,
    accounts: { registryMode: 'follow' },
  };
}

function refusal(name: string): GildeErrorPayload {
  return {
    code: 'INTERNAL',
    key: 'error.unknown',
    params: {},
    detail: `${SHOWCASE_MARKER}: command ${name} not found`,
    retryable: false,
  };
}

/** What crosses the IPC boundary is JSON: a copy, `undefined` fields dropped. */
function overIpc<T>(value: T): T {
  return value === undefined ? value : (JSON.parse(JSON.stringify(value)) as T);
}

/** Rust's `DISMISS_COOLDOWN_MS` (src-tauri/src/supervise/health.rs): a dismissal holds this long. */
export const DISMISS_COOLDOWN_MS = 10 * 60_000;

/**
 * `health_dismiss`: the alert is marked dismissed until `at` + the cooldown and
 * the overall level recomputed — Rust's rule (`AlertBook::dismiss`).
 */
function dismissed(snapshot: HealthSnapshot, alertId: string, at: number): HealthSnapshot {
  const alerts = snapshot.alerts.map((a) =>
    a.id === alertId ? { ...a, dismissed: true, dismissedUntil: at + DISMISS_COOLDOWN_MS } : a,
  );
  const open = alerts.filter((a) => !a.dismissed);
  const overall = open.some((a) => a.severity === 'action') ? 'action' : open.length > 0 ? 'attention' : 'ok';
  return { ...snapshot, at: Math.max(at, snapshot.at), alerts, overall };
}

/** The backend of one scene. Its state (settings, folders, health) lives for the page. */
export function createShowcaseBackend(world: ShowcaseWorld, initialConfig: AppConfig): ShowcaseBackendHandle {
  let config = initialConfig;
  let projects = world.projects;
  let health = world.health;
  const identities = new Map<string, RepoIdentity>(world.identities.map((id) => [id.path, id]));
  const views = new Map(world.views.map((v) => [v.teamId, v]));
  const asked: CommandName[] = [];

  const handlers: Handlers = {
    app_settings_get: () => config,
    app_settings_set: ({ patch }) => {
      config = { ...config, ...patch };
      return config;
    },
    // Reveal the worktree folder: a browser page has no file manager behind it.
    app_reveal: () => undefined,
    sidecar_status: () => ({ state: 'ready', generation: 1, restarts: 0, lastError: null, nodePath: null, scriptPath: null }),
    health_get: () => health,
    health_dismiss: ({ alertId }) => {
      health = dismissed(health, alertId, Date.now());
      return health;
    },
    cco_team_list: () => world.list,
    cco_team_get: ({ teamId }) => views.get(teamId) ?? null,
    repo_identity: ({ paths }) =>
      paths.map(
        (path) =>
          identities.get(path) ?? {
            path,
            kind: 'notRepo',
            key: `path:${path}`,
            commonDir: null,
            root: null,
            toplevel: null,
            linkedWorktree: false,
            branch: null,
            origin: null,
          },
      ),
    // The engine floors its polls at a minute; the next automatic one is a minute away.
    cco_workers: () => ({ ...world.workers, nextPollAt: Date.now() + world.workers.minPollMs }),
    project_list: () => projects,
    worktree_list: () => [],
    project_remove: ({ id }) => {
      projects = projects.filter((p) => p.id !== id);
    },
  };

  const backend: ShowcaseBackend = async (name, args) => {
    asked.push(name);
    const handler = handlers[name] as ((a: unknown) => unknown) | undefined;
    if (handler === undefined) throw refusal(name);
    return overIpc(handler(args));
  };

  return { backend, asked };
}
