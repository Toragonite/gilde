/**
 * The shell through the real App and event bridge, on a fake core:
 *  - every view change is reported with `app_view_state` [F-04];
 *  - the global keys (⌘0, ⌘K, ⌘J, ⌘B, ⌘/), nothing during IME composition;
 *  - the sidebar's Needs-you header equals the queue's numbers and is hidden
 *    on Home; the tree is one Tab stop with → / ← expand and collapse;
 *  - F6 cycles sidebar → main.
 */

import { act, fireEvent, render } from '@testing-library/react';

import App from '../App';
import type { AppBootstrapRes } from '../lib/gen/AppBootstrapRes';
import type { Member } from '../lib/gen/Member';
import type { MemberKey } from '../lib/gen/MemberKey';
import type { ProjectId } from '../lib/gen/ProjectId';
import type { Session } from '../lib/gen/Session';
import type { StoreSnapshot } from '../lib/gen/StoreSnapshot';
import type { Team } from '../lib/gen/Team';
import type { TeamKey } from '../lib/gen/TeamKey';
import type { ViewState } from '../lib/gen/ViewState';
import { appStore } from '../store/useAppStore';
import { flush, withFakeCore } from '../test/backend';
import type { FakeCore } from '../test/backend';
import { pending, project, queueItem, session, snapshot, uid } from '../test/fixtures';
import { assert, assertDeepEqual, assertEqual, domTest } from '../test/harness';
import { setSlotsForTests } from './slots';
import { press, scenario } from './testing';

const P = 'checkout' as ProjectId;
const TEAM = 't_auth' as TeamKey;

function bootstrap(): AppBootstrapRes {
  return {
    buildId: 'showcase',
    version: '2.0.0',
    locale: 'en',
    theme: 'light',
    launch: { kind: 'restore' },
    uiState: {},
    platform: { macosMajor: 15, arm64: true },
    features: { orchestrator: false },
    compat: [],
  };
}

const lead = session(10, { projectId: P, title: 'lead', startedAt: 1, team: { teamKey: TEAM, memberKey: 'm_lead' as MemberKey, role: 'lead', colour: null } });
const member = session(11, { projectId: P, title: 'member', startedAt: 2, team: { teamKey: TEAM, memberKey: 'm_db' as MemberKey, role: 'member', colour: 'green' } });

function teamOf(): Team {
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
    colour: s.team?.colour ?? null,
    vendor: 'claude',
    backend: 'pane',
    joinedAt: role === 'lead' ? 0 : 1,
    presence: { kind: 'present' },
    pane: null,
    planModeRequired: false,
    currentTask: null,
  };
}

function theWorld(): StoreSnapshot {
  const a = session(1, {
    projectId: P,
    title: 'payment retry',
    startedAt: 5,
    activity: { state: { kind: 'needsApproval' }, since: 0, confidence: 'high', evidence: null, episode: 'e1' as never },
    pending: [pending('r1')],
  });
  const b = session(2, { projectId: P, title: 'docs', startedAt: 6, activity: { state: { kind: 'newReply' }, since: 0, confidence: 'high', evidence: null, episode: 'e2' as never } });
  return snapshot(1, {
    projects: [project('checkout', { liveSessions: 3 })],
    sessions: [a, b, lead, member],
    teams: [teamOf()],
    members: [memberOf(lead, 'team-lead', 'lead'), memberOf(member, 'db-agent', 'member')],
    queue: { rev: 1, items: [queueItem(1, 1), queueItem(2, 5, { reason: { kind: 'newReply' }, primary: 'review' })], needYou: 1, also: 1, pausedUntil: null },
  });
}

const HANDLERS = {
  app_bootstrap: () => bootstrap(),
  store_subscribe: () => theWorld(),
  app_view_state: () => ({}),
  ui_state_set: () => ({}),
  app_mark: () => ({}),
  app_input_activity: () => ({}),
};

async function mount(core: FakeCore) {
  const view = render(<App />);
  await act(async () => {
    await flush(10);
  });
  assertEqual(appStore.getState().world.status, 'live', 'the world is live');
  void core;
  return view;
}

const lastView = (core: FakeCore) => {
  const calls = core.calls('app_view_state');
  return calls[calls.length - 1]?.req as ViewState | undefined;
};

domTest(
  'F-04: navigating sends app_view_state with the screen and the visible session',
  scenario(async () => {
    await withFakeCore(HANDLERS, async (core) => {
      const view = await mount(core);
      assertEqual(lastView(core)?.screen, 'home', 'Home on arrival');
      const row = view.getAllByRole('treeitem').find((el) => el.textContent?.startsWith('payment retry'));
      assert(row, 'the session row');
      fireEvent.click(row);
      await act(async () => {});
      assertEqual(lastView(core)?.screen, 'session', 'Session reported');
      assertEqual(lastView(core)?.visibleUids.join(), uid(1), 'its uid is visible');
      press('Digit0', { meta: true, target: document.body });
      await act(async () => {});
      assertEqual(lastView(core)?.screen, 'home', '⌘0 reported Home');
    });
  }),
);

domTest(
  'global keys: ⌘J opens the next item with its card focused; ⌘K and ⌘/ open their layers; IME composition does nothing',
  scenario(async () => {
    await withFakeCore(HANDLERS, async (core) => {
      await mount(core);
      press('KeyJ', { meta: true, isComposing: true, target: document.body });
      assertEqual(appStore.getState().ui.view.screen, 'home', 'nothing during composition');
      press('KeyJ', { meta: true, target: document.body });
      const ui = appStore.getState().ui;
      assertEqual(ui.view.screen === 'session' ? ui.view.uid : null, uid(1), '⌘J opens rank 0');
      assertEqual(ui.focusCard?.uid, uid(1), 'with its card focused');
      press('KeyK', { meta: true, target: document.body });
      assert(document.querySelector('[role="dialog"] input[role="combobox"]') !== null, '⌘K opens the palette');
      press('Escape');
      assertEqual(appStore.getState().ui.paletteOpen, false, 'Esc closes it');
      press('Slash', { meta: true, target: document.body });
      assert(document.body.textContent?.includes('Keyboard shortcuts') ?? false, '⌘/ opens the help sheet');
    });
  }),
);

domTest(
  'Needs you: hidden on Home; elsewhere its header equals the queue (n · +m)',
  scenario(async () => {
    await withFakeCore(HANDLERS, async (core) => {
      const view = await mount(core);
      assertEqual(view.container.querySelector('.sb-needs'), null, 'hidden on Home');
      act(() => appStore.getState().navigate({ screen: 'history' }));
      const header = view.container.querySelector('#sb-needs-h');
      assertEqual(header?.getAttribute('aria-label'), 'Needs you: 1 item, also 1', 'the queue numbers');
      assertEqual(view.container.querySelectorAll('.sb-q').length, 2, 'every visible item once');
    });
  }),
);

domTest(
  'the tree is one Tab stop; → expands a team, ← collapses it; names carry state and ownership',
  scenario(async () => {
    await withFakeCore(HANDLERS, async (core) => {
      const view = await mount(core);
      const items = view.getAllByRole('treeitem');
      assertEqual(items.filter((el) => el.tabIndex === 0).length, 1, 'one Tab stop');
      const team = items.find((el) => el.textContent?.startsWith('Team auth-split'));
      assert(team, 'the team row');
      assertEqual(team.getAttribute('aria-expanded'), 'false', 'never auto-expanded');
      act(() => team.focus());
      fireEvent.keyDown(team, { code: 'ArrowRight', key: 'ArrowRight' });
      const expanded = view.getAllByRole('treeitem').find((el) => el.textContent?.startsWith('Team auth-split'));
      assertEqual(expanded?.getAttribute('aria-expanded'), 'true', '→ expands');
      const db = view.getAllByRole('treeitem').find((el) => el.textContent?.startsWith('db-agent'));
      assert(db?.textContent?.toLowerCase().includes('in gilde') ?? false, 'ownership in the name');
      fireEvent.keyDown(expanded as HTMLElement, { code: 'ArrowLeft', key: 'ArrowLeft' });
      const collapsed = view.getAllByRole('treeitem').find((el) => el.textContent?.startsWith('Team auth-split'));
      assertEqual(collapsed?.getAttribute('aria-expanded'), 'false', '← collapses');
      const row = view.getAllByRole('treeitem').find((el) => el.textContent?.startsWith('payment retry'));
      assert(row?.textContent?.includes('Needs approval for') ?? false, 'a waiting row says for how long');
    });
  }),
);

domTest(
  'F6 cycles the regions: sidebar → main',
  scenario(async () => {
    await withFakeCore(HANDLERS, async (core) => {
      await mount(core);
      act(() => (document.activeElement as HTMLElement | null)?.blur());
      press('F6', { target: document.body });
      assert(document.activeElement?.closest('[data-region="sidebar"]') !== null, 'the sidebar');
      press('F6');
      assert(document.activeElement?.closest('[data-region="main"]') !== null, 'then main');
    });
  }),
);

domTest(
  '⌘B hides and shows the pinned sidebar',
  scenario(async () => {
    await withFakeCore(HANDLERS, async (core) => {
      const view = await mount(core);
      assert(view.container.querySelector('nav.sidebar') !== null, 'shown');
      press('KeyB', { meta: true, target: document.body });
      assertEqual(view.container.querySelector('nav.sidebar'), null, 'hidden');
      press('KeyB', { meta: true, target: document.body });
      assert(view.container.querySelector('nav.sidebar') !== null, 'back');
    });
  }),
);

domTest(
  "A26.1: Home reports its focus card's session as visible (no notification for it); other screens do not",
  scenario(async () => {
    await withFakeCore(HANDLERS, async (core) => {
      await mount(core);
      const home = lastView(core);
      assertEqual(home?.screen, 'home', 'Home');
      assertEqual(home?.visibleUids.join(), uid(1), "the focus card's session is visible");
      act(() => appStore.getState().navigate({ screen: 'history' }));
      await act(async () => {});
      assertEqual(lastView(core)?.visibleUids.length, 0, 'History shows no session');
      assertEqual(appStore.getState().ui.homeFocusUid, null, 'cleared when Home unmounts');
    });
  }),
);

domTest(
  'A26.2: "+ on a project" and ⌘N in a session open New session with the project',
  scenario(async () => {
    // T18's sheet stands in (a sheet that is not in the build closes at once).
    setSlotsForTests({ NewSessionSheet: () => null });
    await withFakeCore(HANDLERS, async (core) => {
      const view = await mount(core);
      const add = view.container.querySelector<HTMLButtonElement>('.sb-add');
      assert(add, 'the + button');
      fireEvent.click(add);
      assertDeepEqual(appStore.getState().ui.sheetContext, { projectId: P }, '+ passes its project');
      act(() => appStore.getState().closeSheet());
      act(() => appStore.getState().openSession(uid(2)));
      press('KeyN', { meta: true, target: document.body });
      assertDeepEqual(appStore.getState().ui.sheetContext, { projectId: P }, '⌘N: the project of the session in front');
    });
  }),
);
