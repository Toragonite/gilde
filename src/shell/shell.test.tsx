/**
 * Shell view-switching tests — M1.5 BACKLOG 3, 6 and 9.
 *
 * Rendered to static markup on Node, like `test/render.test.tsx`: there is no
 * DOM, so what a rail button *does* is asserted through the one store field it
 * writes (`ui.activeView`, exactly what `Rail`'s `onClick` calls) and what the
 * whole shell renders afterwards — the sidebar landmark's name, the sidebar's
 * own content, the tab bar and the pane. That is the part a type check cannot
 * prove: before this, pressing 디스패치 or 그래프 moved a highlight and nothing
 * else.
 */

import { renderToStaticMarkup } from 'react-dom/server';

import { assert, assertDeepEqual, assertEqual } from '../test/harness';
import { laneTest as test, laneUiState } from '../test/lanes';
import { AppShell, registerSidebar } from './AppShell';
import { Sidebar } from '../sidebar/Sidebar';
import { nodeStatusOfSession } from '../components/StatusDot';
import { registerPaneView } from '../panes/Pane';
import type { PaneViewProps } from '../panes/Pane';
import { emptyGraph, emptyTotals } from '../store/graphSlice';
import { useAppStore } from '../store/useAppStore';
import { cancelPersistLayout, pickPersistedLayout } from '../store/uiSlice';
import type { PersistedLayout, Tab } from '../store/uiSlice';
import { initialDispatchState } from '../store/dispatchSlice';
import { exitedTabPatches, sessionStateCounts, viewOfInfo } from '../store/sessionSlice';
import type { SessionView } from '../store/sessionSlice';
import type { AppConfig, GraphNode, PtySessionInfo, SessionId } from '../lib/ipc';
import { translate } from '../i18n';
import { SessionRow, sessionStateText } from '../sidebar/SessionRow';
import { TerminalPane, sessionActionsFor } from '../terminal/TerminalPane';

function reset(): void {
  cancelPersistLayout();
  useAppStore.setState({
    graph: emptyGraph(),
    ui: laneUiState(),
    toasts: [],
    agents: [],
    sidecar: null,
    sessions: new Map(),
    // The dispatch lane is shared state and another suite may have filled it;
    // this panel is asserted against an empty one.
    dispatch: initialDispatchState(),
  });
}

function render(): string {
  // zustand v5 renders statically from `getInitialState()`; copying the live
  // state across is what makes a static render show what a browser would.
  Object.assign(useAppStore.getInitialState(), useAppStore.getState());
  return renderToStaticMarkup(<AppShell />);
}

/** A pane view that only says which descriptor reached it. */
function stub(label: string) {
  return function Stub({ descriptor }: PaneViewProps) {
    return <div data-stub={descriptor.view}>{label}</div>;
  };
}

test('the rail switches the sidebar landmark and what the main area shows', () => {
  reset();
  const disposers = [
    registerSidebar(Sidebar),
    registerPaneView('dispatch', stub('DISPATCH-BOARD')),
    registerPaneView('graph', stub('GRAPH-CANVAS')),
    registerPaneView('monitor', stub('MONITOR-TABLE')),
  ];
  try {
    // Sessions is where the shell starts: the tree, and no tile of its own.
    const sessions = render();
    assert(
      sessions.includes('aria-label="사이드바 · 세션"'),
      'the sidebar landmark names the sessions view',
    );
    assert(sessions.includes('aria-label="프로젝트 추가"'), 'and it holds the session tree');
    assert(sessions.includes('이 패널에 열린 것이 없습니다.'), 'no view forced a tile open');

    // 디스패치 — the sidebar becomes the task list, and the board opens.
    useAppStore.getState().setActiveView('dispatch');
    const dispatch = render();
    assert(
      dispatch.includes('aria-label="사이드바 · 디스패치"'),
      'the landmark followed the rail',
    );
    assert(dispatch.includes('aria-label="새 디스패치"'), 'the sidebar offers the composer');
    // Nothing has answered `dispatch_list` yet, so the panel says it is still
    // asking rather than claiming there are no tasks.
    assert(dispatch.includes('불러오는 중…'), 'and the task list is honest about not knowing yet');
    assert(!dispatch.includes('aria-label="프로젝트 추가"'), 'the project tree stepped aside');
    assert(dispatch.includes('DISPATCH-BOARD'), 'the pane shows the dispatch board');

    // 그래프 — the canvas takes the pane; the sidebar goes back to the tree.
    useAppStore.getState().setActiveView('graph');
    const graph = render();
    assert(graph.includes('aria-label="사이드바 · 그래프"'), 'the landmark followed again');
    assert(graph.includes('GRAPH-CANVAS'), 'the pane shows the graph canvas');
    assert(graph.includes('aria-label="프로젝트 추가"'), 'the session tree is back');

    // 모니터 — same contract, and the tab bar now holds all three.
    useAppStore.getState().setActiveView('monitor');
    const monitor = render();
    assert(monitor.includes('aria-label="사이드바 · 모니터"'), 'the landmark followed once more');
    assert(monitor.includes('MONITOR-TABLE'), 'the pane shows the monitor');

    const tabs = useAppStore.getState().ui.tabs;
    assertEqual(tabs.length, 3, 'each view opened exactly one tab');

    // Going back focuses the tab that is already there instead of opening a
    // second one — the canvas is a place, not a document.
    useAppStore.getState().setActiveView('graph');
    assertEqual(useAppStore.getState().ui.tabs.length, 3, 'no duplicate tab');
    assert(render().includes('GRAPH-CANVAS'), 'and the canvas is in front again');
  } finally {
    for (const dispose of disposers) dispose();
    reset();
  }
});

test('the rail button carries the current view, and one field drives all of it', () => {
  reset();
  useAppStore.getState().setActiveView('dispatch');
  const html = render();
  // `aria-current` is what an assistive client and the GUI smoke driver read.
  assert(
    html.includes('aria-current="true" aria-label="디스패치"'),
    'the dispatch rail button is marked current',
  );
  assertEqual(useAppStore.getState().ui.activeView, 'dispatch', 'one field, not two');
  reset();
});

test('a new tab lands in the first empty pane, not on top of the active one', () => {
  reset();
  const store = useAppStore.getState();
  store.setSplit(2);
  const first = store.openTab({ title: 'a', descriptor: { view: 'graph' }, closable: true });

  let ui = useAppStore.getState().ui;
  assertEqual(ui.panes.slots[0].tabId, first, 'the first tab took the empty active pane');
  assertEqual(ui.activePaneId, 'pane-1', 'and focus stayed there');

  // BACKLOG 9: pane-1 is busy, pane-2 is blank — the session belongs in pane-2.
  const second = useAppStore.getState().openTab({
    title: 'claude',
    descriptor: { view: 'terminal', sessionId: 's-2' },
    closable: true,
  });
  ui = useAppStore.getState().ui;
  assertEqual(ui.panes.slots[0].tabId, first, 'the first pane kept what it was showing');
  assertEqual(ui.panes.slots[1].tabId, second, 'the new session went into the empty pane');
  assertEqual(ui.activePaneId, 'pane-2', 'and focus followed it');

  // With no pane left over, the active pane takes it — the old behaviour.
  const third = useAppStore.getState().openTab({
    title: 'codex',
    descriptor: { view: 'terminal', sessionId: 's-3' },
    closable: true,
  });
  ui = useAppStore.getState().ui;
  assertEqual(ui.panes.slots[0].tabId, first, 'the other pane is still untouched');
  assertEqual(ui.panes.slots[1].tabId, third, 'the full grid falls back to the active pane');
  reset();
});

test('a four-way split fills its empty panes in order', () => {
  reset();
  useAppStore.getState().setSplit(4);
  const ids = ['s-1', 's-2', 's-3', 's-4'].map((sessionId) =>
    useAppStore.getState().openTab({
      title: sessionId,
      descriptor: { view: 'terminal', sessionId },
      closable: true,
    }),
  );
  const slots = useAppStore.getState().ui.panes.slots;
  for (let i = 0; i < 4; i++) {
    assertEqual(slots[i].tabId, ids[i], `pane ${i + 1} took the ${i + 1}th session`);
  }
  reset();
});

// ---------------------------------------------------------------------------
// BACKLOG 6 — the layout survives a host crash, its dead tabs marked exited
// ---------------------------------------------------------------------------

function sessionInfo(id: SessionId, pid: number): PtySessionInfo {
  return {
    id,
    spawnId: `spawn-${id}`,
    projectId: 'p1',
    worktreeId: 'w1',
    agent: 'claude',
    accountId: null,
    title: 'auth-refactor',
    cwd: '/tmp/wt',
    cols: 120,
    rows: 40,
    pid,
    pgid: pid,
    openedAt: 1_000,
    detached: false,
    firstRetainedSeq: 0,
    nextSeq: 0,
  };
}

/** The projection's node for one session — a crash row is `exited` there. */
function sessionGraphNode(id: string): GraphNode {
  return {
    id: `session:${id}`,
    kind: 'session',
    parentId: 'worktree:w1',
    label: 'auth-refactor',
    status: 'exited',
    confidence: 1,
    badges: [],
    metrics: {},
    meta: { vendor: 'claude' },
    updatedAt: 2_000,
  };
}

test('a pane layout with open terminals round-trips through config.layout', () => {
  reset();
  const store = useAppStore.getState();
  store.setSplit(2);
  const a = store.openTab({
    title: 'claude',
    sublabel: 'auth-refactor',
    descriptor: { view: 'terminal', sessionId: 's-1' },
    status: 'working',
    closable: true,
  });
  const b = useAppStore.getState().openTab({
    title: 'codex',
    descriptor: { view: 'terminal', sessionId: 's-2' },
    closable: true,
  });

  const layout: PersistedLayout = pickPersistedLayout(useAppStore.getState().ui);
  const saved = { layout } as unknown as AppConfig;

  // A fresh launch after a SIGKILL: nothing but the persisted layout.
  reset();
  useAppStore.getState().hydrateFromConfig(saved);
  const ui = useAppStore.getState().ui;
  assertEqual(ui.tabs.length, 2, 'both tabs came back');
  assertEqual(ui.panes.slots[0].tabId, a, 'the first pane kept its tab');
  assertEqual(ui.panes.slots[1].tabId, b, 'and the second pane kept its own');
  assert(
    ui.tabs.every((tab) => tab.status === undefined),
    'a restored tab claims nothing about liveness until the backend answers',
  );

  // A restored id must never be handed out again.
  const fresh = useAppStore.getState().openTab({
    title: 'graph',
    descriptor: { view: 'graph' },
    closable: true,
  });
  assert(fresh !== a && fresh !== b, 'the new tab got an id of its own');
  assertEqual(useAppStore.getState().ui.tabs.length, 3, 'nothing was overwritten');
  reset();
});

test('a session whose pty died with the host is listed as exited, not lost', () => {
  const crashed = viewOfInfo(sessionInfo('s-1', 0));
  assertEqual(crashed.status.kind, 'exited', 'a row with no process is over');
  assertEqual(crashed.hostCrash, true, 'and it is marked as a host-crash row');
  assertEqual(crashed.confidence, 1, 'there is nothing uncertain about it');
  assert(crashed.scrollbackLost, 'the ring died with the process');

  const live = viewOfInfo(sessionInfo('s-2', 4321));
  assertEqual(live.status.kind, 'starting', 'a real pid is still a live session');
  assertEqual(live.hostCrash, false, 'and is not a crash row');
});

test('restored tabs are marked exited against what the backend still has', () => {
  const tabs: Tab[] = [
    {
      id: 'tab-1',
      title: 'claude',
      descriptor: { view: 'terminal', sessionId: 's-1' },
      closable: true,
    },
    {
      id: 'tab-2',
      title: 'codex',
      descriptor: { view: 'terminal', sessionId: 's-gone' },
      closable: true,
    },
    { id: 'tab-3', title: '그래프', descriptor: { view: 'graph' }, closable: true },
  ];
  const sessions = new Map<SessionId, SessionView>([
    ['s-1', viewOfInfo(sessionInfo('s-1', 0))],
  ]);

  const patches = exitedTabPatches(tabs, sessions, '호스트 비정상 종료');
  assertEqual(patches.length, 2, 'the graph tab is not a session and is left alone');
  assertEqual(patches[0].tabId, 'tab-1', 'the crashed session tab is patched');
  assertEqual(patches[0].patch.status, 'exited', 'and marked exited');
  assertEqual(patches[0].patch.badge, '호스트 비정상 종료', 'with the reason on the tab');
  assertEqual(patches[1].tabId, 'tab-2', 'a session the backend no longer knows is exited too');
  assertEqual(patches[1].patch.badge, undefined, 'but claims no cause it cannot know');

  // Idempotent: running it again over the patched tabs changes nothing.
  const settled: Tab[] = [
    { ...tabs[0], status: 'exited', badge: '호스트 비정상 종료' },
    { ...tabs[1], status: 'exited' },
    tabs[2],
  ];
  assertEqual(
    exitedTabPatches(settled, sessions, '호스트 비정상 종료').length,
    0,
    'a second pass is a no-op',
  );
});

// ---------------------------------------------------------------------------
// M1.5 integration — the visible half of BACKLOG 6
//
// The lane work landed the row and the dismissal; what was missing is that a
// human could see the cause and could not reach an action that answers
// `not_found`. Both are rendered, so both are asserted on the rendered markup.
// ---------------------------------------------------------------------------

test('the sidebar row names the cause, not just that the session exited', () => {
  reset();
  const crashed = viewOfInfo(sessionInfo('s-1' as SessionId, 0));
  const graph = emptyGraph();
  graph.nodes.set(`session:s-1`, sessionGraphNode('s-1'));
  useAppStore.setState({ graph, sessions: new Map([[crashed.id, crashed]]) });
  Object.assign(useAppStore.getInitialState(), useAppStore.getState());

  const row = renderToStaticMarkup(
    <SessionRow
      node={sessionGraphNode('s-1')}
      depth={0}
      selected={false}
      attention={false}
      onSelect={() => {}}
      onOpen={() => {}}
    />,
  );
  const exited = translate('ko', 'session.status.exited');
  const cause = translate('ko', 'session.hostCrash');
  assert(row.includes(`${exited} · ${cause}`), 'the row reads 종료됨 · 호스트 비정상 종료');

  // A live session says only what it is doing — the cause is not decoration.
  const live = viewOfInfo(sessionInfo('s-2' as SessionId, 4321));
  useAppStore.setState({ sessions: new Map([[live.id, live]]) });
  Object.assign(useAppStore.getInitialState(), useAppStore.getState());
  const liveRow = renderToStaticMarkup(
    <SessionRow
      node={sessionGraphNode('s-2')}
      depth={0}
      selected={false}
      attention={false}
      onSelect={() => {}}
      onOpen={() => {}}
    />,
  );
  assert(!liveRow.includes(cause), 'a session that did not crash is not labelled as one');
  reset();
});

test('sessionStateText joins the status and the cause, and only when there is one', () => {
  assertEqual(
    sessionStateText('종료됨', true, '호스트 비정상 종료'),
    '종료됨 · 호스트 비정상 종료',
    'the cause is joined onto the status word',
  );
  assertEqual(
    sessionStateText('작업 중', false, '호스트 비정상 종료'),
    '작업 중',
    'and a session that did not crash reads exactly as before',
  );
});

test('a crashed row is offered only the actions that cannot answer not_found', () => {
  const live = sessionActionsFor(false).map((a) => a.id);
  assert(live.includes('restart') && live.includes('stop'), 'a live session keeps its menu');

  const crashed = sessionActionsFor(true).map((a) => a.id);
  assertEqual(crashed.join(','), 'reveal,dismiss', 'a crash row gets reveal and dismiss');
  for (const dead of ['restart', 'stop', 'gentle', 'detach', 'transcript']) {
    assert(!crashed.includes(dead as (typeof crashed)[number]), `${dead} is not offered`);
  }
  assert(
    crashed.every((id) => !sessionActionsFor(true).find((a) => a.id === id)?.confirm),
    'and nothing left needs a confirmation, because nothing left ends a process',
  );
});

test('a crashed session renders its cause instead of attaching to a dead pty', () => {
  reset();
  const crashed = viewOfInfo(sessionInfo('s-1' as SessionId, 0));
  const graph = emptyGraph();
  graph.nodes.set(`session:s-1`, sessionGraphNode('s-1'));
  useAppStore.setState({ graph, sessions: new Map([[crashed.id, crashed]]) });
  Object.assign(useAppStore.getInitialState(), useAppStore.getState());

  const pane = renderToStaticMarkup(
    <TerminalPane
      descriptor={{ view: 'terminal', sessionId: 's-1' as SessionId }}
      tab={{ id: 'tab-1', title: 'claude', descriptor: { view: 'terminal', sessionId: 's-1' as SessionId }, closable: true }}
      paneId="pane-1"
      active
    />,
  );
  assert(
    pane.includes(translate('ko', 'session.hostCrash.body')),
    'the pane says what happened in one sentence',
  );
  assert(
    pane.includes(translate('ko', 'session.hostCrash.dismiss')),
    'and offers the one action that applies',
  );
  assert(!pane.includes('xterm'), 'no terminal is mounted, so nothing calls session_attach');
  assert(!pane.includes(translate('ko', 'session.composer.send')), 'and there is no composer');
  reset();
});

// ---------------------------------------------------------------------------
// M1.5 BACKLOG 15a — the footer counts the sessions the pills describe
//
// The 2026-09-09 hands-on session had one claude session sitting on a `/model`
// menu: its pill read `Waiting for input` and the footer read `1 working · 0
// waiting`. Both were reading real data — the footer was reading
// `graph.totals`, Rust's own fold over the graph projection, i.e. a *second*
// mapping of the same status on a different cadence. There is one now
// (`selectSessionStateCounts`), and this is the disagreement it makes
// impossible.
// ---------------------------------------------------------------------------

/** One session view in a given status, shaped exactly like a live one. */
function sessionInState(id: SessionId, status: SessionView['status']): SessionView {
  return { ...viewOfInfo(sessionInfo(id, 4321)), status, confidence: 0.85 };
}

test('the status bar counts a waiting session as waiting, whatever the totals say', () => {
  reset();
  const waiting = sessionInState('s-1' as SessionId, {
    kind: 'waitingForInput',
    prompt: 'Enter to select · ↑/↓ to navigate · Esc to cancel',
  });
  const idle = sessionInState('s-2' as SessionId, { kind: 'idle' });

  // The totals op still arrives, and still says something else: the graph fold
  // is what was wrong. If the footer ever reads it again, this test says so.
  useAppStore.getState().applyGraphPatch({
    rev: 1,
    ops: [
      {
        op: 'totals',
        totals: { ...emptyTotals(), sessions: 2, sessionsWorking: 1, sessionsIdle: 1 },
      },
    ],
  });
  useAppStore.setState({
    sessions: new Map([
      [waiting.id, waiting],
      [idle.id, idle],
    ]),
  });

  const html = render();
  assert(
    html.includes('작업 0 · 대기 1 · 유휴 1'),
    'one waiting and one idle session are counted as exactly that',
  );
  assert(
    !html.includes('작업 1 · 대기 0'),
    'and the graph fold that said 1 working no longer reaches the footer',
  );

  // The pill and the footer now read one status through one mapping.
  assertDeepEqual(
    sessionStateCounts(useAppStore.getState().sessions),
    { working: 0, waiting: 1, idle: 1 },
    'the selector is the only thing counting',
  );
  assertEqual(
    nodeStatusOfSession(waiting.status),
    'waiting',
    'and it is the same normalization the pill draws its dot from',
  );

  // A session that starts working moves both numbers, together.
  useAppStore.setState({
    sessions: new Map([
      [waiting.id, { ...waiting, status: { kind: 'working', since: 1_000 } }],
      [idle.id, idle],
    ]),
  });
  assert(render().includes('작업 1 · 대기 0 · 유휴 1'), 'and the count follows the pill');
  reset();
});
