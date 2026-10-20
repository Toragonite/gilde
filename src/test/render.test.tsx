/**
 * Shell render smoke test.
 *
 * Renders the whole component tree to static markup on Node. It has no DOM, so
 * it proves what a type check cannot: the shell mounts, every panel renders its
 * empty state, the status bar renders the counters that a `totals` GraphOp
 * produced, and no component reaches for a browser API during render.
 *
 * Note on zustand v5: static rendering reads `getInitialState()`, not
 * `getState()` — that is how it hydrates without a mismatch. `commit()` below
 * copies the live state into the initial snapshot so a static render sees what
 * a browser render would see.
 */

import { renderToStaticMarkup } from 'react-dom/server';

import { assert } from './harness';
import { laneTest as test, laneUiState } from './lanes';
import { AppShell, registerSidebar } from '../shell/AppShell';
import { Sidebar } from '../sidebar/Sidebar';
import { emptyGraph, emptyTotals } from '../store/graphSlice';
import { useAppStore } from '../store/useAppStore';
import type { AppStore } from '../store/useAppStore';

function seed(patch: Partial<AppStore>): void {
  useAppStore.setState({
    graph: emptyGraph(),
    ui: laneUiState(),
    toasts: [],
    agents: [],
    sidecar: null,
    // The footer's state distribution is counted off this map now, not off
    // `graph.totals` (M1.5 BACKLOG 15a), so an empty one is part of "an empty
    // store" — and another suite's sessions must not leak into these numbers.
    sessions: new Map(),
    ...patch,
  });
}

function commit(): void {
  Object.assign(useAppStore.getInitialState(), useAppStore.getState());
}

function render(): string {
  commit();
  return renderToStaticMarkup(<AppShell />);
}

test('the shell renders from an empty store with honest empty states', () => {
  seed({});
  const html = render();

  assert(html.includes('class="shell"'), 'the grid shell rendered');
  assert(html.includes('class="rail"'), 'the rail rendered');
  assert(html.includes('class="sidebar"'), 'the sidebar rendered');
  assert(html.includes('class="tabbar"'), 'the tab bar rendered');
  assert(html.includes('class="pane-grid"'), 'the pane grid rendered');
  assert(html.includes('class="statusbar"'), 'the status bar rendered');
  assert(html.includes('이 패널에 열린 것이 없습니다.'), 'an unassigned pane shows its empty state');
  assert(html.includes('aria-label="인스펙터"'), 'the inspector rendered');
  assert(!html.includes('undefined'), 'nothing rendered the string "undefined"');

  // Two landmarks, two names. While the rail and the sidebar shared one
  // `aria-label`, macOS System Events answered a query for the second group's
  // children with the first group's, and every button inside the sidebar was
  // unreachable from the accessibility tree (PINNED §23-3).
  assert(
    html.includes('class="rail" aria-label="탐색"'),
    'the rail landmark has its own name',
  );
  assert(
    html.includes('class="sidebar" aria-label="사이드바 · 세션"'),
    'and the sidebar landmark names the view it belongs to',
  );
});

test('a totals op moves the status bar counters', () => {
  seed({});
  assert(render().includes('세션 0 · 워크트리 0 · 실행 중 런 0'), 'the counters start at zero');

  // Exactly the shape a Rust-injected demo `GraphOp` arrives in.
  useAppStore.getState().applyGraphPatch({
    rev: 1,
    ops: [
      {
        op: 'totals',
        totals: {
          ...emptyTotals(),
          projects: 1,
          worktrees: 2,
          sessions: 3,
          sessionsWorking: 1,
          sessionsWaiting: 1,
          sessionsIdle: 1,
          runsActive: 4,
          orphans: 5,
          rssBytes: 2_200_000_000,
          rssBudgetPercent: 41,
          pressureLevel: 'normal',
          sidecarState: 'ready',
        },
      },
    ],
  });

  const after = render();
  assert(after.includes('세션 3 · 워크트리 2 · 실행 중 런 4'), 'the counters followed the op');
  // …but the state distribution does *not* follow the op (M1.5 BACKLOG 15a).
  // `totals.sessionsWorking` is Rust's second fold of the same fact and it was
  // the half that disagreed with the pill, so the footer stopped reading it:
  // no session in the store means no session in any bucket, whatever the op
  // claims. `shell.test.tsx` pins the counting itself.
  assert(
    after.includes('작업 0 · 대기 0 · 유휴 0'),
    'the state distribution comes from the sessions, not from the totals op',
  );
  assert(after.includes('고아 5'), 'the orphan count is shown');
  assert(after.includes('41%'), 'the memory gauge shows the budget percentage');
  assert(after.includes('압력 정상'), 'the pressure level is shown, in Korean (F15)');
  assert(!after.includes('rev 1'), 'no revision counter (R5)');
});

test('an upsertNode op fills the sidebar tree and the inspector', () => {
  seed({});
  const dispose = registerSidebar(Sidebar);
  const store = useAppStore.getState();
  store.applyGraphPatch({
    rev: 1,
    ops: [
      {
        op: 'upsertNode',
        node: {
          id: 'project:p1',
          kind: 'project',
          parentId: null,
          label: 'gilde-code',
          status: 'ok',
          badges: [],
          metrics: {},
          meta: {},
          updatedAt: 1,
        },
      },
      {
        op: 'upsertNode',
        node: {
          id: 'worktree:w1',
          kind: 'worktree',
          parentId: 'project:p1',
          label: 'pty-manager',
          sublabel: 'gilde/pty-manager',
          status: 'dirty',
          badges: ['dirty'],
          metrics: { insertions: 412, deletions: 38 },
          meta: { branch: 'gilde/pty-manager' },
          updatedAt: 1,
        },
      },
    ],
  });
  store.selectNode('worktree:w1');

  const html = render();
  assert(html.includes('gilde-code'), 'the project row rendered');
  assert(html.includes('pty-manager'), 'the worktree row rendered');
  assert(html.includes('data-status="dirty"'), 'the status dot carries the status');
  assert(html.includes('감독 사실'), 'the inspector shows its supervision section');
  assert(html.includes('gilde/pty-manager'), 'the inspector shows the branch');
  dispose();
});

test('switching the locale re-renders every string', () => {
  seed({ ui: { ...laneUiState(), locale: 'en' } });
  const html = render();
  assert(html.includes('Nothing selected.'), 'the inspector empty state is English');
  assert(html.includes('0 sessions · 0 worktrees · 0 runs'), 'the status bar is English');
  assert(!html.includes('워크트리'), 'no Korean is left behind');
});

test('an open tab whose view is not registered renders an empty pane, never a "not available" notice', () => {
  seed({});
  useAppStore.getState().openTab({
    title: 'claude',
    sublabel: 'pty-manager',
    descriptor: { view: 'terminal', sessionId: 'session-1' },
    status: 'working',
    closable: true,
  });
  const html = render();
  assert(html.includes('claude'), 'the tab renders its title');
  assert(html.includes('data-status="working"'), 'the tab renders its status dot');
  assert(html.includes('이 패널에 열린 것이 없습니다.'), 'the pane is simply empty');
  assert(!html.includes('아직 사용할 수 없습니다'), 'no "not available yet" fallback (R7)');
});
