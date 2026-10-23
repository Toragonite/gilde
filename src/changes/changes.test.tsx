/**
 * Changes tests (ux-ia §3.13, contract §4.2.5, A17.4–A17.6): the file list
 * with made-by, ActionState reasons (`changes.ownerWriting {owner}`,
 * `changes.stopToDiscard`), preview → sheet → confirm token, discard needs
 * every loss ticked and never deletes the branch, destructive sheets have no
 * ⌘⏎ and focus Cancel, watched sessions are read-only, errors keep their
 * detail.
 */

import { act, cleanup, fireEvent, render } from '@testing-library/react';

import { resetHotkeysForTests } from '../hooks/useHotkeys';
import { translate } from '../i18n';
import type { ChangesView as ChangesData } from '../lib/gen/ChangesView';
import type { GitCommitReq } from '../lib/gen/GitCommitReq';
import type { LocationId } from '../lib/gen/LocationId';
import type { Session } from '../lib/gen/Session';
import type { WorktreeDiscardReq } from '../lib/gen/WorktreeDiscardReq';
import { appStore } from '../store/useAppStore';
import { initialUiState } from '../store/uiSlice';
import { emptyWorld } from '../store/worldSlice';
import { flush, withFakeCore } from '../test/backend';
import type { FakeCore, Handler } from '../test/backend';
import { session, snapshot, uid } from '../test/fixtures';
import { assert, assertDeepEqual, assertEqual, domTest } from '../test/harness';
import { ChangesView } from './ChangesView';
import { CommitSheet, DiscardWorktreeSheet, MergeSheet } from './sheets';

/** The view plus the sheets, mounted the way the shell mounts them (A23.5). */
function Mounted({ n }: { n: number }) {
  return (
    <>
      <ChangesView uid={uid(n)} />
      <CommitSheet />
      <MergeSheet />
      <DiscardWorktreeSheet />
    </>
  );
}

let rev = 500;

function data(extra: Partial<ChangesData> = {}): ChangesData {
  return {
    location: 'l_1' as LocationId,
    base: { branch: 'main', commitShort: '4e1c9a2', newerOnMain: 2 },
    files: [
      { pathRel: 'app/settings/usePushPrefs.ts', change: 'modified', added: 71, removed: 22, parts: ['usePushPrefs()', 'toPref()'], madeBy: { kind: 'thisSession' } },
      { pathRel: 'services/api/app.module.ts', change: 'modified', added: 2, removed: 0, parts: ['AppModule'], madeBy: { kind: 'notFromKnownSession' } },
    ],
    commits: [{ commitShort: 'a91f3c2', subject: 'Extract preference mapping', at: 0 }],
    sharedFolder: null,
    actions: { commit: { kind: 'enabled' }, merge: { kind: 'enabled' }, discard: { kind: 'enabled' } },
    ...extra,
  };
}

function harness(handlers: Record<string, Handler>, run: (core: FakeCore) => Promise<void>) {
  return async () => {
    resetHotkeysForTests();
    appStore.setState({ world: emptyWorld(), ui: { ...initialUiState(), locale: 'en' } });
    try {
      await withFakeCore(handlers, run);
    } finally {
      cleanup();
      resetHotkeysForTests();
      appStore.setState({ world: emptyWorld(), ui: initialUiState() });
    }
  };
}

function setSessions(sessions: Session[]) {
  act(() => {
    appStore.getState().applyStoreSnapshot(snapshot(++rev, { sessions }));
  });
}

async function settle() {
  await act(async () => {
    await flush(10);
  });
}

domTest(
  'Files with +/−, changed parts and made-by; Diff and Open go to the IDE',
  harness({ changes_get: () => data(), ide_open: () => ({}) }, async (core) => {
    setSessions([session(1)]);
    const view = render(<ChangesView uid={uid(1)} />);
    await settle();
    assert(view.getByText('app/settings/usePushPrefs.ts') !== null, 'file');
    assert(view.getByText('usePushPrefs(), toPref()') !== null, 'changed parts');
    assert(view.getByText(translate('en', 'changes.madeBy.this')) !== null, 'this session');
    assert(view.getByText(translate('en', 'changes.unknownSource')) !== null, 'not from a known session');
    const row = view.getByText('app/settings/usePushPrefs.ts').closest('li') as HTMLElement;
    fireEvent.keyDown(row, { key: 'Enter', code: 'Enter' });
    fireEvent.keyDown(row, { key: 'o', code: 'KeyO', metaKey: true });
    await settle();
    assertDeepEqual(
      core.calls('ide_open').map((c) => (c.req as { target: { kind: string } }).target.kind),
      ['diff', 'file'],
      '⏎ = Diff, ⌘O = Open',
    );
  }),
);

domTest(
  'Disabled actions show their reason; ownerWriting names the owner; hidden actions are absent',
  harness(
    {
      changes_get: () =>
        data({
          actions: {
            commit: { kind: 'disabled', reason: 'changes.ownerWriting' },
            merge: { kind: 'hidden' },
            discard: { kind: 'disabled', reason: 'changes.stopToDiscard' },
          },
        }),
    },
    async () => {
      setSessions([session(1)]);
      const view = render(<ChangesView uid={uid(1)} />);
      await settle();
      assert(view.getByText(translate('en', 'changes.ownerWriting', { owner: 'claude' })) !== null, 'owner line with {owner}');
      assert(view.getByText(translate('en', 'changes.stopToDiscard')) !== null, 'A17.6 stop-to-discard line');
      assertEqual(view.queryByText(translate('en', 'changes.merge')), null, 'merge hidden');
      assert((view.getByText(translate('en', 'changes.commit')) as HTMLButtonElement).disabled, 'commit disabled');
    },
  ),
);

domTest(
  'Watched sessions: the list stays, no actions',
  harness({ changes_get: () => data() }, async () => {
    setSessions([session(1, { caps: { ...session(1).caps, changes: 'readOnly' } })]);
    const view = render(<ChangesView uid={uid(1)} />);
    await settle();
    assert(view.getByText('app/settings/usePushPrefs.ts') !== null, 'list');
    assertEqual(view.queryByText(translate('en', 'changes.commit')), null, 'no commit');
    assertEqual(view.queryByText(translate('en', 'changes.discard')), null, 'no discard');
  }),
);

domTest(
  'Commit: preview → sheet → ⌘⏎ commits the ticked paths with the confirm token',
  harness(
    {
      changes_get: () => data(),
      git_commit_preview: () => ({ files: data().files, author: { name: 'U', email: 'u@example.com' }, confirmToken: 'tok-c' }),
      git_commit: () => ({ commit: 'b00b1e5' }),
    },
    async (core) => {
      setSessions([session(1, { title: 'Split the auth middleware' })]);
      act(() => appStore.getState().openSession(uid(1), { sub: 'changes' }));
      const view = render(<Mounted n={1} />);
      await settle();
      fireEvent.click(view.getByText(translate('en', 'changes.commit')));
      await settle();
      const box = view.getByLabelText('services/api/app.module.ts') as HTMLInputElement;
      act(() => fireEvent.click(box));
      fireEvent.keyDown(document.activeElement ?? document.body, { key: 'Enter', code: 'Enter', metaKey: true });
      await settle();
      const req = core.calls('git_commit')[0]?.req as GitCommitReq;
      assertEqual(req.confirmToken, 'tok-c', 'the token');
      assertEqual(req.message, 'Split the auth middleware', 'message prefilled from the title');
      assertDeepEqual(req.paths, ['app/settings/usePushPrefs.ts'], 'only ticked paths');
      assert(view.getByText(translate('en', 'changes.committed', { commit: 'b00b1e5' })) !== null, 'result line');
    },
  ),
);

domTest(
  'Discard: every loss needs its own tick; focus starts on Cancel; no ⌘⏎; the branch is kept',
  harness(
    {
      changes_get: () => data(),
      worktree_discard_preview: () => ({
        losses: [
          { id: 'l1', kind: 'uncommittedChanges', count: 2 },
          { id: 'l2', kind: 'unmergedCommits', count: 1 },
        ],
        branchKept: true,
        confirmToken: 'tok-d',
      }),
      worktree_discard: () => ({}),
    },
    async (core) => {
      setSessions([session(1)]);
      act(() => appStore.getState().openSession(uid(1), { sub: 'changes' }));
      const view = render(<Mounted n={1} />);
      await settle();
      fireEvent.click(view.getByText(translate('en', 'changes.discard')));
      await settle();
      assertEqual(document.activeElement?.textContent, translate('en', 'common.cancel'), 'focus on Cancel');
      const confirm = view.getByText(translate('en', 'changes.discardConfirm')) as HTMLButtonElement;
      assert(confirm.disabled, 'disabled until every loss is ticked');
      fireEvent.click(view.getByLabelText(translate('en', 'changes.loss.uncommittedChanges', { n: 2 })));
      assert(confirm.disabled, 'one of two is not enough');
      fireEvent.keyDown(document.body, { key: 'Enter', code: 'Enter', metaKey: true });
      fireEvent.click(view.getByLabelText(translate('en', 'changes.loss.unmergedCommits', { n: 1 })));
      fireEvent.keyDown(document.body, { key: 'Enter', code: 'Enter', metaKey: true });
      await settle();
      assertEqual(core.calls('worktree_discard').length, 0, '⌘⏎ never confirms a destructive sheet');
      assert(view.getByText(translate('en', 'changes.branchKept')) !== null, 'branch kept');
      fireEvent.click(confirm);
      await settle();
      const req = core.calls('worktree_discard')[0]?.req as WorktreeDiscardReq;
      assertDeepEqual(req, { uid: uid(1), confirmToken: 'tok-d', acceptedLosses: ['l1', 'l2'] }, 'accepted losses, no deleteBranch (A17.5)');
      assertEqual(appStore.getState().ui.sheet, null, 'closeSheet()');
      assert(view.getByText(translate('en', 'changes.discarded')) !== null, 'the outcome line on the view');
    },
  ),
);

domTest(
  'Errors keep their sentence and the detail in mono (A17.4 codes)',
  harness(
    {
      changes_get: () => {
        throw { code: 'GIT_FAILED', key: 'error.gitFailed', params: {}, detail: 'fatal: bad object HEAD', retryable: false };
      },
    },
    async () => {
      setSessions([session(1)]);
      const view = render(<ChangesView uid={uid(1)} />);
      await settle();
      assert(view.getByText(translate('en', 'error.gitFailed')) !== null, 'sentence');
      assert(view.getByText('fatal: bad object HEAD') !== null, 'detail');
    },
  ),
);
