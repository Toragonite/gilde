/**
 * Sidebar unit tests (T21).
 *
 * Three things are worth pinning here, and they are exactly the three that a
 * type check cannot see:
 *
 *  1. the tree is a pure function of the graph projection — one input, no fetch;
 *  2. the slug preview agrees with `src-tauri/src/worktree/naming.rs`, because
 *     it is a promise the dialog makes about what Rust will create;
 *  3. the removal card offers exactly three modes and disables the ones the
 *     backend refused, with the reason.
 */

import { renderToStaticMarkup } from 'react-dom/server';

import { assert, assertDeepEqual, assertEqual } from '../test/harness';
import { laneTest as test, laneUiState } from '../test/lanes';
import { emptyGraph, emptyTotals } from '../store/graphSlice';
import type { GraphView } from '../store/graphSlice';
import { useAppStore } from '../store/useAppStore';
import type { AppStore } from '../store/useAppStore';
import {
  ALIAS_PROVIDER_OPTIONS,
  applyWorktreeChanged,
  edgeId,
  initialWorktreeState,
  resetFolderPicker,
  setFolderPicker,
  previewBranch,
  previewSlug,
  previewWorktreePath,
  projectRoster,
  slugBody,
  slugBodyOf,
  slugIssue,
} from '../store/worktreeSlice';
import type { RosterEntry } from '../store/worktreeSlice';
import { formatBytes } from '../lib/format';
import type { GraphNode, Project, RemoveMode, RemovePreflight, Worktree } from '../lib/ipc';
import { translate } from '../i18n';
import { Inspector } from '../shell/Inspector';
import { buildRows, rowOffsets, visibleRange } from './ProjectTree';
import { Sidebar } from './Sidebar';
import { sessionTitle } from './SessionRow';
import { isNarrowRow, middleEllipsis, worktreeMeta } from './WorktreeRow';
import { NewWorktreeForm } from './NewWorktreeDialog';
import { REMOVE_MODES, RemoveWorktreeCard, disabledReason } from '../dialogs/DirtyWorktreeDialog';

// ---------------------------------------------------------------------------
// Fixtures — shaped exactly like what Rust sends, never like demo data
// ---------------------------------------------------------------------------

function node(id: string, kind: GraphNode['kind'], label: string, parentId: string | null = null): GraphNode {
  return {
    id,
    kind,
    parentId,
    label,
    status: kind === 'worktree' ? 'clean' : 'ok',
    badges: [],
    metrics: {},
    meta: {},
    updatedAt: 1,
  };
}

function graphWith(nodes: GraphNode[]): GraphView {
  const graph = emptyGraph();
  for (const n of nodes) graph.nodes.set(n.id, n);
  return graph;
}

function project(id: string, name: string): Project {
  return {
    id,
    path: `/repo/${name}`,
    canonicalPath: `/repo/${name}`,
    repoSlug: `${name}-0badc0de`,
    name,
    color: '',
    defaultBaseRef: 'main',
    addedAt: 1,
    lastOpenedAt: null,
  };
}

function worktree(id: string, projectId: string, slug: string, locked = false): Worktree {
  return {
    id,
    projectId,
    slug,
    branch: `gilde/${slug}`,
    path: `/wt/${slug}`,
    baseSha: 'deadbee',
    origin: 'created',
    createdAt: 1,
    removedAt: null,
    locked,
    verifiedAt: null,
    postCreateCmd: null,
  };
}

function seedStore(patch: Partial<AppStore>): void {
  useAppStore.setState({
    graph: emptyGraph(),
    ui: laneUiState(),
    toasts: [],
    agents: [],
    sidecar: null,
    sessions: new Map(),
    worktree: initialWorktreeState(),
    ...patch,
  });
  Object.assign(useAppStore.getInitialState(), useAppStore.getState());
}

// ---------------------------------------------------------------------------
// 1. The tree is a projection of the graph and nothing else (acceptance 1)
// ---------------------------------------------------------------------------

test('buildRows flattens project → worktree → session from the graph alone', () => {
  const graph = graphWith([
    node('project:p1', 'project', 'gilde-code'),
    node('worktree:w1', 'worktree', 'pty-manager', 'project:p1'),
    node('session:s1', 'session', 'ring buffer', 'worktree:w1'),
    node('run:r1', 'run', 'headless codex', 'worktree:w1'),
    node('worktree:w2', 'worktree', 'sidecar-rpc', 'project:p1'),
  ]);

  const rows = buildRows(graph, {});
  assertDeepEqual(
    rows.map((row) => `${row.kind}:${row.node.label}`),
    [
      'project:gilde-code',
      'worktree:pty-manager',
      'session:headless codex',
      'session:ring buffer',
      'worktree:sidecar-rpc',
    ],
    'both lanes hang under the worktree, sorted by label',
  );
  assertEqual(rows[0].kind === 'project' ? rows[0].children : -1, 2, 'the project counts worktrees');
  assertEqual(rows[1].kind === 'worktree' ? rows[1].children : -1, 2, 'the worktree counts children');
});

test('collapsing hides children but keeps the counts honest', () => {
  const graph = graphWith([
    node('project:p1', 'project', 'p'),
    node('worktree:w1', 'worktree', 'w', 'project:p1'),
    node('session:s1', 'session', 's', 'worktree:w1'),
  ]);
  const rows = buildRows(graph, { 'worktree:w1': true });
  assertEqual(rows.length, 2, 'the session is hidden');
  assertEqual(rows[1].kind === 'worktree' ? rows[1].children : -1, 1, 'the count still says 1');
});

test('the filter keeps the ancestors of a matching session', () => {
  const graph = graphWith([
    node('project:p1', 'project', 'p'),
    node('worktree:w1', 'worktree', 'pty-manager', 'project:p1'),
    node('session:s1', 'session', 'ring buffer', 'worktree:w1'),
    node('worktree:w2', 'worktree', 'sidecar-rpc', 'project:p1'),
  ]);
  const rows = buildRows(graph, {}, 'ring');
  assertDeepEqual(
    rows.map((row) => row.node.id),
    ['project:p1', 'worktree:w1', 'session:s1'],
    'the unmatched worktree is dropped, the matched path is kept',
  );
});

// ---------------------------------------------------------------------------
// 2. Windowing (acceptance 8)
// ---------------------------------------------------------------------------

test('row offsets and the visible window respect the two row heights', () => {
  const graph = emptyGraph();
  graph.nodes.set('project:p', node('project:p', 'project', 'p'));
  for (let i = 0; i < 3; i++) {
    const w = node(`worktree:w${i}`, 'worktree', `w${i}`, 'project:p');
    graph.nodes.set(w.id, w);
    const s = node(`session:s${i}`, 'session', `s${i}`, w.id);
    graph.nodes.set(s.id, s);
  }
  const rows = buildRows(graph, {});
  const { offsets, total } = rowOffsets(rows);
  assertEqual(offsets[0], 0, 'the first row starts at 0');
  assertEqual(offsets[1], 26, 'a project row is 26px');
  assertEqual(offsets[2], 52, 'a worktree row is 26px');
  assertEqual(offsets[3], 94, 'a session row is 42px');
  assertEqual(total, 26 + 3 * (26 + 42), 'the total is the sum of the row heights');

  const [first, last] = visibleRange(offsets, total, 0, 60, 0);
  assertEqual(first, 0, 'the window starts at the top');
  assert(last >= 3 && last <= rows.length, 'the window covers the visible rows only');

  const [scrolledFirst] = visibleRange(offsets, total, 100, 60, 0);
  assertEqual(scrolledFirst, 3, 'binary search lands on the row under the scroll offset');
});

// ---------------------------------------------------------------------------
// 3. Slug preview mirrors naming.rs (acceptance 5)
// ---------------------------------------------------------------------------

test('slugBody matches the naming.rs sanitize pipeline', () => {
  assertEqual(slugBody('Fix PTY Resize'), 'fix-pty-resize', 'lowercased and dashed');
  assertEqual(slugBody('  ///lots__of---junk!!  '), 'lots-of-junk', 'runs collapse and edges trim');
  assertEqual(slugBody('Café Déjà-Vu'), 'cafe-deja-vu', 'NFKD folds the accents');
  assertEqual(slugBody('Ｈｅｌｌｏ１２３'), 'hello123', 'fullwidth folds to ASCII');
  assertEqual(slugBody('한글만'), '', 'Hangul drops out, exactly as in Rust');
  assertEqual(slugBody('v1.2.3'), 'v1-2-3', 'dots are separators');
  assertEqual(slugBody('a'.repeat(200)).length, 40, 'the body is capped at MAX_SLUG_BODY');
  assertEqual(slugBody(`${'b'.repeat(39)}-tail`), 'b'.repeat(39), 'a cut on a separator leaves no dash');
});

test('the preview shows the slug, the branch and the central path', () => {
  assertEqual(previewSlug('Fix PTY Resize'), 'fix-pty-resize-······', 'the random half is a placeholder');
  assertEqual(previewBranch('Fix PTY Resize'), 'gilde/fix-pty-resize-······', 'branches live under gilde/');
  assertEqual(previewSlug('한글만'), 'task-······', 'the Rust fallback body is used');
  assertEqual(
    previewWorktreePath('/Users/x/.gilde/worktrees/', 'api-0badc0de', 'Fix Login'),
    '/Users/x/.gilde/worktrees/api-0badc0de/fix-login-······',
    'the path is the central one, outside the repository',
  );
});

test('slugBodyOf strips only a real short id', () => {
  assertEqual(slugBodyOf('fix-login-ab12cd'), 'fix-login', 'six hex characters are the short id');
  assertEqual(slugBodyOf('fix-login-tail'), 'fix-login-tail', 'a word is not a short id');
});

test('submission is blocked with a reason, including a duplicate name', () => {
  assertEqual(slugIssue('', []), 'empty', 'an empty title is refused');
  assertEqual(slugIssue('한글만', []), 'unsupported', 'a title with no slug is refused');
  assertEqual(slugIssue('Fix login', ['fix-login-ab12cd']), 'duplicate', 'a taken name is refused');
  assertEqual(slugIssue('Fix login', ['other-ab12cd']), null, 'a free name passes');
});

test('cursor is offered unverified and unchecked', () => {
  const cursor = ALIAS_PROVIDER_OPTIONS.find((o) => o.key === 'cursor-worktree');
  assert(cursor !== undefined, 'the cursor convention is offered');
  assertEqual(cursor?.verified, false, 'it is marked unverified (DESIGN §7-6)');
  assertEqual(cursor?.defaultOn, false, 'and it starts off');
  assertEqual(
    ALIAS_PROVIDER_OPTIONS.find((o) => o.key === 'claude-w')?.defaultOn,
    true,
    'the verified claude convention is on by default',
  );
});

// ---------------------------------------------------------------------------
// 4. The projection folds into the one graph store
// ---------------------------------------------------------------------------

test('projectRoster writes nodes and edges in the shape model/graph.rs uses', () => {
  const roster: RosterEntry[] = [
    { project: project('p1', 'api'), worktrees: [worktree('w1', 'p1', 'fix-login-ab12cd', true)] },
  ];
  const { nodes, edges } = projectRoster(emptyGraph(), roster, 42);

  const wt = nodes.get('worktree:w1');
  assert(wt !== undefined, 'the worktree node exists');
  assertEqual(wt?.parentId, 'project:p1', 'it hangs under its project');
  assertEqual(wt?.sublabel, 'gilde/fix-login-ab12cd', 'the branch is the sublabel');
  assert(wt?.badges.includes('locked') === true, 'the lock column becomes the locked badge');
  assert(edges.has(edgeId('contains', 'project:p1', 'worktree:w1')), 'the contains edge id matches Rust');
});

test('a re-projection drops what the roster no longer has and keeps live state', () => {
  const first = projectRoster(
    emptyGraph(),
    [{ project: project('p1', 'api'), worktrees: [worktree('w1', 'p1', 'a-ab12cd'), worktree('w2', 'p1', 'b-ab12cd')] }],
    1,
  );
  const graph: GraphView = { ...emptyGraph(), nodes: first.nodes, edges: first.edges };
  // A live session and a status the fs watcher pushed.
  graph.nodes.set('session:s1', node('session:s1', 'session', 's', 'worktree:w1'));
  const w1 = graph.nodes.get('worktree:w1');
  if (w1) graph.nodes.set('worktree:w1', { ...w1, status: 'dirty', metrics: { dirtyFiles: 3 } });

  const second = projectRoster(
    graph,
    [{ project: project('p1', 'api'), worktrees: [worktree('w1', 'p1', 'a-ab12cd')] }],
    2,
  );
  assert(!second.nodes.has('worktree:w2'), 'the removed worktree is gone');
  assert(second.nodes.has('session:s1'), 'another lane is untouched');
  assertEqual(second.nodes.get('worktree:w1')?.status, 'dirty', 'live status survives a refresh');
  assertEqual(second.nodes.get('worktree:w1')?.metrics.dirtyFiles, 3, 'live metrics survive too');
});

test('gilde://worktree/changed becomes node metrics, status and badges', () => {
  const { nodes, edges } = projectRoster(
    emptyGraph(),
    [{ project: project('p1', 'api'), worktrees: [worktree('w1', 'p1', 'a-ab12cd')] }],
    1,
  );
  const graph: GraphView = { ...emptyGraph(), nodes, edges };
  const next = applyWorktreeChanged(graph, {
    worktreeId: 'w1',
    dirty: 12,
    added: 412,
    removed: 38,
    ahead: 3,
    behind: 0,
    at: 99,
  });
  const wt = next.nodes.get('worktree:w1');
  assertEqual(wt?.status, 'dirty', 'a dirty tree reads dirty');
  assertEqual(wt?.metrics.dirtyFiles, 12, 'the dirty count lands on the node');
  assertEqual(wt?.metrics.insertions, 412, '+N lands on the node');
  assertEqual(wt?.metrics.deletions, 38, '−M lands on the node');
  assert(wt?.badges.includes('unmerged') === true, 'ahead > 0 raises the unmerged badge');
  assert(wt?.badges.includes('dirty') === true, 'and the dirty badge');
});

// ---------------------------------------------------------------------------
// 5. Removal modes (acceptance 6)
// ---------------------------------------------------------------------------

function preflight(patch: Partial<RemovePreflight>): RemovePreflight {
  return {
    dirtyFiles: 0,
    untracked: 0,
    unmergedCommits: 0,
    liveSessions: [],
    runningRuns: [],
    aliasLinks: [],
    foreignPids: [],
    allowedModes: ['keepBranch', 'commitThenRemove', 'archiveThenRemove'],
    ...patch,
  };
}

test('a mode the backend refused is disabled with the reason', () => {
  const dirty = preflight({ dirtyFiles: 12, allowedModes: ['commitThenRemove', 'archiveThenRemove'] });
  assertEqual(disabledReason('keepBranch', dirty), 'worktree.blocked.dirty', 'dirty blocks KeepBranch');
  assertEqual(disabledReason('commitThenRemove', dirty), null, 'commit stays available');

  const live = preflight({ liveSessions: ['s1'], allowedModes: [] });
  assertEqual(disabledReason('archiveThenRemove', live), 'worktree.blocked.sessions', 'a live session blocks every mode');
  assertEqual(disabledReason('keepBranch', null), 'common.loading', 'nothing is offered before preflight lands');
});

// ---------------------------------------------------------------------------
// 6. Static render — real store data, honest empty states, no fixture
// ---------------------------------------------------------------------------

test('the sidebar renders the graph projection and its Korean empty state', () => {
  seedStore({});
  const booting = renderToStaticMarkup(<Sidebar />);
  assert(booting.includes('불러오는 중…'), 'before the roster lands the sidebar says so, not "empty"');

  seedStore({ worktree: { ...initialWorktreeState(), loaded: true } });
  const empty = renderToStaticMarkup(<Sidebar />);
  assert(empty.includes('추가된 프로젝트가 없습니다.'), 'an empty projection shows the empty state');
  assert(empty.includes('프로젝트 추가'), 'one sentence, one action');
  assert(!empty.includes('undefined'), 'nothing rendered the string "undefined"');

  const wt = node('worktree:w1', 'worktree', 'pty-manager', 'project:p1');
  wt.sublabel = 'gilde/pty-manager-ab12cd';
  wt.status = 'dirty';
  wt.badges = ['locked', 'unmerged'];
  wt.metrics = { insertions: 412, deletions: 38, dirtyFiles: 12, ahead: 3 };
  const session = node('session:s1', 'session', 'ring buffer', 'worktree:w1');
  session.status = 'working';
  session.confidence = 0.9;
  session.meta = { vendor: 'claude' };
  session.metrics = { rssBytes: 1_200_000_000 };

  seedStore({
    graph: graphWith([node('project:p1', 'project', 'gilde-code'), wt, session]),
  });
  const html = renderToStaticMarkup(<Sidebar />);
  assert(html.includes('gilde-code'), 'the project row rendered');
  assert(html.includes('pty-manager'), 'the worktree name rendered');
  assert(html.includes('+412 −38'), 'the diff stat rendered');
  // BACKLOG 7: the rest of the meta is one tooltip, not six chips on the row.
  assert(
    html.includes(
      'title="gilde/pty-manager-ab12cd · +412 −38 · dirty 12 · 앞선 커밋 3 · 세션 1개 · 잠김 · 미병합 · 변경 있음"',
    ),
    'the facts that no longer fit are the row tooltip, in full',
  );
  assert(html.includes('ring buffer'), 'the session title is the first line');
  assert(html.includes('>Claude Code<'), 'the agent is a text badge, not a logo');
  assert(html.includes(formatBytes(1_200_000_000, 'ko')), 'the RSS chip rendered');
  assert(html.includes('data-status="working"'), 'the shared status dot rendered');
});

// ---------------------------------------------------------------------------
// 6a. M1.5 BACKLOG 7 — the worktree name is never pushed off its own row
// ---------------------------------------------------------------------------

/**
 * The worktree row as the sidebar renders it, isolated from the rest.
 *
 * `sidebarWidth` is a parameter because the row is a function of it now
 * (BACKLOG 15d): a wide sidebar prints the status word and the whole name, a
 * narrow one drops the word and elides the name in the middle. The default is
 * wide, so a test that is not about width is not silently testing the narrow
 * layout.
 */
function worktreeRowHtml(
  patch: (n: GraphNode) => void,
  sessions: GraphNode[] = [],
  sidebarWidth = 420,
  label = 'reader-thread-를-ring-buffer로-교체',
): string {
  const wt = node('worktree:w1', 'worktree', label, 'project:p1');
  patch(wt);
  seedStore({
    graph: graphWith([node('project:p1', 'project', 'gilde-code'), wt, ...sessions]),
    ui: { ...laneUiState(), sidebarWidth },
  });
  return renderToStaticMarkup(<Sidebar />);
}

test('a long worktree name keeps the row and the meta collapses to one tooltip', () => {
  const html = worktreeRowHtml((wt) => {
    wt.sublabel = 'gilde/reader-thread-ab12cd';
    wt.status = 'dirty';
    wt.badges = ['locked', 'unmerged'];
    wt.metrics = { insertions: 792, deletions: 115, dirtyFiles: 7, ahead: 8 };
  });

  // The name is present, whole, and in the flexing element — the CSS gives
  // `.sb-wt-name` `flex: 1 1 auto; min-width: 0` and the ellipsis, and every
  // sibling on the row is `flex: none`.
  //
  // It also carries its own `title`: it is the one element on the row that
  // ellipsises, and the row's tooltip below starts at the branch and never
  // repeats the name — so without this a truncated name could not be read at
  // all (M1.5 integration, BACKLOG 7).
  assert(
    html.includes(
      'class="mono s12 sb-wt-name" title="reader-thread-를-ring-buffer로-교체">reader-thread-를-ring-buffer로-교체<',
    ),
    'the name renders in the element that owns the row width, and is its own tooltip',
  );

  // Exactly two meta items survive on the row: the change count and the status.
  assert(html.includes('+792 −115'), 'the change count stays on the row');
  assert(html.includes('>변경 있음<'), 'so does the status word');
  assert(!html.includes('>↑8<'), 'the ahead count is off the row');
  assert(!html.includes('dirty 7<'), 'the dirty file count is off the row');
  assert(!html.includes('>미병합<'), 'the unmerged badge is off the row');
  assert(!html.includes('>잠김<'), 'the lock is off the row');

  // …and none of it is lost: the row's tooltip states every one of them.
  const tooltip = worktreeMeta(
    { ...node('worktree:w1', 'worktree', 'x'), sublabel: 'gilde/reader-thread-ab12cd', status: 'dirty', badges: ['locked', 'unmerged'], metrics: { insertions: 792, deletions: 115, dirtyFiles: 7, ahead: 8 } },
    0,
    (key, params) => translate('ko', key, params),
    'ko',
  );
  assertEqual(
    tooltip,
    'gilde/reader-thread-ab12cd · +792 −115 · dirty 7 · 앞선 커밋 8 · 잠김 · 미병합 · 변경 있음',
    'the tooltip carries branch, change, dirty, ahead, lock and unmerged',
  );
  assert(html.includes(`title="${tooltip}"`), 'and the row is that tooltip');
});

// ---------------------------------------------------------------------------
// 6a-bis. M1.5 BACKLOG 15d — a 300 px sidebar still shows which worktree it is
//
// BACKLOG 7 got the meta down to two items and the name was still cut:
// `readme-date-cl…`, which is what every worktree sharing that prefix looks
// like. The suffix is the identity — `…-claude-676a02` — so the row gives the
// name the width of the status word and elides in the middle rather than at
// the end.
// ---------------------------------------------------------------------------

test('middleEllipsis keeps the tail, which is the half that identifies a worktree', () => {
  assertEqual(
    middleEllipsis('readme-date-sync-x-claude-676a02', 24),
    'readme-…-x-claude-676a02',
    'the sha and the agent survive; the middle is what goes',
  );
  assertEqual(
    middleEllipsis('pty-manager', 24),
    'pty-manager',
    'a name that fits is never touched',
  );
  assertEqual(
    Array.from(middleEllipsis('reader-thread-를-ring-buffer로-교체', 24)).length,
    24,
    'the budget is counted in code points, not UTF-16 units',
  );
  assert(isNarrowRow(300), 'the default 300 px sidebar is a narrow row');
  assert(!isNarrowRow(420), 'a widened one is not');
});

test('a 32-character worktree name keeps its suffix in a 300 px sidebar', () => {
  const html = worktreeRowHtml(
    (wt) => {
      wt.sublabel = 'gilde/readme-date-sync-x-claude-676a02';
      wt.status = 'clean';
      wt.metrics = { insertions: 12, deletions: 3 };
    },
    [],
    300,
    'readme-date-sync-x-claude-676a02',
  );

  // What the eye actually gets: the visible text of the name element.
  const name = /class="mono s12 sb-wt-name"[^>]*>([^<]*)</.exec(html)?.[1] ?? '';
  assert(
    name.includes('-claude-676a02'),
    `the distinguishing suffix is on screen, got ${JSON.stringify(name)}`,
  );
  assert(name.includes('…'), 'and the middle is where the name was cut');
  assert(!name.endsWith('…'), 'never the end — that is the old truncation');

  // The full name is still one hover away (BACKLOG 7 kept this and so does it).
  assert(
    html.includes('title="readme-date-sync-x-claude-676a02"'),
    'the name carries the whole string as its tooltip',
  );

  // The change counts stay; the status word is what made room for the name, and
  // it is still on the dot and in the row tooltip, so nothing was lost.
  assert(html.includes('+12 −3'), 'the change counts stay on the row');
  assert(!html.includes('>변경 없음<'), 'the status word steps aside on a narrow row');
  assert(html.includes('data-status="clean"'), 'the dot still carries the status');
  assert(html.includes('title="변경 없음"'), 'and names it for a pointer or a screen reader');
  assert(
    html.includes('title="gilde/readme-date-sync-x-claude-676a02 · +12 −3 · 변경 없음"'),
    'the row tooltip still spells every fact out',
  );

  // Widen the sidebar and both come back.
  const wide = worktreeRowHtml(
    (wt) => {
      wt.sublabel = 'gilde/readme-date-sync-x-claude-676a02';
      wt.status = 'clean';
      wt.metrics = { insertions: 12, deletions: 3 };
    },
    [],
    420,
    'readme-date-sync-x-claude-676a02',
  );
  assert(wide.includes('>readme-date-sync-x-claude-676a02<'), 'a wide row prints the whole name');
  assert(wide.includes('>변경 없음<'), 'and the status word with it');
});

test('a clean worktree row prints a name and a status, nothing else', () => {
  const html = worktreeRowHtml((wt) => {
    wt.sublabel = 'gilde/clean-ab12cd';
    wt.status = 'clean';
  });
  assert(!html.includes('+0 −0'), 'a worktree with no diff shows no change count');
  assert(html.includes('title="gilde/clean-ab12cd · 변경 없음"'), 'the tooltip is just as short');
});

// ---------------------------------------------------------------------------
// 6b. M1.5 BACKLOG 8 — a session row never repeats its own agent badge
// ---------------------------------------------------------------------------

test('a session with no prompt summary is titled by its worktree, not by its agent', () => {
  const session = node('session:s1', 'session', 'claude', 'worktree:w1');
  session.meta = { vendor: 'claude' };
  const html = worktreeRowHtml((wt) => {
    wt.sublabel = 'gilde/pty-manager-ab12cd';
  }, [session]);

  assert(
    html.includes('class="sb-session-title">reader-thread-를-ring-buffer로-교체<'),
    'the title falls back to the worktree the session runs in',
  );
  assert(html.includes('class="badge" data-tone="neutral">Claude Code<'), 'the agent is still a badge');
  assert(
    !html.includes('class="sb-session-title">claude<'),
    'the title never repeats the badge',
  );
});

test('sessionTitle keeps a real summary and falls back in order', () => {
  assertEqual(
    sessionTitle('reader thread를 ring buffer로 교체', 'claude', 'pty-manager', '새 세션'),
    'reader thread를 ring buffer로 교체',
    'a real prompt summary wins',
  );
  assertEqual(sessionTitle('claude', 'claude', 'pty-manager', '새 세션'), 'pty-manager', 'the badge is not a title');
  assertEqual(sessionTitle('Claude', 'claude', 'pty-manager', '새 세션'), 'pty-manager', 'case does not save it');
  assertEqual(sessionTitle('  ', 'claude', 'pty-manager', '새 세션'), 'pty-manager', 'nor does whitespace');
  assertEqual(sessionTitle('claude', 'claude', undefined, '새 세션'), '새 세션', 'with no worktree, the last resort');
  assertEqual(sessionTitle('', 'shell', '', '새 세션'), '새 세션', 'an empty worktree label is not a title');
});

// ---------------------------------------------------------------------------
// 6c. M1.5 BACKLOG 13 — the inspector states the branch once
//
// It lives in this file because the inspector renders the same graph node the
// worktree row above does, and this task owns exactly that one header row of
// `shell/Inspector.tsx`; there is no shell test module to put it in.
// ---------------------------------------------------------------------------

function inspectorHtml(selected: GraphNode): string {
  seedStore({
    graph: graphWith([selected]),
    ui: { ...laneUiState(), selectedNodeId: selected.id },
  });
  return renderToStaticMarkup(<Inspector />);
}

function occurrences(haystack: string, needle: string): number {
  return haystack.split(needle).length - 1;
}

test('the inspector labels a branch once, whichever field carries it', () => {
  const wt = node('worktree:w1', 'worktree', 'pty-manager', 'project:p1');
  wt.sublabel = 'gilde/pty-manager-ab12cd';
  wt.meta = { branch: 'gilde/pty-manager-ab12cd', cwd: '/wt/pty-manager' };
  const worktree = inspectorHtml(wt);
  assertEqual(occurrences(worktree, '브랜치'), 1, 'the label is printed once, not twice');
  assertEqual(
    occurrences(worktree, 'gilde/pty-manager-ab12cd'),
    1,
    'and so is the branch itself — the header row is the one that survives',
  );

  // A session has no `sublabel` branch, so the supervision row is the one row.
  const session = node('session:s1', 'session', 'ring buffer', 'worktree:w1');
  session.meta = { vendor: 'claude', branch: 'gilde/pty-manager-ab12cd' };
  const live = inspectorHtml(session);
  assertEqual(occurrences(live, '브랜치'), 1, 'a session still shows its branch, once');
  assert(live.includes('gilde/pty-manager-ab12cd'), 'from meta.branch');
});

test('a project sublabel is its repo slug and is not labelled a branch', () => {
  const project = node('project:p1', 'project', 'gilde-code');
  project.sublabel = 'gilde-code-0badc0de';
  const html = inspectorHtml(project);
  assertEqual(occurrences(html, '브랜치'), 0, 'a repo slug is not a branch');
  assert(html.includes('저장소'), 'it is labelled for what it is');
  assert(html.includes('gilde-code-0badc0de'), 'and the slug is still shown');
});

test('the orphan group shows a count and is hidden at zero', () => {
  seedStore({});
  assert(
    !renderToStaticMarkup(<Sidebar />).includes('고아 프로세스'),
    'no orphans means no group',
  );

  const graph = graphWith([]);
  graph.totals = { ...emptyTotals(), orphans: 3 };
  seedStore({ graph });
  assert(
    renderToStaticMarkup(<Sidebar />).includes('고아 프로세스'),
    'totals.orphans alone is enough to show the group',
  );
});

test('the new-worktree form previews the slug, the path and the post-create command', () => {
  seedStore({});
  const html = renderToStaticMarkup(
    <NewWorktreeForm
      value={{
        projectId: 'p1',
        taskName: 'Fix PTY Resize',
        baseRef: '',
        providers: ['claude-w', 'generic'],
        postCreateCmd: '',
        launch: false,
        agent: '',
      }}
      projects={[{ id: 'p1', name: 'api' }]}
      repoSlug="api-0badc0de"
      defaultBaseRef="main"
      worktreesRoot="/Users/x/.gilde/worktrees"
      agents={[]}
      issue={null}
      onChange={() => {}}
      onSubmit={() => {}}
    />,
  );
  assert(html.includes('fix-pty-resize-······'), 'the slug preview follows the title live');
  assert(html.includes('gilde/fix-pty-resize-······'), 'the branch preview is shown');
  assert(
    html.includes('/Users/x/.gilde/worktrees/api-0badc0de/fix-pty-resize-······'),
    'the resulting central path is shown',
  );
  assert(html.includes('생성 후 실행할 명령'), 'post_create_cmd ships in M1 (DECISIONS)');
  assert(html.includes('생성 후 에이전트 실행'), 'the launch option is offered');
  assert(html.includes('.cursor/worktrees'), 'the cursor convention is listed');
  assert(html.includes('미검증'), 'and marked unverified');
  assert(!html.includes('checked=""') || !html.includes('.cursor/worktrees" checked'), 'cursor is not pre-checked');
  assert(!html.includes('undefined'), 'nothing rendered the string "undefined"');
});

test('a slug conflict is stated on the form, not swallowed', () => {
  seedStore({});
  const html = renderToStaticMarkup(
    <NewWorktreeForm
      value={{
        projectId: 'p1',
        taskName: 'Fix login',
        baseRef: '',
        providers: [],
        postCreateCmd: '',
        launch: false,
        agent: '',
      }}
      projects={[{ id: 'p1', name: 'api' }]}
      repoSlug="api-0badc0de"
      defaultBaseRef="main"
      worktreesRoot="/w"
      agents={[]}
      issue="duplicate"
      onChange={() => {}}
      onSubmit={() => {}}
    />,
  );
  assert(html.includes('같은 이름의 워크트리가 이미 있습니다'), 'the reason is spelled out');
});

test('the removal card is facts plus exactly three modes', () => {
  seedStore({});
  assertEqual(REMOVE_MODES.length, 3, 'three modes, and Force is not one of them');

  const state = {
    open: true,
    worktreeId: 'w1',
    label: 'pty-manager',
    branch: 'gilde/pty-manager-ab12cd',
    preflight: preflight({
      dirtyFiles: 12,
      untracked: 2,
      unmergedCommits: 3,
      aliasLinks: ['/repo/.claude/worktrees/pty-manager'],
      allowedModes: ['commitThenRemove', 'archiveThenRemove'] as const as RemoveMode[],
    }),
    stats: {
      dirtyFiles: 12,
      staged: 0,
      untracked: 2,
      conflicted: 0,
      added: 412,
      removed: 38,
      ahead: 3,
      behind: 0,
      lastCommitAt: null,
      lastCommitSubject: null,
    },
    loading: false,
    busy: false,
    error: null,
  };

  const html = renderToStaticMarkup(
    <RemoveWorktreeCard state={state} selected={null} onPick={() => {}} />,
  );
  assert(html.includes('브랜치 유지하고 제거'), 'KeepBranch is offered');
  assert(html.includes('커밋 후 제거'), 'CommitThenRemove is offered');
  assert(html.includes('보관 후 제거'), 'ArchiveThenRemove is offered');
  assert(!/force/i.test(html), 'there is no force mode');
  assert(html.includes('disabled=""'), 'the refused mode is disabled, not hidden');
  assert(
    html.includes('커밋되지 않은 변경이 있습니다'),
    'and the disabled mode carries the reason',
  );
  assert(html.includes('+412 −38'), 'the card shows the change size');
  assert(html.includes('변경 12 · 추적 안 됨 2'), 'the card shows what is uncommitted');
  assert(html.includes('/repo/.claude/worktrees/pty-manager'), 'the alias link is listed');
  assert(html.includes('삭제하지 않습니다'), 'the card says the branch survives');
});

// ---------------------------------------------------------------------------
// 7. Adding a project (acceptance 4)
// ---------------------------------------------------------------------------

test('the folder picker is the only way in, and cancelling does nothing', async () => {
  seedStore({});
  let calls = 0;
  setFolderPicker(async () => {
    calls += 1;
    return null;
  });
  try {
    const result = await useAppStore.getState().addProject();
    assertEqual(result, null, 'a cancelled picker adds nothing');
    assertEqual(calls, 1, 'the picker ran exactly once');
    assertEqual(useAppStore.getState().worktree.error, null, 'and nothing is reported as an error');
  } finally {
    resetFolderPicker();
  }
});

test('a non-repository is refused with the localized sentence, not a raw token', () => {
  // Exactly what `project_add` rejects with (commands/project.rs → error.rs).
  seedStore({
    worktree: {
      ...initialWorktreeState(),
      loaded: true,
      error: {
        code: 'REFUSED',
        key: 'error.project.notARepository',
        params: { path: '/Users/x/Downloads' },
        detail: null,
        retryable: false,
      },
    },
  });
  const html = renderToStaticMarkup(<Sidebar />);
  assert(
    html.includes('/Users/x/Downloads는 git 저장소가 아닙니다. 저장소 폴더를 고르세요.'),
    'the refusal renders in Korean with the path interpolated',
  );
  assert(!html.includes('error.project'), 'the raw key never reaches the screen');
});
