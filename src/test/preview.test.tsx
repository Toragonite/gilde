/**
 * The preview build (`src/features.ts`): what a first launch shows, and that no
 * experimental lane is reachable or visible — rail, hotkeys, palette (see
 * palette.test.tsx), settings (dialogs.test.tsx), status bar, tab bar, drawer,
 * sidebar. And the flag brings every lane back.
 *
 * Rendered to static markup like render.test.tsx.
 */

import { renderToStaticMarkup } from 'react-dom/server';

import { EXPERIMENTAL_LANES, isPaneViewEnabled, isViewEnabled, railViews } from '../features';
import { EXPERIMENTAL_HOTKEYS, activeHotkeyIds } from '../hooks/useHotkeys';
import { translate } from '../i18n';
import { agentDisplayName } from '../lib/agents';
import { AppShell, inspectorShownFor, registerSidebar, sidebarShownFor } from '../shell/AppShell';
import { connectionReading } from '../shell/StatusBar';
import { titleContext } from '../shell/TitleBar';
import { Sidebar } from '../sidebar/Sidebar';
import { emptyGraph } from '../store/graphSlice';
import { initialTeamState } from '../store/teamSlice';
import { initialUiState } from '../store/uiSlice';
import { initialWorktreeState } from '../store/worktreeSlice';
import { useAppStore } from '../store/useAppStore';
import type { AppStore } from '../store/useAppStore';
import fixture from '../../src-tauri/tests/fixtures/cco/team-snapshot.json';
import type { CcoTeamView } from '../lib/ipc';
import { assert, assertEqual, test } from './harness';
import { laneTest, laneUiState, previewTest } from './lanes';

const ko = (key: Parameters<typeof translate>[1], params?: Record<string, string | number>) =>
  translate('ko', key, params);

function seed(patch: Partial<AppStore> = {}): void {
  useAppStore.setState({
    graph: emptyGraph(),
    ui: initialUiState(),
    toasts: [],
    agents: [],
    sidecar: null,
    sessions: new Map(),
    team: { ...initialTeamState() },
    worktree: initialWorktreeState(),
    ...patch,
  });
}

function render(): string {
  Object.assign(useAppStore.getInitialState(), useAppStore.getState());
  return renderToStaticMarkup(<AppShell />);
}

function railButtons(html: string): string[] {
  const rail = html.match(/<nav class="rail"[\s\S]*?<\/nav>/)?.[0] ?? '';
  return [...rail.matchAll(/<button[^>]*aria-label="([^"]+)"/g)].map((m) => m[1]!);
}

function teamView(): CcoTeamView {
  const raw = (fixture as unknown as { view: CcoTeamView }).view;
  return { ...JSON.parse(JSON.stringify(raw)), generation: 1, believed: true };
}

test('the default build is the preview (the flag is off unless VITE_GILDE_EXPERIMENTAL_LANES=1)', () => {
  assertEqual(EXPERIMENTAL_LANES, false, 'the unit-test bundle is built without the flag');
});

previewTest('the preview rail has Team, then Worker accounts, then settings — nothing else', () => {
  assertEqual(railViews().join(','), 'team,workers', 'the rail views');
  for (const view of ['sessions', 'dispatch', 'graph', 'monitor']) {
    assert(!isViewEnabled(view), `${view} cannot be entered`);
  }
  for (const pane of ['terminal', 'dispatch', 'graph', 'monitor']) {
    assert(!isPaneViewEnabled(pane), `a ${pane} tab cannot be shown`);
  }
  seed({ agents: [{ kind: 'claude', ok: true, installed: true, verified: true, absPath: '/x/claude', version: '1.0.0' } as never] });
  const html = render();
  assertEqual(
    railButtons(html).join(','),
    [ko('view.team'), ko('view.workers'), ko('view.settings')].join(','),
    'exactly Team, Worker accounts and Settings',
  );
  assert(!html.includes('rail-dots'), 'no agent-status dots in the preview rail');
});

laneTest('the experimental flag restores every lane, after Team and Worker accounts', () => {
  assertEqual(railViews().join(','), 'team,workers,sessions,dispatch,graph,monitor', 'the rail views');
  seed({ ui: laneUiState() });
  const html = render();
  assertEqual(
    railButtons(html).join(','),
    ['view.team', 'view.workers', 'view.sessions', 'view.dispatch', 'view.graph', 'view.monitor', 'view.settings']
      .map((k) => ko(k as never))
      .join(','),
    'the lanes reappear after Team and Worker accounts',
  );
  for (const id of EXPERIMENTAL_HOTKEYS) {
    assert(activeHotkeyIds().includes(id), `${id} is claimed with the lanes on`);
  }
});

previewTest('the preview hotkeys: ⌘1 Team, ⌘2 Worker accounts, and no lane binding', () => {
  const ids = activeHotkeyIds();
  assert(ids.includes('viewTeam') && ids.includes('viewWorkers'), 'Team and Worker accounts have hotkeys');
  for (const id of EXPERIMENTAL_HOTKEYS) assert(!ids.includes(id), `${id} is not claimed in the preview`);
  for (const id of ['newSession', 'switcher', 'viewDispatch', 'viewGraph', 'viewMonitor', 'toggleDrawer']) {
    assert(!ids.includes(id as never), `${id} (⌘T ⌘P ⌘⇧D ⌘⇧G ⌘⇧M ⌘J) is not claimed`);
  }
});

previewTest('a first launch opens on Team: its tab, its sidebar, no inspector, no drawer', () => {
  seed();
  const dispose = registerSidebar(Sidebar);
  try {
    useAppStore.getState().revealView(useAppStore.getState().ui.activeView);
    const ui = useAppStore.getState().ui;
    assertEqual(ui.activeView, 'team', 'Team is the default view');
    assertEqual(ui.tabs.length, 1, 'one tab');
    assertEqual(ui.tabs[0]?.descriptor.view, 'team', 'and it is the Team tab');

    const html = render();
    assert(html.includes('data-view="team"'), 'the shell is on the Team view');
    assert(html.includes('data-inspector="closed"'), 'the inspector is closed');
    assert(html.includes('data-drawer="closed"') && !html.includes('class="drawer"'), 'there is no drawer');
    assert(html.includes(ko('team.folders')) && html.includes(ko('team.addFolder')), 'the sidebar lists project folders with Add');
    assert(html.includes(ko('team.list.label')), 'and the teams');
    assert(!html.includes(ko('sidebar.addProject')) && !html.includes(ko('view.sessions')), 'not the Sessions project tree');
    // Title bar (F6) and the two switches (F16).
    assert(html.includes(`>${ko('title.teams')}<`), 'the title bar says Teams, not "No project"');
    assert(html.includes('>English<'), 'the language switch names the language, not "en"');
    assert(html.includes('data-theme-pref="system"'), 'the theme switch is an icon button');
    // Tab bar: no split control, no "New session" (R7, F-flag).
    assert(!html.includes(`aria-label="${ko('tab.split')}"`), 'no split control');
    assert(!html.includes(ko('sidebar.newSession')), 'no "New session"');
    // Status bar: one connection indicator, nothing of the lanes (R12).
    const footer = html.match(/<footer class="statusbar"[\s\S]*?<\/footer>/)?.[0] ?? '';
    assert(footer.includes('data-testid="connection"'), 'one connection indicator');
    assert(footer.includes(ko('statusbar.connection.starting')), 'which says it is connecting before the first answer');
    for (const word of ['세션 0', '작업 0', '메모리', '압력', '사이드카', '>ko<', 'rev ']) {
      assert(!footer.includes(word), `no "${word}" in the preview status bar`);
    }
  } finally {
    dispose();
  }
});

previewTest('Worker accounts hides the sidebar; Team opens the inspector only for a selected member', () => {
  assertEqual(sidebarShownFor('workers', true), false, 'no sidebar on Worker accounts');
  assertEqual(sidebarShownFor('team', true), true, 'a sidebar on Team');
  assertEqual(inspectorShownFor('workers', true, true), false, 'never an inspector on Worker accounts');
  assertEqual(inspectorShownFor('team', true, false), false, 'none on Team without a member');
  assertEqual(inspectorShownFor('team', false, true), true, 'a member opens it');

  const view = teamView();
  seed({
    team: {
      ...initialTeamState(),
      views: new Map([[view.teamId, view]]),
      selectedTeamId: view.teamId,
      selectedMember: 'alice',
    },
  });
  useAppStore.getState().setActiveView('team');
  let html = render();
  assert(html.includes('data-inspector="open"'), 'the member opened the inspector');
  assert(html.includes(ko('team.member.details', { name: 'alice' })), 'with its details');
  assert(html.includes(`>${view.state.name}<`), 'and the title bar names the team (F6)');

  useAppStore.getState().setActiveView('workers');
  html = render();
  assert(html.includes('data-sidebar="hidden"'), 'Worker accounts: no sidebar');
  assert(html.includes('data-inspector="closed"'), 'and no inspector');
  assertEqual(titleContext(useAppStore.getState()).key, 'view.workers', 'the title bar says Worker accounts');
});

test('the connection indicator is plain words; anything but connected opens the diagnostics', () => {
  assertEqual(connectionReading('ready').key, 'statusbar.connection.ready', 'ready → Connected');
  assertEqual(connectionReading('ready').healthy, true, 'and needs no action');
  assertEqual(connectionReading('down').key, 'statusbar.connection.down', 'down → Not connected — details');
  assertEqual(connectionReading('starting').key, 'statusbar.connection.starting', 'starting → Connecting');
  assertEqual(connectionReading(null).key, 'statusbar.connection.starting', 'no answer yet → Connecting');
  assertEqual(connectionReading('degraded').healthy, false, 'degraded is not healthy');
  assertEqual(ko('statusbar.connection.ready'), '연결됨', 'Korean');
  assertEqual(translate('en', 'statusbar.connection.down'), 'Not connected — details', 'English');

  seed({ sidecar: { state: 'down', generation: 1, restarts: 0, lastError: null } });
  const html = render();
  assert(/<button[^>]*data-testid="connection"/.test(html), 'not connected is a button');
  useAppStore.getState().setSettingsOpen(true, 'diagnostics');
  assertEqual(useAppStore.getState().ui.settingsFocus, 'diagnostics', 'which opens Settings › Diagnostics');
  useAppStore.getState().closeOverlays();
  assertEqual(useAppStore.getState().ui.settingsFocus, null, 'and the focus does not outlive the sheet');
});

test('agents are named by product everywhere (F13)', () => {
  assertEqual(agentDisplayName('claude'), 'Claude Code', 'claude');
  assertEqual(agentDisplayName('codex'), 'Codex', 'codex');
  assertEqual(agentDisplayName('cursorAgent'), 'Cursor', 'cursorAgent');
  assertEqual(agentDisplayName('cursor'), 'Cursor', 'cursor');
  assertEqual(agentDisplayName('agy'), 'Antigravity', 'agy');
  assertEqual(agentDisplayName('gemini'), 'Antigravity', 'gemini is the same CLI family');
  assertEqual(agentDisplayName('newcli'), 'newcli', 'an unknown id is shown as it is');
  assertEqual(ko('view.workers'), '작업자 계정', 'one term for accounts (ko)');
  assertEqual(translate('en', 'view.workers'), 'Worker accounts', 'one term for accounts (en)');
});
