/**
 * ⌘⏎ / ⌘⌫ with focus in the sidebar (A44.2, contract §8.6): the keys never
 * answer a request other than the one the focused row names. On the row of
 * the session in front they reach that session's card, which decides them
 * through `shortcutAction` (inert on a screen prompt that is not High
 * confidence) behind its own guard; anywhere else in the sidebar they are
 * inert.
 */

import { act, render } from '@testing-library/react';

import { CardStack } from '../approval/CardStack';
import { resetAnswersForTests } from '../approval/answer';
import { GUARD_MS } from '../hooks/useAnswerGuard';
import type { PendingInteraction } from '../lib/gen/PendingInteraction';
import type { ScreenPromptKeyReq } from '../lib/gen/ScreenPromptKeyReq';
import type { Session } from '../lib/gen/Session';
import { shellLayout } from '../shell/layout';
import { press, scenario, seed, step } from '../shell/testing';
import { withFakeCore } from '../test/backend';
import { pending, queueItem, session, snapshot, uid } from '../test/fixtures';
import { assertDeepEqual, assertEqual, domTest } from '../test/harness';
import { Sidebar } from './Sidebar';

const terminal = { kind: 'inGilde' as const, mode: 'terminal' as const, pty: null, hostedTeam: false, transport: 'pty' as const };
const needsApproval = { state: { kind: 'needsApproval' as const }, since: 0, evidence: null };

function prompt(requestId: string, confidence: 'high' | 'likely'): PendingInteraction {
  return pending(requestId, {
    kind: 'screenPrompt',
    promptRev: `rev-${requestId}`,
    confidence,
    options: [
      { key: '1', label: 'Yes', role: 'allowOnce' },
      { key: '2', label: "Yes, and don't ask again", role: 'allowSession' },
      { key: '3', label: 'No', role: 'deny' },
    ],
  });
}

/** Session 2 is open (its card stack has the shortcuts); session 3 waits elsewhere. */
function world(rev: number, front: PendingInteraction) {
  const s2: Session = session(2, {
    title: 'front',
    control: terminal,
    caps: { ...session(2).caps, send: 'ptyPaste', approve: 'screenKeys' },
    activity: { ...needsApproval, confidence: front.confidence, episode: 'e_2' as never },
    pending: [front],
  });
  const s3: Session = session(3, {
    title: 'other',
    activity: { ...needsApproval, confidence: 'high', episode: 'e_3' as never },
    pending: [pending('r3', { title: 'Run rm -rf build' })],
  });
  seed(
    snapshot(rev, {
      sessions: [s2, s3],
      queue: { rev, items: [queueItem(2, 1), queueItem(3, 1)], needYou: 2, also: 0, pausedUntil: null },
    }),
  );
}

/** The sidebar beside the Session screen's card stack, as the shell lays them out. */
function Front() {
  return (
    <>
      <Sidebar layout={shellLayout(1440, 1, 'session', true, false)} />
      <CardStack uid={uid(2)} shortcuts />
    </>
  );
}

function row(container: HTMLElement, n: number): HTMLElement {
  const el = container.querySelector<HTMLElement>(`.sb-qlist [data-split-uid="${uid(n)}"]`);
  if (!el) throw new Error(`no Needs-you row for ${n}`);
  return el;
}

const handlers = {
  screen_prompt_key: () => ({ outcome: 'sent' }),
  interaction_answer: () => ({ outcome: 'answered', previous: null }),
};

const onSession2 = { view: { screen: 'session' as const, uid: uid(2), sub: 'conversation' as const } };

domTest(
  'A44.2: ⌘⌫ on the Needs-you row of a likely screen prompt sends nothing; at High it reaches that card and sends "No"',
  scenario(async (c) => {
    resetAnswersForTests();
    await withFakeCore(handlers, async (core) => {
      const view = render(<Front />);
      world(1, prompt('sp_low', 'likely'));
      step(c, GUARD_MS + 10);
      act(() => row(view.container, 2).focus());
      press('Backspace', { meta: true });
      press('Enter', { meta: true });
      await act(async () => {});
      const calls = () => core.calls('screen_prompt_key').map((x) => x.req as ScreenPromptKeyReq);
      assertDeepEqual(calls(), [], 'not High confidence: ⌘⌫ and ⌘⏎ are inert from the sidebar too');

      world(2, prompt('sp_high', 'high'));
      step(c, GUARD_MS + 10);
      act(() => row(view.container, 2).focus());
      press('Backspace', { meta: true });
      await act(async () => {});
      assertDeepEqual(
        calls(),
        [{ uid: uid(2), requestId: 'sp_high', promptRev: 'rev-sp_high', action: { kind: 'key', key: '3' } }],
        "the row names the card in front: the card's own decision sends the last key",
      );
    });
  }, onSession2),
);

domTest(
  'A44.2: ⌘⏎ / ⌘⌫ on another session\'s row, or on a nav row, answer nothing — not the card in front',
  scenario(async (c) => {
    resetAnswersForTests();
    await withFakeCore(handlers, async (core) => {
      const view = render(<Front />);
      world(1, prompt('sp_high', 'high'));
      step(c, GUARD_MS + 10);
      act(() => row(view.container, 3).focus());
      press('Enter', { meta: true });
      press('Backspace', { meta: true });
      await act(async () => {});
      assertEqual(core.calls('screen_prompt_key').length, 0, "the card in front (session 2) is not the row's request");
      assertEqual(core.calls('interaction_answer').length, 0, "and the row's own request (session 3) shows no card here");
      const home = view.container.querySelector<HTMLElement>('.sb-nav-row');
      act(() => home?.focus());
      press('Enter', { meta: true });
      await act(async () => {});
      assertEqual(core.calls('screen_prompt_key').length, 0, 'a nav row names no request: inert');
    });
  }, onSession2),
);
