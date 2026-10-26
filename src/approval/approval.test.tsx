/**
 * The card's acceptance tests (contract §8.6, §15.2 T15, §15.5, §15.6):
 * M7 keys (digits only while the card has focus, ⌘⏎/⌘⌫ on the top card),
 * `default_to_no`, stacking, the withdrawn line, IME and key repeat,
 * T-ui-guard on cards (clicks and keys), T-ui-answerInRouting, screen-prompt
 * cards, questions, and every detail line through i18n (A19.7); A44: the
 * shared shortcut decision for surfaces outside the card (A44.2) and the
 * guard after ⌘J restarting for a replaced request (A44.1).
 */

import { act, cleanup, fireEvent, render } from '@testing-library/react';

import { GUARD_MS, noteWindowActivation, refreshGuards, resetAnswerGuardForTests } from '../hooks/useAnswerGuard';
import { resetHotkeysForTests } from '../hooks/useHotkeys';
import { translate } from '../i18n';
import { manualClock, setClockForTests } from '../lib/clock';
import type { ManualClock } from '../lib/clock';
import type { InteractionAnswerReq } from '../lib/gen/InteractionAnswerReq';
import type { PendingInteraction } from '../lib/gen/PendingInteraction';
import type { ScreenPromptKeyReq } from '../lib/gen/ScreenPromptKeyReq';
import type { Session } from '../lib/gen/Session';
import { resetAnnouncerForTests } from '../lib/notify';
import { SessionScreen } from '../session/SessionScreen';
import { useShellKeys } from '../shell/useShellKeys';
import { appStore, useAppStore } from '../store/useAppStore';
import { emptyWorld } from '../store/worldSlice';
import { initialUiState } from '../store/uiSlice';
import { withFakeCore, flush } from '../test/backend';
import type { FakeCore } from '../test/backend';
import { pending, queueItem, session, snapshot, uid } from '../test/fixtures';
import { assert, assertDeepEqual, assertEqual, domTest, test } from '../test/harness';
import { answerShortcut, resetAnswersForTests } from './answer';
import { CardStack } from './CardStack';
import { FocusCard } from './FocusCard';
import { allowsNote, composerModeFor, isArmed, isPermissionShaped, shortcutAction, trustOption } from './decisions';
import { replyIntentOf, resetReplyIntentsForTests } from './replyIntent';

let rev = 1;

function setSessions(sessions: Session[]): void {
  act(() => {
    appStore.getState().applyStoreSnapshot(snapshot(++rev, { sessions }));
  });
}

function reset(): void {
  resetHotkeysForTests();
  resetAnswerGuardForTests();
  resetAnswersForTests();
  resetAnnouncerForTests();
  resetReplyIntentsForTests();
  appStore.setState({ world: emptyWorld(), ui: { ...initialUiState(), locale: 'en' } });
}

const answered = { outcome: 'answered', previous: null };

function harness(run: (c: ManualClock, core: FakeCore) => Promise<void>) {
  return async () => {
    const c = manualClock(1_000_000);
    setClockForTests(c);
    reset();
    try {
      await withFakeCore(
        {
          interaction_answer: () => answered,
          screen_prompt_key: () => ({ outcome: 'sent' }),
          owner_focus: () => ({ precision: 'app' }),
          interaction_detail: () => ({ inputPretty: '{"command":"npm test"}', toolName: 'Bash', decisionReason: null }),
        },
        (core) => run(c, core),
      );
    } finally {
      cleanup();
      setClockForTests(null);
      reset();
    }
  };
}

/** Let answer promises resolve inside act. */
async function settle(): Promise<void> {
  await act(async () => {
    await flush();
  });
}

function pass(c: ManualClock, ms = GUARD_MS) {
  act(() => {
    c.advance(ms);
    refreshGuards();
  });
}

function key(target: Element, code: string, extra: Record<string, unknown> = {}) {
  const k = code.startsWith('Digit') ? code.slice(5) : code === 'Enter' ? 'Enter' : code === 'Backspace' ? 'Backspace' : code;
  fireEvent.keyDown(target, { code, key: k, ...extra });
}

const tool = (requestId: string, extra: Partial<PendingInteraction> = {}) =>
  pending(requestId, { alwaysLabel: 'npm test', tool: { tool: 'Bash', verb: 'run', object: 'npm test' }, ...extra });

function answers(core: FakeCore): InteractionAnswerReq[] {
  return core.calls('interaction_answer').map((c) => c.req as InteractionAnswerReq);
}

// ---------------------------------------------------------------------------
// Pure rules
// ---------------------------------------------------------------------------

test('shortcutAction: default_to_no keeps ⌘⏎ inert with the defaultNo line; ⌘⌫ denies', () => {
  const p = pending('r1', { defaultToNo: true });
  assertDeepEqual(shortcutAction(p, 'primary'), { kind: 'inert', line: 'approval.defaultNo' }, '⌘⏎ inert');
  assertDeepEqual(shortcutAction(p, 'deny'), { kind: 'decide', decision: { kind: 'deny' } }, '⌘⌫ denies');
});

test('shortcutAction: screen prompts answer only when permission-shaped and High', () => {
  const options = [
    { key: '1', label: 'Yes', role: 'allowOnce' as const },
    { key: '2', label: "Yes, and don't ask again", role: 'other' as const },
    { key: '3', label: 'No', role: 'deny' as const },
  ];
  const live = pending('sp_1', { kind: 'screenPrompt', options, promptRev: 'abcd', defaultToNo: false });
  assertDeepEqual(shortcutAction(live, 'primary'), { kind: 'screen', action: { kind: 'key', key: '1' }, thenTerminal: false }, '⌘⏎ = 1');
  assertDeepEqual(shortcutAction(live, 'deny'), { kind: 'screen', action: { kind: 'key', key: '3' }, thenTerminal: false }, '⌘⌫ = last');
  const likely = { ...live, confidence: 'likely' as const, defaultToNo: true };
  assertDeepEqual(shortcutAction(likely, 'primary'), { kind: 'inert', line: 'approval.useOwnKeys' }, 'Likely: inert');
  const question = pending('sp_2', { kind: 'screenPrompt', options: options.map((o) => ({ ...o, role: 'answer' as const })), defaultToNo: true });
  assertDeepEqual(shortcutAction(question, 'deny'), { kind: 'inert', line: 'approval.useOwnKeys' }, 'question-shaped: inert');
});

test('A25.1: option roles — trustFolder marks the trust action, allowWithNote the note-capable allow; the interim markers are retired', () => {
  const trust = pending('sp_t', { kind: 'setupPrompt', options: [{ key: '2', label: 'Yes, I trust this folder', role: 'trustFolder' }] });
  assertEqual(trustOption(trust)?.key, '2', 'the trust option');
  assertEqual(trustOption({ ...trust, kind: 'screenPrompt' }), null, 'only on setup cards');
  assertEqual(trustOption(pending('sp_o', { kind: 'setupPrompt', options: [{ key: '1', label: 'Yes', role: 'allowOnce' }] })), null, 'allowOnce on a setup card means nothing');

  const note = pending('sp_n', {
    kind: 'screenPrompt',
    promptRev: 'abcd',
    defaultToNo: false,
    options: [
      { key: '1', label: 'Yes', role: 'allowWithNote' },
      { key: '2', label: 'Yes, and switch to auto mode', role: 'other' },
      { key: '3', label: 'No', role: 'deny' },
    ],
  });
  assert(allowsNote(note), 'Allow with a note is offered');
  assert(isPermissionShaped(note), 'the note-capable allow is still the allow digit');
  assertDeepEqual(shortcutAction(note, 'primary'), { kind: 'screen', action: { kind: 'key', key: '1' }, thenTerminal: false }, '⌘⏎ = 1');
  const plain = { ...note, options: note.options.map((o) => (o.key === '1' ? { ...o, role: 'allowOnce' as const } : o)) };
  assert(!allowsNote(plain), 'no note without the role');
  const interim = { ...plain, options: [...plain.options, { key: 'note', label: 'note', role: 'allowSession' as const }] };
  assert(!allowsNote(interim), "the interim {key:'note', role:'allowSession'} marker is retired");
});

test('T-ui-composerByKind: what ⏎ sends per card kind [F-11, N-12]', () => {
  assertEqual(composerModeFor(null).kind, 'send', 'no card: a message');
  assertEqual(composerModeFor(pending('a')).kind, 'denyWithMessage', 'tool approval');
  const oneQ = pending('q', { kind: 'question', questions: [{ text: 'Which?', options: ['A', 'B'], multiSelect: false, header: null, descriptions: [], previews: [] }] });
  assertDeepEqual(composerModeFor(oneQ), { kind: 'freeAnswer', requestId: 'q', questionIndex: 0 }, 'one question: free answer');
  const twoQ = pending('q2', {
    kind: 'question',
    questions: [
      { text: 'Which?', options: ['A'], multiSelect: false, header: null, descriptions: [], previews: [] },
      { text: 'And?', options: ['B'], multiSelect: false, header: null, descriptions: [], previews: [] },
    ],
  });
  assertEqual(composerModeFor(twoQ).kind, 'replyInstead', 'several questions: a reply instead of them, never a partial map');
  assertEqual(composerModeFor(pending('p', { kind: 'planApproval' })).kind, 'keepPlanning', 'plan');
  assertEqual(composerModeFor(pending('s', { kind: 'screenPrompt', promptRev: 'ab' })).kind, 'screenDenyWithReply', 'screen');
  assertEqual(composerModeFor(pending('u', { kind: 'setupPrompt', promptRev: 'ab' })).kind, 'keepDraft', 'setup: draft kept');
  assertEqual(composerModeFor(pending('w', { kind: 'watchedApproval' })).kind, 'keepDraft', 'watched: draft kept');
});

test('F-38: typing is armed only when the card was on screen ≥ 700 ms before the first keystroke', () => {
  assert(!isArmed(null, null, 5000), 'no card');
  assert(!isArmed(1000, 1500, 5000), 'draft started 500 ms after the card: not armed');
  assert(isArmed(1000, 1700, 5000), 'started at 700 ms: armed');
  assert(isArmed(1000, null, 1700), 'empty draft after 700 ms: armed (the hint shows)');
  assert(!isArmed(1000, 900, 5000), 'a draft begun before the card: not armed');
});

// ---------------------------------------------------------------------------
// M7 keys on the card
// ---------------------------------------------------------------------------

domTest(
  'M7: digits answer only while the card has focus — never from a text field or elsewhere',
  harness(async (c, core) => {
    setSessions([session(1, { pending: [tool('r1')] })]);
    const view = render(
      <>
        <textarea aria-label="composer" />
        <CardStack uid={uid(1)} />
      </>,
    );
    pass(c);
    const textarea = view.getByLabelText('composer');
    textarea.focus();
    key(textarea, 'Digit1');
    key(document.body, 'Digit1');
    await settle();
    assertEqual(answers(core).length, 0, 'a digit outside the card never answers');

    const card = view.container.querySelector('.g-card') as HTMLElement;
    card.focus();
    key(card, 'Digit1');
    await settle();
    assertDeepEqual(answers(core), [{ uid: uid(1), requestId: 'r1', decision: { kind: 'allowOnce' }, via: 'digit' }], '1 = allow once, via digit');
  }),
);

domTest(
  'M7: 2 = always allow only when button 2 is shown; 3 = deny; Esc never denies',
  harness(async (c, core) => {
    setSessions([session(1, { pending: [tool('r1', { alwaysLabel: null })] })]);
    const view = render(<CardStack uid={uid(1)} onLeave={() => {}} />);
    pass(c);
    assert(view.queryByText(/Always allow/) === null, 'button 2 hidden without a label');
    const card = view.container.querySelector('.g-card') as HTMLElement;
    card.focus();
    key(card, 'Digit2');
    key(card, 'Escape');
    await settle();
    assertEqual(answers(core).length, 0, '2 does nothing without button 2; Esc does not deny');
    key(card, 'Digit3');
    await settle();
    assertDeepEqual(answers(core).map((a) => a.decision.kind), ['deny'], '3 denies');
  }),
);

domTest(
  'M7: ⌘⏎ allows and ⌘⌫ denies the top card from the composer when the stack has shortcuts',
  harness(async (c, core) => {
    setSessions([session(1, { pending: [tool('r1'), tool('r2')] })]);
    const view = render(
      <>
        <textarea aria-label="composer" />
        <CardStack uid={uid(1)} shortcuts />
      </>,
    );
    pass(c);
    assert(view.getByText('1 of 2') !== null, 'stack position on the first card');
    assert(view.getByText('2 of 2') !== null, 'and on the second');
    const textarea = view.getByLabelText('composer');
    textarea.focus();
    key(textarea, 'Enter', { metaKey: true });
    await settle();
    assertDeepEqual(answers(core), [{ uid: uid(1), requestId: 'r1', decision: { kind: 'allowOnce' }, via: 'shortcut' }], '⌘⏎ answers the oldest card');
  }),
);

domTest(
  'M7: without shortcuts (an unfocused tile) ⌘⏎ does nothing',
  harness(async (c, core) => {
    setSessions([session(1, { pending: [tool('r1')] })]);
    render(<CardStack uid={uid(1)} />);
    pass(c);
    key(document.body, 'Enter', { metaKey: true });
    key(document.body, 'Backspace', { metaKey: true });
    await settle();
    assertEqual(answers(core).length, 0, 'no shortcut on an unfocused stack');
  }),
);

// ---------------------------------------------------------------------------
// default_to_no
// ---------------------------------------------------------------------------

domTest(
  'default_to_no: focus lands on Deny; ⌘⏎ is inert with the line; 1 still allows',
  harness(async (c, core) => {
    setSessions([session(1, { pending: [tool('r1', { defaultToNo: true })] })]);
    const view = render(<CardStack uid={uid(1)} shortcuts focusToken={1} />);
    await act(async () => {
      await new Promise((r) => setTimeout(r, 30));
    });
    const deny = view.container.querySelector('[data-role="deny"]');
    assertEqual(document.activeElement, deny, 'focus on [3] Deny');
    pass(c);
    key(document.body, 'Enter', { metaKey: true });
    await settle();
    assertEqual(answers(core).length, 0, '⌘⏎ does not allow');
    assert(view.getByText(translate('en', 'approval.defaultNo')) !== null, 'the defaultNo line shows');
    key(deny as Element, 'Digit1');
    await settle();
    assertDeepEqual(answers(core).map((a) => a.decision.kind), ['allowOnce'], '[1] still works');
  }),
);

// ---------------------------------------------------------------------------
// T-ui-guard on cards
// ---------------------------------------------------------------------------

domTest(
  'T-ui-guard: a new card ignores clicks, digits and ⌘⏎ for 700 ms, visibly; window activation guards again',
  harness(async (c, core) => {
    setSessions([session(1, { pending: [tool('r1')] })]);
    const view = render(<CardStack uid={uid(1)} shortcuts />);
    const allow = view.getByText(translate('en', 'approval.allowOnce')).closest('button') as HTMLButtonElement;
    assertEqual(allow.getAttribute('aria-disabled'), 'true', 'announced as unavailable');
    assert(view.container.querySelector('[data-guard-note]') !== null, 'the guard is visible');
    // Only the answering controls dim (they are aria-disabled); the card's
    // text keeps its contrast during the guard.
    const card = view.container.querySelector('.g-card') as HTMLElement;
    assertEqual(card.getAttribute('data-guarded'), null, 'the card itself is not dimmed');
    const answering = [...card.querySelectorAll('button[data-role]')];
    assertEqual(answering.length, 3, 'allow once, always, deny');
    for (const b of answering) {
      assertEqual(b.getAttribute('data-guarded'), 'true', `${b.textContent} dims`);
      assertEqual(b.getAttribute('aria-disabled'), 'true', `${b.textContent} is aria-disabled`);
    }
    fireEvent.click(allow);
    card.focus();
    key(card, 'Digit1');
    key(card, 'Enter', { metaKey: true });
    await settle();
    assertEqual(answers(core).length, 0, 'nothing answered during the guard');

    pass(c);
    act(() => noteWindowActivation());
    fireEvent.click(allow);
    await settle();
    assertEqual(answers(core).length, 0, 'window activation starts the guard again [F-30]');

    pass(c);
    fireEvent.click(allow);
    await settle();
    assertDeepEqual(answers(core).map((a) => a.via), ['button'], 'answers after the guard');
  }),
);

domTest(
  'T-ui-guard: when the top card is answered, the next card is guarded again (no double-click carry-over)',
  harness(async (c, core) => {
    setSessions([session(1, { pending: [tool('r1'), tool('r2')] })]);
    const view = render(<CardStack uid={uid(1)} shortcuts />);
    pass(c);
    key(document.body, 'Enter', { metaKey: true });
    await settle();
    setSessions([session(1, { pending: [tool('r2')] })]);
    key(document.body, 'Enter', { metaKey: true });
    await settle();
    assertDeepEqual(answers(core).map((a) => a.requestId), ['r1'], 'the new top card is guarded');
    assert(view.container.querySelector('[data-guard-note]') !== null, 'visibly');
  }),
);

domTest(
  'IME and key repeat never answer',
  harness(async (c, core) => {
    setSessions([session(1, { pending: [tool('r1')] })]);
    const view = render(<CardStack uid={uid(1)} shortcuts />);
    pass(c);
    const card = view.container.querySelector('.g-card') as HTMLElement;
    card.focus();
    key(card, 'Digit1', { isComposing: true });
    key(card, 'Digit1', { keyCode: 229 });
    key(card, 'Digit1', { repeat: true });
    key(card, 'Enter', { metaKey: true, isComposing: true });
    key(card, 'Enter', { metaKey: true, repeat: true });
    await settle();
    assertEqual(answers(core).length, 0, 'composition and repeat are ignored');
  }),
);

// ---------------------------------------------------------------------------
// Withdrawn, ended, answered here
// ---------------------------------------------------------------------------

domTest(
  'Withdrawn: a card that leaves without our answer collapses to "Answered elsewhere."; ours leaves no line',
  harness(async (c) => {
    setSessions([session(1, { pending: [tool('r1'), tool('r2')] })]);
    const view = render(<CardStack uid={uid(1)} shortcuts />);
    pass(c);
    key(document.body, 'Enter', { metaKey: true });
    await settle();
    setSessions([session(1, { pending: [tool('r2')] })]);
    assertEqual(view.queryByText(translate('en', 'approval.answeredElsewhere')), null, 'our own answer: no line');
    setSessions([session(1, { pending: [] })]);
    assert(view.getByText(translate('en', 'approval.answeredElsewhere')) !== null, 'withdrawn: the muted line');
    assertEqual(view.container.querySelectorAll('.g-card').length, 0, 'the card is gone');
  }),
);

domTest(
  'Ended: a card that leaves with the process shows "The pending request ended with the session."',
  harness(async () => {
    setSessions([session(1, { pending: [tool('r1')] })]);
    const view = render(<CardStack uid={uid(1)} />);
    setSessions([session(1, { pending: [], control: { kind: 'notRunning', lastOrigin: null, endedHow: 'ended' } })]);
    assert(view.getByText(translate('en', 'card.sessionEnded')) !== null, 'the ended line');
  }),
);

domTest(
  'An answer that comes back withdrawn shows the same line; nothing is retried',
  harness(async (c, core) => {
    core.on('interaction_answer', () => ({ outcome: 'withdrawn', previous: null }));
    setSessions([session(1, { pending: [tool('r1')] })]);
    const view = render(<CardStack uid={uid(1)} />);
    pass(c);
    fireEvent.click(view.getByText(translate('en', 'approval.allowOnce')).closest('button') as HTMLElement);
    await settle();
    assert(view.getByText(translate('en', 'approval.answeredElsewhere')) !== null, 'withdrawn line');
    assertEqual(answers(core).length, 1, 'one call, no retry');
  }),
);

domTest(
  'Errors are lines, never retries: CONFLICT{tooFresh} shows the guard line',
  harness(async (c, core) => {
    core.on('interaction_answer', () => {
      throw { code: 'CONFLICT', key: 'error.conflict', params: { changed: 'tooFresh' }, detail: null, retryable: false };
    });
    setSessions([session(1, { pending: [tool('r1')] })]);
    const view = render(<CardStack uid={uid(1)} />);
    pass(c);
    fireEvent.click(view.getByText(translate('en', 'approval.allowOnce')).closest('button') as HTMLElement);
    await settle();
    assert(view.getByText(translate('en', 'guard.justChanged')) !== null, 'the guard line');
    assertEqual(answers(core).length, 1, 'no automatic retry');
  }),
);

// ---------------------------------------------------------------------------
// answer_in routing (F-15) and watched cards (M8)
// ---------------------------------------------------------------------------

domTest(
  'T-ui-answerInRouting: a mirrored card answers in the answer_in session, with "answered at the lead"',
  harness(async (c, core) => {
    const leadCopy = pending('sp_00aa', {
      kind: 'screenPrompt',
      promptRev: 'lead-rev',
      onBehalfOf: 'db-agent' as PendingInteraction['onBehalfOf'],
      options: [
        { key: '1', label: 'Yes', role: 'allowOnce' },
        { key: '2', label: 'No', role: 'deny' },
      ],
    });
    const mirror = { ...leadCopy, promptRev: 'stale-rev', answerIn: uid(9), onBehalfOf: null };
    setSessions([
      session(9, { caps: { ...session(9).caps, approve: 'screenKeys' }, pending: [leadCopy] }),
      session(2, { caps: { ...session(2).caps, approve: 'none' }, pending: [mirror] }),
    ]);
    const view = render(<CardStack uid={uid(2)} shortcuts />);
    pass(c);
    assert(view.getByText(translate('en', 'team.answeredAtLead')) !== null, 'labelled as answered at the lead');
    const card = view.container.querySelector('.g-card') as HTMLElement;
    card.focus();
    key(card, 'Digit2');
    await settle();
    const calls = core.calls('screen_prompt_key').map((x) => x.req as ScreenPromptKeyReq);
    assertDeepEqual(calls, [{ uid: uid(9), requestId: 'sp_00aa', promptRev: 'lead-rev', action: { kind: 'key', key: '2' } }], 'routed to the lead with its promptRev');
  }),
);

domTest(
  'Watched approvals offer only Go to {owner}; ⌘⏎ focuses the owner, never answers',
  harness(async (c, core) => {
    const s = session(3, {
      control: { kind: 'watching', origin: 'vsCode', owner: { appPid: 1, appName: 'VS Code', kind: 'vsCode', bundleId: 'com.microsoft.VSCode' } },
      caps: { ...session(3).caps, approve: 'goToOwner', ownerFocus: true },
      pending: [pending('w1', { kind: 'watchedApproval', title: 'Edit db/seed.ts' })],
    });
    setSessions([s]);
    const view = render(<CardStack uid={uid(3)} shortcuts />);
    pass(c);
    assertEqual(view.container.querySelectorAll('.g-card button').length, 1, 'one action');
    assert(view.getByText(translate('en', 'action.goToOwner', { owner: 'VS Code' })) !== null, 'Go to VS Code');
    key(document.body, 'Enter', { metaKey: true });
    await settle();
    assertEqual(core.calls('owner_focus').length, 1, '⌘⏎ = Go to owner');
    assertEqual(answers(core).length, 0, 'never answered from Gilde');
  }),
);

// ---------------------------------------------------------------------------
// Screen prompts, setup cards, questions, plans
// ---------------------------------------------------------------------------

domTest(
  'Screen prompt: digits send the key with the prompt digest; the last option says "· stops the turn"',
  harness(async (c, core) => {
    const p = pending('sp_1', {
      kind: 'screenPrompt',
      title: 'Do you want to proceed?',
      promptRev: 'rev1',
      options: [
        { key: '1', label: 'Yes', role: 'allowOnce' },
        { key: '2', label: 'No', role: 'deny' },
      ],
    });
    setSessions([session(4, { caps: { ...session(4).caps, approve: 'screenKeys' }, pending: [p] })]);
    const view = render(<CardStack uid={uid(4)} />);
    pass(c);
    assert(view.getAllByText(translate('en', 'screen.noStopsTurn'), { exact: false }).length > 0, 'deny label data [F-53]');
    const card = view.container.querySelector('.g-card') as HTMLElement;
    card.focus();
    key(card, 'Digit1');
    await settle();
    assertDeepEqual(core.calls('screen_prompt_key').map((x) => x.req), [{ uid: uid(4), requestId: 'sp_1', promptRev: 'rev1', action: { kind: 'key', key: '1' } }], 'key 1 with promptRev');
  }),
);

domTest(
  'Setup card: never answered by keys; one action, Continue in the terminal',
  harness(async (c, core) => {
    let shown = 0;
    const p = pending('sp_2', { kind: 'setupPrompt', title: 'Choose a theme', promptRev: 'rev2', defaultToNo: true });
    setSessions([session(5, { caps: { ...session(5).caps, approve: 'screenKeys' }, pending: [p] })]);
    const view = render(<CardStack uid={uid(5)} shortcuts onShowTerminal={() => shown++} />);
    pass(c);
    const card = view.container.querySelector('.g-card') as HTMLElement;
    card.focus();
    key(card, 'Digit1');
    key(card, 'Enter', { metaKey: true });
    await settle();
    assertEqual(core.calls('screen_prompt_key').length, 0, 'nothing is sent to a setup prompt');
    assertEqual(view.queryByText(translate('en', 'card.trustContinue')), null, 'no trust button without the marker');
    fireEvent.click(view.getByText(translate('en', 'card.continueInTerminal')));
    assertEqual(shown, 1, 'the terminal sub-view');
  }),
);

domTest(
  'Trust card (A6, A25.1): "Trust this folder and continue" for the trustFolder option sends TrustFolder; its key is never sent',
  harness(async (c, core) => {
    const p = pending('sp_3', {
      kind: 'setupPrompt',
      title: 'Quick safety check: Is this a project you created or one you trust?',
      promptRev: 'rev3',
      defaultToNo: true,
      options: [{ key: '2', label: 'Yes, I trust this folder', role: 'trustFolder' }],
    });
    setSessions([session(5, { caps: { ...session(5).caps, approve: 'screenKeys' }, pending: [p] })]);
    const view = render(<CardStack uid={uid(5)} shortcuts />);
    pass(c);
    const card = view.container.querySelector('.g-card') as HTMLElement;
    card.focus();
    key(card, 'Digit2');
    key(card, 'Enter', { metaKey: true });
    await settle();
    assertEqual(core.calls('screen_prompt_key').length, 0, 'neither the digit nor ⌘⏎ answers a setup card');
    const trust = view.getByText(translate('en', 'card.trustContinue')).closest('button') as HTMLButtonElement;
    assert(trust.classList.contains('g-cbtn-primary'), 'the primary action');
    const terminal = view.getByText(translate('en', 'card.continueInTerminal')).closest('button') as HTMLButtonElement;
    assert(!terminal.classList.contains('g-cbtn-primary'), 'Continue in the terminal is secondary');
    fireEvent.click(trust);
    await settle();
    assertDeepEqual(
      core.calls('screen_prompt_key').map((x) => x.req),
      [{ uid: uid(5), requestId: 'sp_3', promptRev: 'rev3', action: { kind: 'trustFolder' } }],
      'one TrustFolder action with the prompt digest',
    );
  }),
);

domTest(
  'Allow with a note (A25.1): offered for the allowWithNote option; it moves to the composer with the intent, option 1 stays the primary allow',
  harness(async (c, core) => {
    let replies = 0;
    const no = { key: '2', label: 'No', role: 'deny' as const };
    const options: PendingInteraction['options'] = [{ key: '1', label: 'Yes', role: 'allowWithNote' }, no];
    const p = pending('sp_4', { kind: 'screenPrompt', title: 'Do you want to proceed?', promptRev: 'rev4', defaultToNo: false, options });
    setSessions([session(6, { caps: { ...session(6).caps, approve: 'screenKeys' }, pending: [p] })]);
    const view = render(<CardStack uid={uid(6)} onReply={() => replies++} />);
    pass(c);
    assert(view.getByText(translate('en', 'state.needsApproval')) !== null, 'an approval');
    const one = view.container.querySelector('button[aria-keyshortcuts="1"]') as HTMLButtonElement;
    assert(one.classList.contains('g-cbtn-primary'), 'option 1 is the primary allow');
    fireEvent.click(view.getByText(translate('en', 'approval.allowWithNote')));
    await settle();
    assertEqual(replies, 1, 'focus moves to the composer');
    assertDeepEqual(replyIntentOf(uid(6)), { requestId: 'sp_4', kind: 'allowWithNote', target: uid(6) }, 'the composer sends AllowWithNote for this request');
    assertEqual(core.calls('screen_prompt_key').length, 0, 'nothing is sent until the note is typed');

    cleanup();
    resetReplyIntentsForTests();
    setSessions([session(6, { caps: { ...session(6).caps, approve: 'screenKeys' }, pending: [{ ...p, options: [{ key: '1', label: 'Yes', role: 'allowOnce' }, no] }] })]);
    const plain = render(<CardStack uid={uid(6)} onReply={() => replies++} />);
    pass(c);
    assertEqual(plain.queryByText(translate('en', 'approval.allowWithNote')), null, 'not offered without the role');
  }),
);

domTest(
  'Question: one single-select question answers on its digit; free text is never guessed',
  harness(async (c, core) => {
    const q = pending('q1', {
      kind: 'question',
      title: '',
      questions: [{ text: 'Apply to staging only?', options: ['Staging only', 'Both'], multiSelect: false, header: null, descriptions: [], previews: [] }],
    });
    setSessions([session(6, { pending: [q] })]);
    const view = render(<CardStack uid={uid(6)} />);
    pass(c);
    const card = view.container.querySelector('.g-card') as HTMLElement;
    card.focus();
    key(card, 'Digit2');
    await settle();
    assertDeepEqual(answers(core).map((a) => a.decision), [{ kind: 'answer', answers: [{ questionIndex: 0, labels: ['Both'], freeText: null }] }], 'answer by label');
  }),
);

domTest(
  'Question: duplicate question texts — only the first is answerable (F-37)',
  harness(async (c, core) => {
    const q = pending('q2', {
      kind: 'question',
      detailLines: [{ key: 'card.duplicateQuestion', params: {} }],
      questions: [
        { text: 'Same?', options: ['A', 'B'], multiSelect: false, header: null, descriptions: [], previews: [] },
        { text: 'Same?', options: ['C'], multiSelect: false, header: null, descriptions: [], previews: [] },
      ],
    });
    setSessions([session(7, { pending: [q] })]);
    const view = render(<CardStack uid={uid(7)} />);
    pass(c);
    assert(view.getByText(translate('en', 'card.duplicateQuestion')) !== null, 'the note');
    assertEqual(view.queryByText('C'), null, 'the duplicate offers no options');
    const card = view.container.querySelector('.g-card') as HTMLElement;
    card.focus();
    key(card, 'Digit1');
    await settle();
    assertDeepEqual(answers(core).map((a) => a.decision), [{ kind: 'answer', answers: [{ questionIndex: 0, labels: ['A'], freeText: null }] }], 'only the first');
  }),
);

domTest(
  'Question: chips and option descriptions show; an answer typed in "Other" goes with the picks of the other questions',
  harness(async (c, core) => {
    const q = pending('q3', {
      kind: 'question',
      questions: [
        {
          text: 'Where should the codex servers go?',
          header: 'app-server',
          options: ['Hide them', 'Own room'],
          descriptions: ['Not sessions: hidden from the Floor.', null],
          previews: [null, null],
          multiSelect: false,
        },
        { text: 'Which mode first?', header: null, options: ['Conversation'], descriptions: [], previews: [], multiSelect: false },
      ],
    });
    setSessions([session(9, { pending: [q] })]);
    const view = render(<CardStack uid={uid(9)} />);
    pass(c);
    assert(view.getByText('app-server') !== null, 'the chip');
    assert(view.getByText('Not sessions: hidden from the Floor.') !== null, 'the description');
    fireEvent.click(view.getByText('Own room'));
    const other = view.getByLabelText(translate('en', 'card.otherLabel', { index: 2 })) as HTMLInputElement;
    fireEvent.change(other, { target: { value: 'both modes, but Conversation first' } });
    fireEvent.keyDown(other, { key: 'Enter', code: 'Enter' });
    await settle();
    assertDeepEqual(
      answers(core).map((a) => a.decision),
      [
        {
          kind: 'answer',
          answers: [
            { questionIndex: 0, labels: ['Own room'], freeText: null },
            { questionIndex: 1, labels: [], freeText: 'both modes, but Conversation first' },
          ],
        },
      ],
      'the pick and the typed answer, in one map',
    );
  }),
);

domTest(
  'Plan: 1 approves; 3 moves to the composer (Keep planning carries a reply)',
  harness(async (c, core) => {
    let replied = 0;
    setSessions([session(8, { pending: [pending('p1', { kind: 'planApproval', title: '' })] })]);
    const view = render(<CardStack uid={uid(8)} onReply={() => replied++} />);
    pass(c);
    assert(view.getAllByText(translate('en', 'plan.ready')).length > 0, 'Plan ready');
    const card = view.container.querySelector('.g-card') as HTMLElement;
    card.focus();
    key(card, 'Digit3');
    await settle();
    assertEqual(replied, 1, '3 = reply');
    assertEqual(answers(core).length, 0, 'nothing sent without the reply');
    key(card, 'Digit1');
    await settle();
    assertDeepEqual(answers(core).map((a) => a.decision.kind), ['approvePlan'], '1 = approve');
  }),
);

// ---------------------------------------------------------------------------
// Copy (A19.7, ux-ia #73/#74)
// ---------------------------------------------------------------------------

domTest(
  'A19.7: detail lines render through i18n; vendor text only as card.detailText; unknown keys never show raw',
  harness(async () => {
    const p = tool('r1', {
      detailLines: [
        { key: 'card.detailText', params: { text: 'npm run migrate -- --name x' } },
        { key: 'card.aSubagent', params: {} },
        { key: 'not.a.key', params: {} },
      ],
    });
    setSessions([session(1, { pending: [p] })]);
    const view = render(<CardStack uid={uid(1)} />);
    assert(view.getByText('npm run migrate -- --name x') !== null, 'vendor text verbatim');
    assert(view.getByText(translate('en', 'card.aSubagent')) !== null, 'a key renders its copy');
    assertEqual(view.queryByText('not.a.key'), null, 'an unknown key is never shown raw');
  }),
);

domTest(
  'Titles: an empty title renders the generic sentence per verb; degraded cards say so',
  harness(async () => {
    setSessions([
      session(1, { details: { ...session(1).details, location: '/x/services/api' }, pending: [tool('r1', { title: '' })] }),
      session(2, { pending: [tool('r2', { title: '', degraded: true })] }),
    ]);
    const a = render(<CardStack uid={uid(1)} />);
    assert(a.getByText(translate('en', 'approval.command', { agent: 'claude', dir: 'services/api' })) !== null, 'run → command sentence');
    a.unmount();
    const b = render(<CardStack uid={uid(2)} />);
    assert(b.getByText(translate('en', 'card.degraded', { agent: 'claude' })) !== null, 'degraded sentence');
  }),
);

domTest(
  'Row variant (Home, Board): the top card only, with the stack position',
  harness(async () => {
    setSessions([session(1, { pending: [tool('r1'), tool('r2')] })]);
    const view = render(<CardStack uid={uid(1)} variant="row" />);
    assertEqual(view.container.querySelectorAll('.g-card').length, 1, 'one card');
    assert(view.getByText('1 of 2') !== null, '"1 of 2"');
    assertEqual(view.queryByText(translate('en', 'approval.denyWhy')), null, 'no reply control without a host composer');
  }),
);

// ---------------------------------------------------------------------------
// Home's focus card (A23.5, T14's shape)
// ---------------------------------------------------------------------------

domTest(
  'FocusCard: autoFocus lands on Deny for default_to_no; ⌘⏎ inert there; answers go to answerUid',
  harness(async (c, core) => {
    const p = tool('r1', { defaultToNo: true });
    setSessions([session(1, { pending: [p] }), session(9)]);
    const s = appStore.getState().world.sessions.get(uid(1)) as Session;
    const view = render(<FocusCard session={s} interaction={p} answerUid={uid(9)} autoFocus />);
    await act(async () => {
      await new Promise((r) => setTimeout(r, 30));
    });
    assertEqual(document.activeElement?.getAttribute('data-role'), 'deny', 'focus on Deny');
    pass(c);
    key(document.body, 'Enter', { metaKey: true });
    await settle();
    assertEqual(answers(core).length, 0, '⌘⏎ inert on default_to_no');
    key(document.activeElement as Element, 'Digit3');
    await settle();
    assertDeepEqual(answers(core).map((a) => [a.uid, a.decision.kind]), [[uid(9), 'deny']], 'routed to answerUid');
    assertEqual(view.container.querySelectorAll('.g-card[data-variant="row"]').length, 1, 'compact');
  }),
);

// ---------------------------------------------------------------------------
// A44 — one shortcut decision (A44.2), guards keyed on the request (A44.1)
// ---------------------------------------------------------------------------

test('A44.2 answerShortcut: every surface sends only what shortcutAction decides', async () => {
  const sent: unknown[] = [];
  await withFakeCore(
    {
      interaction_answer: (req) => {
        sent.push(req);
        return answered;
      },
      screen_prompt_key: (req) => {
        sent.push(req);
        return { outcome: 'sent' };
      },
    },
    async () => {
      resetAnswersForTests();
      const options: PendingInteraction['options'] = [
        { key: '1', label: 'Yes', role: 'allowOnce' },
        { key: '2', label: 'No', role: 'deny' },
      ];
      const likely = pending('sp_l', { kind: 'screenPrompt', promptRev: 'r', confidence: 'likely', options });
      assertDeepEqual(answerShortcut(uid(1), likely, 'deny'), { kind: 'inert', line: 'approval.useOwnKeys' }, 'likely: ⌘⌫ inert');
      assertDeepEqual(answerShortcut(uid(1), likely, 'primary'), { kind: 'inert', line: 'approval.useOwnKeys' }, 'likely: ⌘⏎ inert');
      const headerless = pending('sp_h', { kind: 'screenPrompt', promptRev: 'r', defaultToNo: true, options });
      assertDeepEqual(answerShortcut(uid(1), headerless, 'primary'), { kind: 'inert', line: 'approval.useOwnKeys' }, 'A35.2 default_to_no screen card: inert');
      const careful = tool('r_no', { defaultToNo: true });
      assertDeepEqual(answerShortcut(uid(1), careful, 'primary'), { kind: 'inert', line: 'approval.defaultNo' }, 'default_to_no: ⌘⏎ inert');
      const question = pending('q', { kind: 'question', questions: [{ text: 'Which?', options: ['a'], multiSelect: false, header: null, descriptions: [], previews: [] }] });
      assertEqual(answerShortcut(uid(1), question, 'primary').kind, 'open', 'a question needs its card');
      assertEqual(answerShortcut(uid(1), pending('p', { kind: 'planApproval' }), 'deny').kind, 'open', "a plan's reply needs its card");
      assertEqual(answerShortcut(uid(1), pending('w', { kind: 'watchedApproval' }), 'primary').kind, 'inert', 'a watched card is never answered');
      assertEqual(sent.length, 0, 'nothing sent so far');
      const run = answerShortcut(uid(1), careful, 'deny');
      assert(run.kind === 'sent', 'default_to_no: ⌘⌫ still denies');
      if (run.kind === 'sent') await run.outcome;
      const high = answerShortcut(uid(2), pending('sp_ok', { kind: 'screenPrompt', promptRev: 'r9', options }), 'deny');
      if (high.kind === 'sent') await high.outcome;
      assertDeepEqual(
        sent,
        [
          { uid: uid(1), requestId: 'r_no', decision: { kind: 'deny' }, via: 'shortcut' },
          { uid: uid(2), requestId: 'sp_ok', promptRev: 'r9', action: { kind: 'key', key: '2' } },
        ],
        'deny, then the High prompt\'s last key',
      );
    },
  );
});

/** The shell's keys (⌘J) and the screen the main view shows, as the app mounts them. */
function JumpHost() {
  useShellKeys({ toggleSidebar: () => {}, overlayOpen: false, closeOverlay: () => {} });
  const view = useAppStore((s) => s.ui.view);
  return view.screen === 'session' ? <SessionScreen uid={view.uid} sub="conversation" /> : null;
}

domTest(
  'A44.1: after ⌘J, a stacked request that replaces the answered one starts a new 700 ms guard',
  async () => {
    const c = manualClock(2_000_000);
    setClockForTests(c);
    reset();
    try {
      await withFakeCore(
        {
          interaction_answer: () => answered,
          convo_subscribe: () => ({ subId: 'v_00000000000000c1', source: 'stream', note: null }),
          convo_unsubscribe: () => ({}),
          session_mark_seen: () => ({}),
          app_view_state: () => ({}),
        },
        async (core) => {
          const two = [tool('r_a', { title: 'Run npm test' }), tool('r_b', { title: 'Run rm -rf build' })];
          const s1 = session(1, { activity: { state: { kind: 'needsApproval' }, since: 0, confidence: 'high', evidence: null, episode: 'e_1' as never }, pending: two });
          const queue = (r: number) => ({ rev: r, items: [queueItem(1, 1)], needYou: 1, also: 0, pausedUntil: null });
          act(() => appStore.getState().applyStoreSnapshot(snapshot(++rev, { sessions: [s1], queue: queue(rev) })));
          render(<JumpHost />);
          key(document.body, 'KeyJ', { metaKey: true, key: 'j' });
          await settle();
          const v = appStore.getState().ui.view;
          assertEqual(v.screen === 'session' ? v.uid : null, uid(1), '⌘J opened the session');
          key(document.body, 'Enter', { metaKey: true });
          await settle();
          assertEqual(answers(core).length, 0, 'the ⌘J arrival is guarded');
          pass(c, GUARD_MS + 10);
          key(document.body, 'Enter', { metaKey: true });
          await settle();
          const ids = () => answers(core).map((a) => a.requestId);
          assertDeepEqual(ids(), ['r_a'], '⌘⏎ answers the top card');
          act(() => appStore.getState().applyStoreSnapshot(snapshot(++rev, { sessions: [{ ...s1, pending: [two[1] as PendingInteraction] }], queue: queue(rev) })));
          pass(c, 150);
          key(document.body, 'Enter', { metaKey: true });
          await settle();
          assertDeepEqual(ids(), ['r_a'], 'r_b took the top 150 ms ago: a second ⌘⏎ answers nothing');
          pass(c, GUARD_MS);
          key(document.body, 'Enter', { metaKey: true });
          await settle();
          assertDeepEqual(ids(), ['r_a', 'r_b'], 'after its own 700 ms, r_b answers');
        },
      );
    } finally {
      cleanup();
      setClockForTests(null);
      reset();
    }
  },
);
