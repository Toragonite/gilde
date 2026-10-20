/**
 * Command palette, worktree switcher and keyboard layer tests (T27).
 *
 * Five things are pinned here, and they are exactly the five a type check
 * cannot see:
 *
 *  1. the fuzzy matcher ranks the way the list claims it does;
 *  2. every palette entry M1-PLAN T27 acceptance 1 names exists, is localized in
 *     both tables, and carries the keybinding that runs the same action;
 *  3. the switcher's rows are a pure function of the projection, ranked
 *     recency → dirty, and Enter targets the most recently active live session;
 *  4. `git checkout` appears nowhere outside `worktree/mod.rs` (DESIGN §7-4) —
 *     the assertion greps the Rust tree;
 *  5. the ⌘/ sheet documents exactly the bindings the window layer claims, and
 *     no other key is claimed, so a terminal keeps everything else.
 */

import { renderToStaticMarkup } from 'react-dom/server';

import { assert, assertEqual } from '../test/harness';
import { laneTest as test, laneUiState, previewTest } from '../test/lanes';
import { SHELL_HOTKEYS, activeHotkeyIds, matchesCombo } from '../hooks/useHotkeys';
import { translate } from '../i18n';
import { en } from '../i18n/en';
import { ko } from '../i18n/ko';
import type { AgentBinary, GraphNode, SessionId } from '../lib/ipc';
import { emptyGraph } from '../store/graphSlice';
import type { GraphView } from '../store/graphSlice';
import type { SessionView } from '../store/sessionSlice';
import { initialWorktreeState } from '../store/worktreeSlice';
import { useAppStore } from '../store/useAppStore';
import type { AppStore } from '../store/useAppStore';
import { CommandPalette, stepSelection } from './CommandPalette';
import {
  HELP_ENTRIES,
  buildPaletteActions,
  helpEntries,
  formatCombo,
  fuzzyMatch,
  paletteContext,
  rankActions,
  shellCombos,
} from './commands';
import type { PaletteAction } from './commands';
import { HelpSheet } from '../dialogs/HelpSheet';
import {
  WorktreeSwitcher,
  buildSwitcherRows,
  newestLiveSession,
  rankSwitcherRows,
} from '../sidebar/WorktreeSwitcher';

// ---------------------------------------------------------------------------
// Fixtures — shaped exactly like what Rust sends, never like demo data
// ---------------------------------------------------------------------------

function node(
  id: string,
  kind: GraphNode['kind'],
  label: string,
  parentId: string | null,
  extra: Partial<GraphNode> = {},
): GraphNode {
  return {
    id,
    kind,
    parentId,
    label,
    status: 'ok',
    badges: [],
    metrics: {},
    meta: {},
    updatedAt: 1,
    ...extra,
  };
}

function graphWith(nodes: GraphNode[]): GraphView {
  const graph = emptyGraph();
  for (const item of nodes) graph.nodes.set(item.id, item);
  return graph;
}

function agent(kind: AgentBinary['kind'], installed: boolean): AgentBinary {
  return {
    kind,
    absPath: installed ? `/usr/local/bin/${kind}` : null,
    version: installed ? '1.0.0' : null,
    detectedAt: 1,
    installed,
    ok: installed,
    verified: installed,
    verifiedRange: null,
    source: installed ? 'detected' : 'notFound',
  };
}

function sessionView(id: SessionId, worktreeId: string, openedAt: number): SessionView {
  return {
    id,
    info: {
      id,
      spawnId: `spawn-${id}`,
      projectId: 'p1',
      worktreeId,
      agent: 'claude',
      accountId: null,
      title: 'claude',
      cwd: `/wt/${worktreeId}`,
      cols: 80,
      rows: 24,
      pid: 100,
      pgid: 100,
      openedAt,
      detached: false,
      firstRetainedSeq: 0,
      nextSeq: 0,
    },
    status: { kind: 'idle' },
    confidence: 1,
    evidence: [],
    since: openedAt,
    updatedAt: openedAt,
    attention: false,
    exit: null,
    scrollbackLost: false,
  };
}

function seed(patch: Partial<AppStore>): void {
  useAppStore.setState({
    graph: emptyGraph(),
    ui: laneUiState(),
    toasts: [],
    agents: [],
    sidecar: null,
    sessions: new Map(),
    lastAgentByWorktree: {},
    worktree: initialWorktreeState(),
    ...patch,
  });
}

function commit(): void {
  Object.assign(useAppStore.getInitialState(), useAppStore.getState());
}

/** Two projects, three worktrees, two sessions — the shape the switcher ranks. */
function seededGraph(now: number): GraphView {
  return graphWith([
    node('project:p1', 'project', 'gilde-code', null),
    node('project:p2', 'project', 'acme', null),
    node('worktree:w1', 'worktree', 'pty-manager', 'project:p1', {
      sublabel: 'gilde/pty-manager',
      status: 'dirty',
      badges: ['dirty'],
      meta: { branch: 'gilde/pty-manager' },
      metrics: { dirtyFiles: 3, insertions: 412, deletions: 38, ahead: 2 },
      updatedAt: now - 600_000,
    }),
    node('worktree:w2', 'worktree', 'perf-spike', 'project:p1', {
      sublabel: 'gilde/perf-spike',
      status: 'clean',
      meta: { branch: 'gilde/perf-spike' },
      metrics: { dirtyFiles: 0, insertions: 0, deletions: 0 },
      updatedAt: now - 600_000,
    }),
    node('worktree:w3', 'worktree', 'auth-refactor', 'project:p2', {
      sublabel: 'gilde/auth-refactor',
      status: 'clean',
      meta: { branch: 'gilde/auth-refactor' },
      updatedAt: now,
    }),
    node('session:s1', 'session', 'claude', 'worktree:w1', {
      status: 'working',
      meta: { vendor: 'claude' },
      metrics: { rssBytes: 2_200_000_000, lastActivityAt: now - 1000 },
      updatedAt: now - 1000,
    }),
    node('session:s2', 'session', 'codex', 'worktree:w1', {
      status: 'exited',
      meta: { vendor: 'codex' },
      metrics: { rssBytes: 0, lastActivityAt: now - 60_000 },
      updatedAt: now - 60_000,
    }),
  ]);
}

// ---------------------------------------------------------------------------
// 1. Fuzzy matching
// ---------------------------------------------------------------------------

test('fuzzy match is a subsequence with word-start and adjacency bonuses', () => {
  assert(fuzzyMatch('', '무엇이든') !== null, 'an empty query matches everything');
  assert(fuzzyMatch('wt', 'worktree') !== null, 'a subsequence matches');
  assert(fuzzyMatch('xyz', 'worktree') === null, 'a non-subsequence does not');
  assert(fuzzyMatch('그래프', '그래프 열기') !== null, 'a Korean query matches Korean text');

  const exact = fuzzyMatch('graph', 'graph')!;
  const scattered = fuzzyMatch('graph', 'g r a p h zzzzzzzz')!;
  assert(exact.score > scattered.score, 'a tight match outranks a scattered one');

  const short = fuzzyMatch('mon', 'monitor')!;
  const long = fuzzyMatch('mon', 'move a node to monitor')!;
  assert(short.score > long.score, 'the shorter candidate wins the tie');

  assertEqual(fuzzyMatch('wt', 'worktree')!.positions.length, 2, 'positions are reported');
});

// ---------------------------------------------------------------------------
// 2. Keyboard layer (DESIGN §9-8)
// ---------------------------------------------------------------------------

interface FakeKey {
  code: string;
  metaKey?: boolean;
  altKey?: boolean;
  shiftKey?: boolean;
  ctrlKey?: boolean;
}

function key(fields: FakeKey): KeyboardEvent {
  return {
    code: fields.code,
    metaKey: fields.metaKey ?? false,
    altKey: fields.altKey ?? false,
    shiftKey: fields.shiftKey ?? false,
    ctrlKey: fields.ctrlKey ?? false,
  } as KeyboardEvent;
}

function eventFor(combo: string): KeyboardEvent {
  const parts = combo.split('+').map((part) => part.trim().toLowerCase());
  const name = parts[parts.length - 1];
  const code = /^[a-z]$/.test(name)
    ? `Key${name.toUpperCase()}`
    : /^[0-9]$/.test(name)
      ? `Digit${name}`
      : name === 'backslash'
        ? 'Backslash'
        : name === 'slash'
          ? 'Slash'
          : name === 'comma'
            ? 'Comma'
            : name === 'escape'
              ? 'Escape'
              : name === 'arrowleft'
                ? 'ArrowLeft'
                : name === 'arrowright'
                  ? 'ArrowRight'
                  : name;
  return key({
    code,
    metaKey: parts.includes('mod'),
    altKey: parts.includes('alt'),
    shiftKey: parts.includes('shift'),
  });
}

test('every DESIGN §9-8 binding is claimed by the window layer', () => {
  for (const combo of shellCombos()) {
    assert(matchesCombo(eventFor(combo), combo), `${combo} matches its own event`);
  }
});

test('a terminal keeps every key the shell does not list', () => {
  const combos = shellCombos();
  const claimed = (event: KeyboardEvent) => combos.some((combo) => matchesCombo(event, combo));

  // ⌘-codes the shell deliberately does not take.
  for (const code of ['KeyS', 'KeyF', 'KeyQ', 'KeyA', 'KeyC', 'KeyV']) {
    assert(!claimed(key({ code, metaKey: true })), `⌘${code} reaches the terminal`);
  }
  // Everything without ⌘ — the whole TUI vocabulary.
  for (const code of ['KeyB', 'KeyW', 'KeyT', 'Enter', 'ArrowUp', 'Escape']) {
    if (code === 'Escape') continue; // Escape is claimed, but only while a sheet is open.
    assert(!claimed(key({ code })), `${code} alone reaches the terminal`);
  }
  assert(!claimed(key({ code: 'KeyC', ctrlKey: true })), 'Ctrl+C reaches the process');
  assert(!claimed(key({ code: 'KeyB', ctrlKey: true })), 'Ctrl+B reaches the process');
  // ⌘⌥B is a binding; ⌥B alone is not.
  assert(!claimed(key({ code: 'KeyB', altKey: true })), '⌥B reaches the terminal');
});

test('the help sheet documents exactly the bindings that exist', () => {
  const documented = new Set(helpEntries().map((entry) => entry.id));
  for (const id of Object.keys(SHELL_HOTKEYS)) {
    assert(documented.has(id as never), `⌘/ documents ${id}`);
  }
  documented.delete('focusTab');
  assertEqual(
    documented.size,
    Object.keys(SHELL_HOTKEYS).length,
    'no entry documents a binding that does not exist',
  );
  for (const entry of HELP_ENTRIES) {
    assert(ko[entry.labelKey] !== undefined, `${entry.labelKey} exists in ko`);
    assert(en[entry.labelKey] !== undefined, `${entry.labelKey} exists in en`);
  }
});

test('combos render as macOS glyphs', () => {
  assertEqual(formatCombo(SHELL_HOTKEYS.palette), '⌘K', 'command palette');
  assertEqual(formatCombo(SHELL_HOTKEYS.splitDown), '⌘⇧\\', 'split down');
  assertEqual(formatCombo(SHELL_HOTKEYS.toggleInspector), '⌘⌥B', 'inspector');
  assertEqual(formatCombo(SHELL_HOTKEYS.nextTab), '⌘⌥→', 'next tab');
  assertEqual(formatCombo(SHELL_HOTKEYS.help), '⌘/', 'help');
  assertEqual(formatCombo(SHELL_HOTKEYS.closeOverlay), 'esc', 'escape');
});

// ---------------------------------------------------------------------------
// 3. The palette registry
// ---------------------------------------------------------------------------

function actionsFor(now = Date.now()): PaletteAction[] {
  seed({
    graph: seededGraph(now),
    agents: [agent('claude', true), agent('codex', true), agent('gemini', false)],
    sessions: new Map([['s1', sessionView('s1', 'w1', now)]]),
    ui: { ...laneUiState(), selectedNodeId: 'worktree:w1' },
  });
  return buildPaletteActions(paletteContext(useAppStore.getState()));
}

test('⌘K offers every command M1-PLAN T27 acceptance 1 names', () => {
  const actions = actionsFor();
  const ids = new Set(actions.map((action) => action.id));
  for (const id of [
    'view.sessions',
    'view.dispatch',
    'view.graph',
    'view.monitor',
    'view.settings',
    'drawer.monitor',
    'session.new',
    'session.new.claude',
    'session.new.codex',
    'session.new.gemini',
    'session.transcript',
    'worktree.create',
    'worktree.remove',
    'worktree.switch',
    'dispatch.new',
    'process.orphans',
    'app.reloadSignatures',
    'app.restartSidecar',
  ]) {
    assert(ids.has(id), `the palette offers ${id}`);
  }
  // Removed for the preview, in every build: the diagnostics shortcut (Settings
  // has it) and the language switch (title bar and Settings › Appearance only).
  assert(!ids.has('app.diagnostics') && !ids.has('app.locale'), 'no duplicate entry points');
  assert(!ids.has('drawer.logs') && !ids.has('drawer.events'), 'the drawer has no Logs or Events tab');
  const views = actions.filter((a) => a.id.startsWith('view.')).map((a) => a.id);
  assertEqual(
    views.join(','),
    'view.team,view.workers,view.sessions,view.dispatch,view.graph,view.monitor,view.settings',
    'Team and Worker accounts come first, then the lanes',
  );
});

previewTest('the preview palette offers Team, Worker accounts, settings, help — and no lane', () => {
  seed({ agents: [agent('claude', true)] });
  const ids = buildPaletteActions(paletteContext(useAppStore.getState())).map((a) => a.id);
  assertEqual(
    ids.join(','),
    'view.team,view.workers,view.settings,toggleSidebar,closePane,app.help',
    'exactly the preview entries, in order',
  );
  for (const prefix of ['session.', 'worktree.', 'dispatch.', 'drawer.', 'process.', 'view.sessions', 'view.graph', 'view.monitor', 'view.dispatch', 'app.reloadSignatures', 'app.restartSidecar', 'splitRight', 'toggleInspector']) {
    assert(!ids.some((id) => id.startsWith(prefix)), `${prefix}* is hidden in the preview`);
  }
});

previewTest('the preview claims only its own hotkeys, and the help sheet documents exactly those', () => {
  const combos = new Set(shellCombos());
  for (const combo of ['mod+k', 'mod+1', 'mod+2', 'mod+b', 'mod+comma', 'mod+slash', 'escape', 'mod+w']) {
    assert(combos.has(combo), `${combo} is claimed`);
  }
  for (const combo of ['mod+t', 'mod+p', 'mod+shift+d', 'mod+shift+g', 'mod+shift+m', 'mod+j', 'mod+backslash', 'mod+shift+backslash', 'mod+alt+b', 'mod+alt+1']) {
    assert(!combos.has(combo), `${combo} is not claimed in the preview`);
  }
  const help = helpEntries();
  assertEqual(
    new Set(help.map((e) => e.id)).size,
    activeHotkeyIds().length,
    'the help sheet documents every claimed binding and nothing else',
  );
  assert(help.some((e) => e.id === 'viewTeam' && e.combo === 'mod+1'), '⌘1 is Team, in the help sheet');
  assert(help.some((e) => e.id === 'viewWorkers' && e.combo === 'mod+2'), '⌘2 is Worker accounts');
  assert(!help.some((e) => e.id === 'focusTab' || e.id === 'newSession'), 'no lane binding is documented');
});

test('every palette entry is localized in both tables and binds a real hotkey', () => {
  const combos = new Set(Object.values(SHELL_HOTKEYS) as string[]);
  for (const action of actionsFor()) {
    assert(ko[action.labelKey] !== undefined, `${action.id} has a Korean label`);
    assert(en[action.labelKey] !== undefined, `${action.id} has an English label`);
    if (action.disabledKey) {
      assert(ko[action.disabledKey] !== undefined, `${action.id} has a Korean reason`);
    }
    for (const name of Object.values(action.labelParamKeys ?? {})) {
      assert(ko[name] !== undefined, `${action.id} interpolates a real key`);
    }
    if (action.combo) {
      assert(combos.has(action.combo), `${action.id} shows a binding the shell actually has`);
    }
  }
});

test('an agent that is not installed is listed with its reason, not hidden', () => {
  const actions = actionsFor();
  const gemini = actions.find((action) => action.id === 'session.new.gemini')!;
  assertEqual(gemini.disabledKey, 'session.new.notInstalled', 'gemini says why it cannot run');
  const claude = actions.find((action) => action.id === 'session.new.claude')!;
  assertEqual(claude.disabledKey, undefined, 'an installed agent is runnable');
  assertEqual(claude.detail, 'pty-manager', 'and names the worktree it would open in');
});

test('with no worktree the session and removal commands say so', () => {
  seed({ agents: [agent('claude', true)] });
  const actions = buildPaletteActions(paletteContext(useAppStore.getState()));
  const claude = actions.find((action) => action.id === 'session.new.claude')!;
  assertEqual(claude.disabledKey, 'session.new.noWorktree', 'no worktree to open a session in');
  const remove = actions.find((action) => action.id === 'worktree.remove')!;
  assertEqual(remove.disabledKey, 'session.new.noWorktree', 'nothing to remove');
  const transcript = actions.find((action) => action.id === 'session.transcript')!;
  assertEqual(transcript.disabledKey, 'palette.disabled.noSession', 'no transcript without a session');
});

test('the query ranks the command it names first', () => {
  const actions = actionsFor();
  // The same label the palette paints: an interpolated parameter that is itself
  // a message key is resolved before the query ever sees it.
  const label = (action: PaletteAction) => {
    const params: Record<string, string | number> = { ...action.labelParams };
    for (const [name, key] of Object.entries(action.labelParamKeys ?? {})) {
      params[name] = translate('ko', key);
    }
    return translate('ko', action.labelKey, params);
  };
  const graphFirst = rankActions('그래프', actions, label);
  assertEqual(graphFirst[0]?.action.id, 'view.graph', '그래프 finds the graph panel');
  const sidecar = rankActions('sidecar', actions, label);
  assertEqual(sidecar[0]?.action.id, 'app.restartSidecar', 'sidecar finds the restart');
  assertEqual(rankActions('zzzz', actions, label).length, 0, 'a query with no match returns none');
  assertEqual(
    rankActions('', actions, label).length,
    actions.length,
    'an empty query keeps every action in declared order',
  );
});

test('the selection skips rows that cannot be activated', () => {
  const enabled = (i: number) => i !== 1;
  assertEqual(stepSelection(0, 1, 3, enabled), 2, 'down skips the disabled row');
  assertEqual(stepSelection(2, -1, 3, enabled), 0, 'up skips it too');
  assertEqual(stepSelection(0, 1, 1, () => false), 0, 'a list with nothing runnable does not move');
});

// ---------------------------------------------------------------------------
// 4. The worktree switcher
// ---------------------------------------------------------------------------

test('switcher rows are a pure projection of the graph', () => {
  const now = 1_700_000_000_000;
  const rows = buildSwitcherRows(seededGraph(now), new Map([['s1', sessionView('s1', 'w1', now)]]));
  assertEqual(rows.length, 3, 'one row per worktree');

  const w1 = rows.find((row) => row.worktreeId === 'w1')!;
  assertEqual(w1.projectLabel, 'gilde-code', 'the project name is carried');
  assertEqual(w1.branch, 'gilde/pty-manager', 'the branch is carried');
  assertEqual(w1.dirtyFiles, 3, 'dirty count');
  assertEqual(w1.insertions, 412, 'insertions');
  assertEqual(w1.deletions, 38, 'deletions');
  assertEqual(w1.ahead, 2, 'ahead');
  assertEqual(w1.rssBytes, 2_200_000_000, 'RSS is summed over the sessions');
  assertEqual(w1.liveSessions, 1, 'the exited session is not live');
  assertEqual(w1.recency, now - 1000, 'recency follows the newest session activity');
  assert(w1.haystacks.includes('claude'), 'a session name is searchable');
});

test('ranking is recency first, dirty second', () => {
  const now = 1_700_000_000_000;
  const rows = buildSwitcherRows(seededGraph(now), new Map());
  const order = rankSwitcherRows('', rows).map((row) => row.label);
  assertEqual(order[0], 'pty-manager', 'the worktree with the newest activity leads');
  assertEqual(order[1], 'auth-refactor', 'then the next most recent');
  assertEqual(order[2], 'perf-spike', 'the clean, equally old worktree comes after the dirty one');

  const filtered = rankSwitcherRows('auth', rows);
  assertEqual(filtered.length, 1, 'the query filters');
  assertEqual(filtered[0].label, 'auth-refactor', 'and finds the right one');

  const byBranch = rankSwitcherRows('gilde/perf', rows);
  assertEqual(byBranch[0]?.label, 'perf-spike', 'a branch is searchable');
  const byProject = rankSwitcherRows('acme', rows);
  assertEqual(byProject[0]?.label, 'auth-refactor', 'a project name is searchable');
});

test('Enter targets the most recently active live session', () => {
  const now = 1_700_000_000_000;
  const rows = buildSwitcherRows(seededGraph(now), new Map());
  const w1 = rows.find((row) => row.worktreeId === 'w1')!;
  assertEqual(newestLiveSession(w1)?.id, 's1', 'the exited codex session is skipped');
  const w2 = rows.find((row) => row.worktreeId === 'w2')!;
  assertEqual(newestLiveSession(w2), null, 'a worktree with no session offers a new one instead');
});

// ---------------------------------------------------------------------------
// 5. Rendering
// ---------------------------------------------------------------------------

test('the palette renders its groups, shortcuts and reasons', () => {
  const now = Date.now();
  actionsFor(now);
  useAppStore.setState({ ui: { ...useAppStore.getState().ui, paletteOpen: true } });
  commit();
  const html = renderToStaticMarkup(<CommandPalette />);

  assert(html.includes('class="palette"'), 'the palette frame rendered');
  assert(html.includes('명령 검색'), 'the field carries its Korean placeholder');
  assert(html.includes('그래프 열기'), 'a panel command is listed');
  assert(html.includes('새 세션 · Claude Code'), 'one entry per detected agent, by product name');
  assert(html.includes('사이드카 재시작'), 'the sidecar restart is listed');
  assert(html.includes('고아 프로세스 정리'), 'the orphan cleanup is listed');
  assert(html.includes('설정 열기'), 'settings are reachable');
  assert(html.includes('⌘K') || html.includes('⌘P'), 'entries show their keybinding');
  assert(html.includes('이 머신에 설치되어 있지 않습니다.'), 'the disabled reason is visible');
  assert(!html.includes('undefined'), 'nothing rendered the string "undefined"');
});

test('the switcher renders one row per worktree with its live numbers', () => {
  const now = Date.now();
  seed({
    graph: seededGraph(now),
    agents: [agent('claude', true)],
    sessions: new Map([['s1', sessionView('s1', 'w1', now)]]),
    ui: { ...laneUiState(), switcherOpen: true },
  });
  commit();
  const html = renderToStaticMarkup(<WorktreeSwitcher />);

  assert(html.includes('pty-manager'), 'the worktree slug');
  assert(html.includes('gilde/pty-manager'), 'the branch');
  assert(html.includes('+412 −38'), 'the diff stat');
  assert(html.includes('dirty 3'), 'the dirty count');
  assert(html.includes('↑2'), 'the ahead count');
  assert(html.includes('data-status="dirty"'), 'the live status dot');
  assert(html.includes('프로젝트 · 워크트리 · 브랜치 · 세션'), 'the placeholder names the fields');
  assert(!html.includes('undefined'), 'nothing rendered the string "undefined"');
});

test('the help sheet is data, so both locales render every binding', () => {
  for (const table of [ko, en]) {
    for (const entry of HELP_ENTRIES) {
      assert(table[entry.labelKey].length > 0, `${entry.labelKey} is not empty`);
    }
  }
  assert(typeof HelpSheet === 'function', 'the sheet is a component');
  assert(ko['help.terminalNote'].includes('⌘'), 'the terminal rule is stated in the sheet');
});

// ---------------------------------------------------------------------------
// 6. DESIGN §7-4 — `git checkout` lives in exactly one place
// ---------------------------------------------------------------------------

/**
 * The Rust tree, inlined by Vite at bundle time.
 *
 * The test runner has no file-system access of its own (the front end is
 * compiled without Node types on purpose), so the sources arrive as `?raw`
 * modules. The glob is rooted at the repository, which is also Vite's root.
 */
const RUST_SOURCES = import.meta.glob('/src-tauri/src/**/*.rs', {
  query: '?raw',
  import: 'default',
  eager: true,
}) as Record<string, string>;

test('`git checkout` is never called outside the worktree create path', () => {
  const paths = Object.keys(RUST_SOURCES);
  assert(paths.length > 20, 'the Rust tree was actually read');
  assert(
    paths.includes('/src-tauri/src/worktree/mod.rs'),
    'the worktree module is part of what was read',
  );

  const offenders: string[] = [];
  for (const path of paths) {
    // A git argv can only carry the subcommand as a string literal; prose in a
    // doc comment explaining why it is absent is not a call.
    if (!RUST_SOURCES[path].includes('"checkout"')) continue;
    if (path === '/src-tauri/src/worktree/mod.rs') continue;
    offenders.push(path);
  }
  assertEqual(offenders.join(', '), '', 'no module outside worktree/mod.rs runs git checkout');
});
