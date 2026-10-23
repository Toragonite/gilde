/**
 * Sidebar → Split (ux-ia §3.8 step 1; KEYMAP `split.add` scope L; A30.5; T32):
 * ⌘D on a focused session row, dragging a session row onto Main, the 7th-tile
 * choice through `addToSplitOrAsk`, and focus that never moves.
 */

import { act, cleanup, fireEvent, render } from '@testing-library/react';

import { registerHotkeys } from '../hooks/useHotkeys';
import type { Member } from '../lib/gen/Member';
import type { MemberKey } from '../lib/gen/MemberKey';
import type { ProjectId } from '../lib/gen/ProjectId';
import type { Session } from '../lib/gen/Session';
import type { SessionUid } from '../lib/gen/SessionUid';
import type { Team } from '../lib/gen/Team';
import type { TeamKey } from '../lib/gen/TeamKey';
import { MainView } from '../shell/MainView';
import { setSlotsForTests } from '../shell/slots';
import { scenario, seed } from '../shell/testing';
import { resetReplaceForTests, usePendingAdd } from '../split/replace';
import { appStore } from '../store/useAppStore';
import { withFakeCore } from '../test/backend';
import { project, queueItem, session, snapshot, uid } from '../test/fixtures';
import { assert, assertDeepEqual, assertEqual, domTest } from '../test/harness';
import { NeedsYouList } from './NeedsYouList';
import { ProjectTree } from './ProjectTree';
import { SESSION_DRAG_TYPE } from './splitAdd';

const P = 'checkout' as ProjectId;
const TEAM = 't_auth' as TeamKey;

function team(): Team {
  return {
    key: TEAM,
    source: 'claudeNative',
    name: 'auth-split',
    rawId: 'session-1',
    projectId: P,
    lead: 'm_lead' as MemberKey,
    leadBinding: 'hosted',
    hosted: true,
    externalHost: null,
    caps: {} as never,
    glyphState: { kind: 'working' },
    progress: { done: 1, total: 2, inferred: false, approx: false, inFlight: null, bottlenecks: [] },
    startedAt: 0,
    endedAt: null,
    paneHosting: 'hosted',
    rosterStale: false,
    paneTiles: [],
  };
}

function memberOf(s: Session, name: string, role: 'lead' | 'member'): Member {
  return {
    key: s.team?.memberKey as MemberKey,
    teamKey: TEAM,
    sessionUid: s.uid,
    name,
    agentId: null,
    role,
    colour: null,
    vendor: 'claude',
    backend: 'pane',
    joinedAt: role === 'lead' ? 0 : 1,
    presence: { kind: 'present' },
    pane: null,
    planModeRequired: false,
    currentTask: null,
  };
}

function world() {
  const teamSession = (n: number, role: 'lead' | 'member'): Session =>
    session(n, { projectId: P, title: `t${n}`, startedAt: n, team: { teamKey: TEAM, memberKey: `m_${n}` as MemberKey, role, colour: null } });
  const lead = teamSession(10, 'lead');
  const member = teamSession(11, 'member');
  seed(
    snapshot(1, {
      projects: [project('checkout', { liveSessions: 4 })],
      sessions: [
        session(1, { projectId: P, title: 'payment retry', startedAt: 5 }),
        session(2, { projectId: P, title: 'docs', startedAt: 6 }),
        lead,
        member,
      ],
      teams: [team()],
      members: [memberOf(lead, 'team-lead', 'lead'), memberOf(member, 'db-agent', 'member')],
      queue: { rev: 1, items: [queueItem(2, 1)], needYou: 1, also: 0, pausedUntil: null },
    }),
  );
}

function rowOf(container: HTMLElement, startsWith: string): HTMLElement {
  const row = [...container.querySelectorAll<HTMLElement>('[role="treeitem"]')].find((el) => el.textContent?.startsWith(startsWith));
  assert(row, `row ${startsWith}`);
  return row;
}

function cmdD(target: Element) {
  fireEvent.keyDown(target, { code: 'KeyD', key: 'd', metaKey: true });
}

function dataTransfer(data: Record<string, string> = {}) {
  const store: Record<string, string> = { ...data };
  return {
    get types() {
      return Object.keys(store);
    },
    setData: (k: string, v: string) => {
      store[k] = v;
    },
    getData: (k: string) => store[k] ?? '',
    effectAllowed: 'all',
    dropEffect: 'none',
    store,
  };
}

let pendingSeen: SessionUid | null = null;
function PendingProbe() {
  pendingSeen = usePendingAdd();
  return null;
}

function mountAll() {
  setSlotsForTests({ HistoryScreen: () => <p>history</p>, SplitScreen: () => <p>split</p> });
  return render(
    <>
      <ProjectTree />
      <NeedsYouList />
      <MainView label="Main" sidebarShown />
      <PendingProbe />
    </>,
  );
}

const T32 = (run: () => Promise<void>) =>
  scenario(async () => {
    resetReplaceForTests();
    await withFakeCore({}, async () => {
      try {
        await run();
      } finally {
        cleanup();
        resetReplaceForTests();
      }
    });
  }, { view: { screen: 'history' } });

domTest(
  'T32 ⌘D on a focused session row adds it to the Split; focus and the view stay',
  T32(async () => {
    world();
    const view = mountAll();
    const row = rowOf(view.container, 'payment retry');
    act(() => row.focus());
    cmdD(row);
    assertDeepEqual(appStore.getState().ui.split.uids, [uid(1)], 'added');
    assertEqual(document.activeElement, row, 'focus stays on the row');
    assertEqual(appStore.getState().ui.view.screen, 'history', 'no navigation');
    const needs = view.container.querySelector<HTMLElement>(`.sb-q[data-split-uid="${uid(2)}"]`);
    assert(needs, 'the Needs-you row of session 2');
    act(() => needs.focus());
    cmdD(needs);
    assertDeepEqual(appStore.getState().ui.split.uids, [uid(1), uid(2)], 'Needs you adds too');
    assertEqual(document.activeElement, needs, 'focus stays');
  }),
);

domTest(
  'T32 ⌘D on a team row or a project header does nothing — and never falls through to the open session\'s ⌘D',
  T32(async () => {
    world();
    let fellThrough = 0;
    const off = registerHotkeys(() => [{ id: 'session.split.add', combo: 'mod+d', run: () => fellThrough++ }], { scope: 'S' });
    try {
      const view = mountAll();
      for (const name of ['Team auth-split', 'checkout']) {
        const row = rowOf(view.container, name);
        act(() => row.focus());
        cmdD(row);
        assertEqual(document.activeElement, row, `${name}: focus stays`);
      }
      assertDeepEqual(appStore.getState().ui.split.uids, [], 'nothing added');
      assertEqual(fellThrough, 0, 'consumed by the list');
    } finally {
      off();
    }
  }),
);

domTest(
  'T32 a full Split asks which tile to replace (addToSplitOrAsk), from ⌘D and from a drop',
  T32(async () => {
    world();
    const six = [3, 4, 5, 6, 7, 8].map(uid);
    act(() => appStore.setState((s) => ({ ui: { ...s.ui, split: { uids: six, focused: null, zoomed: null } } })));
    const view = mountAll();
    const row = rowOf(view.container, 'payment retry');
    act(() => row.focus());
    cmdD(row);
    assertEqual(appStore.getState().ui.view.screen, 'split', 'the Split opens with the choice');
    assertEqual(pendingSeen, uid(1), 'session 1 waits for a tile');
    assertDeepEqual(appStore.getState().ui.split.uids, six, 'nothing replaced yet');
    resetReplaceForTests();
    const main = view.container.querySelector('main') as HTMLElement;
    fireEvent.drop(main, { dataTransfer: dataTransfer({ [SESSION_DRAG_TYPE]: uid(2) }) });
    assertEqual(pendingSeen, uid(2), 'a drop asks the same way');
  }),
);

domTest(
  'T32 drag: session rows are draggable with the Gilde type, other rows are not; a drop on Main adds and opens the Split',
  T32(async () => {
    world();
    const view = mountAll();
    const session1 = rowOf(view.container, 'payment retry');
    assertEqual(session1.getAttribute('draggable'), 'true', 'a session row drags');
    for (const name of ['Team auth-split', 'checkout']) assertEqual(rowOf(view.container, name).getAttribute('draggable'), null, `${name} does not drag`);

    const dt = dataTransfer();
    fireEvent.dragStart(session1, { dataTransfer: dt });
    assertDeepEqual(dt.store, { [SESSION_DRAG_TYPE]: uid(1) }, 'only the uid, under the Gilde type');

    act(() => session1.focus());
    const main = view.container.querySelector('main') as HTMLElement;
    fireEvent.dragOver(main, { dataTransfer: dt });
    assert(view.container.querySelector('.shell-drop') !== null, 'the drop affordance');
    fireEvent.dragLeave(main, { dataTransfer: dt, relatedTarget: document.body });
    assertEqual(view.container.querySelector('.shell-drop'), null, 'gone when the drag leaves');
    fireEvent.dragOver(main, { dataTransfer: dt });
    fireEvent.drop(main, { dataTransfer: dt });
    assertDeepEqual(appStore.getState().ui.split.uids, [uid(1)], 'added');
    assertEqual(appStore.getState().ui.view.screen, 'split', 'elsewhere on Main: the Split opens');
    assertEqual(view.container.querySelector('.shell-drop'), null, 'affordance cleared');
    assertEqual(document.activeElement, session1, 'focus did not move');

    fireEvent.drop(view.container.querySelector('main') as HTMLElement, { dataTransfer: dataTransfer({ [SESSION_DRAG_TYPE]: uid(2) }) });
    assertDeepEqual(appStore.getState().ui.split.uids, [uid(1), uid(2)], 'on the Split itself: a tile');
    assertEqual(appStore.getState().ui.view.screen, 'split', 'stays');
  }),
);

domTest(
  'T32 drop: anything but a known session row is ignored (another type, a stale uid, text)',
  T32(async () => {
    world();
    const view = mountAll();
    const main = view.container.querySelector('main') as HTMLElement;
    fireEvent.dragOver(main, { dataTransfer: dataTransfer({ 'text/plain': 'hello' }) });
    assertEqual(view.container.querySelector('.shell-drop'), null, 'no affordance for text');
    fireEvent.drop(main, { dataTransfer: dataTransfer({ 'text/plain': uid(1) }) });
    fireEvent.drop(main, { dataTransfer: dataTransfer({ [SESSION_DRAG_TYPE]: uid(99) }) });
    fireEvent.drop(main, { dataTransfer: dataTransfer({ [SESSION_DRAG_TYPE]: 'not-a-uid' }) });
    assertDeepEqual(appStore.getState().ui.split.uids, [], 'nothing added');
    assertEqual(appStore.getState().ui.view.screen, 'history', 'no navigation');
  }),
);
