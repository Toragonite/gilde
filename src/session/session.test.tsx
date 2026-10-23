/**
 * Session screen tests (ux-ia §3.4–§3.6, §4.3, §4.6; contract §15.2 T15,
 * §15.6 F-04, A10.4/A19.5, A21.3): the card takes focus only on request, a
 * new card never steals it from the composer, the ownership and continue bars
 * replace the composer, Start in Terminal mode, the sub-views, Details (spend
 * unknown, never 0), New reply seen after 2 s, and app_view_state on view
 * changes.
 */

import { act, cleanup, fireEvent, render, within } from '@testing-library/react';

import { resetAnswersForTests } from '../approval/answer';
import { resetAnswerGuardForTests } from '../hooks/useAnswerGuard';
import { resetHotkeysForTests } from '../hooks/useHotkeys';
import { translate } from '../i18n';
import type { Session } from '../lib/gen/Session';
import type { ViewState } from '../lib/gen/ViewState';
import { getAnnouncements, resetAnnouncerForTests } from '../lib/notify';
import { appStore } from '../store/useAppStore';
import { initialUiState } from '../store/uiSlice';
import { startViewStateReporter } from '../store/viewState';
import { emptyWorld } from '../store/worldSlice';
import { flush, withFakeCore } from '../test/backend';
import type { FakeCore, Handler } from '../test/backend';
import { pending, session, snapshot, uid } from '../test/fixtures';
import { assert, assertDeepEqual, assertEqual, domTest } from '../test/harness';
import { SessionScreen } from './SessionScreen';
import { StopSessionSheet } from './StopSessionSheet';

let rev = 700;

function setSessions(sessions: Session[]) {
  act(() => {
    appStore.getState().applyStoreSnapshot(snapshot(++rev, { sessions }));
  });
}

const base: Record<string, Handler> = {
  convo_subscribe: () => ({ subId: 'v_00000000000000d1', source: 'stream', note: null }),
  convo_unsubscribe: () => ({}),
  changes_get: () => ({ location: 'l', base: null, files: [], commits: [], sharedFolder: null, actions: { commit: { kind: 'hidden' }, merge: { kind: 'hidden' }, discard: { kind: 'hidden' } } }),
  session_mark_seen: () => ({}),
  owner_focus: () => ({ precision: 'app' }),
  session_continue: (req) => ({ uid: (req as { uid: string }).uid }),
  session_retry_as_terminal: (req) => ({ uid: (req as { uid: string }).uid }),
  session_stop_preview: () => ({ how: 'claudeStop', ownerText: { key: 'sheet.stopSession', params: {} }, confirmToken: 'tok-s' }),
  session_stop: () => ({ stopped: true, survivors: 0 }),
  session_technical: () => ({ pid: 4242, startedAt: 1, commandRedacted: ['claude', '--resume', '***'], parentChain: ['launchd'] }),
  app_view_state: () => ({}),
};

function harness(run: (core: FakeCore) => Promise<void>, extra: Record<string, Handler> = {}) {
  return async () => {
    resetHotkeysForTests();
    resetAnswerGuardForTests();
    resetAnswersForTests();
    resetAnnouncerForTests();
    appStore.setState({ world: emptyWorld(), ui: { ...initialUiState(), locale: 'en' } });
    try {
      await withFakeCore({ ...base, ...extra }, run);
    } finally {
      cleanup();
      resetHotkeysForTests();
      appStore.setState({ world: emptyWorld(), ui: initialUiState() });
    }
  };
}

async function settle(turns = 10) {
  await act(async () => {
    await flush(turns);
    await new Promise((r) => setTimeout(r, 30));
  });
}

domTest(
  'Opening with a focus request focuses the card (Deny on default_to_no); a new card later never steals the composer',
  harness(async () => {
    setSessions([session(1, { pending: [pending('r1', { defaultToNo: true })] })]);
    act(() => appStore.getState().openSession(uid(1), { focusCard: true }));
    const view = render(<SessionScreen uid={uid(1)} sub="conversation" />);
    await settle();
    assertEqual(document.activeElement?.getAttribute('data-role'), 'deny', 'focus on Deny');

    const composer = view.getByLabelText(translate('en', 'composer.placeholder', { agent: 'claude' })) as HTMLTextAreaElement;
    composer.focus();
    setSessions([session(1, { pending: [pending('r1', { defaultToNo: true }), pending('r2')] })]);
    await settle();
    assertEqual(document.activeElement, composer, 'a new card does not take focus');
  }),
);

domTest(
  'Landmarks: the screen adds no main or header of its own; it is a section named by its heading (ux-ia §10)',
  harness(async () => {
    setSessions([session(1)]);
    const view = render(<SessionScreen uid={uid(1)} sub="conversation" />);
    await settle();
    assertEqual(view.container.querySelectorAll('main, header, [role="main"], [role="banner"]').length, 0, 'the shell owns the one main');
    const section = view.container.querySelector('section.g-session') as HTMLElement;
    const heading = section.querySelector('h1') as HTMLElement;
    assert(heading.id !== '' && section.getAttribute('aria-labelledby') === heading.id, 'named by its h1');
    cleanup();
    const gone = render(<SessionScreen uid={uid(99)} sub="conversation" />);
    await settle();
    assertEqual(gone.container.querySelectorAll('main, header').length, 0, 'not for a session that is gone either');
    const goneSection = gone.container.querySelector('section.g-session') as HTMLElement;
    assertEqual(document.getElementById(goneSection.getAttribute('aria-labelledby') ?? '')?.textContent, translate('en', 'session.header'), 'a heading names it');
  }),
);

domTest(
  'A new card in the open session is announced once, assertively',
  harness(async () => {
    setSessions([session(1)]);
    render(<SessionScreen uid={uid(1)} sub="conversation" />);
    setSessions([session(1, { pending: [pending('r7', { title: 'Run the migration' })] })]);
    await settle();
    const said = getAnnouncements().assertive?.text ?? '';
    assert(said.includes('Run the migration'), `announced: ${said}`);
  }),
);

domTest(
  'Watching: the ownership bar replaces the composer; ⌘⏎ goes to the owner; "Why can\'t I answer here?" explains',
  harness(async (core) => {
    const s = session(2, {
      control: { kind: 'watching', origin: 'vsCode', owner: { appPid: 1, appName: 'VS Code', kind: 'vsCode', bundleId: 'com.microsoft.VSCode' } },
      caps: { ...session(2).caps, send: 'none', approve: 'goToOwner', ownerFocus: true, continueOptions: ['fork'], interrupt: false },
    });
    setSessions([s]);
    const view = render(<SessionScreen uid={uid(2)} sub="conversation" />);
    await settle();
    assertEqual(view.queryByLabelText(translate('en', 'composer.placeholder', { agent: 'claude' })), null, 'no composer');
    assert(view.getByText(translate('en', 'observed.banner', { owner: 'VS Code' })) !== null, 'the sentence');
    assert(view.getByText(translate('en', 'action.continueCopy')) !== null, 'Continue a copy');
    fireEvent.keyDown(document.body, { key: 'Enter', code: 'Enter', metaKey: true });
    await settle();
    assertEqual(core.calls('owner_focus').length, 1, '⌘⏎ = Go to VS Code');
    fireEvent.click(view.getByText(translate('en', 'observed.whyLink')));
    assert(view.getByText(translate('en', 'observed.why', { owner: 'VS Code' })) !== null, 'why popover');
  }),
);

domTest(
  'Not running: Continue in Gilde resumes the same session',
  harness(async (core) => {
    setSessions([
      session(3, {
        control: { kind: 'notRunning', lastOrigin: null, endedHow: 'ended' },
        activity: { ...session(3).activity, state: { kind: 'ended' } },
        caps: { ...session(3).caps, send: 'none', continueOptions: ['resume', 'fork'] },
        endedAt: 1_700_000_000_000,
      }),
    ]);
    const view = render(<SessionScreen uid={uid(3)} sub="conversation" />);
    await settle();
    fireEvent.click(view.getByText(translate('en', 'action.continue')));
    await settle();
    assertDeepEqual((core.calls('session_continue')[0]?.req as { how: unknown }).how, { kind: 'resume' }, 'resume');
  }),
);

domTest(
  'Start in Terminal mode: offered for a Conversation start that failed on this claude version',
  harness(async (core) => {
    setSessions([
      session(4, {
        activity: { ...session(4).activity, state: { kind: 'error', sub: { kind: 'didntStart', reason: 'compatFailed' } } },
        caps: { ...session(4).caps, continueOptions: ['retryAsTerminal'] },
      }),
    ]);
    const view = render(<SessionScreen uid={uid(4)} sub="conversation" />);
    await settle();
    assert(view.getByText(translate('en', 'session.retryAsTerminalHint')) !== null, 'the hint');
    fireEvent.click(view.getByText(translate('en', 'action.startAsTerminal')));
    await settle();
    assertEqual(core.calls('session_retry_as_terminal').length, 1, 'session_retry_as_terminal');
  }),
);

domTest(
  'Sub-views: ⌥⌘2 opens Changes; the third tab is Log for a Conversation session',
  harness(async () => {
    setSessions([session(5, { caps: { ...session(5).caps, terminalView: false, logView: true } })]);
    act(() => appStore.getState().openSession(uid(5)));
    const view = render(<SessionScreen uid={uid(5)} sub="conversation" />);
    await settle();
    assert(view.getByRole('tab', { name: translate('en', 'session.tab.log') }) !== null, 'Log tab');
    fireEvent.keyDown(document.body, { key: '2', code: 'Digit2', metaKey: true, altKey: true });
    const v = appStore.getState().ui.view;
    assertEqual(v.screen === 'session' ? v.sub : null, 'changes', '⌥⌘2 = Changes');
  }),
);

domTest(
  'Details: spend absent reads "unknown", never 0; L2 says "Connection: helper"; technical details on demand',
  harness(async (core) => {
    setSessions([
      session(6, {
        details: { ...session(6).details, connection: 'l2', usageToday: null, compatState: 'unverified' },
        usageToday: null,
      }),
    ]);
    const view = render(<SessionScreen uid={uid(6)} sub="conversation" />);
    await settle();
    fireEvent.click(view.getByLabelText(translate('en', 'session.details')));
    assert(view.getByText(translate('en', 'details.connectionHelper')) !== null, 'L2 line');
    assert(view.getByText(translate('en', 'details.unverifiedVersion')) !== null, 'unverified');
    const today = view.getByText(translate('en', 'details.today')).parentElement?.textContent ?? '';
    assert(today.includes(translate('en', 'common.unknown')), `spend unknown: ${today}`);
    assert(!/\b0\b/.test(today.replace(translate('en', 'details.today'), '')), 'never 0');
    assertEqual(core.calls('session_technical').length, 0, 'not fetched until asked');
    fireEvent.click(view.getByText(translate('en', 'details.technical'), { exact: false }));
    await settle();
    assert(view.getByText('claude --resume ***') !== null, 'redacted command line');
  }),
);

domTest(
  'A21.3: a lead\'s "different claude install" line renders through i18n in the header',
  harness(async () => {
    setSessions([session(7, { activity: { ...session(7).activity, evidence: { key: 'details.teammateBinaryDiffers', params: {} } } })]);
    const view = render(<SessionScreen uid={uid(7)} sub="conversation" />);
    await settle();
    assert(view.getAllByText(translate('en', 'details.teammateBinaryDiffers')).length > 0, 'the line');
  }),
);

domTest(
  'New reply becomes seen after 2 s on screen',
  harness(async (core) => {
    setSessions([session(8, { activity: { ...session(8).activity, state: { kind: 'newReply' } } })]);
    render(<SessionScreen uid={uid(8)} sub="conversation" />);
    await settle();
    assertEqual(core.calls('session_mark_seen').length, 0, 'not at once');
    await act(async () => {
      await new Promise((r) => setTimeout(r, 2100));
    });
    assertEqual(core.calls('session_mark_seen').length, 1, 'after the dwell');
  }),
);

domTest(
  'F-04: opening the session (and leaving it) sends app_view_state',
  harness(async (core) => {
    setSessions([session(9)]);
    const stop = startViewStateReporter(
      (listener) => appStore.subscribe((s) => listener(s.ui, s.world)),
      { ui: appStore.getState().ui, world: appStore.getState().world },
    );
    try {
      act(() => appStore.getState().openSession(uid(9)));
      act(() => appStore.getState().navigate({ screen: 'home' }));
      await settle();
      const states = core.calls('app_view_state').map((c) => c.req as ViewState);
      assert(states.some((v) => v.screen === 'session' && v.focusedUid === uid(9) && v.visibleUids.includes(uid(9))), 'session reported');
      assertEqual(states[states.length - 1]?.screen, 'home', 'and the change back');
    } finally {
      stop();
    }
  }),
);

domTest(
  'Stop session: ⇧⌘⌫ opens the shell\'s sheet (A23.5); Gilde\'s own session reads sheet.stopSession (A24.7); Cancel first, no ⌘⏎',
  harness(async (core) => {
    setSessions([session(10, { activity: { ...session(10).activity, state: { kind: 'working' } } })]);
    act(() => appStore.getState().openSession(uid(10)));
    const view = render(
      <>
        <SessionScreen uid={uid(10)} sub="conversation" />
        <StopSessionSheet />
      </>,
    );
    await settle();
    fireEvent.keyDown(document.body, { key: 'Backspace', code: 'Backspace', metaKey: true, shiftKey: true });
    await settle();
    assertEqual(appStore.getState().ui.sheet, 'stopSession', 'the sheet id');
    assert(view.getByText(translate('en', 'sheet.stopSession')) !== null, 'the core sends the dedicated sentence (A25.3), shown as sent');
    assertEqual(document.activeElement?.textContent, translate('en', 'common.cancel'), 'focus on Cancel');
    fireEvent.keyDown(document.activeElement ?? document.body, { key: 'Enter', code: 'Enter', metaKey: true });
    await settle();
    assertEqual(core.calls('session_stop').length, 0, '⌘⏎ never confirms');
    fireEvent.click(within(view.getByRole('dialog')).getByText(translate('en', 'action.stop')));
    await settle();
    assertDeepEqual(core.calls('session_stop').map((c) => c.req), [{ uid: uid(10), confirmToken: 'tok-s' }], 'stopped with the token');
    assertEqual(appStore.getState().ui.sheet, null, 'closed');
    assert(view.getByText(translate('en', 'session.stopped')) !== null, 'the outcome line');
  }),
);
