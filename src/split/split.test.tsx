/**
 * Split and tile tests (contract §15.2 T16: tile focus/keys, never steal
 * focus, layout persistence; §15.6 F-04 app_view_state, F-11 composer per
 * card kind, F-15 answer_in routing, F-30 guard on window activation;
 * ux-ia §3.7, J4; A44.1 a tile's guard restarts for a replaced request,
 * A44.3 a mirrored card's reply intent lives on the tile's composer).
 */

import { act, cleanup, fireEvent, render } from '@testing-library/react';

import { resetAnswersForTests } from '../approval/answer';
import { resetReplyIntentsForTests } from '../approval/replyIntent';
import { GUARD_MS, refreshGuards, resetAnswerGuardForTests } from '../hooks/useAnswerGuard';
import { resetHotkeysForTests } from '../hooks/useHotkeys';
import { translate } from '../i18n';
import { manualClock, setClockForTests } from '../lib/clock';
import type { ManualClock } from '../lib/clock';
import type { PendingInteraction } from '../lib/gen/PendingInteraction';
import type { ScreenPromptKeyReq } from '../lib/gen/ScreenPromptKeyReq';
import type { Session } from '../lib/gen/Session';
import type { SessionUid } from '../lib/gen/SessionUid';
import type { ViewState } from '../lib/gen/ViewState';
import { resetAnnouncerForTests } from '../lib/notify';
import { PERSIST_DEBOUNCE_MS, startUiPersistence } from '../store/persist';
import { appStore } from '../store/useAppStore';
import { initialUiState } from '../store/uiSlice';
import { startViewStateReporter } from '../store/viewState';
import { emptyWorld } from '../store/worldSlice';
import { flush, withFakeCore } from '../test/backend';
import type { FakeCore, Handler } from '../test/backend';
import { pending, session, snapshot, uid } from '../test/fixtures';
import { assert, assertDeepEqual, assertEqual, domTest, test } from '../test/harness';
import { focusOrder, noteFocused, resetFocusForTests } from './focus';
import { clampRatio, leastRecent, panesGrid, spatialNext, splitGrid, touch } from './layout';
import { addToSplitOrAsk, resetReplaceForTests } from './replace';
import { SplitScreen } from './SplitScreen';

// ---------------------------------------------------------------------------
// Pure rules
// ---------------------------------------------------------------------------

test('splitGrid: 1 → one, 2 → side by side, 3 → 1 + 2, 4 → 2×2, 5–6 → 3×2 (2 columns at 900–1199, one below 900)', () => {
  assertDeepEqual(splitGrid(0, 'wide').cells, [], 'empty');
  assertEqual(splitGrid(1, 'wide').columns, 1, 'one');
  const two = splitGrid(2, 'wide');
  assertDeepEqual([two.columns, two.rows], [2, 1], 'side by side');
  const three = splitGrid(3, 'wide');
  assertDeepEqual(three.cells[0], { col: 0, row: 0, colSpan: 1, rowSpan: 2 }, '3: the first tile is tall');
  const four = splitGrid(4, 'wide');
  assertDeepEqual([four.columns, four.rows], [2, 2], '2×2');
  const five = splitGrid(5, 'wide');
  assertDeepEqual([five.columns, five.rows], [3, 2], '5 → 3×2');
  assertDeepEqual(five.cells[4], { col: 1, row: 1, colSpan: 2, rowSpan: 1 }, 'the 5th spans the gap');
  const six = splitGrid(6, 'wide');
  assertEqual(six.cells.length, 6, 'six cells');
  const sixMid = splitGrid(6, 'medium');
  assertDeepEqual([sixMid.columns, sixMid.rows], [2, 3], 'at most two columns at 900–1199');
  assertEqual(splitGrid(6, 'narrow').cells.length, 1, 'one tile below 900');
  assertEqual(splitGrid(9, 'wide').cells.length, 6, 'clamped to 6');
});

test('panesGrid: the lead column plus teammates in two columns (one at 900–1199); none fit below 900', () => {
  assertEqual(panesGrid(0, 'wide').cells.length, 1, 'lead only');
  const four = panesGrid(4, 'wide');
  assertDeepEqual(four.cells[0], { col: 0, row: 0, colSpan: 1, rowSpan: 2 }, 'lead spans the rows');
  assertEqual(four.columns, 3, 'lead + two teammate columns');
  const three = panesGrid(3, 'wide');
  assertDeepEqual(three.cells[3], { col: 1, row: 1, colSpan: 2, rowSpan: 1 }, 'the third teammate spans the bottom');
  const mid = panesGrid(3, 'medium');
  assertDeepEqual([mid.columns, mid.rows], [2, 3], 'teammates stack on the right');
  assertEqual(panesGrid(7, 'wide').cells.length, 5, 'capacity 4 teammates');
  assertEqual(panesGrid(3, 'narrow').cells.length, 1, 'narrow: one tile');
});

test('spatialNext: arrows move on the logical grid and stop at the edges', () => {
  const cells = splitGrid(4, 'wide').cells; // 0 1 / 2 3
  assertEqual(spatialNext(cells, 0, 'right'), 1, '0 → right = 1');
  assertEqual(spatialNext(cells, 0, 'down'), 2, '0 → down = 2');
  assertEqual(spatialNext(cells, 3, 'up'), 1, '3 → up = 1');
  assertEqual(spatialNext(cells, 3, 'left'), 2, '3 → left = 2');
  assertEqual(spatialNext(cells, 0, 'left'), null, 'edge');
  const three = splitGrid(3, 'wide').cells; // a b / a c
  assertEqual(spatialNext(three, 2, 'left'), 0, 'c → left = the tall a');
  assertEqual(spatialNext(three, 0, 'right'), 1, 'a → right prefers the top row');
});

test('least recently focused: never-focused first, else the oldest; touch moves to the front', () => {
  const order = touch(touch(touch([], 'a'), 'b'), 'c'); // c b a
  assertDeepEqual(order, ['c', 'b', 'a'], 'most recent first');
  assertEqual(leastRecent(order, ['a', 'b', 'c']), 'a', 'oldest');
  assertEqual(leastRecent(order, ['b', 'x', 'c']), 'x', 'never focused wins');
  assertEqual(leastRecent([], []), null, 'nothing');
  assertEqual(clampRatio(0.9), 0.8, 'divider max 80 % (A30.3)');
  assertEqual(clampRatio(0.1), 0.2, 'divider min 20 % (A30.3)');
  assertEqual(clampRatio(Number.NaN), 0.4, 'default 40 %');
});

// ---------------------------------------------------------------------------
// DOM
// ---------------------------------------------------------------------------

let rev = 3000;

function setSessions(sessions: Session[]) {
  act(() => {
    appStore.getState().applyStoreSnapshot(snapshot(++rev, { sessions }));
  });
}

function setSplit(uids: SessionUid[], focused: SessionUid | null = null) {
  act(() => {
    const ui = appStore.getState().ui;
    appStore.setState({ ui: { ...ui, view: { screen: 'split' }, split: { uids, focused, zoomed: null } } });
  });
}

const base: Record<string, Handler> = {
  convo_subscribe: () => ({ subId: 'v_00000000000000e1', source: 'stream', note: null }),
  convo_unsubscribe: () => ({}),
  session_mark_seen: () => ({}),
  app_view_state: () => ({}),
  ui_state_set: () => ({}),
  owner_focus: () => ({ precision: 'app' }),
  interaction_answer: () => ({ outcome: 'answered' }),
  screen_prompt_key: () => ({ outcome: 'sent' }),
};

function harness(run: (core: FakeCore) => Promise<void>, extra: Record<string, Handler> = {}) {
  return async () => {
    resetHotkeysForTests();
    resetAnswerGuardForTests();
    resetAnswersForTests();
    resetAnnouncerForTests();
    resetFocusForTests();
    resetReplaceForTests();
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
  const code = /^[0-9]$/.test(k) ? `Digit${k}` : k;
  fireEvent.keyDown(document.body, { key: k, code, metaKey: !!mods.meta, altKey: !!mods.alt, shiftKey: !!mods.shift });
};

const composerLabel = (agent = 'claude') => translate('en', 'composer.placeholder', { agent });
const tiles = (container: HTMLElement) => [...container.querySelectorAll<HTMLElement>('section.g-tile')];

domTest(
  'Split: tiles in set order with ⌘n hints in a 2×2 grid; an empty Split shows one sentence and Add session',
  harness(async () => {
    setSessions([session(1), session(2), session(3), session(4)]);
    setSplit([uid(1), uid(2), uid(3), uid(4)]);
    const view = render(<SplitScreen band="wide" />);
    await settle();
    const list = tiles(view.container);
    assertEqual(list.length, 4, 'four tiles');
    assertEqual(list[0]?.getAttribute('data-uid'), uid(1), 'set order');
    assert(list[3]?.textContent?.includes('⌘4') ?? false, '⌘4 hint');
    assertEqual(list[0]?.getAttribute('aria-label'), translate('en', 'tile.label', { index: 1, tiles: 4, name: 'session 1', state: 'Ready' }), 'tile name');
    const grid = view.container.querySelector<HTMLElement>('.g-tiles');
    assert((grid?.style.gridTemplateColumns ?? '').startsWith('repeat(2'), 'two columns');
    cleanup();
    setSplit([]);
    const empty = render(<SplitScreen band="wide" />);
    await settle();
    assert(empty.getByText(translate('en', 'split.empty')) !== null, 'one sentence');
    assertEqual(empty.getAllByText(translate('en', 'split.add')).length, 2, 'Add session (header + empty state)');
  }),
);

domTest(
  'Split ⌘n: the store focus moves and the DOM follows — the composer (In Gilde), the card when one is pending, the body when watched',
  harness(async () => {
    const watched = session(3, {
      control: { kind: 'watching', origin: 'vsCode', owner: { appPid: 1, appName: 'VS Code', kind: 'vsCode', bundleId: 'com.microsoft.VSCode' } },
      caps: { ...session(3).caps, send: 'none', approve: 'goToOwner', ownerFocus: true },
    });
    setSessions([session(1), session(2, { pending: [pending('r2', { defaultToNo: true })] }), watched]);
    setSplit([uid(1), uid(2), uid(3)]);
    const view = render(<SplitScreen band="wide" />);
    await settle();
    key('1', { meta: true });
    await settle();
    assertEqual(appStore.getState().ui.split.focused, uid(1), 'store focus');
    assertEqual(document.activeElement?.getAttribute('aria-label'), composerLabel(), '⌘1 → its composer');
    key('2', { meta: true });
    await settle();
    assertEqual(appStore.getState().ui.split.focused, uid(2), '⌘2');
    assertEqual(document.activeElement?.getAttribute('data-role'), 'deny', '⌘2 → its card (Deny on default_to_no)');
    key('3', { meta: true });
    await settle();
    assert(document.activeElement?.classList.contains('g-tile-body') ?? false, '⌘3 → the watched tile body');
    const third = tiles(view.container)[2];
    assertEqual(third?.getAttribute('data-focused'), 'true', 'the focus frame');
  }),
);

domTest(
  'Never steal focus: typing in tile 1; a card arrives in tile 2 and a 3rd tile is added — focus and the store focus stay',
  harness(async () => {
    setSessions([session(1), session(2), session(3)]);
    setSplit([uid(1), uid(2)], uid(1));
    const view = render(<SplitScreen band="wide" />);
    await settle();
    const composer = view.getAllByLabelText(composerLabel())[0] as HTMLTextAreaElement;
    act(() => composer.focus());
    fireEvent.change(composer, { target: { value: 'half a thought' } });
    setSessions([session(1), session(2, { pending: [pending('r9')] }), session(3)]);
    act(() => {
      appStore.getState().addToSplit(uid(3));
    });
    await settle();
    assertEqual(tiles(view.container).length, 3, 'the new tile is there');
    assertEqual(document.activeElement, composer, 'focus stayed in the composer');
    assertEqual(appStore.getState().ui.split.focused, uid(1), 'store focus unchanged');
    assertEqual(appStore.getState().ui.drafts[uid(1)], 'half a thought', 'the draft is kept');
  }),
);

domTest(
  'Clicking into a tile focuses it; ⇧⌘⏎ zooms it (only it renders) and back; app_view_state follows every change [F-04]',
  harness(async (core) => {
    setSessions([session(1), session(2), session(3)]);
    setSplit([uid(1), uid(2), uid(3)]);
    const stop = startViewStateReporter(
      (listener) => appStore.subscribe((s) => listener(s.ui, s.world)),
      { ui: appStore.getState().ui, world: appStore.getState().world },
    );
    try {
      const view = render(<SplitScreen band="wide" />);
      await settle();
      const second = tiles(view.container)[1] as HTMLElement;
      act(() => {
        second.querySelector<HTMLElement>('.g-tile-body')?.focus();
      });
      await settle();
      assertEqual(appStore.getState().ui.split.focused, uid(2), 'DOM focus → store focus');
      key('Enter', { meta: true, shift: true });
      await settle();
      assertEqual(tiles(view.container).length, 1, 'only the zoomed tile');
      assertEqual(appStore.getState().ui.split.zoomed, uid(2), 'zoomed');
      const reports = core.calls('app_view_state').map((c) => c.req as ViewState);
      const last = reports[reports.length - 1] as ViewState;
      assertEqual(last.zoomedUid, uid(2), 'zoom reported');
      assertDeepEqual(last.visibleUids, [uid(2)], 'only the zoomed tile is visible');
      assert(reports.some((r) => r.focusedUid === uid(2)), 'focus reported');
      key('Enter', { meta: true, shift: true });
      await settle();
      assertEqual(tiles(view.container).length, 3, 'restored');
    } finally {
      stop();
    }
  }),
);

domTest(
  'Layout persistence: the set and its focus are saved as splitSet; × closes a tile without stopping its session',
  harness(async (core) => {
    setSessions([session(1), session(2), session(3)]);
    setSplit([uid(1), uid(2), uid(3)], uid(1));
    const stop = startUiPersistence((l) => appStore.subscribe((s) => l(s.ui)), appStore.getState().ui);
    try {
      const view = render(<SplitScreen band="wide" />);
      await settle();
      key('2', { meta: true });
      await settle();
      const closeButtons = view.getAllByLabelText(translate('en', 'action.closeTile'));
      fireEvent.click(closeButtons[2] as HTMLElement);
      await settle(PERSIST_DEBOUNCE_MS + 60);
      const saved = core.calls('ui_state_set').map((c) => c.req as { key: string; value: unknown }).filter((r) => r.key === 'splitSet');
      assert(saved.length > 0, 'splitSet saved');
      assertDeepEqual(saved[saved.length - 1]?.value, { uids: [uid(1), uid(2)], focused: uid(2) }, 'set and focus');
      assertEqual(core.calls('session_stop').length + core.calls('session_stop_preview').length, 0, 'closing a tile never stops');
    } finally {
      stop();
    }
  }),
);

domTest(
  '⌥⌘ arrows move focus spatially: from tile 1 of 4, → is tile 2 and ↓ is tile 3',
  harness(async () => {
    setSessions([session(1), session(2), session(3), session(4)]);
    setSplit([uid(1), uid(2), uid(3), uid(4)], uid(1));
    render(<SplitScreen band="wide" />);
    await settle();
    key('ArrowRight', { meta: true, alt: true });
    await settle();
    assertEqual(appStore.getState().ui.split.focused, uid(2), '→');
    key('ArrowLeft', { meta: true, alt: true });
    key('ArrowDown', { meta: true, alt: true });
    await settle();
    assertEqual(appStore.getState().ui.split.focused, uid(3), '↓');
  }),
);

domTest(
  'Below 900 px one tile shows with the tile strip; ⌘3 brings tile 3',
  harness(async () => {
    setSessions([session(1), session(2), session(3)]);
    setSplit([uid(1), uid(2), uid(3)], uid(1));
    const view = render(<SplitScreen band="narrow" />);
    await settle();
    assertEqual(tiles(view.container).length, 1, 'one tile');
    const strip = view.getByRole('toolbar', { name: translate('en', 'tile.strip') });
    assertEqual(strip.querySelectorAll('button').length, 3, 'a strip item per tile');
    key('3', { meta: true });
    await settle();
    assertEqual(tiles(view.container)[0]?.getAttribute('data-uid'), uid(3), 'tile 3 shows');
    fireEvent.click(strip.querySelectorAll('button')[1] as HTMLElement);
    await settle();
    assertEqual(tiles(view.container)[0]?.getAttribute('data-uid'), uid(2), 'a strip pick shows that tile');
  }),
);

domTest(
  'A 7th session asks which tile to replace, preselecting the least recently focused; Replace swaps it in',
  harness(async () => {
    const six = [1, 2, 3, 4, 5, 6].map((n) => session(n));
    setSessions([...six, session(7, { title: 'the seventh' })]);
    setSplit(six.map((s) => s.uid), uid(1));
    for (const n of [2, 3, 4, 5, 6, 1]) noteFocused('split', uid(n)); // tile 2 is the oldest
    assertEqual(focusOrder('split')[5], uid(2), 'order');
    const view = render(<SplitScreen band="wide" />);
    await settle();
    let outcome = '';
    act(() => {
      outcome = addToSplitOrAsk(uid(7));
    });
    await settle();
    assertEqual(outcome, 'ask', 'asks');
    assertEqual(appStore.getState().ui.split.uids.length, 6, 'nothing replaced yet');
    assert(view.getByText(translate('en', 'split.max'), { exact: false }) !== null, '"Up to 6 sessions side by side."');
    const picked = view.container.querySelector<HTMLInputElement>('.g-split-replace input:checked');
    assertEqual(picked?.value, uid(2), 'least recently focused preselected');
    fireEvent.click(view.getByText(translate('en', 'split.replace')));
    await settle();
    const uids = appStore.getState().ui.split.uids;
    assert(!uids.includes(uid(2)) && uids.includes(uid(7)), `replaced: ${uids.join(',')}`);
    assertEqual(appStore.getState().ui.split.focused, uid(7), 'the new tile is focused');
  }),
);

domTest(
  'T-ui-answerInRouting: a mirrored card on a tile answers in its answer_in session [F-15]',
  harness(async (core) => {
    const lead = session(9, { pending: [pending('r-team', { onBehalfOf: null })] });
    const member = session(8, { pending: [pending('r-team', { answerIn: uid(9) })] });
    setSessions([lead, member]);
    setSplit([uid(8)], uid(8));
    const view = render(<SplitScreen band="wide" />);
    await settle(800);
    assert(view.getByText(translate('en', 'team.answeredAtLead')) !== null, '"answered at the lead"');
    const allow = view.container.querySelector<HTMLElement>('[data-role="allowOnce"]');
    assert(allow !== null, 'allow button');
    fireEvent.click(allow as HTMLElement);
    await settle();
    const call = core.calls('interaction_answer')[0]?.req as { uid: string; requestId: string } | undefined;
    assertEqual(call?.uid, uid(9), 'answered in the lead');
    assertEqual(call?.requestId, 'r-team', 'the exact request');
  }),
);

domTest(
  'T-ui-guard: the 700 ms guard after window activation ignores a click on a tile card, then lets it through [F-30]',
  harness(async (core) => {
    const clock = manualClock(5_000_000);
    setClockForTests(clock);
    setSessions([session(1, { pending: [pending('r1')] })]);
    setSplit([uid(1)], uid(1));
    const view = render(<SplitScreen band="wide" />);
    await settle();
    clock.advance(1000);
    act(() => refreshGuards());
    act(() => {
      window.dispatchEvent(new Event('focus'));
    });
    await settle();
    fireEvent.click(view.container.querySelector('[data-role="allowOnce"]') as HTMLElement);
    await settle();
    assertEqual(core.calls('interaction_answer').length, 0, 'ignored during the guard');
    clock.advance(800);
    act(() => refreshGuards());
    await settle();
    fireEvent.click(view.container.querySelector('[data-role="allowOnce"]') as HTMLElement);
    await settle();
    assertEqual(core.calls('interaction_answer').length, 1, 'answered after the guard');
  }),
);

domTest(
  'T-ui-composerByKind: the focused tile\'s composer maps ⏎ per card kind — a question gets the reply as its answer [F-11]',
  harness(async (core) => {
    const clock = manualClock(7_000_000);
    setClockForTests(clock);
    const question = pending('q1', {
      kind: 'question',
      title: 'Which region?',
      questions: [{ text: 'Which region?', options: ['eu', 'us'], multiSelect: false, header: null, descriptions: [], previews: [] }],
      options: [
        { key: '1', label: 'eu', role: 'answer' },
        { key: '2', label: 'us', role: 'answer' },
      ],
    });
    setSessions([session(1, { pending: [question] })]);
    setSplit([uid(1)], uid(1));
    const view = render(<SplitScreen band="wide" />);
    await settle();
    clock.advance(1500); // the card has been on screen ≥ 700 ms (armed, F-38)
    const composer = view.getByLabelText(composerLabel()) as HTMLTextAreaElement;
    act(() => composer.focus());
    fireEvent.change(composer, { target: { value: 'ap-northeast' } });
    await settle();
    fireEvent.keyDown(composer, { key: 'Enter', code: 'Enter' });
    await settle();
    const call = core.calls('interaction_answer')[0]?.req as { decision: { kind: string } } | undefined;
    assertEqual(call?.decision.kind, 'answer', 'the reply is the answer, never a denial');
    assertEqual(core.calls('turn_send').length, 0, 'not sent as a message');
  }),
);

domTest(
  'Esc in a tile: composer → body → frame (the terminal keeps its own Esc)',
  harness(async () => {
    setSessions([session(1)]);
    setSplit([uid(1)], uid(1));
    const view = render(<SplitScreen band="wide" />);
    await settle();
    const composer = view.getByLabelText(composerLabel()) as HTMLTextAreaElement;
    act(() => composer.focus());
    fireEvent.keyDown(composer, { key: 'Escape', code: 'Escape' });
    assert(document.activeElement?.classList.contains('g-tile-body') ?? false, 'body');
    fireEvent.keyDown(document.activeElement as HTMLElement, { key: 'Escape', code: 'Escape' });
    assert(document.activeElement?.classList.contains('g-tile') ?? false, 'frame');
  }),
);

domTest(
  'A30.2: Split publishes the tiles it renders — all when wide, the shown one below 900 px, the zoomed one — and clears on unmount',
  harness(async (core) => {
    setSessions([session(1), session(2), session(3)]);
    setSplit([uid(1), uid(2), uid(3)], uid(2));
    const stop = startViewStateReporter(
      (l) => appStore.subscribe((s) => l(s.ui, s.world)),
      { ui: appStore.getState().ui, world: appStore.getState().world },
    );
    try {
      const wide = render(<SplitScreen band="wide" />);
      await settle();
      assertDeepEqual(appStore.getState().ui.visibleUids, [uid(1), uid(2), uid(3)], 'wide: every tile');
      wide.unmount();
      await settle();
      assertEqual(appStore.getState().ui.visibleUids, null, 'cleared on unmount');
      const narrow = render(<SplitScreen band="narrow" />);
      await settle();
      assertDeepEqual(appStore.getState().ui.visibleUids, [uid(2)], 'below 900 px: only the shown tile, not the strip');
      const reports = core.calls('app_view_state').map((c) => c.req as ViewState);
      assertDeepEqual((reports[reports.length - 1] as ViewState).visibleUids, [uid(2)], 'app_view_state reports it');
      key('3', { meta: true });
      await settle();
      assertDeepEqual(appStore.getState().ui.visibleUids, [uid(3)], 'a strip pick changes what is visible');
      key('Enter', { meta: true, shift: true });
      await settle();
      assertDeepEqual(appStore.getState().ui.visibleUids, [uid(3)], 'zoomed: the zoomed tile');
      narrow.unmount();
    } finally {
      stop();
    }
  }),
);

// ---------------------------------------------------------------------------
// A44.3 — a reply intent belongs to the composer that opens (G7c-10)
// ---------------------------------------------------------------------------

/**
 * A pane teammate (db-agent, session 8) whose permission prompt renders in its
 * lead (session 9): the lead holds the request, the member's tile shows a
 * mirrored copy (`answer_in` = lead) with a stale promptRev [F-15].
 */
function mirroredTile(clock: ManualClock) {
  const leadCopy = pending('sp_00aa', {
    kind: 'screenPrompt',
    promptRev: 'lead-rev',
    onBehalfOf: 'db-agent' as PendingInteraction['onBehalfOf'],
    options: [
      { key: '1', label: 'Yes', role: 'allowWithNote' },
      { key: '2', label: 'No', role: 'deny' },
    ],
  });
  const mirror = { ...leadCopy, promptRev: 'stale-rev', answerIn: uid(9), onBehalfOf: null };
  const terminal = { kind: 'inGilde' as const, mode: 'terminal' as const, pty: null, hostedTeam: false, transport: 'pty' as const };
  const caps = { ...session(1).caps, send: 'ptyPaste' as const, approve: 'screenKeys' as const };
  setSessions([
    session(9, { title: 'lead', control: { ...terminal, hostedTeam: true }, caps, pending: [leadCopy] }),
    session(8, { title: 'db-agent', control: terminal, caps, pending: [mirror] }),
  ]);
  setSplit([uid(8)], uid(8));
  const view = render(<SplitScreen band="wide" />);
  const tick = async (ms: number) => {
    await act(async () => {
      clock.advance(ms);
      refreshGuards();
      await flush(10);
    });
  };
  const composer = () => {
    const el = view.container.querySelector<HTMLTextAreaElement>('textarea');
    if (!el) throw new Error('no composer on the member tile');
    return el;
  };
  const typeAndSend = async (text: string) => {
    act(() => composer().focus());
    fireEvent.change(composer(), { target: { value: text } });
    await tick(10);
    fireEvent.keyDown(composer(), { key: 'Enter', code: 'Enter' });
    await tick(10);
  };
  return { view, tick, typeAndSend };
}

function intentHarness(run: (clock: ManualClock, core: FakeCore) => Promise<void>) {
  return harness(async (core) => {
    const clock = manualClock(5_000_000);
    setClockForTests(clock);
    resetReplyIntentsForTests();
    try {
      await run(clock, core);
    } finally {
      resetReplyIntentsForTests();
    }
  }, { turn_send: () => ({ delivery: 'sent', position: null }) });
}

const screenCalls = (core: FakeCore) => core.calls('screen_prompt_key').map((x) => x.req as ScreenPromptKeyReq);

domTest(
  'A44.3 (G7c-10): "Allow with a note" on a mirrored tile card sends allowWithNote to the lead, never a deny',
  intentHarness(async (clock, core) => {
    const { view, tick, typeAndSend } = mirroredTile(clock);
    await tick(1000);
    const note = view.container.querySelector<HTMLButtonElement>('[data-action="allowWithNote"]');
    if (!note) throw new Error('no "Allow with a note" on the mirrored card');
    fireEvent.click(note);
    await tick(1000);
    await typeAndSend('use the staging database');
    const calls = screenCalls(core);
    assertEqual(calls.length, 1, `one answer (got ${JSON.stringify(calls)})`);
    assertDeepEqual(
      calls[0],
      { uid: uid(9), requestId: 'sp_00aa', promptRev: 'lead-rev', action: { kind: 'allowWithNote', text: 'use the staging database' } },
      "allowWithNote, in the lead, with the lead's fresh promptRev",
    );
  }),
);

domTest(
  'A44.3: "Deny and say why" on a mirrored tile card sends denyWithReply to the lead',
  intentHarness(async (clock, core) => {
    const { view, tick, typeAndSend } = mirroredTile(clock);
    await tick(1000);
    const why = view.container.querySelector<HTMLButtonElement>('[data-action="denyWhy"]');
    if (!why) throw new Error('no "Deny and say why" on the mirrored card');
    fireEvent.click(why);
    await tick(1000);
    assertEqual(document.activeElement?.tagName, 'TEXTAREA', 'focus is in the tile\'s composer');
    await typeAndSend('not on production');
    assertDeepEqual(
      screenCalls(core),
      [{ uid: uid(9), requestId: 'sp_00aa', promptRev: 'lead-rev', action: { kind: 'denyWithReply', text: 'not on production' } }],
      'denyWithReply, in the lead',
    );
  }),
);

domTest(
  'A44.3: typing in the member tile without choosing on the mirrored card never answers the lead\'s prompt',
  intentHarness(async (clock, core) => {
    const { tick, typeAndSend } = mirroredTile(clock);
    await tick(1500); // the card has been on screen ≥ 700 ms: typing would be armed (F-38)
    await typeAndSend('what are you doing?');
    assertDeepEqual(screenCalls(core), [], 'a missing intent never falls back to a Deny');
    const sends = core.calls('turn_send').map((x) => x.req as { uid: string; text: string });
    assertDeepEqual(sends.map((s) => [s.uid, s.text]), [[uid(8), 'what are you doing?']], "the composer's normal send, to its own session");
  }),
);

domTest(
  'A44.1: a Split tile — a stacked request that replaces the answered one starts a new 700 ms guard',
  harness(async (core) => {
    const clock = manualClock(6_000_000);
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
    const caps = { ...session(1).caps, send: 'ptyPaste' as const, approve: 'screenKeys' as const };
    const terminal = { kind: 'inGilde' as const, mode: 'terminal' as const, pty: null, hostedTeam: false, transport: 'pty' as const };
    const s1 = session(1, { control: terminal, caps, pending: [prompt('sp_a'), prompt('sp_b')] });
    setSessions([s1]);
    setSplit([uid(1)], uid(1));
    render(<SplitScreen band="wide" />);
    await settle();
    const tick = (ms: number) =>
      act(() => {
        clock.advance(ms);
        refreshGuards();
      });
    tick(GUARD_MS + 10);
    key('Enter', { meta: true });
    await settle();
    const ids = () => screenCalls(core).map((c) => c.requestId);
    assertDeepEqual(ids(), ['sp_a'], 'the focused tile answers its top card');
    setSessions([{ ...s1, pending: [prompt('sp_b')] }]);
    tick(150);
    key('Enter', { meta: true });
    await settle();
    assertDeepEqual(ids(), ['sp_a'], 'sp_b took the tile\'s top 150 ms ago: a second ⌘⏎ answers nothing');
    tick(GUARD_MS);
    key('Enter', { meta: true });
    await settle();
    assertDeepEqual(ids(), ['sp_a', 'sp_b'], 'after its own 700 ms, sp_b answers');
  }),
);
