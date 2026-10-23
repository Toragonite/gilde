/**
 * Accounts & health and the Clean-up sheet (ux-ia §3.15, §5.8, §5.9;
 * contract §4.2.7, A10.4/A19.5 spend never 0, A24.1/A24.8 accounts from the
 * store, C7 Discovered accounts).
 */

import { act, cleanup, fireEvent, render, within } from '@testing-library/react';

import { resetHotkeysForTests } from '../hooks/useHotkeys';
import { translate } from '../i18n';
import { formatTokens } from '../lib/format';
import type { Account } from '../lib/gen/Account';
import type { ProcessRecord } from '../lib/gen/ProcessRecord';
import type { Session } from '../lib/gen/Session';
import type { TeamKey } from '../lib/gen/TeamKey';
import { appStore } from '../store/useAppStore';
import { initialUiState } from '../store/uiSlice';
import { emptyWorld } from '../store/worldSlice';
import { flush, withFakeCore } from '../test/backend';
import type { FakeCore, Handler } from '../test/backend';
import { session, snapshot, uid } from '../test/fixtures';
import { assert, assertDeepEqual, assertEqual, domTest, test } from '../test/harness';
import { AccountsScreen } from './AccountsScreen';
import { healthRows, localDay, spendRows, tokensToday, validPct } from './model';
import { CleanUpSheet, resetCleanUpRequestForTests } from './StopSheet';

const en = (key: Parameters<typeof translate>[1], params?: Parameters<typeof translate>[2]) => translate('en', key, params);
let rev = 1200;
const TODAY = localDay(Date.now());

function account(id: string, extra: Partial<Account> = {}): Account {
  return {
    id: id as never,
    vendor: 'claude',
    configDir: `/Users/u/.${id}` as never,
    label: id,
    isDefault: false,
    source: 'user',
    usableForNew: true,
    signIn: 'signedIn',
    quota: null,
    liveSessions: 0,
    ...extra,
  };
}

function leftover(n: number, extra: Partial<ProcessRecord> = {}): ProcessRecord {
  return {
    key: `x_${100 + n}_${1000 + n}` as never,
    pid: 100 + n,
    startSec: 1000 + n,
    vendor: 'claude',
    role: 'teammate',
    sessionUid: null,
    gildeOwned: false,
    rssBytes: (n + 1) * 100 * 1024 * 1024,
    leftover: { reason: 'teammateOfGoneLead', since: 1, heldScans: 2 },
    ...extra,
  };
}

function world(extra: Parameters<typeof snapshot>[1]) {
  act(() => appStore.getState().applyStoreSnapshot(snapshot(++rev, extra)));
}

async function settle(turns = 10) {
  await act(async () => {
    await flush(turns);
    await new Promise((r) => setTimeout(r, 20));
  });
}

function harness(run: (core: FakeCore) => Promise<void>, extra: Record<string, Handler> = {}) {
  return async () => {
    resetHotkeysForTests();
    resetCleanUpRequestForTests();
    appStore.setState({ world: emptyWorld(), ui: { ...initialUiState(), locale: 'en' }, toasts: [] });
    try {
      await withFakeCore(
        {
          quota_refresh: (req) => {
            const a = appStore.getState().world.accounts.get((req as { accountId: never }).accountId);
            return { quota: a?.quota ?? { windows: [], fetchedAt: 1, stale: false, unavailable: 'notReported' } };
          },
          account_set_default: () => ({}),
          account_confirm: (req) => ({ account: account((req as { accountId: string }).accountId) }),
          health_stop_preview: (req) => ({
            targets: (req as { processKeys: string[] }).processKeys.map((k) => ({ processKey: k, what: `db-agent ${k}`, why: 'teammateOfGoneLead', memoryBytes: 1024 * 1024, how: 'termAfterIdentity' })),
            refused: [],
            confirmToken: 'tok-1',
          }),
          health_stop: () => ({ stopped: 2, survivors: 0, undoToken: 'undo-1' }),
          undo_cancel: () => ({ cancelled: true }),
          app_view_state: () => ({}),
          ...extra,
        },
        run,
      );
    } finally {
      cleanup();
      resetHotkeysForTests();
      appStore.setState({ world: emptyWorld(), ui: initialUiState(), toasts: [] });
    }
  };
}

// ---------------------------------------------------------------------------
// Pure rules
// ---------------------------------------------------------------------------

test('T18 A10.4 spend: absent or another day is unknown (null), never 0', () => {
  assertEqual(tokensToday(null, TODAY), null, 'absent');
  assertEqual(tokensToday({ day: '2000-01-01', inputTokens: 5, outputTokens: 5, cachedTokens: null, costMicros: null }, TODAY), null, 'yesterday’s total is not today’s');
  assertEqual(tokensToday({ day: TODAY, inputTokens: null, outputTokens: null, cachedTokens: 9, costMicros: null }, TODAY), null, 'no input/output');
  assertEqual(tokensToday({ day: TODAY, inputTokens: 1000, outputTokens: 200, cachedTokens: null, costMicros: null }, TODAY), 1200, 'input + output');
  assertEqual(validPct(140), null, 'out of range is absent, never clamped');
  assertEqual(validPct(-1), null, 'negative');
  assertEqual(validPct(38), 38, 'in range');
});

test('T18 spend and health rows: a team is one row; a partial sum is a lower bound; unknown memory stays out', () => {
  const team = (n: number, role: 'lead' | 'member', usage: Session['usageToday'], memoryBytes: number | null): Session =>
    session(n, { team: { teamKey: 'tk' as TeamKey, memberKey: `m${n}` as never, role, colour: null }, usageToday: usage, memoryBytes });
  const sessions = new Map<Session['uid'], Session>([
    [uid(1), team(1, 'lead', { day: TODAY, inputTokens: 100, outputTokens: 0, cachedTokens: null, costMicros: null }, 1000)],
    [uid(2), team(2, 'member', null, null)],
    [uid(3), session(3, { usageToday: null, memoryBytes: null })],
  ]);
  const w = { sessions, teams: new Map() };
  const spend = spendRows(w, TODAY);
  assertEqual(spend.length, 2, 'team + solo');
  const teamRow = spend.find((r) => r.teamKey === 'tk');
  assertEqual(teamRow?.tokens, 100, 'known part');
  assertEqual(teamRow?.partial, true, 'a lower bound');
  assertEqual(teamRow?.members, 2, 'members');
  assertEqual(spend.find((r) => r.uid === uid(3))?.tokens, null, 'unknown stays unknown');
  const health = healthRows(w);
  assertEqual(health.find((r) => r.teamKey === 'tk')?.memory, 1000, 'unknown member memory is not added as 0');
  assertEqual(health.find((r) => r.uid === uid(3))?.memory, null, 'unknown memory');
  assertEqual(health.find((r) => r.teamKey === 'tk')?.processes, 2, 'process count');
});

// ---------------------------------------------------------------------------
// The screen
// ---------------------------------------------------------------------------

domTest(
  'T18 A10.4 Spent today: "unknown" (with the owner) and never 0; a reported total shows as tokens',
  harness(async () => {
    world({
      sessions: [
        session(1, { usageToday: null }),
        session(2, { usageToday: null, control: { kind: 'watching', origin: 'vsCode', owner: null } }),
        session(3, { usageToday: { day: TODAY, inputTokens: 1000, outputTokens: 200, cachedTokens: null, costMicros: null } }),
      ],
      accounts: [account('work', { isDefault: true })],
    });
    const view = render(<AccountsScreen section="accounts" />);
    await settle();
    const list = view.getByRole('list', { name: en('accounts.heading.spend') });
    const text = list.textContent ?? '';
    assert(text.includes(en('accounts.spend.tokens', { amount: formatTokens(1200, 'en') as string })), 'the reported total');
    assert(text.includes(en('accounts.spend.unknownIn', { owner: en('origin.vscode') })), 'unknown, runs in VS Code');
    assert(text.includes(en('common.unknown')), 'unknown');
    assert(!/(^|\D)0 tokens/.test(text), `never "0 tokens": ${text}`);
  }),
);

domTest(
  'T18 accounts: a Discovered sibling is unusable until confirmed; the radio sets the default; quotas refresh on open',
  harness(async (core) => {
    world({
      accounts: [
        account('default', { label: 'Default', isDefault: true, source: 'ambient', configDir: '/Users/u/.claude' as never }),
        account('work'),
        account('found', { source: 'discovered', usableForNew: false, label: 'claude-found' }),
      ],
    });
    const view = render(<AccountsScreen section="accounts" />);
    await settle();
    assertEqual(core.calls('quota_refresh').length, 3, 'one refresh per account on open');
    const group = view.getByRole('radiogroup', { name: 'claude' });
    const radios = within(group).getAllByRole('radio') as HTMLButtonElement[];
    assertEqual(radios.length, 3, 'three accounts');
    const found = radios.find((r) => r.getAttribute('aria-label')?.includes('claude-found')) as HTMLButtonElement;
    assert(found.disabled, 'a Discovered account cannot be the default');
    assert(view.getByText(en('accounts.discovered')) !== null, 'the confirm sentence');
    fireEvent.click(view.getByText(en('accounts.confirm')));
    await settle();
    assertDeepEqual(core.calls('account_confirm')[0]?.req, { accountId: 'found' }, 'confirm');
    const work = radios.find((r) => r.getAttribute('aria-label')?.includes('work')) as HTMLButtonElement;
    assertEqual(work.getAttribute('aria-checked'), 'false', 'not the default yet');
    fireEvent.click(work);
    await settle();
    assertDeepEqual(core.calls('account_set_default')[0]?.req, { accountId: 'work' }, 'set default');
    assert(view.getByText(en('account.default')) !== null, 'the core\'s "Default" label reads in the UI language');
    assert(view.getByText('~/.claude') !== null, 'the home folder reads ~');
  }),
);

domTest(
  'T18 quota: bars per window; an unknown share reads "unknown"; a vendor without numbers says so; signed out offers Sign in',
  harness(async () => {
    world({
      accounts: [
        account('work', {
          isDefault: true,
          quota: {
            windows: [
              { kind: 'fiveHour', label: null, usedPct: 38, resetsAt: null },
              { kind: 'week', label: null, usedPct: null, resetsAt: null },
            ],
            fetchedAt: Date.now() - 40_000,
            stale: false,
            unavailable: null,
          },
        }),
        account('cur', { vendor: 'cursor', configDir: null, label: 'Default', isDefault: true, source: 'ambient', quota: { windows: [], fetchedAt: 1, stale: false, unavailable: 'notReported' } }),
        account('out', { signIn: 'signedOut' }),
      ],
    });
    const view = render(<AccountsScreen section="accounts" />);
    await settle();
    const meter = view.getByRole('meter', { name: en('accounts.windowMeter', { window: en('accounts.window.fiveHour'), pct: 38 }) });
    assertEqual(meter.getAttribute('aria-valuenow'), '38', '38%');
    assert(view.getAllByText(en('common.unknown')).length >= 1, 'week: unknown');
    assert(view.getByText(en('accounts.noLimits')) !== null, 'cursor: limits not available');
    assert(view.getByText(en('accounts.defaultLogin')) !== null, 'cursor default login');
    assert(view.getByText(en('action.signIn')) !== null, 'Sign in…');
    assert(view.getAllByText(en('state.error.signIn')).length >= 1, 'Sign-in needed');
  }),
);

domTest(
  'T18 Clean-up: focus on Cancel, ⌘⏎ never confirms; Stop schedules behind the undo window and the toast undoes it',
  harness(async (core) => {
    world({ leftovers: [leftover(1), leftover(2)] });
    act(() => appStore.getState().openSheet('cleanUp'));
    const view = render(<CleanUpSheet />);
    await settle();
    assertDeepEqual(core.calls('health_stop_preview')[0]?.req, { processKeys: ['x_102_1002', 'x_101_1001'] }, 'every leftover, largest first');
    assertEqual(document.activeElement?.textContent, en('common.cancel'), 'focus on Cancel');
    assert(view.getByText(en('sheet.cleanup', { n: 2, memory: '2 MB' }).split(' and ')[0] as string, { exact: false }) !== null, 'the sentence');
    assert(view.getAllByText(en('leftover.teammateOfGoneLead')).length === 2, 'why');
    assert(view.getAllByText(en('stopHow.termAfterIdentity')).length === 2, 'how');
    fireEvent.keyDown(document.activeElement as Element, { code: 'Enter', key: 'Enter', metaKey: true });
    await settle();
    assertEqual(core.calls('health_stop').length, 0, '⌘⏎ does not confirm a destructive sheet');
    fireEvent.click(view.getByText(en('sheet.cleanup.stopN', { n: 2 })));
    await settle();
    assertDeepEqual(core.calls('health_stop')[0]?.req, { confirmToken: 'tok-1' }, 'the previewed token');
    assertEqual(appStore.getState().ui.sheet, null, 'closed');
    const toast = appStore.getState().toasts.slice(-1)[0];
    assertEqual(toast?.key, 'toast.cleanup.scheduled', 'the outcome toast');
    assertEqual(toast?.undoToken, 'undo-1', 'with Undo');
    await act(async () => {
      await appStore.getState().undoToast(toast?.id as number);
    });
    assertDeepEqual(core.calls('undo_cancel')[0]?.req, { undoToken: 'undo-1' }, 'undo_cancel');
  }),
);

domTest(
  'T18 Stop… on one leftover covers only it; ▸ shows the technical facts on demand',
  harness(
    async (core) => {
      world({ leftovers: [leftover(1), leftover(2)] });
      const view = render(
        <>
          <AccountsScreen section="health" />
          <CleanUpSheet />
        </>,
      );
      await settle();
      assert(!(view.container.textContent ?? '').includes('pid 101'), 'pid hidden until ▸');
      const rows = view.getAllByText(en('accounts.stopOne'));
      fireEvent.click(rows[1] as HTMLElement);
      await settle();
      assertDeepEqual(core.calls('health_stop_preview')[0]?.req, { processKeys: ['x_101_1001'] }, 'only that process');
      act(() => appStore.getState().closeSheet());
      await settle();
      fireEvent.click(view.getAllByRole('button', { name: en('accounts.technical') })[1] as HTMLElement);
      await settle();
      assertDeepEqual(core.calls('health_process_detail')[0]?.req, { processKey: 'x_101_1001' }, 'detail on demand');
      assert((view.container.textContent ?? '').includes('pid 101'), 'pid shown');
      assert((view.container.textContent ?? '').includes('--settings ***'), 'the redacted command');
    },
    { health_process_detail: () => ({ commandRedacted: ['claude', '--settings', '***'], parentChain: ['launchd'], startedAt: 1_700_000_000_000 }) },
  ),
);

domTest(
  'T18 Clean-up: CONFLICT re-reads the preview and nothing is stopped on stale facts; refused targets are named',
  harness(async (core) => {
    let first = true;
    core.on('health_stop', () => {
      if (first) {
        first = false;
        throw { code: 'CONFLICT', key: 'error.conflict', params: { changed: 'targets' }, detail: null, retryable: false };
      }
      return { stopped: 1, survivors: 0, undoToken: null };
    });
    core.on('health_stop_preview', () => ({
      targets: [{ processKey: 'x_101_1001', what: 'db-agent', why: 'quitSurvivor', memoryBytes: 1024, how: 'claudeStop' }],
      refused: [{ processKey: 'x_102_1002', reason: 'identityUnverified' }],
      confirmToken: `tok-${core.calls('health_stop_preview').length}`,
    }));
    world({ leftovers: [leftover(1), leftover(2)] });
    act(() => appStore.getState().openSheet('cleanUp'));
    const view = render(<CleanUpSheet />);
    await settle();
    assert(view.getByText(en('sheet.cleanup.refused', { n: 1 })) !== null, 'refused, never stopped');
    fireEvent.click(view.getByText(en('sheet.cleanup.stopN', { n: 1 })));
    await settle();
    assertEqual(core.calls('health_stop_preview').length, 2, 'previewed again');
    assert(view.getByText(en('error.conflict')) !== null, 'says it changed');
    assertEqual(appStore.getState().ui.sheet, 'cleanUp', 'still open');
    fireEvent.click(view.getByText(en('sheet.cleanup.stopN', { n: 1 })));
    await settle();
    assertDeepEqual(core.calls('health_stop')[1]?.req, { confirmToken: 'tok-2' }, 'the fresh token');
  }),
);
