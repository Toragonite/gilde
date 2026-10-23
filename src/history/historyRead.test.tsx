/**
 * History Read of a session World does not hold (contract A29.2(c), T18b):
 * the Session screen's read-only Not-running view from `history_row`.
 */

import { act, cleanup, fireEvent, render } from '@testing-library/react';

import { resetHotkeysForTests } from '../hooks/useHotkeys';
import { translate } from '../i18n';
import type { HistoryRow } from '../lib/gen/HistoryRow';
import type { ProjectId } from '../lib/gen/ProjectId';
import { SessionScreen } from '../session';
import { appStore } from '../store/useAppStore';
import { initialUiState } from '../store/uiSlice';
import { emptyWorld } from '../store/worldSlice';
import { flush, withFakeCore } from '../test/backend';
import type { FakeCore, Handler } from '../test/backend';
import { project, session, snapshot, uid } from '../test/fixtures';
import { assert, assertDeepEqual, assertEqual, domTest, test } from '../test/harness';
import { screenTitle } from '../shell/screenTitle';

const en = (key: Parameters<typeof translate>[1], params?: Parameters<typeof translate>[2]) => translate('en', key, params);
let rev = 2600;
const STARTED = new Date(2026, 9, 20, 9, 12).getTime();

function row(extra: Partial<HistoryRow> = {}): HistoryRow {
  return {
    uid: uid(40),
    vendor: 'claude',
    projectId: 'p1' as ProjectId,
    accountId: null,
    origin: 'vsCode',
    startedAt: STARTED,
    endedAt: STARTED + 88 * 60_000,
    running: false,
    titleOnly: false,
    label: 'title',
    title: 'Fix refresh-token race',
    continueOptions: ['resume', 'fork'],
    continuedIn: null,
    ...extra,
  };
}

async function settle(ms = 30) {
  await act(async () => {
    await flush(10);
    await new Promise((r) => setTimeout(r, ms));
  });
}

function harness(run: (core: FakeCore) => Promise<void>, extra: Record<string, Handler> = {}) {
  return async () => {
    resetHotkeysForTests();
    appStore.setState({ world: emptyWorld(), ui: { ...initialUiState(), locale: 'en' } });
    act(() => appStore.getState().applyStoreSnapshot(snapshot(++rev, { projects: [project('p1', { name: 'checkout-service' })] })));
    try {
      await withFakeCore(
        {
          history_row: () => row(),
          convo_subscribe: () => ({ subId: 'v_00000000000000e1', source: 'transcript', note: null }),
          convo_unsubscribe: () => ({}),
          session_continue: (req) => ({ uid: (req as { uid: string }).uid }),
          session_mark_seen: () => ({}),
          app_view_state: () => ({}),
          ...extra,
        },
        run,
      );
    } finally {
      cleanup();
      resetHotkeysForTests();
      appStore.setState({ world: emptyWorld(), ui: initialUiState() });
    }
  };
}

domTest(
  'T18b A29.2: a uid World does not hold reads from history_row — header from the row, Conversation only, the Continue bar',
  harness(async (core) => {
    const view = render(<SessionScreen uid={uid(40)} sub="changes" />);
    await settle();
    assertDeepEqual(core.calls('history_row')[0]?.req, { uid: uid(40) }, 'history_row');
    assert(view.getByRole('heading', { level: 1, name: 'Fix refresh-token race' }) !== null, 'the row title');
    const text = view.container.textContent ?? '';
    assert(text.includes('checkout-service'), 'project');
    assert(text.includes('claude') && text.includes(en('history.ranIn', { owner: en('origin.vscode') })), 'vendor and where it ran');
    assert(text.includes('09:12–10:40'), 'started–ended');
    assertDeepEqual(core.calls('convo_subscribe')[0]?.req && (core.calls('convo_subscribe')[0]?.req as { uid: string }).uid, uid(40), 'Conversation through convo_subscribe');
    assertEqual(view.container.querySelector('textarea'), null, 'no composer');
    assertEqual(view.container.querySelector('[role="tablist"]'), null, 'no Changes, no Terminal/Log');
    assert(!text.includes(en('session.tab.changes')), 'no Changes tab');
    assert(text.includes(en('ended.banner', { time: '10:40' })), 'the ended line');
    fireEvent.keyDown(document.body, { code: 'Enter', key: 'Enter', metaKey: true });
    await settle();
    const req = core.calls('session_continue')[0]?.req as { uid: string; how: unknown };
    assertEqual(req.uid, uid(40), 'continue this session');
    assertDeepEqual(req.how, { kind: 'resume' }, '⌘⏎ = History\'s ⏎ (resume)');
  }),
);

domTest(
  'T18b A29.2: a row that runs elsewhere offers a copy; one with no way to continue says so',
  harness(async (core) => {
    core.on('history_row', () => row({ running: true, continueOptions: ['fork'], endedAt: null }));
    const view = render(<SessionScreen uid={uid(40)} sub="conversation" />);
    await settle();
    fireEvent.click(view.getByText(en('action.continueCopy'), { exact: false }));
    await settle();
    assertDeepEqual((core.calls('session_continue')[0]?.req as { how: unknown }).how, { kind: 'fork' }, 'fork');
    cleanup();
    const subscribed = core.calls('convo_subscribe').length;
    core.on('history_row', () => row({ titleOnly: true, vendor: 'agy', continueOptions: [], title: null, label: 'vendorDate' }));
    const second = render(<SessionScreen uid={uid(40)} sub="conversation" />);
    await settle();
    assert(second.getByText(en('history.cantContinue')) !== null, "can't be continued");
    assert(second.getByText(en('history.noContent')) !== null, 'title only: no conversation');
    assertEqual(core.calls('convo_subscribe').length, subscribed, 'title only: nothing to subscribe to');
  }),
);

domTest(
  'T18b A29.2: NOT_FOUND and a core without history_row (notBuilt) both read "no longer available"',
  harness(async (core) => {
    core.on('history_row', () => {
      throw { code: 'NOT_FOUND', key: 'error.notFound', params: { entity: 'session' }, detail: null, retryable: false };
    });
    const first = render(<SessionScreen uid={uid(41)} sub="conversation" />);
    await settle();
    assert(first.getByText(en('session.gone')) !== null, 'NOT_FOUND');
    cleanup();
    core.on('history_row', () => {
      throw { code: 'UNSUPPORTED', key: 'error.unsupported', params: { cap: 'notBuilt' }, detail: null, retryable: false };
    });
    const second = render(<SessionScreen uid={uid(42)} sub="conversation" />);
    await settle();
    assert(second.getByText(en('session.gone')) !== null, 'notBuilt');
  }),
);

domTest(
  'T18b A29.2: when the uid appears in the store, the store entity wins',
  harness(async () => {
    const view = render(<SessionScreen uid={uid(40)} sub="conversation" />);
    await settle();
    assert(view.container.querySelector('[data-history-read]') !== null, 'the read-only view');
    act(() =>
      appStore.getState().applyStoreSnapshot(
        snapshot(++rev, { projects: [project('p1')], sessions: [session(40, { title: 'Live again', projectId: 'p1' as ProjectId })] }),
      ),
    );
    await settle();
    assertEqual(view.container.querySelector('[data-history-read]'), null, 'gone');
    assert(view.getByRole('heading', { level: 1, name: 'Live again' }) !== null, 'the store entity');
    assert(view.container.querySelector('[role="tablist"]') !== null, 'the full Session screen');
  }),
);

test('T31: the top bar names a History read from its row; the store entity wins; another uid stays unknown', () => {
  const t = (k: Parameters<typeof translate>[1], p?: Parameters<typeof translate>[2]) => translate('en', k, p);
  const view = { screen: 'session' as const, uid: uid(40), sub: 'conversation' as const };
  const read = { uid: uid(40), title: 'Fix refresh-token race' };
  assertEqual(screenTitle(t, view, emptyWorld(), 'en', read), en('shell.screen.session', { name: 'Fix refresh-token race' }), 'from the row');
  assertEqual(screenTitle(t, view, emptyWorld(), 'en', { uid: uid(41), title: 'Other' }), en('shell.screen.session', { name: en('common.unknown') }), 'another uid');
  assertEqual(screenTitle(t, view, emptyWorld(), 'en'), en('shell.screen.session', { name: en('common.unknown') }), 'none');
  const world = { ...emptyWorld(), sessions: new Map([[uid(40), session(40, { title: 'Live again' })]]) };
  assertEqual(screenTitle(t, view, world, 'en', read), en('shell.screen.session', { name: 'Live again' }), 'the store entity wins');
});

domTest(
  'T31: the read view publishes its title while mounted and clears it on unmount',
  harness(async (core) => {
    core.on('history_row', () => row({ title: null, label: 'vendorDate', vendor: 'codex' }));
    const view = render(<SessionScreen uid={uid(40)} sub="conversation" />);
    await settle();
    const title = appStore.getState().ui.readTitle;
    assertEqual(title?.uid, uid(40), 'this uid');
    assert(title !== null && title.title.startsWith('codex'), `the vendor + date label: ${title?.title}`);
    view.unmount();
    assertEqual(appStore.getState().ui.readTitle, null, 'cleared');
  }),
);
