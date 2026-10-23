/**
 * New session sheet (ux-ia §3.12, §5.8; contract §15.2 T18 acceptance:
 * zero-choice `⌘N ⏎`, chip keys; A24.4 picked folder; A26.2 sheet context).
 */

import { act, cleanup, fireEvent, render } from '@testing-library/react';

import { resetHotkeysForTests } from '../hooks/useHotkeys';
import { translate } from '../i18n';
import type { Location } from '../lib/gen/Location';
import type { NewSessionOptions } from '../lib/gen/NewSessionOptions';
import type { ProjectId } from '../lib/gen/ProjectId';
import type { SessionStartReq } from '../lib/gen/SessionStartReq';
import { clockTime } from '../shell/words';
import { appStore } from '../store/useAppStore';
import { initialUiState } from '../store/uiSlice';
import { emptyWorld } from '../store/worldSlice';
import { flush, withFakeCore } from '../test/backend';
import type { FakeCore, Handler } from '../test/backend';
import { project, session, snapshot, uid } from '../test/fixtures';
import { assert, assertDeepEqual, assertEqual, domTest } from '../test/harness';
import { NewSessionSheet, resetNewSessionDraftForTests } from './NewSessionSheet';
import { options } from './testing';

const en = (key: Parameters<typeof translate>[1], params?: Parameters<typeof translate>[2]) => translate('en', key, params);
let rev = 900;

function location(id: string, projectId: string): Location {
  return {
    id: id as never,
    projectId: projectId as ProjectId,
    path: `/Users/u/${projectId}` as never,
    kind: 'main',
    branch: 'main',
    worktreeName: null,
    gildeCreated: false,
    exists: true,
  };
}

function world(extra: Parameters<typeof snapshot>[1] = {}) {
  act(() => {
    appStore.getState().applyStoreSnapshot(
      snapshot(++rev, {
        projects: [project('p1', { order: 0 }), project('p2', { order: 1 })],
        locations: [location('l_main', 'p1')],
        ...extra,
      }),
    );
  });
}

function openSheet(context?: { projectId?: ProjectId }) {
  act(() => {
    const ui = appStore.getState().ui;
    appStore.setState({ ui: { ...ui, sheet: 'newSession', sheetContext: context ?? null } as typeof ui });
  });
}

async function settle(turns = 10) {
  await act(async () => {
    await flush(turns);
    await new Promise((r) => setTimeout(r, 20));
  });
}

const START_UID = uid(77);

function harness(run: (core: FakeCore) => Promise<void>, extra: Record<string, Handler> = {}) {
  return async () => {
    resetHotkeysForTests();
    resetNewSessionDraftForTests();
    appStore.setState({ world: emptyWorld(), ui: { ...initialUiState(), locale: 'en' } });
    try {
      await withFakeCore(
        {
          newsession_options: () => options(),
          session_start: () => ({ uid: START_UID }),
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

function prompt(): HTMLTextAreaElement {
  return document.querySelector('textarea[aria-label]') as HTMLTextAreaElement;
}

function press(code: string, opts: { alt?: boolean; meta?: boolean; shift?: boolean; isComposing?: boolean; keyCode?: number; target?: Element | null } = {}) {
  const target = opts.target ?? document.activeElement ?? document.body;
  fireEvent.keyDown(target, {
    code,
    key: code.startsWith('Digit') ? code.slice(5) : code,
    altKey: !!opts.alt,
    metaKey: !!opts.meta,
    shiftKey: !!opts.shift,
    isComposing: !!opts.isComposing,
    keyCode: opts.keyCode,
  });
}

function startReq(core: FakeCore): Omit<SessionStartReq, 'clientRequestId'> {
  const calls = core.calls('session_start');
  assertEqual(calls.length, 1, 'one session_start');
  const { clientRequestId, ...rest } = calls[0]?.req as SessionStartReq;
  assert(typeof clientRequestId === 'string' && clientRequestId.length > 0, 'an idempotency key');
  return rest;
}

const DEFAULT_REQ = {
  projectId: 'p1',
  vendor: 'claude',
  accountId: 'a_claude_work',
  model: 'sonnet',
  location: { kind: 'here', locationId: 'l_main' },
  permissionMode: 'default',
  controlMode: 'conversation',
  team: false,
};

domTest(
  'T18 zero-choice: ⌘N, type, ⏎ starts with every default and opens the session',
  harness(async (core) => {
    world();
    openSheet();
    render(<NewSessionSheet />);
    await settle();
    assertEqual(document.activeElement, prompt(), 'the prompt has focus');
    assertDeepEqual(core.calls('newsession_options')[0]?.req, { projectId: 'p1', vendor: null }, 'options for the first project');
    fireEvent.change(prompt(), { target: { value: 'Fix the login bug' } });
    press('Enter');
    await settle();
    assertDeepEqual(startReq(core), { ...DEFAULT_REQ, prompt: 'Fix the login bug' }, 'the defaults, untouched');
    const ui = appStore.getState().ui;
    assertEqual(ui.sheet, null, 'the sheet closed');
    assertDeepEqual(ui.view, { screen: 'session', uid: START_UID, sub: 'conversation' }, 'the new session opens');
  }),
);

domTest(
  'T18 zero-choice: ⏎ with an empty prompt starts with no first message',
  harness(async (core) => {
    world();
    openSheet();
    render(<NewSessionSheet />);
    await settle();
    press('Enter');
    await settle();
    assertEqual(startReq(core).prompt, null, 'no first message');
  }),
);

domTest(
  'T18 ⏎ before the options arrive starts as soon as they do (once)',
  harness(async (core) => {
    let release: (o: NewSessionOptions) => void = () => undefined;
    core.on('newsession_options', () => new Promise<NewSessionOptions>((r) => (release = r)));
    world();
    openSheet();
    render(<NewSessionSheet />);
    await settle();
    fireEvent.change(prompt(), { target: { value: 'go' } });
    press('Enter');
    press('Enter');
    await settle();
    assertEqual(core.calls('session_start').length, 0, 'waits for the options');
    assert(document.body.textContent?.includes(en('newsession.starting')), 'says it is starting');
    release(options());
    await settle();
    assertEqual(startReq(core).prompt, 'go', 'started once, with the prompt');
  }),
);

domTest(
  'T18 ⏎ never starts during Korean composition or with ⇧; ⇧⏎ is a newline',
  harness(async (core) => {
    world();
    openSheet();
    render(<NewSessionSheet />);
    await settle();
    fireEvent.change(prompt(), { target: { value: '로그인' } });
    press('Enter', { isComposing: true });
    press('Enter', { keyCode: 229 });
    press('Enter', { shift: true });
    await settle();
    assertEqual(core.calls('session_start').length, 0, 'nothing started');
    assertEqual(appStore.getState().ui.sheet, 'newSession', 'still open');
  }),
);

domTest(
  'T18 chip keys: ⌥1…⌥7 open the chips in order from the prompt; Esc closes only the menu',
  harness(async () => {
    world();
    openSheet();
    render(<NewSessionSheet />);
    await settle();
    const order = ['project', 'vendor', 'account', 'model', 'where', 'permissions', 'mode'];
    for (const [i, id] of order.entries()) {
      prompt().focus();
      press(`Digit${i + 1}`, { alt: true });
      await settle(4);
      const chip = document.querySelector(`[data-chip="${id}"]`) as HTMLElement;
      assertEqual(chip.getAttribute('aria-expanded'), 'true', `⌥${i + 1} opens ${id}`);
      assertEqual(chip.getAttribute('aria-keyshortcuts'), `Alt+${i + 1}`, `${id} names its key`);
      assert(document.querySelector('[role="menu"]') !== null, `${id} menu`);
      assert(document.querySelector('[role="menu"]')?.contains(document.activeElement), `focus is in the ${id} menu`);
      press('Escape');
      await settle(4);
      assertEqual(document.querySelector('[role="menu"]'), null, `Esc closes the ${id} menu`);
      assertEqual(appStore.getState().ui.sheet, 'newSession', 'the sheet stays open');
      assertEqual(document.activeElement, chip, 'focus back on the chip');
    }
    press('Escape');
    await settle(4);
    assertEqual(appStore.getState().ui.sheet, null, 'a second Esc cancels the sheet');
  }),
);

domTest(
  'T18 chip menus: ↑↓ move, a pick sets the value; ⌥8 turns Team on (Terminal, pane hint)',
  harness(async (core) => {
    world();
    openSheet();
    render(<NewSessionSheet />);
    await settle();
    press('Digit4', { alt: true, target: prompt() });
    await settle(4);
    assertEqual(document.activeElement?.textContent?.includes('sonnet'), true, 'focus on the checked model');
    press('ArrowUp');
    assertEqual(document.activeElement?.textContent?.includes('opus'), true, '↑ moves');
    fireEvent.click(document.activeElement as HTMLElement);
    await settle(4);
    press('Digit8', { alt: true, target: prompt() });
    await settle(4);
    const team = document.querySelector('[data-chip="team"]') as HTMLButtonElement;
    assertEqual(team.getAttribute('aria-pressed'), 'true', 'team on');
    assert((document.querySelector('[data-chip="mode"]') as HTMLButtonElement).disabled, 'mode fixed while Team is on');
    assert(document.body.textContent?.includes(en('team.paneHint')), 'the pane hint line');
    press('Enter', { meta: true });
    await settle();
    const req = startReq(core);
    assertEqual(req.model, 'opus', 'picked model');
    assertEqual(req.team, true, 'team');
    assertEqual(req.controlMode, 'terminal', 'Terminal');
  }),
);

domTest(
  'T18 A26.2: the sheet context preselects its project over the open view',
  harness(async (core) => {
    world({ sessions: [session(5, { projectId: 'p1' as ProjectId })] });
    act(() => appStore.getState().openSession(uid(5)));
    openSheet({ projectId: 'p2' as ProjectId });
    render(<NewSessionSheet />);
    await settle();
    assertEqual((core.calls('newsession_options')[0]?.req as { projectId: string }).projectId, 'p2', 'context wins');
    cleanup();
    act(() => appStore.getState().closeSheet());
    openSheet();
    render(<NewSessionSheet />);
    await settle();
    const last = core.calls('newsession_options').slice(-1)[0]?.req as { projectId: string };
    assertEqual(last.projectId, 'p1', 'no context: the project open in Main');
  }),
);

domTest(
  'T18 A24.4: with no project, ⏎ picks a folder and starts there with no prior session',
  harness(
    async (core) => {
      act(() => appStore.getState().applyStoreSnapshot(snapshot(++rev, {})));
      openSheet();
      render(<NewSessionSheet />);
      await settle();
      assert(document.body.textContent?.includes(en('newsession.noProject')), 'asks for a folder');
      assertEqual(core.calls('newsession_options').length, 0, 'no options without a project');
      press('Enter', { target: prompt() });
      await settle();
      assertEqual(core.calls('project_pick').length, 1, 'the folder picker');
      assertDeepEqual(core.calls('newsession_options')[0]?.req, { projectId: 'p9', vendor: null }, 'options for the picked folder');
      assertEqual(startReq(core).projectId, 'p9', 'started in the picked folder');
    },
    { project_pick: () => ({ project: project('p9') }) },
  ),
);

domTest(
  'T18 §5.8 lines: "near the limit" shows the reset as a time; the switch line undoes with one click',
  harness(async (core) => {
    const at = new Date(2026, 9, 23, 14, 10).getTime();
    core.on('newsession_options', () =>
      options({
        notices: [
          { key: 'accounts.switched', params: { a: 'Personal', b: 'Work', p: 91 } },
          { key: 'accounts.allNear', params: { vendor: 'codex', time: at } },
        ],
      }),
    );
    world();
    openSheet();
    render(<NewSessionSheet />);
    await settle();
    const text = document.body.textContent ?? '';
    assert(text.includes(en('accounts.allNear', { vendor: 'codex', time: clockTime(at, 'en') })), 'the reset as a clock time');
    assert(!text.includes(String(at)) && !text.includes(at.toLocaleString('en')), 'never the raw epoch');
    assert(text.includes(en('accounts.switched', { a: 'Personal', b: 'Work', p: 91 })), 'the switch, said once');
    fireEvent.click(document.querySelector('.ns-line-action') as HTMLElement);
    await settle(4);
    assert(!(document.body.textContent ?? '').includes(en('accounts.switched', { a: 'Personal', b: 'Work', p: 91 })), 'the line goes');
    press('Enter', { target: prompt() });
    await settle();
    assertEqual(startReq(core).accountId, 'a_claude_personal', 'started on the account the user kept');
  }),
);

domTest(
  'T18 "Don\'t ask" shows the warning line; a start error stays in the sheet with its detail',
  harness(
    async (core) => {
      world();
      openSheet();
      render(<NewSessionSheet />);
      await settle();
      press('Digit6', { alt: true, target: prompt() });
      await settle(4);
      const bypass = [...document.querySelectorAll('[role="menuitemradio"]')].find((b) => b.textContent?.includes(en('newsession.perm.bypassPermissions'))) as HTMLElement;
      fireEvent.click(bypass);
      await settle(4);
      assert(document.body.textContent?.includes(en('newsession.bypassWarn')), 'warning');
      press('Enter', { target: prompt() });
      await settle();
      assertEqual(core.calls('session_start').length, 1, 'tried');
      assertEqual(appStore.getState().ui.sheet, 'newSession', 'stays open');
      assert(document.body.textContent?.includes(en('error.spawnFailed')), 'the sentence');
      assert(document.body.textContent?.includes('exec: not found'), 'the detail, in mono');
    },
    {
      session_start: () => {
        throw { code: 'SPAWN_FAILED', key: 'error.spawnFailed', params: {}, detail: 'exec: not found', retryable: true };
      },
    },
  ),
);

domTest(
  'T18 Cancel keeps the typed task for the next open; a start clears it',
  harness(async () => {
    world();
    openSheet();
    const first = render(<NewSessionSheet />);
    await settle();
    fireEvent.change(prompt(), { target: { value: 'half a thought' } });
    press('Escape', { target: prompt() });
    await settle(4);
    assertEqual(appStore.getState().ui.sheet, null, 'cancelled');
    first.unmount();
    openSheet();
    const second = render(<NewSessionSheet />);
    await settle();
    assertEqual(prompt().value, 'half a thought', 'kept');
    press('Enter', { target: prompt() });
    await settle();
    second.unmount();
    openSheet();
    render(<NewSessionSheet />);
    await settle();
    assertEqual(prompt().value, '', 'cleared after a start');
  }),
);
