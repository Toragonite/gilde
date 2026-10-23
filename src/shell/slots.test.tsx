/**
 * Screen slots: a screen whose module is in the build renders with its props;
 * one that is not shows one neutral sentence; a sheet without a component is
 * closed again at once.
 */

import { act, render } from '@testing-library/react';

import type { SessionUid } from '../lib/gen/SessionUid';
import { appStore } from '../store/useAppStore';
import { assert, assertEqual, domTest } from '../test/harness';
import { uid } from '../test/fixtures';
import { MainView } from './MainView';
import { SheetHost } from './SheetHost';
import { setSlotsForTests } from './slots';
import type { SessionScreenProps } from './slots';
import { scenario } from './testing';

domTest(
  'a missing screen shows one sentence inside the named main landmark',
  scenario(() => {
    setSlotsForTests({ HistoryScreen: null });
    act(() => appStore.getState().navigate({ screen: 'history' }));
    const view = render(<MainView label="History" sidebarShown />);
    const main = view.getByRole('main');
    assertEqual(main.getAttribute('aria-label'), 'History', 'named after the screen');
    assert(main.textContent?.includes('not in this build') ?? false, 'the sentence');
  }),
);

domTest(
  'a screen in the build renders with its props',
  scenario(() => {
    let seen: SessionUid | null = null;
    function SessionScreen({ uid: u, sub }: SessionScreenProps) {
      seen = u;
      return <p>session {sub}</p>;
    }
    setSlotsForTests({ SessionScreen });
    act(() => appStore.getState().openSession(uid(4), { sub: 'changes' }));
    const view = render(<MainView label="x" sidebarShown />);
    assertEqual(seen, uid(4), 'the uid');
    assert(view.getByText('session changes') !== null, 'the sub-view');
  }),
);

domTest(
  'a sheet whose component is not in the build closes again',
  scenario(() => {
    setSlotsForTests({ NewSessionSheet: null });
    render(<SheetHost />);
    act(() => appStore.getState().openSheet('newSession'));
    assertEqual(appStore.getState().ui.sheet, null, 'closed');
  }),
);
