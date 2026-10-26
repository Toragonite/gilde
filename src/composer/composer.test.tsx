/**
 * Composer tests (contract §6.1.3, §8.6, §15.5 T15, §15.6 T14–T16):
 * T-ui-composerByKind end to end, the armed rule (F-38), IME, ⌥⏎ send-now,
 * queued chips with Edit/Cancel, the stop feedback and "Deny and stop"
 * without a message field; A44.3: the reply intent a card records on this
 * composer is sent exactly as chosen, and a missing or stale one is never a
 * Deny.
 */

import { act, cleanup, fireEvent, render } from '@testing-library/react';
import { useRef } from 'react';

import { resetAnswersForTests } from '../approval/answer';
import { CardStack } from '../approval/CardStack';
import { replyIntentOf, resetReplyIntentsForTests, setReplyIntent } from '../approval/replyIntent';
import { refreshGuards, resetAnswerGuardForTests } from '../hooks/useAnswerGuard';
import { resetHotkeysForTests } from '../hooks/useHotkeys';
import { translate } from '../i18n';
import { manualClock, setClockForTests } from '../lib/clock';
import type { ManualClock } from '../lib/clock';
import type { ClientMsgId } from '../lib/gen/ClientMsgId';
import type { InteractionAnswerReq } from '../lib/gen/InteractionAnswerReq';
import type { PendingInteraction } from '../lib/gen/PendingInteraction';
import type { Session } from '../lib/gen/Session';
import type { TurnSendReq } from '../lib/gen/TurnSendReq';
import { appStore } from '../store/useAppStore';
import { initialUiState } from '../store/uiSlice';
import { emptyWorld } from '../store/worldSlice';
import { flush, withFakeCore } from '../test/backend';
import type { FakeCore } from '../test/backend';
import { pending, session, snapshot, uid } from '../test/fixtures';
import { assert, assertDeepEqual, assertEqual, domTest, test } from '../test/harness';
import { Composer, STILL_STOPPING_MS } from './Composer';
import { composerIntent } from './send';

let rev = 100;

function setSessions(sessions: Session[]) {
  act(() => {
    appStore.getState().applyStoreSnapshot(snapshot(++rev, { sessions }));
  });
}

function reset() {
  resetHotkeysForTests();
  resetAnswerGuardForTests();
  resetAnswersForTests();
  resetReplyIntentsForTests();
  appStore.setState({ world: emptyWorld(), ui: { ...initialUiState(), locale: 'en' } });
}

function harness(run: (c: ManualClock, core: FakeCore) => Promise<void>) {
  return async () => {
    const c = manualClock(5_000_000);
    setClockForTests(c);
    reset();
    try {
      await withFakeCore(
        {
          turn_send: () => ({ delivery: 'sent', position: null }),
          interaction_answer: () => ({ outcome: 'answered', previous: null }),
          screen_prompt_key: () => ({ outcome: 'sent' }),
          turn_interrupt: () => ({ outcome: 'interrupting' }),
          turn_force_stop: () => ({ stopped: true }),
          turn_cancel_queued: () => ({ cancelled: true }),
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

async function settle() {
  await act(async () => {
    await flush();
  });
}

function input(view: ReturnType<typeof render>): HTMLTextAreaElement {
  return view.getByLabelText(translate('en', 'composer.placeholder', { agent: 'claude' })) as HTMLTextAreaElement;
}

function type(el: HTMLTextAreaElement, value: string) {
  fireEvent.change(el, { target: { value } });
}

function enter(el: HTMLElement, extra: Record<string, unknown> = {}) {
  fireEvent.keyDown(el, { key: 'Enter', code: 'Enter', ...extra });
}

const sends = (core: FakeCore) => core.calls('turn_send').map((x) => x.req as TurnSendReq);
const answers = (core: FakeCore) => core.calls('interaction_answer').map((x) => x.req as InteractionAnswerReq);

/** Render with a card that has been on screen for `ms` before typing starts. */
async function armedWith(c: ManualClock, card: PendingInteraction, extra: Partial<Session> = {}) {
  setSessions([session(1, { pending: [card], ...extra })]);
  const view = render(<Composer uid={uid(1)} />);
  act(() => c.advance(800));
  // The arming timer re-renders after 700 real ms; here the manual clock moved.
  view.rerender(<Composer uid={uid(1)} />);
  return view;
}

domTest(
  'No card: ⏎ sends a message (priority next); ⇧⏎ never sends; the draft clears',
  harness(async (_c, core) => {
    setSessions([session(1)]);
    const view = render(<Composer uid={uid(1)} />);
    const el = input(view);
    type(el, 'hello');
    enter(el, { shiftKey: true });
    await settle();
    assertEqual(sends(core).length, 0, '⇧⏎ is a newline');
    enter(el);
    await settle();
    assertEqual(sends(core).length, 1, 'sent once');
    assertEqual(sends(core)[0]?.text, 'hello', 'the text');
    assertEqual(sends(core)[0]?.priority, 'next', 'priority next');
    assertEqual(el.value, '', 'draft cleared');
  }),
);

domTest(
  'T-ui-ime: ⏎ that finishes a Korean syllable never sends; a held ⏎ sends once',
  harness(async (_c, core) => {
    setSessions([session(1)]);
    const view = render(<Composer uid={uid(1)} />);
    const el = input(view);
    type(el, '안녕');
    enter(el, { isComposing: true });
    enter(el, { keyCode: 229 });
    enter(el, { repeat: true });
    await settle();
    assertEqual(sends(core).length, 0, 'composition and repeat never send');
  }),
);

domTest(
  '⌥⏎ sends now in Conversation mode; the hint says it may restart the reply',
  harness(async (_c, core) => {
    setSessions([session(1, { activity: { ...session(1).activity, state: { kind: 'working' } } })]);
    const view = render(<Composer uid={uid(1)} />);
    const el = input(view);
    type(el, 'stop and use pnpm');
    assert(view.getByText(translate('en', 'composer.sendNowRestarts')) !== null, 'the send-now hint');
    enter(el, { altKey: true });
    await settle();
    assertEqual(sends(core)[0]?.priority, 'now', 'priority now');
  }),
);

domTest(
  'T-ui-composerByKind: tool approval → DenyWithMessage, never also a user message [M7]',
  harness(async (c, core) => {
    const view = await armedWith(c, pending('r1'));
    assert(view.getByText(translate('en', 'approval.composerHint', { agent: 'claude' })) !== null, 'armed hint');
    const el = input(view);
    type(el, 'use the staging database');
    enter(el);
    await settle();
    assertDeepEqual(answers(core), [{ uid: uid(1), requestId: 'r1', decision: { kind: 'denyWithMessage', message: 'use the staging database' }, via: 'shortcut' }], 'deny with the reply');
    assertEqual(sends(core).length, 0, 'not also sent as a message');
  }),
);

domTest(
  'F-38: a draft begun within 700 ms of the card is sent as a message and the card stays',
  harness(async (c, core) => {
    setSessions([session(1, { pending: [pending('r1')] })]);
    const view = render(<Composer uid={uid(1)} />);
    act(() => c.advance(300));
    const el = input(view);
    type(el, 'already typing');
    assertEqual(view.queryByText(translate('en', 'approval.composerHint', { agent: 'claude' })), null, 'no hint: not armed');
    act(() => c.advance(2000));
    enter(el);
    await settle();
    assertEqual(answers(core).length, 0, 'no answer');
    assertEqual(sends(core).length, 1, 'an ordinary message');
  }),
);

domTest(
  'T-ui-composerByKind: question → Answer with free text (one question); several → focus the card',
  harness(async (c, core) => {
    const one = pending('q1', { kind: 'question', questions: [{ text: 'Which?', options: ['A', 'B'], multiSelect: false }] });
    const view = await armedWith(c, one);
    assert(view.getByText(translate('en', 'approval.composerAnswerHint', { agent: 'claude' })) !== null, 'answer hint');
    const el = input(view);
    type(el, 'neither, use C');
    enter(el);
    await settle();
    assertDeepEqual(answers(core).map((a) => a.decision), [{ kind: 'answer', answers: [{ questionIndex: 0, labels: [], freeText: 'neither, use C' }] }], 'free answer');
    cleanup();

    let focused = 0;
    const two = pending('q2', {
      kind: 'question',
      questions: [
        { text: 'Which?', options: ['A'], multiSelect: false },
        { text: 'And?', options: ['B'], multiSelect: false },
      ],
    });
    setSessions([session(2, { pending: [two] })]);
    const v2 = render(<Composer uid={uid(2)} onFocusCard={() => focused++} />);
    act(() => c.advance(800));
    const el2 = input(v2);
    type(el2, 'partial');
    enter(el2);
    await settle();
    assertEqual(focused, 1, 'focus moves to the card');
    assertEqual(answers(core).length, 1, 'no partial answer map is sent [N-12]');
    assertEqual(el2.value, 'partial', 'the draft stays');
  }),
);

domTest(
  'T-ui-composerByKind: plan → KeepPlanning; screen prompt → DenyWithReply; setup → nothing, draft kept',
  harness(async (c, core) => {
    const plan = await armedWith(c, pending('p1', { kind: 'planApproval' }));
    assert(plan.getByText(translate('en', 'plan.composerHint', { agent: 'claude' })) !== null, 'plan hint');
    type(input(plan), 'split step 2');
    enter(input(plan));
    await settle();
    assertDeepEqual(answers(core).map((a) => a.decision), [{ kind: 'keepPlanning', message: 'split step 2' }], 'keep planning');
    cleanup();

    const screen = await armedWith(c, pending('sp_1', { kind: 'screenPrompt', promptRev: 'rev' }), { caps: { ...session(1).caps, approve: 'screenKeys', send: 'ptyPaste' } });
    type(input(screen), 'not that file');
    enter(input(screen));
    await settle();
    assertDeepEqual(core.calls('screen_prompt_key').map((x) => x.req), [{ uid: uid(1), requestId: 'sp_1', promptRev: 'rev', action: { kind: 'denyWithReply', text: 'not that file' } }], 'deny with reply');
    cleanup();

    const setup = await armedWith(c, pending('sp_2', { kind: 'setupPrompt', promptRev: 'rev2' }));
    type(input(setup), 'hello');
    enter(input(setup));
    await settle();
    assertEqual(core.calls('screen_prompt_key').length, 1, 'nothing sent to a setup prompt');
    assertEqual(sends(core).length, 0, 'and no message');
    assertEqual(input(setup).value, 'hello', 'draft kept');
    assert(setup.getByText(translate('en', 'composer.keptDraft')) !== null, 'the kept line');
  }),
);

domTest(
  'Allow with a note: the card\'s intent turns ⏎ into AllowWithNote for that request only',
  harness(async (c, core) => {
    const view = await armedWith(c, pending('sp_3', { kind: 'screenPrompt', promptRev: 'r3' }), { caps: { ...session(1).caps, approve: 'screenKeys' } });
    act(() => setReplyIntent(uid(1), { requestId: 'sp_3' as PendingInteraction['requestId'], kind: 'allowWithNote', target: uid(1) }));
    assert(view.getByText(translate('en', 'approval.noteHint', { agent: 'claude' })) !== null, 'note hint');
    type(input(view), 'and add a test');
    enter(input(view));
    await settle();
    assertDeepEqual(core.calls('screen_prompt_key').map((x) => (x.req as { action: unknown }).action), [{ kind: 'allowWithNote', text: 'and add a test' }], 'allow with note');
  }),
);

domTest(
  'Deny and stop: offered while a tool approval is armed; it carries no message and keeps the draft',
  harness(async (c, core) => {
    const view = await armedWith(c, pending('r9'));
    type(input(view), 'my draft');
    fireEvent.click(view.getByText(translate('en', 'composer.denyAndStop')));
    await settle();
    assertDeepEqual(answers(core).map((a) => a.decision), [{ kind: 'denyAndStop' }], 'deny and stop, no message');
    assertEqual(input(view).value, 'my draft', 'the draft is not consumed');
  }),
);

domTest(
  'Queued messages show with Edit and Cancel; Edit puts the text back in the draft',
  harness(async (_c, core) => {
    const q = { clientMsgId: 'c-1' as ClientMsgId, text: 'then run the linter', queuedAt: 0 };
    setSessions([session(1, { queued: [q], activity: { ...session(1).activity, state: { kind: 'working' } } })]);
    const view = render(<Composer uid={uid(1)} />);
    assert(view.getByText('then run the linter') !== null, 'the queued text');
    fireEvent.click(view.getByText(translate('en', 'composer.edit')));
    await settle();
    assertDeepEqual(core.calls('turn_cancel_queued').map((x) => x.req), [{ uid: uid(1), clientMsgId: 'c-1' }], 'cancelled first');
    assertEqual(input(view).value, 'then run the linter', 'back in the draft');
  }),
);

domTest(
  '"Queued…" holds while its message waits and goes once the agent read it, with nothing typed',
  harness(async (_c, core) => {
    core.on('turn_send', () => ({ delivery: 'queued', position: 1 }));
    const working = { ...session(1).activity, state: { kind: 'working' as const } };
    setSessions([session(1, { activity: working })]);
    const view = render(<Composer uid={uid(1)} />);
    const el = input(view);
    type(el, 'then run the linter');
    enter(el);
    await settle();
    const queuedLine = translate('en', 'composer.queued', { agent: 'claude' });
    const id = sends(core)[0]?.clientMsgId as ClientMsgId;
    setSessions([session(1, { activity: working, queued: [{ clientMsgId: id, text: 'then run the linter', queuedAt: 0 }] })]);
    assert(view.queryByText(queuedLine) !== null, 'shown while the message waits');
    setSessions([session(1, { activity: working, queued: [] })]);
    assertEqual(view.queryByText(queuedLine), null, 'gone once it left the queue');
    assertEqual(el.value, '', 'with nothing typed');
  }),
);

domTest(
  '⌘. stops: "Stopping…", then after 5 s "Still stopping…" with Force stop',
  harness(async (c, core) => {
    setSessions([session(1, { activity: { ...session(1).activity, state: { kind: 'working' } } })]);
    const view = render(<Composer uid={uid(1)} />);
    fireEvent.keyDown(input(view), { key: '.', code: 'Period', metaKey: true });
    await settle();
    assertEqual(core.calls('turn_interrupt').length, 1, 'interrupt sent');
    assert(view.getByText(translate('en', 'composer.stopping')) !== null, 'Stopping…');
    await act(async () => {
      c.advance(STILL_STOPPING_MS);
      await new Promise((r) => setTimeout(r, 300));
    });
    assert(view.getByText(translate('en', 'composer.stillStopping')) !== null, 'Still stopping…');
    fireEvent.click(view.getByText(translate('en', 'action.forceStop')));
    await settle();
    assertEqual(core.calls('turn_force_stop').length, 1, 'force stop');
  }),
);

// -- A33.3: an unknown permission mode ------------------------------------------

function modeChip(view: ReturnType<typeof render>, locale: 'en' | 'ko'): HTMLElement | null {
  const label = translate(locale, 'session.permission.label');
  return view.container.querySelector<HTMLElement>(`[aria-label^="${label}: "]`);
}

domTest(
  'A33.3: a running claude Conversation session with an unknown mode shows "Unknown mode", read-only or with its menu',
  harness(async () => {
    const running = { model: 'claude-haiku-5-5', permissionMode: null } as const;
    setSessions([session(1, running)]);
    let view = render(<Composer uid={uid(1)} />);
    let chip = modeChip(view, 'en');
    assertEqual(chip?.textContent?.trim(), translate('en', 'composer.modeUnknown'), 'read-only chip says Unknown mode');
    assertEqual(chip?.getAttribute('aria-label'), `${translate('en', 'session.permission.label')}: ${translate('en', 'composer.modeUnknown')}`, 'named');
    view.unmount();
    // With modes to choose: the button still says Unknown mode, the menu offers the modes.
    setSessions([session(1, { ...running, caps: { ...session(1).caps, setPermissionMode: ['default', 'plan'] } })]);
    view = render(<Composer uid={uid(1)} />);
    chip = modeChip(view, 'en');
    assert((chip?.textContent ?? '').startsWith(translate('en', 'composer.modeUnknown')), 'the menu button says Unknown mode');
    fireEvent.click(chip as HTMLElement);
    assert(view.getByText(translate('en', 'session.permission.plan')) !== null, 'the modes are offered');
    view.unmount();
    // Korean.
    act(() => appStore.setState({ ui: { ...appStore.getState().ui, locale: 'ko' } }));
    setSessions([session(1, running)]);
    view = render(<Composer uid={uid(1)} />);
    assertEqual(modeChip(view, 'ko')?.textContent?.trim(), '알 수 없는 모드', 'the Korean label');
  }),
);

domTest(
  'A33.3: the unknown label is only for a running claude Conversation session that has reported',
  harness(async () => {
    // A known mode shows that mode.
    setSessions([session(1, { model: 'claude-haiku-5-5', permissionMode: 'plan' })]);
    let view = render(<Composer uid={uid(1)} />);
    assertEqual(modeChip(view, 'en')?.textContent?.trim(), translate('en', 'session.permission.plan'), 'a known mode');
    view.unmount();
    // Before the CLI reported (no model yet) and nothing to choose: no chip.
    setSessions([session(1, { model: null, permissionMode: null })]);
    view = render(<Composer uid={uid(1)} />);
    assertEqual(modeChip(view, 'en'), null, 'no chip before the CLI reported');
    view.unmount();
    // A Terminal session and a codex session with no mode: never "Unknown mode".
    const terminal = { kind: 'inGilde', mode: 'terminal', pty: null, hostedTeam: false, transport: 'pty' } as Session['control'];
    for (const extra of [{ control: terminal }, { vendor: 'codex' as const, control: { ...session(1).control, mode: 'turnBased' as const } }]) {
      setSessions([session(1, { model: 'm', permissionMode: null, ...extra })]);
      view = render(<Composer uid={uid(1)} />);
      const text = modeChip(view, 'en')?.textContent ?? '';
      assert(!text.includes(translate('en', 'composer.modeUnknown')), `no Unknown mode for ${JSON.stringify(extra)}`);
      view.unmount();
    }
  }),
);

// ---------------------------------------------------------------------------
// A44.3 — a reply intent belongs to the composer that opens
// ---------------------------------------------------------------------------

const noteOptions: PendingInteraction['options'] = [
  { key: '1', label: 'Yes', role: 'allowWithNote' },
  { key: '2', label: 'No', role: 'deny' },
];

test('A44.3 composerIntent: an intent is sent exactly as chosen; without a matching one a mirrored card is never denied', () => {
  const rid = (s: string) => s as PendingInteraction['requestId'];
  const own = pending('sp_1', { kind: 'screenPrompt', promptRev: 'own-rev', options: noteOptions });
  const mirror = { ...own, promptRev: 'stale-rev', answerIn: uid(9) };
  const leadCopy = { ...own, promptRev: 'lead-rev' };
  const armed = [0, 1000, 1000] as const; // shown at 0, the draft began at 1000, now 1000
  const at = (top: PendingInteraction, intent: Parameters<typeof composerIntent>[2], host = uid(8), live: PendingInteraction | null = top) =>
    composerIntent(host, top, intent, ...armed, live);

  assertDeepEqual(at(own, null), { kind: 'screenDenyWithReply', requestId: 'sp_1', promptRev: 'own-rev' }, "the composer's own card keeps F-11");
  assertDeepEqual(at(mirror, null, uid(8), leadCopy), { kind: 'send' }, 'mirrored, no intent: an ordinary message, never a Deny');
  assertDeepEqual(
    at(mirror, { requestId: rid('sp_1'), kind: 'allowWithNote', target: uid(9) }, uid(8), leadCopy),
    { kind: 'allowWithNote', requestId: 'sp_1', promptRev: 'lead-rev' },
    "mirrored, Allow with a note: allowWithNote with the lead's promptRev",
  );
  assertDeepEqual(
    at(mirror, { requestId: rid('sp_1'), kind: 'denyWithReply', target: uid(9) }, uid(8), leadCopy),
    { kind: 'screenDenyWithReply', requestId: 'sp_1', promptRev: 'lead-rev' },
    'mirrored, Deny and say why: denyWithReply',
  );
  assertDeepEqual(at(mirror, { requestId: rid('sp_1'), kind: 'allowWithNote', target: uid(8) }, uid(8), leadCopy), { kind: 'send' }, 'an intent naming the wrong target is no answer');
  assertDeepEqual(at(own, { requestId: rid('sp_old'), kind: 'allowWithNote', target: uid(8) }), { kind: 'send' }, "a stale intent never becomes the new card's Deny");
  const approvalCard = pending('r_1');
  assertDeepEqual(at(approvalCard, { requestId: rid('r_1'), kind: 'denyWithReply', target: uid(8) }), { kind: 'denyWithMessage', requestId: 'r_1' }, 'a tool approval: DenyWithMessage');
  assertDeepEqual(at(approvalCard, { requestId: rid('r_1'), kind: 'allowWithNote', target: uid(8) }), { kind: 'send' }, 'a note on a card that takes none is no Deny');
  assertDeepEqual(
    composerIntent(uid(8), own, { requestId: rid('sp_1'), kind: 'allowWithNote', target: uid(8) }, 500, 1000, 1000),
    { kind: 'send' },
    'not armed (F-38): an ordinary message, whatever the intent',
  );
});

/** A host as the Session screen and tiles build it: the stack's reply focuses this composer. */
function CardAndComposer() {
  const ref = useRef<HTMLTextAreaElement | null>(null);
  return (
    <>
      <CardStack uid={uid(1)} onReply={() => ref.current?.focus()} />
      <Composer uid={uid(1)} inputRef={ref} />
    </>
  );
}

domTest(
  'A44.3: "Allow with a note" on a normal card sends allowWithNote to its own session, once',
  harness(async (c, core) => {
    const card = pending('sp_5', { kind: 'screenPrompt', promptRev: 'r5', options: noteOptions });
    setSessions([session(1, { caps: { ...session(1).caps, approve: 'screenKeys', send: 'ptyPaste' }, pending: [card] })]);
    const view = render(<CardAndComposer />);
    act(() => {
      c.advance(800);
      refreshGuards();
    });
    fireEvent.click(view.container.querySelector('[data-action="allowWithNote"]') as HTMLElement);
    await settle();
    assertEqual(document.activeElement, input(view), 'focus moved to the composer');
    assert(view.getByText(translate('en', 'approval.noteHint', { agent: 'claude' })) !== null, 'the note hint');
    type(input(view), 'and add a test');
    enter(input(view));
    await settle();
    assertDeepEqual(
      core.calls('screen_prompt_key').map((x) => x.req),
      [{ uid: uid(1), requestId: 'sp_5', promptRev: 'r5', action: { kind: 'allowWithNote', text: 'and add a test' } }],
      'allowWithNote, exactly as chosen',
    );
    assertEqual(replyIntentOf(uid(1)), null, 'the intent is spent');
  }),
);

domTest(
  'A44.3: a note typed for a request that is replaced before ⏎ is never sent as an answer to the new one',
  harness(async (c, core) => {
    const caps = { ...session(1).caps, approve: 'screenKeys' as const, send: 'ptyPaste' as const };
    const first = pending('sp_6', { kind: 'screenPrompt', promptRev: 'r6', options: noteOptions });
    setSessions([session(1, { caps, pending: [first] })]);
    const view = render(<CardAndComposer />);
    act(() => {
      c.advance(800);
      refreshGuards();
    });
    fireEvent.click(view.container.querySelector('[data-action="allowWithNote"]') as HTMLElement);
    await settle();
    type(input(view), 'only the docs folder');
    setSessions([session(1, { caps, pending: [{ ...first, requestId: 'sp_7' as PendingInteraction['requestId'], promptRev: 'r7' }] })]);
    await settle();
    assertEqual(replyIntentOf(uid(1)), null, 'the intent ended with its request');
    act(() => c.advance(800));
    enter(input(view));
    await settle();
    assertDeepEqual(core.calls('screen_prompt_key').map((x) => x.req), [], 'no answer of any kind to sp_7');
    assertDeepEqual(sends(core).map((s) => s.text), ['only the docs folder'], "the composer's normal send");
  }),
);
