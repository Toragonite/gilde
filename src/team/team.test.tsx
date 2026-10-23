/**
 * Team screen tests (contract §15.2 T16: tile focus/keys, never steal focus,
 * layout persistence, ownership bars for external teams, prefill never sends;
 * §15.5 Board on the no-task-tools fixture; §15.6 F-04, F-15, F-49; A21.4,
 * A23.2; A44.1 a tile's guard restarts for a replaced request; ux-ia §3.8–§3.10, J5).
 */

import { act, cleanup, fireEvent, render } from '@testing-library/react';

import { resetAnswersForTests } from '../approval/answer';
import { GUARD_MS, refreshGuards, resetAnswerGuardForTests } from '../hooks/useAnswerGuard';
import { resetHotkeysForTests } from '../hooks/useHotkeys';
import { manualClock, setClockForTests } from '../lib/clock';
import type { PaneTile } from '../lib/gen/PaneTile';
import type { PendingInteraction } from '../lib/gen/PendingInteraction';
import type { PtyId } from '../lib/gen/PtyId';
import { setTerminalFactory } from '../terminal/terminalPool';
import type { PooledTerminal } from '../terminal/terminalPool';
import { translate } from '../i18n';
import type { Member } from '../lib/gen/Member';
import type { ScreenPromptKeyReq } from '../lib/gen/ScreenPromptKeyReq';
import type { Session } from '../lib/gen/Session';
import type { StoreSnapshot } from '../lib/gen/StoreSnapshot';
import type { Team } from '../lib/gen/Team';
import type { TeamViewMode } from '../lib/gen/TeamViewMode';
import type { ViewState } from '../lib/gen/ViewState';
import { getAnnouncements, resetAnnouncerForTests } from '../lib/notify';
import { resetFocusForTests } from '../split/focus';
import { resetReplaceForTests } from '../split/replace';
import { PERSIST_DEBOUNCE_MS, startUiPersistence } from '../store/persist';
import { appStore, useAppStore } from '../store/useAppStore';
import { initialUiState } from '../store/uiSlice';
import { startViewStateReporter } from '../store/viewState';
import { emptyWorld } from '../store/worldSlice';
import { flush, withFakeCore } from '../test/backend';
import type { FakeCore, Handler } from '../test/backend';
import { pending, session, snapshot, uid } from '../test/fixtures';
import { assert, assertDeepEqual, assertEqual, domTest } from '../test/harness';
import { Board, resetBoardForTests } from './Board';
import { ClosePaneSheet } from './ClosePaneSheet';
import { resetPanesForTests } from './Panes';
import { TeamScreen } from './TeamScreen';
import {
  TK,
  boardTask,
  hostedCaps,
  inferredItem,
  lead,
  member,
  message,
  mkey,
  paneSession,
  team,
  watchedSession,
} from './testing';

let rev = 5000;

function setWorld(parts: Partial<StoreSnapshot>) {
  act(() => {
    appStore.getState().applyStoreSnapshot(snapshot(++rev, parts));
  });
}

/** A hosted team: lead (1) + api-agent (2) + db-agent (3), all In Gilde. */
function hostedWorld(extra: { team?: Partial<Team>; members?: Member[]; sessions?: Session[]; parts?: Partial<StoreSnapshot> } = {}) {
  setWorld({
    teams: [team(extra.team)],
    members: extra.members ?? [lead(), member(2, 'api-agent'), member(3, 'db-agent', { colour: 'green' })],
    sessions: extra.sessions ?? [paneSession(1, {}, 'lead'), paneSession(2), paneSession(3)],
    ...extra.parts,
  });
}

function openTeam(mode: TeamViewMode) {
  act(() => {
    const ui = appStore.getState().ui;
    appStore.setState({ ui: { ...ui, view: { screen: 'team', teamKey: TK, mode } } });
  });
}

/** The screen as the shell mounts it: props follow the main view. */
function Mounted() {
  const view = useAppStore((s) => s.ui.view);
  if (view.screen !== 'team') return null;
  return (
    <>
      <TeamScreen teamKey={view.teamKey} mode={view.mode} band="wide" />
      <ClosePaneSheet />
    </>
  );
}

const base: Record<string, Handler> = {
  convo_subscribe: () => ({ subId: 'v_00000000000000f1', source: 'stream', note: null }),
  convo_unsubscribe: () => ({}),
  session_mark_seen: () => ({}),
  app_view_state: () => ({}),
  ui_state_set: () => ({}),
  owner_focus: () => ({ precision: 'app' }),
  interaction_answer: () => ({ outcome: 'answered' }),
  screen_prompt_key: () => ({ outcome: 'sent' }),
  team_send: () => ({ route: 'memberPane' }),
  team_message_body: () => ({ text: 'Take token verification.\nKeep the exported names.', summary: 'token work', source: 'recipientTranscript' }),
  team_messages_page: () => ({ messages: [], hasMore: false }),
  team_task_title: () => ({ title: null }),
  team_member_close_preview: (req) => ({
    lines: [
      { key: 'team.closePane', params: {} },
      { key: 'team.closePaneUnread', params: { member: (req as { memberKey: string }).memberKey === mkey(2) ? 'api-agent' : 'x' } },
    ],
    confirmToken: 'c_00000000000000000000000000000001',
  }),
  team_member_close: () => ({}),
};

const NEVER_SENT = ['turn_send', 'team_send', 'interaction_answer', 'screen_prompt_key'] as const;

function harness(run: (core: FakeCore) => Promise<void>, extra: Record<string, Handler> = {}) {
  return async () => {
    resetHotkeysForTests();
    resetAnswerGuardForTests();
    resetAnswersForTests();
    resetAnnouncerForTests();
    resetFocusForTests();
    resetReplaceForTests();
    resetPanesForTests();
    resetBoardForTests();
    appStore.setState({ world: emptyWorld(), ui: { ...initialUiState(), locale: 'en', hydrated: true } });
    try {
      await withFakeCore({ ...base, ...extra }, async (core) => {
        try {
          await run(core);
        } finally {
          cleanup();
        }
      });
    } finally {
      resetHotkeysForTests();
      setClockForTests(null);
      setTerminalFactory(null);
      appStore.setState({ world: emptyWorld(), ui: initialUiState() });
    }
  };
}

async function settle(ms = 40) {
  await act(async () => {
    await flush(10);
    await new Promise((r) => setTimeout(r, ms));
  });
}

const key = (k: string, mods: { meta?: boolean; alt?: boolean; shift?: boolean } = {}) => {
  const code = /^[0-9]$/.test(k) ? `Digit${k}` : k.length === 1 ? `Key${k.toUpperCase()}` : k;
  fireEvent.keyDown(document.activeElement ?? document.body, { key: k, code, metaKey: !!mods.meta, altKey: !!mods.alt, shiftKey: !!mods.shift });
};

const tiles = (c: HTMLElement) => [...c.querySelectorAll<HTMLElement>('section.g-tile')];
const composerOf = (c: HTMLElement) => c.querySelector<HTMLTextAreaElement>('textarea.g-composer-input');
const layout = () => appStore.getState().ui.teamLayouts[TK];

domTest(
  'Header and modes: Team name, lead + 2, In Gilde; ⌥⌘2 shows the Board without a history entry and keeps the mode in the layout',
  harness(async () => {
    hostedWorld();
    openTeam('panes');
    const view = render(<Mounted />);
    await settle();
    assert(view.getByRole('heading', { name: translate('en', 'team.title', { name: 'auth-split' }) }) !== null, 'title');
    const meta = view.container.querySelector('.g-team-meta')?.textContent ?? '';
    assert(meta.includes(translate('en', 'team.leadPlus', { members: 2 })) && meta.includes(translate('en', 'ownership.inGilde')), `meta: ${meta}`);
    const tabs = view.getAllByRole('tab').map((t) => t.textContent);
    assertDeepEqual(tabs, ['Panes', 'Board', 'Floor'], 'Panes · Board · Floor');
    const backBefore = appStore.getState().ui.back.length;
    key('2', { meta: true, alt: true });
    await settle();
    const v = appStore.getState().ui.view;
    assertEqual(v.screen === 'team' ? v.mode : null, 'board', '⌥⌘2 = Board');
    assertEqual(appStore.getState().ui.back.length, backBefore, 'no history entry (a sub-view)');
    assertEqual(layout()?.mode, 'board', 'kept in teamLayout');
    assert(view.getByRole('heading', { name: translate('en', 'team.board.members') }) !== null, 'the Board shows');
  }),
);

domTest(
  'Panes: lead + teammates as tiles; ⌘2 focuses the first teammate (store + composer); app_view_state names its session [F-04]',
  harness(async (core) => {
    hostedWorld();
    openTeam('panes');
    const stop = startViewStateReporter(
      (l) => appStore.subscribe((s) => l(s.ui, s.world)),
      { ui: appStore.getState().ui, world: appStore.getState().world },
    );
    try {
      const view = render(<Mounted />);
      await settle();
      const list = tiles(view.container);
      assertEqual(list.length, 3, 'three tiles');
      assertEqual(list[0]?.getAttribute('data-uid'), uid(1), 'the lead first');
      assert(list[0]?.textContent?.includes(translate('en', 'team.lead')) ?? false, 'Lead label');
      key('2', { meta: true });
      await settle();
      assertEqual(layout()?.focused, mkey(2), 'store focus = api-agent');
      assertEqual(document.activeElement?.closest('section')?.getAttribute('data-uid'), uid(2), 'DOM focus in its tile');
      assertEqual(document.activeElement?.tagName, 'TEXTAREA', 'its composer');
      const reports = core.calls('app_view_state').map((c) => c.req as ViewState);
      const last = reports[reports.length - 1] as ViewState;
      assertEqual(last.screen, 'team', 'team screen');
      assertEqual(last.focusedUid, uid(2), 'focused member session reported');
      assertEqual(last.teamKey, TK, 'team key');
    } finally {
      stop();
    }
  }),
);

domTest(
  'A new teammate\'s tile appears with a pulse and one polite announcement; focus stays in the lead\'s composer',
  harness(async () => {
    hostedWorld();
    openTeam('panes');
    const view = render(<Mounted />);
    await settle();
    const leadComposer = composerOf(tiles(view.container)[0] as HTMLElement) as HTMLTextAreaElement;
    act(() => leadComposer.focus());
    hostedWorld({
      members: [lead(), member(2, 'api-agent'), member(3, 'db-agent'), member(4, 'test-agent', { colour: 'pink' })],
      sessions: [paneSession(1, {}, 'lead'), paneSession(2), paneSession(3), paneSession(4)],
    });
    await settle();
    const fresh = view.container.querySelector('section.g-tile[data-new="true"]');
    assertEqual(fresh?.getAttribute('data-uid'), uid(4), 'the new tile pulses');
    assertEqual(document.activeElement, leadComposer, 'focus never moved');
    const said = getAnnouncements().polite?.text ?? '';
    assert(said.includes('test-agent'), `announced: ${said}`);
  }),
);

domTest(
  'Prefill never sends: "Ask the lead to stop db-agent" fills the lead\'s composer and puts the cursor there; nothing is sent',
  harness(async (core) => {
    hostedWorld();
    openTeam('panes');
    const view = render(<Mounted />);
    await settle();
    const dbTile = tiles(view.container)[2] as HTMLElement;
    fireEvent.click(dbTile.querySelector(`[aria-label="${translate('en', 'tile.menu')}"]`) as HTMLElement);
    await settle();
    fireEvent.click(view.getByRole('menuitem', { name: translate('en', 'team.action.askStop', { member: 'db-agent' }) }));
    await settle();
    assertEqual(appStore.getState().ui.drafts[uid(1)], translate('en', 'team.prefill.askStop', { member: 'db-agent' }), 'the lead draft');
    assertEqual(layout()?.focused, mkey(1), 'the lead tile is focused');
    assertEqual(document.activeElement?.closest('section')?.getAttribute('data-uid'), uid(1), 'cursor in the lead tile');
    for (const name of NEVER_SENT) assertEqual(core.calls(name).length, 0, `${name} never called`);
  }),
);

domTest(
  'Board To: composer — a hosted pane member gets team_send; a member reached through the lead is prefilled there and never sent',
  harness(async (core) => {
    hostedWorld({
      members: [lead(), member(2, 'api-agent'), member(4, 'helper', { backend: 'inProcess', pane: null })],
      sessions: [paneSession(1, {}, 'lead'), paneSession(2)],
    });
    openTeam('board');
    const view = render(<Mounted />);
    await settle();
    const select = view.getByLabelText(translate('en', 'team.msg.to')) as HTMLSelectElement;
    const input = view.getByLabelText(translate('en', 'team.msg.input')) as HTMLTextAreaElement;
    fireEvent.change(select, { target: { value: mkey(2) } });
    fireEvent.change(input, { target: { value: 'take the refresh path' } });
    fireEvent.keyDown(input, { key: 'Enter', code: 'Enter' });
    await settle();
    const sent = core.calls('team_send')[0]?.req as { memberKey: string; text: string } | undefined;
    assertEqual(sent?.memberKey, mkey(2), 'typed into api-agent');
    assertEqual(sent?.text, 'take the refresh path', 'the text');
    assertEqual(input.value, '', 'cleared');
    assert(view.getByText(translate('en', 'team.msg.sentNote')) !== null, 'only the fact is kept');

    fireEvent.change(select, { target: { value: mkey(4) } });
    await settle();
    assert(view.getByText(translate('en', 'team.msg.viaLeadNote')) !== null, 'says it goes through the lead');
    fireEvent.change(view.getByLabelText(translate('en', 'team.msg.input')), { target: { value: 'check the cache keys' } });
    fireEvent.keyDown(view.getByLabelText(translate('en', 'team.msg.input')), { key: 'Enter', code: 'Enter' });
    await settle();
    assertEqual(
      appStore.getState().ui.drafts[uid(1)],
      `${translate('en', 'team.prefill.tell', { member: 'helper' })} check the cache keys`,
      'prefilled at the lead',
    );
    assertEqual(core.calls('team_send').length, 1, 'no second team_send');
    assertEqual(core.calls('turn_send').length, 0, 'nothing sent to the lead');
    const v = appStore.getState().ui.view;
    assertEqual(v.screen === 'team' ? v.mode : null, 'panes', 'shown at the lead tile');
  }),
);

domTest(
  'F-49: "Ask the lead to add a task…" is hidden without task tools and offered (prefill only) when they exist',
  harness(async (core) => {
    hostedWorld();
    openTeam('board');
    const view = render(<Mounted />);
    await settle();
    assertEqual(view.queryByText(translate('en', 'team.action.addTask')), null, 'hidden on the inferred board');
    cleanup();
    hostedWorld({ team: { caps: hostedCaps({ observeTasks: 'yes' }) }, parts: { tasks: [boardTask(1, 'Extract verifyToken()', 'inProgress')] } });
    const again = render(<Mounted />);
    await settle();
    const ask = again.getAllByText(translate('en', 'team.action.addTask'));
    fireEvent.click(ask[0] as HTMLElement);
    await settle();
    assertEqual(appStore.getState().ui.drafts[uid(1)], `${translate('en', 'team.prefill.addTask')} `, 'prefilled');
    for (const name of NEVER_SENT) assertEqual(core.calls(name).length, 0, `${name} never called`);
  }),
);

domTest(
  'Board on the no-task-tools fixture: inferred items are the board (never "no shared task list"); counts "≈" or unknown; "never read" to a closed pane',
  harness(async () => {
    hostedWorld({
      team: { progress: { done: 1, total: 2, inferred: true, approx: true, inFlight: null, bottlenecks: [] } },
      members: [lead(), member(2, 'api-agent'), member(3, 'db-agent', { presence: { kind: 'gone', reason: 'processExited', at: 5 } })],
      parts: {
        tasks: [inferredItem(1, mkey(2), 1), inferredItem(2, mkey(2), 2, 'reported')],
        messages: [message(1, mkey(1), mkey(3), { status: 'queued' })],
      },
    });
    openTeam('board');
    const view = render(<Board team={appStore.getState().world.teams.get(TK) as Team} band="wide" tabs={false} />);
    await settle();
    assertEqual(view.queryByText(translate('en', 'team.noTasks')), null, 'never "no shared task list"');
    assert(view.getByText(translate('en', 'floor.request', { k: 1 })) !== null, '"Request 1"');
    assert(view.getAllByText(translate('en', 'floor.inferred')).length >= 2, 'marked inferred');
    assert(view.getByText(translate('en', 'team.count.approx', { value: '2' })) !== null, 'tasks ≈ 2');
    assert(view.getByText(translate('en', 'team.neverRead')) !== null, '"never read"');
    assert(view.getByText(translate('en', 'team.count.approx', { value: '1' })) !== null, 'messages ≈ 1 (partial)');
    cleanup();
    hostedWorld({ parts: { tasks: [], messages: [] } });
    const empty = render(<Board team={appStore.getState().world.teams.get(TK) as Team} band="wide" tabs={false} />);
    await settle();
    assert(empty.getByText(translate('en', 'team.messagesUnknown')) !== null, 'empty log: "Message count unknown", never "no messages"');
    assertEqual(empty.queryByText(translate('en', 'team.noTasks')), null, 'an empty inferred board is not "no work"');
  }),
);

domTest(
  'Message bodies on demand: Show text reads team_message_body; Hide text drops it; the store never holds it',
  harness(async (core) => {
    hostedWorld({ parts: { messages: [message(1, mkey(1), mkey(2), { hasSummary: true })] } });
    openTeam('board');
    const view = render(<Mounted />);
    await settle();
    assertEqual(core.calls('team_message_body').length, 0, 'not read until asked');
    fireEvent.click(view.getByText(translate('en', 'team.msg.showText')));
    await settle();
    assertEqual((core.calls('team_message_body')[0]?.req as { messageKey: string }).messageKey, message(1, mkey(1), mkey(2)).key, 'by message key');
    assert(view.container.textContent?.includes('Keep the exported names.') ?? false, 'the text shows');
    assert(!JSON.stringify([...appStore.getState().world.messages.values()]).includes('exported names'), 'never in the store');
    fireEvent.click(view.getByText(translate('en', 'team.msg.hideText')));
    await settle();
    assert(!(view.container.textContent?.includes('Keep the exported names.') ?? true), 'dropped when hidden');
  }),
);

domTest(
  'Board rows: M focuses the To: composer with that member; ⏎ on a row shows its pane focused',
  harness(async () => {
    hostedWorld();
    openTeam('board');
    const view = render(<Mounted />);
    await settle();
    const rows = [...view.container.querySelectorAll<HTMLElement>('[data-member-row]')];
    assertEqual(rows.length, 3, 'a row per member');
    act(() => (rows[2] as HTMLElement).focus());
    key('m');
    await settle();
    assertEqual((view.getByLabelText(translate('en', 'team.msg.to')) as HTMLSelectElement).value, mkey(3), 'To: db-agent');
    assertEqual(document.activeElement?.getAttribute('aria-label'), translate('en', 'team.msg.input'), 'the To: input has focus');
    act(() => (view.container.querySelectorAll<HTMLElement>('[data-member-row]')[1] as HTMLElement).focus());
    key('Enter');
    await settle();
    const v = appStore.getState().ui.view;
    assertEqual(v.screen === 'team' ? v.mode : null, 'panes', 'Panes');
    assertEqual(layout()?.focused, mkey(2), 'api-agent focused');
    assertEqual(document.activeElement?.closest('section')?.getAttribute('data-uid'), uid(2), 'its tile has focus');
  }),
);

domTest(
  'Ownership bars: an iTerm2 team gets the sentence and Go to iTerm2 in the To: slot; an orchestrator team "Message it from its lead."; tiles keep T15\'s bar',
  harness(async (core) => {
    setWorld({
      teams: [team({ hosted: false, externalHost: 'iTerm2', paneHosting: 'outsideGilde', caps: hostedCaps({ actSend: ['viaLead'], actStop: ['viaLead'] }) })],
      members: [lead(), member(2, 'api-agent', { backend: 'inProcess', pane: null })],
      sessions: [watchedSession(1), watchedSession(2)],
    });
    openTeam('board');
    const view = render(<Mounted />);
    await settle();
    assertEqual(view.queryByLabelText(translate('en', 'team.msg.input')), null, 'no To: composer');
    assert(view.getByText(translate('en', 'team.external')) !== null, 'iTerm2 sentence');
    assertEqual(view.queryByText(translate('en', 'action.askLead')), null, 'no Ask the lead (lead not in Gilde)');
    fireEvent.click(view.getByText(translate('en', 'action.goToOwner', { owner: 'iTerm2' })));
    await settle();
    assertEqual((core.calls('owner_focus')[0]?.req as { uid: string }).uid, uid(1), 'Go to iTerm2 (the lead)');
    fireEvent.click(view.getByRole('tab', { name: 'Panes' }));
    await settle();
    assertEqual(composerOf(view.container), null, 'no tile composer for watched members');
    assert(view.container.querySelectorAll('.g-tile .g-bar').length >= 2, 'each tile shows the ownership bar');
    cleanup();
    setWorld({
      teams: [team({ source: 'orchestrator', hosted: false, paneHosting: 'notApplicable', caps: hostedCaps({ actSend: [], actBroadcast: [], actSpawn: [], actStop: [], observeTasks: 'yes' }) })],
      members: [lead({ backend: 'orchestrator' }), member(2, 'codex-writer', { backend: 'orchestrator', pane: null, vendor: 'codex' })],
      sessions: [watchedSession(1), watchedSession(2)],
    });
    openTeam('board');
    const orch = render(<Mounted />);
    await settle();
    assert(orch.getByText(translate('en', 'orchestrator.messageFromLead')) !== null, 'orchestrator sentence');
    assertEqual(orch.queryByText(translate('en', 'team.action.addTask')), null, 'no actions in v1');
  }),
);

domTest(
  'In-process fallback: Board and Floor only; Panes asked for shows the Board and says why',
  harness(async () => {
    hostedWorld({ team: { paneHosting: 'inProcessFallback' } });
    openTeam('panes');
    const view = render(<Mounted />);
    await settle();
    assertDeepEqual(view.getAllByRole('tab').map((t) => t.textContent), ['Board', 'Floor'], 'no Panes');
    const v = appStore.getState().ui.view;
    assertEqual(v.screen === 'team' ? v.mode : null, 'board', 'the view says board');
    assert(view.getByText(translate('en', 'team.inProcess')) !== null, '"Teammates run inside the lead in this version."');
    assertEqual(tiles(view.container).length, 0, 'no tiles');
  }),
);

domTest(
  'Team › Floor mounts TeamFloor under the team header and mode switch (A23.2)',
  harness(async () => {
    hostedWorld();
    openTeam('floor');
    const view = render(<Mounted />);
    await settle();
    const floor = view.container.querySelector('.g-team-floor');
    assert(floor !== null, 'the Floor body');
    assert((floor?.children.length ?? 0) > 0, 'TeamFloor rendered inside it');
    assert(view.getByRole('tab', { name: 'Floor', selected: true }) !== null, 'Floor tab selected');
  }),
);

domTest(
  'Close pane…: preview lines through i18n (A21.4), focus on Cancel, ⌘⏎ never confirms, Close pane closes with the token',
  harness(async (core) => {
    hostedWorld();
    openTeam('panes');
    const view = render(<Mounted />);
    await settle();
    const apiTile = tiles(view.container)[1] as HTMLElement;
    fireEvent.click(apiTile.querySelector(`[aria-label="${translate('en', 'tile.menu')}"]`) as HTMLElement);
    await settle();
    fireEvent.click(view.getByRole('menuitem', { name: translate('en', 'team.action.closePane') }));
    await settle();
    assertEqual(appStore.getState().ui.sheet, 'closePane', 'the sheet');
    assert(document.body.textContent?.includes(translate('en', 'team.closePane')) ?? false, 'line 1');
    assert(document.body.textContent?.includes(translate('en', 'team.closePaneUnread', { member: 'api-agent' })) ?? false, 'line 2 with the member');
    assertEqual(document.activeElement?.textContent, translate('en', 'common.cancel'), 'focus on Cancel');
    fireEvent.keyDown(document.activeElement as HTMLElement, { key: 'Enter', code: 'Enter', metaKey: true });
    await settle();
    assertEqual(core.calls('team_member_close').length, 0, '⌘⏎ does not confirm');
    const confirm = [...document.querySelectorAll<HTMLButtonElement>('[role="dialog"] button')].find((b) => b.textContent === translate('en', 'team.closePaneTitle'));
    fireEvent.click(confirm as HTMLElement);
    await settle();
    const req = core.calls('team_member_close')[0]?.req as { memberKey: string; confirmToken: string } | undefined;
    assertEqual(req?.memberKey, mkey(2), 'api-agent');
    assertEqual(req?.confirmToken, 'c_00000000000000000000000000000001', 'the token');
    assertEqual(appStore.getState().ui.sheet, null, 'closed');
  }),
);

domTest(
  'A crashed teammate keeps its tile ("Crashed · exit 1") until Close tile; an exited-0 teammate has none',
  harness(async () => {
    hostedWorld({
      members: [
        lead(),
        member(2, 'api-agent', { presence: { kind: 'gone', reason: 'processExited', at: 5 } }),
        member(3, 'db-agent', { presence: { kind: 'gone', reason: 'shutdownApproved', at: 5 } }),
      ],
      sessions: [
        paneSession(1, {}, 'lead'),
        paneSession(2, { activity: { ...session(2).activity, state: { kind: 'crashed', exit: { code: 1, signal: null } } } }),
        paneSession(3, { activity: { ...session(3).activity, state: { kind: 'ended' } } }),
      ],
    });
    openTeam('panes');
    const view = render(<Mounted />);
    await settle();
    assertDeepEqual(tiles(view.container).map((t) => t.getAttribute('data-uid')), [uid(1), uid(2)], 'lead + the crashed tile');
    assert(view.getByText(translate('en', 'tile.crashed', { code: 1 })) !== null, '"Crashed · exit 1"');
    fireEvent.click(view.getByText(translate('en', 'action.closeTile')));
    await settle();
    assertEqual(tiles(view.container).length, 1, 'closed by the user');
  }),
);

domTest(
  'T-ui-answerInRouting: a teammate\'s prompt that renders in the lead — the member tile\'s mirrored card answers in the lead [F-15]',
  harness(async (core) => {
    // A pane teammate's permission prompt renders in the lead's TUI (§7.2.6): a screen prompt on the lead, mirrored on the member.
    const card = {
      kind: 'screenPrompt' as const,
      onBehalfOf: mkey(3),
      title: 'Edit schema.sql',
      promptRev: 'p-1',
      options: [
        { key: '1', label: 'Yes', role: 'allowOnce' as const },
        { key: '2', label: 'Yes, allow all edits during this session', role: 'allowSession' as const },
        { key: '3', label: 'No', role: 'deny' as const },
      ],
    };
    hostedWorld({
      sessions: [
        paneSession(1, { pending: [pending('sp_p-1', card)] }, 'lead'),
        paneSession(2),
        paneSession(3, { pending: [pending('sp_p-1', { ...card, answerIn: uid(1) })] }),
      ],
    });
    openTeam('panes');
    const view = render(<Mounted />);
    await settle(800);
    const dbTile = tiles(view.container)[2] as HTMLElement;
    assert(dbTile.textContent?.includes(translate('en', 'team.answeredAtLead')) ?? false, 'mirrored, "answered at the lead"');
    fireEvent.click(dbTile.querySelector('[data-role="allowOnce"]') as HTMLElement);
    await settle();
    const call = core.calls('screen_prompt_key')[0]?.req as { uid: string; requestId: string; action: { kind: string; key: string } } | undefined;
    assertEqual(call?.uid, uid(1), 'answered in the lead');
    assertEqual(call?.requestId, 'sp_p-1', 'that request');
    assertDeepEqual(call?.action, { kind: 'key', key: '1' }, 'the prompt\'s own key');
    assertEqual(core.calls('interaction_answer').length, 0, 'never a structured answer to a screen prompt');
  }),
);

domTest(
  'Team layout persistence: mode, focus and zoom are saved as teamLayout:<TeamKey>',
  harness(async (core) => {
    hostedWorld();
    openTeam('panes');
    const stop = startUiPersistence((l) => appStore.subscribe((s) => l(s.ui)), appStore.getState().ui);
    try {
      render(<Mounted />);
      await settle();
      key('3', { meta: true });
      await settle();
      key('Enter', { meta: true, shift: true });
      await settle(PERSIST_DEBOUNCE_MS + 60);
      const saved = core.calls('ui_state_set').map((c) => c.req as { key: string; value: unknown }).filter((r) => r.key === `teamLayout:${TK}`);
      assert(saved.length > 0, 'saved');
      assertDeepEqual(saved[saved.length - 1]?.value, { mode: 'panes', focused: mkey(3), zoomed: mkey(3), leadRatio: null, tileOrder: null }, 'mode, focus, zoom (A30.3 fields too)');
    } finally {
      stop();
    }
  }),
);

// ---------------------------------------------------------------------------
// A30 (T16b): pane tiles, visible sets, saved divider and tile order
// ---------------------------------------------------------------------------

const PANE_PTY = 'y_0192f000-0000-7000-8000-0000000000aa' as PtyId;

function paneTile(n: number, state: PaneTile['state'], extra: Partial<PaneTile> = {}): PaneTile {
  return { paneId: `%${n}` as PaneTile['paneId'], state, title: null, pty: null, createdAt: 1_000 + n, ...extra };
}

function fakeTerminals() {
  setTerminalFactory(() => ({
    term: {
      write: () => {},
      dispose: () => {},
      open: () => {},
      focus: () => {},
      loadAddon: () => {},
      onData: () => ({ dispose: () => {} }),
      onBinary: () => ({ dispose: () => {} }),
      cols: 80,
      rows: 24,
      options: {},
    } as unknown as PooledTerminal['term'],
    fit: { fit: () => {} },
    container: document.createElement('div'),
  }));
}

const paneTiles = (c: HTMLElement) => [...c.querySelectorAll<HTMLElement>('section.g-pane-tile')];

domTest(
  'A30.1 Starting: "starting…", then "Teammate did not start" at since + 30 s by the UI clock (no core timer); a failed spawn at once',
  harness(async () => {
    const clock = manualClock(9_000_000);
    setClockForTests(clock);
    hostedWorld({
      members: [lead(), member(2, 'api-agent')],
      sessions: [paneSession(1, {}, 'lead'), paneSession(2)],
      team: {
        paneTiles: [
          paneTile(3, { kind: 'starting', since: 9_000_000 - 29_900, spawnFailed: false }),
          paneTile(4, { kind: 'starting', since: 9_000_000, spawnFailed: true }),
        ],
      },
    });
    openTeam('panes');
    const view = render(<Mounted />);
    await settle();
    const [starting, failed] = paneTiles(view.container);
    assertEqual(starting?.getAttribute('data-phase'), 'starting', 'still starting at 29.9 s');
    assert(starting?.textContent?.includes(translate('en', 'tile.starting')) ?? false, '"starting…"');
    assertEqual(failed?.getAttribute('data-phase'), 'didNotStart', 'a failed spawn: at once');
    assert(failed?.textContent?.includes(translate('en', 'team.didNotStart')) ?? false, '"Teammate did not start"');
    clock.advance(200); // past since + 30 s on the UI clock
    await settle(160); // the tile's one timer fires and re-reads the clock
    assertEqual(paneTiles(view.container)[0]?.getAttribute('data-phase'), 'didNotStart', 'switched at 30 s');
    fireEvent.click(paneTiles(view.container)[1]?.querySelector('button') as HTMLElement);
    await settle();
    assertEqual(paneTiles(view.container).length, 1, '[Close tile] closes the view of a dead pane');
  }),
);

domTest(
  'A30.1 Unknown panes: "Unknown pane started by {lead}", never numbered, focused or zoomed, never visible to notifications; a running one is a read-only terminal',
  harness(async (core) => {
    fakeTerminals();
    hostedWorld({
      members: [lead(), member(2, 'api-agent')],
      sessions: [paneSession(1, {}, 'lead'), paneSession(2)],
      team: {
        paneTiles: [
          paneTile(5, { kind: 'unknown', running: true, exitCode: null }, { title: 'dev-server', pty: PANE_PTY }),
          paneTile(6, { kind: 'unknown', running: false, exitCode: 2 }),
        ],
      },
    });
    openTeam('panes');
    const view = render(<Mounted />);
    await settle();
    const [running, exited] = paneTiles(view.container);
    assert(running?.textContent?.includes(translate('en', 'team.unknownPane', { lead: 'team-lead' })) ?? false, 'unknown pane line');
    assert(running?.querySelector('[inert]') !== null, 'its terminal is inert (read-only)');
    assertEqual((core.calls('pty_attach')[0]?.req as { ptyId: string } | undefined)?.ptyId, PANE_PTY, 'attached to the pane PTY');
    assertEqual(core.calls('pty_write').length, 0, 'nothing typed into it');
    assert(exited?.textContent?.includes(translate('en', 'tile.crashed', { code: 2 })) ?? false, 'exited: "Crashed · exit 2"');
    assertEqual(running?.querySelector('textarea, [aria-label="' + translate('en', 'tile.zoom') + '"]'), null, 'no composer, no zoom');
    assert(!(running?.textContent ?? '').includes('⌘'), 'never numbered');
    key('3', { meta: true });
    await settle();
    assertEqual(layout()?.focused ?? null, null, '⌘3 does not reach a pane tile (only two members)');
    act(() => (running?.querySelector('button') ?? running)?.dispatchEvent(new FocusEvent('focusin', { bubbles: true })));
    assertEqual(layout()?.focused ?? null, null, 'focus inside it never makes it the focused tile');
    assertDeepEqual(appStore.getState().ui.visibleUids, [uid(1), uid(2)], 'visible set: member sessions only');
    assertEqual(core.calls('interaction_answer').length + core.calls('turn_send').length, 0, 'nothing sent');
  }),
);

domTest(
  'A30.2 Panes publishes the sessions it renders (not the strip); the Board publishes the members whose cards show; both clear on unmount',
  harness(async () => {
    const mates = [2, 3, 4, 5, 6].map((n) => member(n, `mate-${n}`));
    hostedWorld({
      members: [lead(), ...mates],
      sessions: [paneSession(1, {}, 'lead'), ...[2, 3, 4, 5].map((n) => paneSession(n)), paneSession(6, { pending: [pending('r-6')] })],
    });
    openTeam('panes');
    const view = render(<Mounted />);
    await settle();
    assertDeepEqual(appStore.getState().ui.visibleUids, [uid(1), uid(2), uid(3), uid(4), uid(5)], 'lead + 4 tiles; mate-6 is in the strip');
    key('2', { meta: true, alt: true });
    await settle();
    assertDeepEqual(appStore.getState().ui.visibleUids, [uid(6)], 'Board: only the row whose card shows');
    view.unmount();
    await settle();
    assertEqual(appStore.getState().ui.visibleUids, null, 'cleared on unmount');
    const tabs = render(<Board team={appStore.getState().world.teams.get(TK) as Team} band="medium" tabs />);
    await settle();
    assertDeepEqual(appStore.getState().ui.visibleUids, [uid(6)], 'tabs: Members tab shows the card');
    fireEvent.click(tabs.getByRole('tab', { name: translate('en', 'team.board.tasks') }));
    await settle();
    assertDeepEqual(appStore.getState().ui.visibleUids, [], 'Tasks tab: no card is visible');
  }),
);

domTest(
  'A30.3 The divider and the tile order are saved in the team layout and come back after a remount',
  harness(async () => {
    const mates = [2, 3, 4, 5, 6].map((n) => member(n, `mate-${n}`));
    hostedWorld({ members: [lead(), ...mates], sessions: [paneSession(1, {}, 'lead'), ...[2, 3, 4, 5, 6].map((n) => paneSession(n))] });
    openTeam('panes');
    const view = render(<Mounted />);
    await settle();
    const divider = view.getByRole('separator', { name: translate('en', 'team.divider') });
    fireEvent.keyDown(divider, { key: 'ArrowRight', code: 'ArrowRight', shiftKey: true });
    await settle();
    assertEqual(layout()?.leadRatio, 0.5, '⇧→ moves the divider 10 % and saves it');
    fireEvent.click(view.getByRole('toolbar', { name: translate('en', 'tile.strip') }).querySelector('button') as HTMLElement);
    await settle();
    const order = layout()?.tileOrder ?? [];
    assert(order.slice(0, 4).includes(mkey(6)), `mate-6 swapped into view and saved: ${order.join(',')}`);
    view.unmount();
    const again = render(<Mounted />);
    await settle();
    assertEqual(again.getByRole('separator', { name: translate('en', 'team.divider') }).getAttribute('aria-valuenow'), '50', 'the divider comes back');
    assert(tiles(again.container).some((t) => t.getAttribute('data-uid') === uid(6)), 'mate-6 is still on screen');
  }),
);

domTest(
  'A44.1: a team tile — a stacked request that replaces the answered one under the same tile starts a new 700 ms guard',
  harness(async (core) => {
    const clock = manualClock(8_000_000);
    setClockForTests(clock);
    const prompt = (requestId: string): PendingInteraction =>
      pending(requestId, {
        kind: 'screenPrompt',
        promptRev: `rev-${requestId}`,
        options: [
          { key: '1', label: 'Yes', role: 'allowOnce' },
          { key: '2', label: 'No', role: 'deny' },
        ],
      });
    const api = paneSession(2, { pending: [prompt('sp_a'), prompt('sp_b')] });
    hostedWorld({ sessions: [paneSession(1, {}, 'lead'), api, paneSession(3)] });
    openTeam('panes');
    render(<Mounted />);
    await settle();
    key('2', { meta: true });
    await settle();
    assertEqual(layout()?.focused, mkey(2), 'api-agent\'s tile is focused');
    const tick = (ms: number) =>
      act(() => {
        clock.advance(ms);
        refreshGuards();
      });
    const ids = () => core.calls('screen_prompt_key').map((c) => (c.req as ScreenPromptKeyReq).requestId);
    tick(GUARD_MS + 10);
    key('Enter', { meta: true });
    await settle();
    assertDeepEqual(ids(), ['sp_a'], '⌘⏎ answers the tile\'s top card');
    setWorld({
      teams: [team()],
      members: [lead(), member(2, 'api-agent'), member(3, 'db-agent', { colour: 'green' })],
      sessions: [paneSession(1, {}, 'lead'), { ...api, pending: [prompt('sp_b')] }, paneSession(3)],
    });
    tick(150);
    key('Enter', { meta: true });
    await settle();
    assertDeepEqual(ids(), ['sp_a'], 'sp_b took the top 150 ms ago: a second ⌘⏎ answers nothing');
    tick(GUARD_MS);
    key('Enter', { meta: true });
    await settle();
    assertDeepEqual(ids(), ['sp_a', 'sp_b'], 'after its own 700 ms, sp_b answers');
  }),
);
