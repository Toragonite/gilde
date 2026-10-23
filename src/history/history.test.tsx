/**
 * History (ux-ia §3.14; contract §4.2.8, §15.2 T18 "History cold-start UI").
 */

import { act, cleanup, fireEvent, render } from '@testing-library/react';

import { resetHotkeysForTests } from '../hooks/useHotkeys';
import { translate } from '../i18n';
import type { HistoryQueryReq } from '../lib/gen/HistoryQueryReq';
import type { HistoryQueryRes } from '../lib/gen/HistoryQueryRes';
import type { HistoryRow } from '../lib/gen/HistoryRow';
import type { ProjectId } from '../lib/gen/ProjectId';
import { appStore } from '../store/useAppStore';
import { initialUiState } from '../store/uiSlice';
import { emptyWorld } from '../store/worldSlice';
import { flush, withFakeCore } from '../test/backend';
import type { FakeCore, Handler } from '../test/backend';
import { project, session, snapshot, uid } from '../test/fixtures';
import { assert, assertDeepEqual, assertEqual, domTest, test } from '../test/harness';
import { HistoryScreen } from './HistoryScreen';
import * as M from './model';
import { KEYMAP } from '../hooks/useHotkeys';
import { en as enTable, ko as koTable } from '../i18n';

const en = (key: Parameters<typeof translate>[1], params?: Parameters<typeof translate>[2]) => translate('en', key, params);
let rev = 2100;
const NOW = Date.now();

function hrow(n: number, extra: Partial<HistoryRow> = {}): HistoryRow {
  return {
    uid: uid(n),
    vendor: 'claude',
    projectId: 'p1' as ProjectId,
    accountId: null,
    origin: 'terminal',
    startedAt: NOW - n * 60_000,
    endedAt: NOW - n * 60_000 + 30_000,
    running: false,
    titleOnly: false,
    label: 'title',
    title: `Session ${n}`,
    continueOptions: ['resume', 'fork'],
    continuedIn: null,
    ...extra,
  };
}

function page(req: unknown, rows: HistoryRow[], coverage = { titlesLoaded: rows.length, total: rows.length }): HistoryQueryRes {
  return { queryId: (req as HistoryQueryReq).queryId, rows, nextCursor: null, coverage, superseded: false };
}

async function settle(ms = 20) {
  await act(async () => {
    await flush(10);
    await new Promise((r) => setTimeout(r, ms));
  });
}

function harness(run: (core: FakeCore) => Promise<void>, extra: Record<string, Handler> = {}) {
  return async () => {
    resetHotkeysForTests();
    appStore.setState({ world: emptyWorld(), ui: { ...initialUiState(), locale: 'en' } });
    act(() =>
      appStore.getState().applyStoreSnapshot(
        snapshot(++rev, {
          projects: [project('p1')],
          sessions: [session(3, { title: 'Session 3' })],
        }),
      ),
    );
    try {
      await withFakeCore(
        {
          history_query: (req) => page(req, [hrow(1), hrow(2, { running: true, continueOptions: ['fork'] }), hrow(3, { running: true, continueOptions: [] })]),
          history_preview: () => ({ firstPrompt: 'Two parallel requests both refresh', lastReply: 'They now share one refresh.', turns: 38, startedAt: 1, endedAt: 2, continuedIn: null }),
          session_continue: (req) => ({ uid: (req as { how: { kind: string } }).how.kind === 'fork' ? uid(99) : (req as { uid: string }).uid }),
          ide_reveal: () => ({}),
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

function option(n: number): HTMLElement {
  return document.getElementById(`hi-row-${uid(n)}`) as HTMLElement;
}

test('T18 history model: query ids, When bounds, day groups, the row action', () => {
  for (let i = 0; i < 50; i++) assert(/^[a-z0-9-]{1,32}$/.test(M.newQueryId()), 'QueryId pattern');
  const now = new Date(2026, 9, 23, 15, 0).getTime();
  assertEqual(M.fromOf('any', now), null, 'any time');
  assertEqual(M.fromOf('today', now), new Date(2026, 9, 23).getTime(), 'start of today');
  assertEqual(M.fromOf('week', now), new Date(2026, 9, 17).getTime(), 'seven days including today');
  assertEqual(M.groupOf(now - 3600_000, now), 'today', 'today');
  assertEqual(M.groupOf(new Date(2026, 9, 22, 9).getTime(), now), 'yesterday', 'yesterday');
  assertEqual(M.groupOf(new Date(2026, 9, 19).getTime(), now), 'week', 'this week');
  assertEqual(M.groupOf(new Date(2026, 8, 1).getTime(), now), 'older', 'older');
  assertEqual(M.queryText('가'.repeat(300)).length, 256, '256 characters');
  const inGilde = session(1);
  const watching = session(2, { control: { kind: 'watching', origin: 'vsCode', owner: null } });
  assertDeepEqual(M.rowAction({ running: true, continueOptions: ['resume'] }, inGilde), { kind: 'open' }, 'in Gilde: open');
  assertDeepEqual(M.rowAction({ running: false, continueOptions: ['resume', 'fork'] }, null), { kind: 'resume' }, 'ended: resume');
  assertDeepEqual(M.rowAction({ running: false, continueOptions: ['resume', 'fork'] }, watching), { kind: 'fork' }, 'alive elsewhere: a copy');
  assertDeepEqual(M.rowAction({ running: false, continueOptions: [] }, null), { kind: 'none' }, 'nothing offered');
  const t = (k: Parameters<typeof translate>[1], p?: Parameters<typeof translate>[2]) => translate('en', k, p);
  assertEqual(M.rowTitle(t, hrow(1, { title: null, label: 'vendorDate', vendor: 'cursor', startedAt: now }), 'en'), en('history.vendorDate', { vendor: 'cursor', date: 'Oct 23' }), 'cursor: vendor and date');
  assertEqual(M.rowTitle(t, hrow(1, { title: '   ' }), 'en'), en('history.untitled'), 'blank title');
});

domTest(
  'T18 History cold start: "Searching titles…" with progress, asked again until complete, selection kept',
  harness(async (core) => {
    let calls = 0;
    core.on('history_query', (req) => {
      calls++;
      return calls === 1
        ? page(req, [hrow(1), hrow(2)], { titlesLoaded: 10, total: 100 })
        : page(req, [hrow(1), hrow(2), hrow(4)], { titlesLoaded: 100, total: 100 });
    });
    const view = render(<HistoryScreen />);
    await settle(200);
    assert(view.getByText(en('history.searching', { loaded: 10, total: 100 })) !== null, 'the cold-start line');
    assert(document.querySelector('.hi-progress') !== null, 'a 1 px bar');
    fireEvent.click(option(2));
    await settle(M.COLD_START_POLL_MS + 200);
    assert(calls >= 2, 'asked again');
    assertEqual(view.queryByText(en('history.searching', { loaded: 10, total: 100 })), null, 'line gone when complete');
    assertEqual(option(2).getAttribute('aria-selected'), 'true', 'the selection stayed');
    assert(option(4) !== null, 'new rows arrived');
    assert(view.getByText(en('history.count', { shown: 3, all: 100 })) !== null, 'the count');
  }),
);

domTest(
  'T18 History rows: ⏎ resumes an ended session, continues a copy of one alive elsewhere, opens one in Gilde; ⌘↓ reads',
  harness(async (core) => {
    const view = render(<HistoryScreen />);
    await settle(200);
    option(1).focus();
    fireEvent.click(option(1));
    fireEvent.keyDown(option(1), { code: 'Enter', key: 'Enter' });
    await settle();
    const first = core.calls('session_continue')[0]?.req as { uid: string; how: unknown };
    assertEqual(first.uid, uid(1), 'row 1');
    assertDeepEqual(first.how, { kind: 'resume' }, 'resume');
    assertDeepEqual(appStore.getState().ui.view, { screen: 'session', uid: uid(1), sub: 'conversation' }, 'opened');

    option(2).focus();
    fireEvent.click(option(2));
    assert(view.getByText(en('action.continueCopy'), { exact: false }) !== null, 'the button reads Continue a copy');
    fireEvent.keyDown(option(2), { code: 'Enter', key: 'Enter' });
    await settle();
    assertDeepEqual((core.calls('session_continue')[1]?.req as { how: unknown }).how, { kind: 'fork' }, 'fork');
    assertEqual((appStore.getState().ui.view as { uid?: string }).uid, uid(99), 'the copy opens');

    option(3).focus();
    fireEvent.click(option(3));
    fireEvent.keyDown(option(3), { code: 'Enter', key: 'Enter' });
    await settle();
    assertEqual(core.calls('session_continue').length, 2, 'a session in Gilde is opened, not continued');
    assertEqual((appStore.getState().ui.view as { uid?: string }).uid, uid(3), 'opened');

    option(1).focus();
    fireEvent.click(option(1));
    fireEvent.keyDown(option(1), { code: 'ArrowDown', key: 'ArrowDown', metaKey: true });
    await settle();
    assertEqual((appStore.getState().ui.view as { uid?: string }).uid, uid(1), '⌘↓ reads it');
  }),
);

domTest(
  'T18 History preview: content on demand; a title-only row reads only its title and says it cannot continue',
  harness(
    async (core) => {
      const view = render(<HistoryScreen />);
      await settle(300);
      await settle(200);
      assert(view.getByText('Two parallel requests both refresh') !== null, 'first prompt (content, on demand)');
      assert(view.getByText(en('history.turns', { n: 38 }), { exact: false }) !== null, 'turns');
      fireEvent.click(option(5));
      await settle(300);
      await settle(200);
      assert(view.getByText(en('history.noContent')) !== null, 'title only');
      assert(view.getByText(en('history.cantContinue')) !== null, "can't be continued");
      assert(!core.calls('history_preview').some((c) => (c.req as { uid: string }).uid === uid(5)), 'no preview asked for a title-only row');
    },
    { history_query: (req) => page(req, [hrow(1), hrow(5, { titleOnly: true, vendor: 'agy', continueOptions: [] })]) },
  ),
);

domTest(
  'T18 History: a resume refused because it runs elsewhere offers a copy',
  harness(
    async (core) => {
      const view = render(<HistoryScreen />);
      await settle(300);
      fireEvent.click(view.getByText(en('action.continue'), { exact: false }));
      await settle();
      assert(view.getByText(en('error.sessionLiveElsewhere')) !== null, 'the reason');
      const copy = [...document.querySelectorAll('.hi-prev-error button')].find((b) => b.textContent === en('action.continueCopy')) as HTMLElement;
      fireEvent.click(copy);
      await settle();
      assertDeepEqual((core.calls('session_continue')[1]?.req as { how: unknown }).how, { kind: 'fork' }, 'a copy');
    },
    {
      history_query: (req) => page(req, [hrow(1)]),
      session_continue: (req) => {
        if ((req as { how: { kind: string } }).how.kind === 'resume') {
          throw { code: 'SESSION_LIVE_ELSEWHERE', key: 'error.sessionLiveElsewhere', params: {}, detail: null, retryable: false };
        }
        return { uid: uid(98) };
      },
    },
  ),
);

domTest(
  'T18 History: an older answer never replaces a newer one; filters reach the query',
  harness(async (core) => {
    let n = 0;
    core.on('history_query', (req) => {
      n++;
      if (n === 1) return new Promise((r) => setTimeout(() => r(page(req, [hrow(7, { title: 'stale' })])), 400));
      return page(req, [hrow(8, { title: 'fresh' })]);
    });
    const view = render(<HistoryScreen />);
    await settle(10);
    fireEvent.change(view.getByRole('searchbox', { name: en('history.search') }), { target: { value: 'fre' } });
    await settle(700);
    assert(view.queryAllByText('fresh').length > 0, 'the newer answer');
    assertEqual(view.queryAllByText('stale').length, 0, 'the older one is dropped');
    const req = core.calls('history_query').slice(-1)[0]?.req as HistoryQueryReq;
    assertEqual(req.text, 'fre', 'the text');
    fireEvent.change(document.getElementById('hi-f-agent') as HTMLSelectElement, { target: { value: 'codex' } });
    fireEvent.change(document.getElementById('hi-f-when') as HTMLSelectElement, { target: { value: 'today' } });
    await settle(300);
    const last = core.calls('history_query').slice(-1)[0]?.req as HistoryQueryReq;
    assertEqual(last.filters.vendor, 'codex', 'agent');
    assertEqual(last.filters.from, M.fromOf('today', Date.now()), 'today');
    assertEqual(last.filters.to, null, 'open end (never from > to)');
    assertEqual(last.limit, M.PAGE, 'page size');
  }),
);

test('T18b A29.5: ⌥⌘F is not registered in v1 (no KEYMAP row, no help strings)', () => {
  assert(!KEYMAP.some((e) => e.id === 'history.everything'), 'no history.everything row');
  assert(!KEYMAP.some((e) => e.combos.includes('mod+alt+f')), 'no ⌥⌘F anywhere');
  assert(!('keys.history.everything' in enTable) && !('keys.history.everything' in koTable), 'no help strings');
});
