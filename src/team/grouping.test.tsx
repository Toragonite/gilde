/**
 * Team view scope + repository grouping tests (worktree-aware).
 *
 * The teams are the real sidecar snapshot (`team-snapshot.json`) with their
 * workspace and member worktrees moved; the repository identities are what
 * Rust's `repo_identity` answers for such paths (the Rust tests create the real
 * git repositories, worktrees and clones and pin that git agrees).
 *
 * What is pinned:
 *
 *  1. scope: All workspaces by default; This workspace disabled with the hint
 *     while no project folder is added; the choice persists with the layout and
 *     a bad saved value falls back to the default;
 *  2. grouping: Repository (default) / Workspace folder / Status;
 *  3. worktree awareness: (a) a lead in a linked worktree, (b) a lead in a
 *     member worktree under the Orchestrator's data root and (c) a second clone
 *     with the same origin all land in ONE repository group, and This workspace
 *     with the main checkout selected includes all three;
 *  4. the list renders at once with provisional per-path groups and regroups
 *     when the identities arrive;
 *  5. labels: basename, parent folder to tell two same-named repositories apart,
 *     "Not a git folder", "<repo> · worktree <branch>", plain status words;
 *  6. members: short branch (unless it is the name) with the full one in the tooltip, a worktree icon,
 *     a warning when the worktree is gone; the inspector shows the path, the
 *     base commit and "Reveal in Finder" (which calls `app_reveal`);
 *  7. project folders are grouped by the same identity;
 *  8. the slice asks only for what it does not know, in chunks, and survives a
 *     backend without the command.
 */

import type { ReactElement } from 'react';
import { renderToStaticMarkup } from 'react-dom/server';

import fixture from '../../src-tauri/tests/fixtures/cco/team-snapshot.json';
import { translate } from '../i18n';
import { cmd } from '../lib/commands';
import type { AppConfig, CcoTeamListResult, CcoTeamView, Project, RepoIdentity } from '../lib/ipc';
import { assert, assertDeepEqual, assertEqual, test } from '../test/harness';
import { IDENTITY_CHUNK, initialTeamState, mergeIdentities } from '../store/teamSlice';
import type { TeamPreviewState } from '../store/teamSlice';
import { cancelPersistLayout, initialUiState, pickPersistedLayout } from '../store/uiSlice';
import type { UiState } from '../store/uiSlice';
import { initialWorktreeState } from '../store/worktreeSlice';
import { useAppStore } from '../store/useAppStore';
import {
  effectiveScope,
  groupFolders,
  groupLabelText,
  groupTeams,
  identityPaths,
  leadWorktree,
  memberBranchShort,
  memberWorktreeFollow,
  orderedTeams,
  repoKeyOf,
  scopeFolder,
  statusGroupOf,
  teamsInScope,
} from './grouping';
import { TeamMemberInspector, revealWorktree, shortSha } from './TeamMemberInspector';
import { TeamPane } from './TeamPane';
import { TeamSidebar } from './TeamSidebar';

// ---------------------------------------------------------------------------
// Fixtures
// ---------------------------------------------------------------------------

const ko = (key: Parameters<typeof translate>[1], params?: Record<string, string | number>) =>
  translate('ko', key, params);

const ORIGIN = 'origin:github.com/Acme/Widget';
const MAIN = '/Users/u/src/widget';
const LINKED = '/Users/u/wt/widget-feature';
const DATA_MEMBER = '/Users/u/.claude-code-orchestrator/worktrees/0a1b2c/demo-hi1/alice';
const CLONE = '/Users/u/forks/widget';
const GADGET = '/Users/u/src/gadget';
const PLAIN = '/Users/u/notes';

let seq = 0;

/** The real snapshot as team `id`, its lead in `workspace`, members' worktrees under the data root. */
function teamAt(id: string, workspace: string, patch: Partial<CcoTeamView['state']> = {}): CcoTeamView {
  const raw = (fixture as unknown as { view: CcoTeamView }).view;
  const view: CcoTeamView = JSON.parse(JSON.stringify(raw));
  seq += 1;
  const state = {
    ...view.state,
    id,
    name: id,
    workspace,
    repoRoot: workspace,
    createdAt: 1_000 + seq,
    members: view.state.members.map((m) => ({
      ...m,
      worktree: {
        ...m.worktree,
        path: `/Users/u/.claude-code-orchestrator/worktrees/0a1b2c/${id}-hi1/${m.name}`,
        branch: `cco/${id}-hi1/${m.name}`,
      },
    })),
    ...patch,
  };
  return { ...view, teamId: id, state, generation: 1, believed: true };
}

function repo(path: string, patch: Partial<RepoIdentity> = {}): RepoIdentity {
  return {
    path,
    kind: 'repo',
    key: ORIGIN,
    commonDir: `${MAIN}/.git`,
    root: MAIN,
    toplevel: path,
    linkedWorktree: false,
    branch: 'main',
    origin: 'github.com/Acme/Widget',
    ...patch,
  };
}

function alone(path: string, kind: RepoIdentity['kind']): RepoIdentity {
  return {
    path,
    kind,
    key: `path:${path}`,
    commonDir: null,
    root: null,
    toplevel: null,
    linkedWorktree: false,
    branch: null,
    origin: null,
  };
}

/** What `repo_identity` answers for the world above. */
function worldIdentities(views: CcoTeamView[] = []): Map<string, RepoIdentity> {
  const ids = new Map<string, RepoIdentity>([
    [MAIN, repo(MAIN)],
    [LINKED, repo(LINKED, { linkedWorktree: true, branch: 'feature/x' })],
    [DATA_MEMBER, repo(DATA_MEMBER, { linkedWorktree: true, branch: 'cco/demo-hi1/alice' })],
    [CLONE, repo(CLONE, { commonDir: `${CLONE}/.git`, root: CLONE })],
    [
      GADGET,
      repo(GADGET, {
        key: 'origin:github.com/Acme/Gadget',
        origin: 'github.com/Acme/Gadget',
        commonDir: `${GADGET}/.git`,
        root: GADGET,
      }),
    ],
    [PLAIN, alone(PLAIN, 'notRepo')],
  ]);
  // Every member worktree is a linked worktree of its lead's repository.
  for (const view of views) {
    const lead = ids.get(view.state.workspace);
    for (const m of view.state.members) {
      if (lead && lead.kind === 'repo') {
        ids.set(m.worktree.path, { ...lead, path: m.worktree.path, toplevel: m.worktree.path, linkedWorktree: true, branch: m.worktree.branch });
      }
    }
  }
  return ids;
}

function project(id: string, path: string, canonicalPath = path): Project {
  return {
    id,
    path,
    canonicalPath,
    repoSlug: id,
    name: canonicalPath.split('/').pop() ?? id,
    color: 'var(--accent)',
    defaultBaseRef: 'main',
    addedAt: 1,
    lastOpenedAt: null,
  };
}

const T_MAIN = teamAt('main', MAIN);
const T_LINKED = teamAt('linked', LINKED);
const T_DATA = teamAt('nested', DATA_MEMBER);
const T_CLONE = teamAt('clone', CLONE);
const T_GADGET = teamAt('gadget', GADGET);
const T_PLAIN = teamAt('plain', PLAIN);
const ALL = [T_MAIN, T_LINKED, T_DATA, T_CLONE, T_GADGET, T_PLAIN];

function listOf(views: CcoTeamView[]): CcoTeamListResult {
  return {
    available: true,
    supported: true,
    dataRoot: '/Users/u/.claude-code-orchestrator',
    teamMode: 'on',
    hosts: [],
    teams: views.map((v) => ({
      teamId: v.teamId,
      name: v.state.name,
      title: v.state.title,
      status: v.state.status,
      source: v.source,
      live: v.live,
      members: v.state.members.length,
      running: 0,
      blocked: 0,
      userUnread: 0,
      leadUnread: 0,
      createdAt: v.state.createdAt,
      owner: v.state.owner,
      rev: v.rev,
      believed: v.believed,
    })),
    generation: 1,
  };
}

function seed(opts: {
  views?: CcoTeamView[];
  ids?: Map<string, RepoIdentity>;
  projects?: Project[];
  ui?: Partial<UiState>;
  team?: Partial<TeamPreviewState>;
}): void {
  cancelPersistLayout();
  const views = opts.views ?? ALL;
  useAppStore.setState({
    ui: { ...initialUiState(), ...opts.ui },
    team: {
      ...initialTeamState(),
      watching: true,
      list: listOf(views),
      views: new Map(views.map((v) => [v.teamId, v])),
      selectedTeamId: views[0]?.teamId ?? null,
      identities: opts.ids ?? new Map(),
      ...opts.team,
    },
    worktree: {
      ...initialWorktreeState(),
      roster: (opts.projects ?? []).map((p) => ({ project: p, worktrees: [] })),
    },
    startTeamWatch: () => () => undefined,
  });
}

function renderNode(node: ReactElement): string {
  Object.assign(useAppStore.getInitialState(), useAppStore.getState());
  return renderToStaticMarkup(node);
}

const renderSidebar = () => renderNode(<TeamSidebar />);
const renderPane = () =>
  renderNode(
    <TeamPane
      descriptor={{ view: 'team' }}
      tab={{ id: 't1', title: 'team', descriptor: { view: 'team' }, closable: true }}
      paneId="pane-1"
      active
    />,
  );

function textOf(html: string): string {
  return html
    .replace(/<[^>]+>/g, ' ')
    .replace(/&amp;/g, '&')
    .replace(/&lt;/g, '<')
    .replace(/&gt;/g, '>')
    .replace(/&quot;/g, '"')
    .replace(/&#x27;/g, "'")
    .replace(/\s+/g, ' ');
}

/** The `data-group` keys of the rendered team groups, in order. */
function renderedGroups(html: string): string[] {
  const sidebarTeams = html.split('team-sb-folders-hdr')[0];
  return [...sidebarTeams.matchAll(/class="col team-sb-group" data-group="([^"]*)"/g)].map((m) =>
    m[1].replace(/&quot;/g, '"').replace(/&amp;/g, '&'),
  );
}

const ids = worldIdentities(ALL);
const names = (views: CcoTeamView[]) => views.map((v) => v.teamId);

// ---------------------------------------------------------------------------
// 1. scope switch
// ---------------------------------------------------------------------------

test('scope: All workspaces is the default; This workspace is disabled with a hint while no folder is added', () => {
  const ui = initialUiState();
  assertEqual(ui.teamScope, 'all', 'default scope');
  assertEqual(ui.teamGroupBy, 'repository', 'default grouping');

  seed({ ids, projects: [] });
  const html = renderSidebar();
  assert(/class="team-scope-all" aria-pressed="true"/.test(html), 'All workspaces is pressed');
  assert(/class="team-scope-workspace"[^>]*disabled=""/.test(html), 'This workspace is disabled');
  assert(textOf(html).includes(ko('team.scope.needsFolder')), 'the one-line hint is shown');
  assertEqual(effectiveScope('workspace', null), 'all', 'a stored "workspace" with no folder acts as all');

  // A saved "This workspace" with no folder: still disabled, still everything shown.
  seed({ ids, projects: [], ui: { teamScope: 'workspace' } });
  const stillAll = renderSidebar();
  assert(/class="team-scope-all" aria-pressed="true"/.test(stillAll), 'falls back to all');
  assertEqual(renderedGroups(stillAll).length, 3, 'every repository group is listed');
});

test('scope: the choice (and grouping and folder) persists with the layout; a bad saved value is the default', () => {
  seed({ ids, projects: [project('p1', MAIN)] });
  const store = useAppStore.getState();
  store.setTeamScope('workspace');
  store.setTeamGroupBy('status');
  store.setTeamFolder('p1');
  const ui = useAppStore.getState().ui;
  assertEqual(ui.teamScope, 'workspace', 'scope set');
  const saved = pickPersistedLayout(ui);
  assertEqual(saved.panes.teamScope, 'workspace', 'scope persisted');
  assertEqual(saved.panes.teamGroupBy, 'status', 'grouping persisted');
  assertEqual(saved.panes.teamFolderId, 'p1', 'folder persisted');
  // A value outside the menu is ignored.
  store.setTeamGroupBy('bogus' as never);
  assertEqual(useAppStore.getState().ui.teamGroupBy, 'status', 'an unknown grouping is refused');
  cancelPersistLayout();

  // Round trip through config.layout.
  useAppStore.setState({ ui: initialUiState() });
  useAppStore.getState().hydrateFromConfig({ layout: saved } as unknown as AppConfig);
  const back = useAppStore.getState().ui;
  assertEqual(back.teamScope, 'workspace', 'scope restored');
  assertEqual(back.teamGroupBy, 'status', 'grouping restored');
  assertEqual(back.teamFolderId, 'p1', 'folder restored');

  useAppStore.setState({ ui: initialUiState() });
  useAppStore.getState().hydrateFromConfig({
    layout: { panes: { teamScope: 'everything', teamGroupBy: 42, teamFolderId: '' } },
  } as unknown as AppConfig);
  const fallback = useAppStore.getState().ui;
  assertEqual(fallback.teamScope, 'all', 'bad scope → default');
  assertEqual(fallback.teamGroupBy, 'repository', 'bad grouping → default');
  assertEqual(fallback.teamFolderId, null, 'empty folder → none');
  cancelPersistLayout();
});

test('scope: "This workspace" means the chosen folder, else the first; the pane header says which', () => {
  const projects = [project('p1', MAIN), project('p2', GADGET)];
  assertEqual(scopeFolder(projects, null)?.id, 'p1', 'none chosen → the first');
  assertEqual(scopeFolder(projects, 'p2')?.id, 'p2', 'the chosen one');
  assertEqual(scopeFolder(projects, 'removed')?.id, 'p1', 'a removed choice → the first');
  assertEqual(scopeFolder([], 'p1'), null, 'no folder');

  seed({ ids, projects, ui: { teamScope: 'workspace', teamFolderId: 'p2' } });
  const sidebar = renderSidebar();
  assert(/class="team-scope-workspace" aria-pressed="true"/.test(sidebar), 'This workspace is pressed');
  assertDeepEqual(renderedGroups(sidebar), ['origin:github.com/Acme/Gadget'], 'only the gadget repository');
  const pane = renderPane();
  assert(textOf(pane).includes(ko('team.scope.of', { name: 'gadget' })), 'the pane header names the workspace');
  assert(pane.includes('data-scope="workspace"'), 'the pane header reflects the scope');

  seed({ ids, projects });
  assert(textOf(renderPane()).includes(ko('team.scope.all')), 'all workspaces in the pane header');

  // A folder with no team in its repository.
  seed({ ids, projects: [project('p3', '/Users/u/empty')], ui: { teamScope: 'workspace' } });
  assert(textOf(renderSidebar()).includes(ko('team.scope.empty')), 'says no team is in this workspace');
});

// ---------------------------------------------------------------------------
// 2 + 3. grouping, worktree-aware
// ---------------------------------------------------------------------------

test('worktree-aware: a lead in a linked worktree, a lead in a data-root member worktree and a second clone are one repository', () => {
  const groups = groupTeams(ALL, 'repository', ids);
  const widget = groups.find((g) => g.key === ORIGIN);
  assert(widget !== undefined, 'the widget group exists');
  assertDeepEqual(names(widget?.teams ?? []), ['main', 'linked', 'nested', 'clone'], '(a) (b) (c) and the main checkout');
  assertEqual(groups.length, 3, 'widget, gadget, the plain folder — and nothing else');
  assertDeepEqual(widget?.paths, [CLONE, MAIN], 'the tooltip lists both checkouts');

  // Every member worktree follows its lead's repository.
  for (const view of [T_MAIN, T_LINKED, T_CLONE]) {
    for (const m of view.state.members) {
      assertEqual(memberWorktreeFollow(view, m, ids), 'same', `${view.teamId}/${m.name}`);
      assertEqual(repoKeyOf(m.worktree.path, ids), ORIGIN, 'a member worktree is the same repository');
    }
  }

  // "This workspace" with the main checkout selected includes all three.
  const shown = teamsInScope(orderedTeams(new Map(ALL.map((v) => [v.teamId, v]))), 'workspace', project('p1', MAIN), ids);
  for (const id of ['main', 'linked', 'nested', 'clone']) assert(names(shown).includes(id), `${id} is in this workspace`);
  assert(!names(shown).includes('gadget') && !names(shown).includes('plain'), 'other repositories are not');

  // The same from a linked-worktree folder or the clone folder.
  for (const folderPath of [LINKED, CLONE]) {
    const fromThere = teamsInScope(ALL, 'workspace', project('px', folderPath), ids);
    assertDeepEqual(names(fromThere), ['main', 'linked', 'nested', 'clone'], `from ${folderPath}`);
  }

  // Rendered: the main checkout selected, This workspace on.
  seed({ ids, projects: [project('p1', MAIN)], ui: { teamScope: 'workspace' } });
  const html = renderSidebar();
  assertDeepEqual(renderedGroups(html), [ORIGIN], 'one repository group');
  for (const id of ['main', 'linked', 'nested', 'clone']) assert(html.includes(`>${id}</span>`), `${id} listed`);
});

test('without an identity yet each team is its own group; when the identities arrive the teams regroup', () => {
  const views = [T_MAIN, T_LINKED, T_CLONE];
  const provisional = groupTeams(views, 'repository', new Map());
  assertEqual(provisional.length, 3, 'one provisional group per path');
  assertDeepEqual(provisional.map((g) => g.key), [`path:${MAIN}`, `path:${LINKED}`, `path:${CLONE}`], 'keyed by path');
  assertDeepEqual(provisional.map((g) => g.label.kind), ['folder', 'folder', 'folder'], 'no repository claim yet');

  // An `unknown` answer (git too slow) keeps the provisional grouping.
  const slow = new Map([[MAIN, alone(MAIN, 'unknown')]]);
  assertEqual(groupTeams(views, 'repository', slow).length, 3, 'unknown is not an identity');

  // The answers arrive in the slice: the same map is kept when nothing changes.
  const merged = mergeIdentities(new Map(), [...ids.values()]);
  assert(mergeIdentities(merged, [repo(MAIN)]) === merged, 'an unchanged answer keeps the map');
  assert(mergeIdentities(merged, [repo(MAIN, { branch: 'other' })]) !== merged, 'a changed answer replaces it');

  seed({ views, ids: new Map() });
  assertEqual(renderedGroups(renderSidebar()).length, 3, 'rendered at once, provisionally');
  useAppStore.setState({ team: { ...useAppStore.getState().team, identities: merged } });
  assertDeepEqual(renderedGroups(renderSidebar()), [ORIGIN], 'regrouped into one repository');

  // Provisional "This workspace": by path until both identities are known.
  const inside = teamAt('sub', `${MAIN}/pkg`);
  assertDeepEqual(names(teamsInScope([inside, T_GADGET], 'workspace', project('p1', MAIN), new Map())), ['sub'], 'a subfolder of the folder');
  assertDeepEqual(
    names(teamsInScope([T_LINKED], 'workspace', project('p1', LINKED, MAIN), new Map())),
    ['linked'],
    'the folder itself',
  );
});

test('grouping modes: Workspace folder keeps each lead folder apart; Status uses plain words', () => {
  const byFolder = groupTeams(ALL, 'folder', ids);
  assertEqual(byFolder.length, ALL.length, 'one group per lead folder');
  const linked = byFolder.find((g) => g.key === `folder:${LINKED}`);
  assertEqual(linked?.label.kind, 'worktree', 'a lead in a linked worktree is labelled as one');
  assertEqual(
    groupLabelText(linked!.label, (k, p) => ko(k, p)),
    ko('team.group.worktree', { repo: 'widget', branch: 'feature/x' }),
    '"<repo> · worktree <branch>"',
  );
  const nested = byFolder.find((g) => g.key === `folder:${DATA_MEMBER}`);
  assertEqual(
    groupLabelText(nested!.label, (k, p) => ko(k, p)),
    ko('team.group.worktree', { repo: 'widget', branch: 'cco/demo-hi1/alice' }),
    'a data-root worktree names its repository',
  );
  const sub = groupTeams([teamAt('s', `${MAIN}/pkg`)], 'folder', new Map([[`${MAIN}/pkg`, repo(`${MAIN}/pkg`, { toplevel: MAIN })]]));
  assertEqual(sub[0].label.name, 'widget/pkg', 'a subfolder is named inside its repository');

  const suspendedTeam = { ...teamAt('gone', MAIN), believed: false };
  const ended = teamAt('ended', MAIN, { status: 'ended' });
  const damaged = teamAt('broken', MAIN, { status: 'damaged' });
  const readonly = teamAt('ro', MAIN, { status: 'readonly' });
  assertEqual(statusGroupOf(suspendedTeam), 'suspended', 'an active team whose host is gone is suspended');
  const byStatus = groupTeams([readonly, damaged, ended, suspendedTeam, T_MAIN], 'status', ids);
  assertDeepEqual(
    byStatus.map((g) => g.key),
    ['status:active', 'status:suspended', 'status:ended', 'status:damaged', 'status:other'],
    'fixed order',
  );
  assertDeepEqual(
    byStatus.map((g) => groupLabelText(g.label, (k, p) => ko(k, p))),
    ['활성', '일시 중지됨 (호스트 없음)', '종료됨', '손상됨', '기타'],
    'plain words',
  );

  seed({ ids, ui: { teamGroupBy: 'status' } });
  const html = renderSidebar();
  assert(/<select[^>]*aria-label="묶는 기준"/.test(html), 'the Group by menu has a name');
  assert(/<option value="status" selected="">상태<\/option>/.test(html), 'Status is selected');
  assert(html.includes('>저장소</option>') && html.includes('>워크스페이스 폴더</option>'), 'all three choices');
  assertDeepEqual(renderedGroups(html), ['status:active'], 'every fixture team is active');
});

// ---------------------------------------------------------------------------
// 5. labels
// ---------------------------------------------------------------------------

test('labels: the repository basename; same-named repositories get their parent folder; not-git folders say so', () => {
  const other = '/Users/u/mirror/widget';
  const local = new Map(ids);
  local.set(other, repo(other, { key: `dir:${other}/.git`, origin: null, commonDir: `${other}/.git`, root: other }));
  const groups = groupTeams([T_MAIN, teamAt('mirror', other), T_GADGET, T_PLAIN], 'repository', local);
  const t = (k: Parameters<typeof translate>[1], p?: Record<string, string | number>) => ko(k, p);
  assertDeepEqual(
    groups.map((g) => groupLabelText(g.label, t)),
    ['widget (src)', 'widget (mirror)', 'gadget', ko('team.group.notGit', { name: 'notes' })],
    'basename, disambiguated, and the plain folder',
  );
  // A gone path is "not a git folder" too.
  const gone = groupTeams([teamAt('g', '/Users/u/removed')], 'repository', new Map([['/Users/u/removed', alone('/Users/u/removed', 'gone')]]));
  assertEqual(gone[0].label.kind, 'notGit', 'a gone folder');

  // The tooltip carries the full path; the header carries only the name.
  seed({ ids, views: [T_MAIN] });
  const html = renderSidebar();
  assert(html.includes(`title="${MAIN}"`), 'the full path is the tooltip');
  assert(/<span class="ell grow strong">widget<\/span>/.test(html), 'the basename is the label');

  // Hostile names are flattened.
  const evil = '/Users/u/‮evil\nname';
  const hostile = groupTeams([teamAt('h', evil)], 'repository', new Map([[evil, alone(evil, 'notRepo')]]));
  const text = groupLabelText(hostile[0].label, t);
  assert(!text.includes('‮') && !text.includes('\n'), 'no bidi override, no newline');
});

// ---------------------------------------------------------------------------
// 6. members and the inspector
// ---------------------------------------------------------------------------

test('members: short branch (unless it is the name) with the full one in the tooltip, a worktree icon, and a warning when it is gone', () => {
  assertEqual(memberBranchShort('cco/demo-hi1/alice'), 'alice', 'cco/<team>-<suffix>/<member> → member');
  assertEqual(memberBranchShort('feature/x'), 'feature/x', 'any other branch as it is');
  assertEqual(memberBranchShort('cco/demo-hi1/sub/alice'), 'sub/alice', 'a nested member branch keeps its tail');

  const alice = T_MAIN.state.members[0];
  const missing = new Map(ids);
  missing.set(alice.worktree.path, alone(alice.worktree.path, 'gone'));
  assertEqual(memberWorktreeFollow(T_MAIN, alice, missing), 'missing', 'gone → missing');
  const foreign = new Map(ids);
  foreign.set(alice.worktree.path, repo(alice.worktree.path, { key: 'dir:/elsewhere/.git' }));
  assertEqual(memberWorktreeFollow(T_MAIN, alice, foreign), 'other', 'another repository');
  assertEqual(memberWorktreeFollow(T_MAIN, alice, new Map()), 'pending', 'not known yet');

  seed({ ids: missing, views: [T_MAIN] });
  const html = renderSidebar();
  assert(html.includes(ko('team.worktree.branchTitle', { branch: alice.worktree.branch })), 'full branch in the tooltip');
  assert(!/team-sb-branch"[^>]*>alice<\/span>/.test(html), 'a short branch equal to the member name is not repeated');
  assert(html.includes(`data-follow="missing"`) && html.includes(`aria-label="${ko('team.worktree.missing')}"`), 'gone worktree warned');
  assert(html.includes(`data-follow="same"`) && html.includes(`aria-label="${ko('team.worktree.marker')}"`), 'the others carry the marker');
  assert(!textOf(html).includes(ko('team.worktree.marker')), 'the marker is an icon, its words only in the tooltip');

  // A branch that is not the member's own name is shown, short.
  const other = teamAt('br', MAIN);
  other.state.members[0].worktree = { ...other.state.members[0].worktree, branch: 'cco/demo-hi1/feature-x' };
  seed({ ids: worldIdentities([other]), views: [other] });
  assert(/team-sb-branch"[^>]*>feature-x<\/span>/.test(renderSidebar()), 'a different short branch is shown');
  assert(!/<input\b/.test(html), 'no text field');
});

test('a team whose lead runs in a linked worktree says so in its header', () => {
  assertDeepEqual(leadWorktree(T_LINKED, ids), { branch: 'feature/x' }, 'linked lead');
  assertEqual(leadWorktree(T_MAIN, ids), null, 'main checkout');
  assertEqual(leadWorktree(T_LINKED, new Map()), null, 'nothing claimed before the identity arrives');
  seed({ ids, views: [T_LINKED] });
  const pane = renderPane();
  assert(textOf(pane).includes(ko('team.leadInWorktree', { branch: 'feature/x' })), 'in the pane header');
  assert(renderSidebar().includes(ko('team.leadInWorktree', { branch: 'feature/x' })), 'in the sidebar row tooltip');
});

test('the inspector shows the worktree path, branch, ahead count and base commit, with Reveal in Finder', async () => {
  const view = teamAt('insp', MAIN);
  const alice = view.state.members[0];
  alice.worktree = { ...alice.worktree, ahead: 3, baseCommit: 'abcdef0123456789abcdef0123456789abcdef01' };
  seed({ ids: worldIdentities([view]), views: [view], team: { selectedTeamId: 'insp', selectedMember: alice.name } });
  const html = renderNode(<TeamMemberInspector />);
  const text = textOf(html);
  assert(text.includes(alice.worktree.path), 'the worktree path');
  assert(text.includes(alice.worktree.branch), 'the full branch');
  assert(text.includes(ko('team.ahead', { n: 3 })), 'ahead count');
  assert(text.includes('abcdef012345') && !text.includes('abcdef0123456789abcdef'), 'the base commit, short');
  assert(html.includes('title="abcdef0123456789abcdef0123456789abcdef01"'), 'the full base commit in the tooltip');
  assert(text.includes(ko('team.worktree.reveal')), 'Reveal in Finder');
  assertEqual(shortSha(null), '', 'no sha');

  // Reveal calls app_reveal with the path; a refusal becomes a toast.
  const original = cmd.appReveal;
  const asked: string[] = [];
  try {
    cmd.appReveal = async (path: string) => {
      asked.push(path);
    };
    await revealWorktree(alice.worktree.path);
    assertDeepEqual(asked, [alice.worktree.path], 'app_reveal(path)');
    cmd.appReveal = async () => {
      throw { code: 'NOT_FOUND', key: 'error.notFound', params: {}, detail: null, retryable: false };
    };
    const before = useAppStore.getState().toasts.length;
    await revealWorktree('/gone');
    assertEqual(useAppStore.getState().toasts.length, before + 1, 'a toast, not a throw');
  } finally {
    cmd.appReveal = original;
    useAppStore.getState().clearToasts();
  }
});

// ---------------------------------------------------------------------------
// 7. project folders
// ---------------------------------------------------------------------------

test('project folders are grouped by the same repository identity; a linked worktree folder sits under its repo', () => {
  const projects = [project('p1', MAIN), project('p2', LINKED, MAIN), project('p3', CLONE), project('p4', GADGET)];
  const groups = groupFolders(projects, ids);
  assertEqual(groups.length, 2, 'widget (three folders) and gadget');
  assertDeepEqual(groups[0].folders.map((f) => f.project.id), ['p1', 'p2', 'p3'], 'in the roster order');
  assertDeepEqual(groups[0].folders[1].worktree, { branch: 'feature/x' }, 'the linked worktree folder');

  seed({ ids, projects });
  const html = renderSidebar();
  const folders = html.split('team-sb-folders-hdr')[1];
  assert(textOf(folders).includes(ko('team.folders.worktree', { branch: 'feature/x' })), 'worktree <branch>');
  assertEqual((folders.match(/data-nested="true"/g) ?? []).length, 3, 'three folders under one header');
  assert(/class="team-sb-folder-pick[^"]*" aria-pressed="true"/.test(folders), 'the folder This workspace means is marked');

  assertDeepEqual(
    identityPaths([T_MAIN], [project('p1', MAIN)]),
    [MAIN, ...T_MAIN.state.members.map((m) => m.worktree.path)].sort(),
    'workspaces, member worktrees and folders are asked once each',
  );
});

// ---------------------------------------------------------------------------
// 8. the slice asks only for what it does not know
// ---------------------------------------------------------------------------

test('the slice asks Rust for unknown paths only, in chunks, and survives a backend without the command', async () => {
  const original = cmd.repoIdentity;
  const calls: string[][] = [];
  try {
    cmd.repoIdentity = async (paths: string[]) => {
      calls.push(paths);
      return paths.map((p) => (ids.get(p) ?? alone(p, 'notRepo')));
    };
    seed({ ids: new Map() });
    const many = Array.from({ length: IDENTITY_CHUNK + 4 }, (_, i) => `/p/${i}`);
    await useAppStore.getState().resolveRepoIdentities([MAIN, MAIN, '', ...many]);
    assertDeepEqual(calls.map((c) => c.length), [IDENTITY_CHUNK, 5], 'deduplicated, empty dropped, chunked');
    assertEqual(useAppStore.getState().team.identities.get(MAIN)?.key, ORIGIN, 'stored');
    await useAppStore.getState().resolveRepoIdentities([MAIN]);
    assertEqual(calls.length, 2, 'a fresh answer is not asked again');

    cmd.repoIdentity = async () => {
      throw { code: 'INTERNAL', key: 'error.internal', params: {}, detail: 'unknown command repo_identity', retryable: false };
    };
    await useAppStore.getState().resolveRepoIdentities(['/new/path']);
    assertEqual(useAppStore.getState().team.identities.has('/new/path'), false, 'nothing stored, nothing thrown');
    assertEqual(useAppStore.getState().team.error, null, 'and no error shown');
  } finally {
    cmd.repoIdentity = original;
  }
});
