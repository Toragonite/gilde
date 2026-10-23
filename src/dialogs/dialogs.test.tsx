/**
 * The Recovered dialog (contract §10.2 step 6, §10.3): non-modal, never takes
 * focus, Clean up… covers exactly the processes it names.
 */

import { act, cleanup, fireEvent, render } from '@testing-library/react';

import { resetCleanUpRequestForTests } from '../accounts/StopSheet';
import { CleanUpSheet } from '../accounts';
import { resetHotkeysForTests } from '../hooks/useHotkeys';
import { translate } from '../i18n';
import type { ProcessRecord } from '../lib/gen/ProcessRecord';
import { appStore } from '../store/useAppStore';
import { initialUiState } from '../store/uiSlice';
import { emptyWorld } from '../store/worldSlice';
import { flush, withFakeCore } from '../test/backend';
import { snapshot } from '../test/fixtures';
import { assert, assertDeepEqual, assertEqual, domTest } from '../test/harness';
import { Dialogs } from './index';
import { resetRecoveredForTests, secondsLeft } from './RecoveredDialog';
import { manualClock, setClockForTests } from '../lib/clock';
import type { BootStatus } from '../lib/gen/BootStatus';
import { test } from '../test/harness';

const en = (key: Parameters<typeof translate>[1], params?: Parameters<typeof translate>[2]) => translate('en', key, params);
let rev = 1900;

function proc(n: number, reason: NonNullable<ProcessRecord['leftover']>['reason']): ProcessRecord {
  return { key: `x_${n}_${n}` as never, pid: n, startSec: n, vendor: 'claude', role: 'session', sessionUid: null, gildeOwned: true, rssBytes: n * 1024 * 1024, leftover: { reason, since: 1, heldScans: 2 } };
}

async function settle() {
  await act(async () => {
    await flush(10);
    await new Promise((r) => setTimeout(r, 20));
  });
}

domTest('T18 Recovered dialog: names crash leftovers without taking focus; Clean up… covers only them and closes it', async () => {
  resetHotkeysForTests();
  resetRecoveredForTests();
  resetCleanUpRequestForTests();
  appStore.setState({ world: emptyWorld(), ui: { ...initialUiState(), locale: 'en' } });
  try {
    await withFakeCore(
      {
        health_stop_preview: (req) => ({ targets: [], refused: [], confirmToken: 't', echo: req }),
      },
      async (core) => {
        const outside = document.createElement('button');
        document.body.appendChild(outside);
        outside.focus();
        const view = render(
          <>
            <Dialogs />
            <CleanUpSheet />
          </>,
        );
        assertEqual(view.container.querySelector('[role="dialog"]'), null, 'nothing without such leftovers');
        act(() => appStore.getState().applyStoreSnapshot(snapshot(++rev, { leftovers: [proc(3, 'gildeCrashLeftover'), proc(2, 'quitSurvivor'), proc(1, 'teammateOfGoneLead')] })));
        await settle();
        const dialog = document.querySelector('.rc') as HTMLElement;
        assert(dialog !== null, 'shown');
        assertEqual(dialog.getAttribute('aria-modal'), 'false', 'non-modal');
        assertEqual(document.activeElement, outside, 'focus stays where the user had it');
        assert((dialog.textContent ?? '').includes(en('recovered.afterCrash', { n: 2 })), 'two from the last run');
        assert(!(dialog.textContent ?? '').includes(en('leftover.teammateOfGoneLead')), 'other leftovers are not its business');
        fireEvent.click([...dialog.querySelectorAll('button')].find((b) => b.textContent === en('action.cleanUp')) as HTMLElement);
        await settle();
        assertEqual(appStore.getState().ui.sheet, 'cleanUp', 'the Clean-up sheet');
        assertDeepEqual(core.calls('health_stop_preview')[0]?.req, { processKeys: ['x_3_3', 'x_2_2'] }, 'exactly those processes');
        assertEqual(document.querySelector('.rc'), null, 'the dialog is done');
        outside.remove();
      },
    );
  } finally {
    cleanup();
    resetHotkeysForTests();
    appStore.setState({ world: emptyWorld(), ui: initialUiState() });
  }
});

function bootWith(recovery: BootStatus['recovery']): BootStatus {
  return { spawnGateOpen: true, firstDiscoveryDone: true, agentsDetected: true, memoryOnly: null, agents: [], agentsScannedAt: null, recovery };
}

test('T18b A29.3: whole seconds left, never below 0', () => {
  assertEqual(secondsLeft(10_000, 0), 10, '10 s');
  assertEqual(secondsLeft(10_000, 9_001), 1, 'a part of a second counts as one');
  assertEqual(secondsLeft(10_000, 12_000), 0, 'past');
});

async function withRecovery(run: (core: Parameters<Parameters<typeof withFakeCore>[1]>[0], clock: ReturnType<typeof manualClock>) => Promise<void>) {
  resetHotkeysForTests();
  resetRecoveredForTests();
  const clock = manualClock(1_000_000);
  setClockForTests(clock);
  appStore.setState({ world: emptyWorld(), ui: { ...initialUiState(), locale: 'en' } });
  try {
    await withFakeCore({ undo_cancel: () => ({ cancelled: true }) }, (core) => run(core, clock));
  } finally {
    cleanup();
    resetHotkeysForTests();
    setClockForTests(null);
    appStore.setState({ world: emptyWorld(), ui: initialUiState() });
  }
}

domTest('T18b A29.3: the undo window counts down without taking focus; [Keep them] = undo_cancel{token} and it closes', async () => {
  await withRecovery(async (core) => {
    const outside = document.createElement('button');
    document.body.appendChild(outside);
    outside.focus();
    render(<Dialogs />);
    act(() => appStore.getState().applyStoreSnapshot(snapshot(++rev, { boot: bootWith({ stopping: 3, undoToken: 'u-1' as never, until: 1_010_000 }) })));
    await settle();
    const dialog = document.querySelector('.rc[data-phase="window"]') as HTMLElement;
    assert(dialog !== null, 'shown');
    assertEqual(dialog.getAttribute('aria-modal'), 'false', 'non-modal');
    assertEqual(document.activeElement, outside, 'focus stays');
    assert((dialog.textContent ?? '').includes(en('recovered.stopping', { n: 3, s: 10 })), '3 processes, 10 s');
    fireEvent.click([...dialog.querySelectorAll('button')].find((b) => b.textContent === en('recovered.keep')) as HTMLElement);
    await settle();
    assertDeepEqual(core.calls('undo_cancel')[0]?.req, { undoToken: 'u-1' }, 'one token for the group');
    assertEqual(document.querySelector('.rc[data-phase="window"]'), null, 'closed after the undo');
    outside.remove();
  });
});

domTest('T18b A29.3: the window closes at `until`; a late subscriber sees less time; afterwards leftovers show with Clean up…', async () => {
  await withRecovery(async (core, clock) => {
    clock.set(1_004_000);
    render(<Dialogs />);
    act(() =>
      appStore.getState().applyStoreSnapshot(
        snapshot(++rev, { boot: bootWith({ stopping: 2, undoToken: 'u-2' as never, until: 1_010_000 }), leftovers: [proc(5, 'gildeCrashLeftover')] }),
      ),
    );
    await settle();
    assert((document.body.textContent ?? '').includes(en('recovered.stopping', { n: 2, s: 6 })), 'six seconds left');
    clock.set(1_010_000);
    await act(async () => {
      await new Promise((r) => setTimeout(r, 400));
    });
    assertEqual(document.querySelector('.rc[data-phase="window"]'), null, 'closed at until (before the core clears it)');
    assertEqual(core.calls('undo_cancel').length, 0, 'nothing cancelled');
    const after = document.querySelector('.rc[data-phase="leftovers"]') as HTMLElement;
    assert(after !== null && (after.textContent ?? '').includes(en('action.cleanUp')), 'the survivors, with Clean up…');
  });
});
