/**
 * The quit sheet (ux-ia §3.16; contract §10.2, §15.2 T18 "quit sheet focus on
 * Cancel", §15.5 T18 `sheet.quit.pendingLost`).
 */

import { act, cleanup, fireEvent, render } from '@testing-library/react';

import { resetHotkeysForTests } from '../hooks/useHotkeys';
import { translate } from '../i18n';
import type { AppQuitPreviewRes } from '../lib/gen/AppQuitPreviewRes';
import type { ProjectId } from '../lib/gen/ProjectId';
import type { TeamKey } from '../lib/gen/TeamKey';
import { appStore } from '../store/useAppStore';
import { initialUiState } from '../store/uiSlice';
import { emptyWorld } from '../store/worldSlice';
import { flush, withFakeCore } from '../test/backend';
import type { FakeCore, Handler } from '../test/backend';
import { project, session, snapshot, uid } from '../test/fixtures';
import { assert, assertDeepEqual, assertEqual, domTest } from '../test/harness';
import { groupByProject, QuitSheet } from './QuitSheet';

const en = (key: Parameters<typeof translate>[1], params?: Parameters<typeof translate>[2]) => translate('en', key, params);
let rev = 1500;

function preview(extra: Partial<AppQuitPreviewRes> = {}): AppQuitPreviewRes {
  return {
    sessions: [
      { uid: uid(1), projectId: 'p1' as ProjectId, waiting: 'approval', teamLead: false },
      { uid: uid(2), projectId: 'p2' as ProjectId, waiting: null, teamLead: false },
      { uid: uid(3), projectId: 'p1' as ProjectId, waiting: null, teamLead: true },
    ],
    elsewhere: 7,
    teams: [{ teamKey: 'tk' as TeamKey, members: 3 }],
    confirmToken: 'q-1' as never,
    ...extra,
  };
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
    appStore.setState({ world: emptyWorld(), ui: { ...initialUiState(), locale: 'en' } });
    act(() =>
      appStore.getState().applyStoreSnapshot(
        snapshot(++rev, {
          projects: [project('p1'), project('p2')],
          sessions: [
            session(1, { title: 'api-refactor', projectId: 'p1' as ProjectId }),
            session(2, { title: 'docs-cleanup', projectId: 'p2' as ProjectId }),
            session(3, { title: 'lead', projectId: 'p1' as ProjectId, team: { teamKey: 'tk' as TeamKey, memberKey: 'm' as never, role: 'lead', colour: null } }),
          ],
          teams: [],
        }),
      ),
    );
    try {
      await withFakeCore({ app_quit_preview: () => preview(), app_quit: () => ({}), app_quit_cancel: () => ({}), ...extra }, run);
    } finally {
      cleanup();
      resetHotkeysForTests();
      appStore.setState({ world: emptyWorld(), ui: initialUiState() });
    }
  };
}

domTest(
  'T18 quit sheet: opens on quit-requested with focus on Cancel; ⌘⏎ does not quit; it names waiting sessions and pending requests are lost',
  harness(async (core) => {
    act(() => appStore.getState().setQuitRequested(true));
    const view = render(<QuitSheet />);
    await settle();
    assertEqual(document.activeElement?.textContent, en('common.cancel'), 'focus on Cancel');
    assert(view.getByText(en('sheet.quit', { n: 3 })) !== null, '3 sessions will stop');
    assert(view.getByText(en('sheet.quit.waitingApproval')) !== null, 'waiting for your approval');
    assert(view.getByText(en('sheet.quit.pendingLost')) !== null, 'pending requests are lost (LP-04)');
    assert(view.getByText(en('sheet.quit.elsewhere', { n: 7 })) !== null, 'elsewhere keeps running');
    assert(view.getByText(en('sheet.quit.lead', { others: 3 })) !== null, 'A29.4: members counts the teammates without the lead → lead + 3');
    const names = [...document.querySelectorAll('.qt-name')].map((e) => e.textContent);
    assertDeepEqual(names, ['api-refactor', 'lead', 'docs-cleanup'], 'grouped by project');
    fireEvent.keyDown(document.activeElement as Element, { code: 'Enter', key: 'Enter', metaKey: true });
    await settle();
    assertEqual(core.calls('app_quit').length, 0, '⌘⏎ never quits');
    fireEvent.keyDown(document.activeElement as Element, { code: 'Escape', key: 'Escape' });
    await settle();
    assertEqual(core.calls('app_quit_cancel').length, 1, 'the core hears the cancel');
    assertEqual(appStore.getState().ui.quitRequested, false, 'request cleared');
  }),
);

domTest(
  'T18 quit sheet: Quit sends the previewed token; CONFLICT re-reads the preview; progress comes from the store',
  harness(async (core) => {
    let first = true;
    core.on('app_quit', () => {
      if (first) {
        first = false;
        throw { code: 'CONFLICT', key: 'error.conflict', params: { changed: 'sessions' }, detail: null, retryable: false };
      }
      return new Promise(() => undefined);
    });
    core.on('app_quit_preview', () => preview({ confirmToken: `q-${core.calls('app_quit_preview').length}` as never }));
    act(() => appStore.getState().openSheet('quit'));
    const view = render(<QuitSheet />);
    await settle();
    fireEvent.click(view.getByText(en('sheet.quit.confirm', { n: 3 })));
    await settle();
    assertEqual(core.calls('app_quit_preview').length, 2, 're-previewed');
    assert(view.getByText(en('sheet.quit.changed')) !== null, 'says so');
    fireEvent.click(view.getByText(en('sheet.quit.confirm', { n: 3 })));
    await settle();
    assertDeepEqual(core.calls('app_quit')[1]?.req, { confirmToken: 'q-2' }, 'the fresh token');
    act(() => appStore.getState().applyStorePatch({ fromRev: rev, toRev: ++rev, ops: [{ kind: 'quit', stopped: 1, total: 3, phase: 'stopping' }] }));
    await settle();
    assert(view.getByText(en('sheet.quit.stopping', { stopped: 1, all: 3 })) !== null, 'progress');
    fireEvent.keyDown(document.activeElement ?? document.body, { code: 'Escape', key: 'Escape' });
    await settle();
    assertEqual(appStore.getState().ui.sheet, 'quit', 'nothing to cancel once the quit runs');
  }),
);

domTest(
  'T18 groupByProject keeps the core order within and across projects',
  async () => {
    const rows = preview().sessions;
    assertDeepEqual(groupByProject(rows).map((r) => r.uid), [uid(1), uid(3), uid(2)], 'p1 first, then p2');
  },
);
