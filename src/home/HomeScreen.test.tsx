/**
 * Focus Home [M15] in the DOM: density, counts, and the input-safety rules of
 * contract §8.6 — the 700 ms guard on clicks and keys (T-ui-guard), the guard
 * on window activation (F-30), hold-while-hovered and the "Handled" ghost
 * (T-ui-hold), key repeat and IME never answering, and answer routing to
 * `answer_in` (T-ui-answerInRouting, F-15); A44: the Next rows' guard is keyed
 * on the request (G7c-08) and their keys are the card stack's decision
 * (G7c-09), never passed on to the focus card.
 */

import { act, fireEvent, render } from '@testing-library/react';

import { GUARD_MS, noteWindowActivation } from '../hooks/useAnswerGuard';
import { GHOST_MS } from '../hooks/useHold';
import type { AttentionItem } from '../lib/gen/AttentionItem';
import type { MemberKey } from '../lib/gen/MemberKey';
import type { PendingInteraction } from '../lib/gen/PendingInteraction';
import type { ScreenPromptKeyReq } from '../lib/gen/ScreenPromptKeyReq';
import type { Session } from '../lib/gen/Session';
import type { StoreSnapshot } from '../lib/gen/StoreSnapshot';
import { appStore } from '../store/useAppStore';
import { withFakeCore } from '../test/backend';
import { pending, queueItem, session, snapshot, uid } from '../test/fixtures';
import { assert, assertDeepEqual, assertEqual, domTest } from '../test/harness';
import { press, scenario, seed, step } from '../shell/testing';
import { HomeScreen } from './HomeScreen';

const approval = (n: number, extra: Partial<Session> = {}) =>
  session(n, {
    title: `task ${n}`,
    activity: { state: { kind: 'needsApproval' }, since: 0, confidence: 'high', evidence: null, episode: `e_${n}` as never },
    pending: [pending(`r${n}`, { title: `Run step ${n}`, tool: { tool: 'Bash', verb: 'run', object: `npm run step-${n}` } })],
    ...extra,
  });

function world(sessions: Session[], items: AttentionItem[]): StoreSnapshot {
  return snapshot(1, {
    sessions,
    queue: { rev: 1, items, needYou: items.filter((i) => i.band <= 2).length, also: items.filter((i) => i.band > 2).length, pausedUntil: null },
  });
}

const answered = { interaction_answer: () => ({ outcome: 'answered', previous: null }) };

function focusSection(container: HTMLElement): HTMLElement {
  const el = container.querySelector<HTMLElement>('section.fh');
  if (!el) throw new Error('no focus card');
  return el;
}

function allowButton(container: HTMLElement): HTMLButtonElement {
  const el = [...container.querySelectorAll<HTMLButtonElement>('section.fh button')].find((b) => b.textContent?.includes('Allow once'));
  if (!el) throw new Error('no Allow once button');
  return el;
}

domTest(
  'M15: header counts are the queue; one focus card; Next ≤ 5 then "n more"; chips for bands 3–6',
  scenario(() => {
    const sessions = Array.from({ length: 8 }, (_, i) => approval(i + 1));
    const error = session(20, { activity: { state: { kind: 'crashed', exit: { code: 1, signal: null } }, since: 0, confidence: 'high', evidence: null, episode: 'e20' as never } });
    const items = [...sessions.map((_, i) => queueItem(i + 1, 1)), queueItem(20, 3, { reason: { kind: 'crashed' }, primary: 'continueInGilde' })];
    const view = render(<HomeScreen />);
    seed(world([...sessions, error], items));
    assert(view.container.textContent?.includes('8 need you') ?? false, 'the header says queue.needYou');
    assertEqual(view.container.querySelectorAll('section.fh').length, 1, 'exactly one focus card');
    const rows = view.container.querySelectorAll('.hr-list li.hr');
    assertEqual(rows.length, 5, 'Next shows five rows');
    assert(view.getByText('2 more') !== null, 'and "2 more"');
    const chip = view.container.querySelector('.fh-chip');
    assertEqual(chip?.textContent, 'Error1', 'band 3 (errors and crashes) is one chip with its count');
  }),
);

domTest(
  'T-ui-guard: the focus card ignores clicks for 700 ms after its item arrives, visibly, then answers',
  scenario(async (c) => {
    await withFakeCore(answered, async (core) => {
      const s1 = approval(1);
      const view = render(<HomeScreen />);
      seed(world([s1], [queueItem(1, 1)]));
      const allow = allowButton(view.container);
      assertEqual(allow.getAttribute('aria-disabled'), 'true', 'guarded on arrival');
      assert(view.container.querySelector('[data-guard-note]') !== null, 'the guard is visible');
      fireEvent.click(allow);
      assertEqual(core.calls('interaction_answer').length, 0, 'a click during the guard does nothing');
      step(c, GUARD_MS);
      fireEvent.click(allowButton(view.container));
      await act(async () => {});
      const calls = core.calls('interaction_answer');
      assertEqual(calls.length, 1, 'answered once the guard ends');
      const req = calls[0]?.req as { uid: string; requestId: string; decision: { kind: string }; via: string };
      assertEqual(req.uid, s1.uid, 'to its session');
      assertEqual(req.requestId, 'r1', 'naming the request it showed');
      assertEqual(req.decision.kind, 'allowOnce', 'allow once');
      assertEqual(req.via, 'button', 'by a button');
    });
  }),
);

domTest(
  'F-30: window activation starts the 700 ms guard again',
  scenario(async (c) => {
    await withFakeCore(answered, async (core) => {
      const view = render(<HomeScreen />);
      seed(world([approval(1)], [queueItem(1, 1)]));
      step(c, GUARD_MS + 10);
      act(() => noteWindowActivation());
      fireEvent.click(allowButton(view.container));
      assertEqual(core.calls('interaction_answer').length, 0, 'guarded after activation');
      step(c, GUARD_MS);
      fireEvent.click(allowButton(view.container));
      await act(async () => {});
      assertEqual(core.calls('interaction_answer').length, 1, 'answers after the guard');
    });
  }),
);

domTest(
  'digits answer only while the card has focus; key repeat and IME composition never answer',
  scenario(async (c) => {
    await withFakeCore(answered, async (core) => {
      const view = render(<HomeScreen />);
      seed(world([approval(1)], [queueItem(1, 1)]));
      step(c, GUARD_MS + 10);
      const card = view.container.querySelector<HTMLElement>('.g-card');
      assert(card, 'the card');
      press('Digit1', { target: document.body });
      assertEqual(core.calls('interaction_answer').length, 0, 'not without focus on the card');
      act(() => card.focus());
      press('Digit1', { repeat: true });
      press('Digit1', { isComposing: true });
      assertEqual(core.calls('interaction_answer').length, 0, 'repeat and IME do nothing');
      press('Digit3');
      await act(async () => {});
      const req = core.calls('interaction_answer')[0]?.req as { decision: { kind: string }; via: string };
      assertEqual(req.decision.kind, 'deny', '3 denies');
      assertEqual(req.via, 'digit', 'by a digit');
    });
  }),
);

domTest(
  '⌘⏎ allows the focus card; it is inert on a default-to-no card, where ⌘⌫ still denies',
  scenario(async (c) => {
    await withFakeCore(answered, async (core) => {
      const noDefault = approval(1, { pending: [pending('r1', { defaultToNo: true })] });
      const view = render(<HomeScreen />);
      seed(world([noDefault], [queueItem(1, 1)]));
      step(c, GUARD_MS + 10);
      act(() => view.container.querySelector<HTMLElement>('.g-card')?.focus());
      press('Enter', { meta: true });
      assertEqual(core.calls('interaction_answer').length, 0, '⌘⏎ inert (K22)');
      press('Backspace', { meta: true });
      await act(async () => {});
      const req = core.calls('interaction_answer')[0]?.req as { decision: { kind: string }; via: string };
      assertEqual(req.decision.kind, 'deny', '⌘⌫ denies');
      assertEqual(req.via, 'shortcut', 'as a shortcut');
    });
  }),
);

domTest(
  'T-ui-hold: the focus card holds its item while hovered; a new #1 waits in Next',
  scenario(() => {
    const view = render(<HomeScreen />);
    seed(world([approval(1), approval(2)], [queueItem(1, 1), queueItem(2, 1)]));
    fireEvent.mouseEnter(focusSection(view.container));
    seed(world([approval(1), approval(2)], [queueItem(2, 1), queueItem(1, 1)]));
    assertEqual(focusSection(view.container).getAttribute('data-item'), `q_${uid(1)}`, 'the card keeps its item');
    const next = view.container.querySelector('.hr-list li.hr');
    assertEqual(next?.getAttribute('data-item'), `q_${uid(2)}`, 'the new #1 is in Next');
    fireEvent.mouseLeave(focusSection(view.container));
    seed(world([approval(1), approval(2)], [queueItem(2, 1), queueItem(1, 1)]));
    assertEqual(focusSection(view.container).getAttribute('data-item'), `q_${uid(2)}`, 'after leaving, rank 0 shows');
  }),
);

domTest(
  'T-ui-hold: an item handled elsewhere under the pointer leaves a "Handled" ghost for 1.5 s',
  scenario((c) => {
    const view = render(<HomeScreen />);
    seed(world([approval(1), approval(2)], [queueItem(1, 1), queueItem(2, 1)]));
    fireEvent.mouseEnter(focusSection(view.container));
    seed(world([approval(2)], [queueItem(2, 1)]));
    const ghost = focusSection(view.container);
    assertEqual(ghost.getAttribute('data-ghost'), 'true', 'a ghost instead of the next item');
    assert(ghost.textContent?.includes('Handled') ?? false, 'it says Handled');
    assertEqual(view.container.querySelectorAll('.g-card').length, 0, 'no answer buttons under the pointer');
    step(c, GHOST_MS + 10);
    seed(world([approval(2)], [queueItem(2, 1)]));
    assertEqual(focusSection(view.container).getAttribute('data-item'), `q_${uid(2)}`, 'then the next item');
  }),
);

domTest(
  "T-ui-hold: the user's own answer leaves no ghost",
  scenario(async (c) => {
    await withFakeCore(answered, async () => {
      const view = render(<HomeScreen />);
      seed(world([approval(1), approval(2)], [queueItem(1, 1), queueItem(2, 1)]));
      fireEvent.mouseEnter(focusSection(view.container));
      step(c, GUARD_MS + 10);
      fireEvent.click(allowButton(view.container));
      await act(async () => {});
      seed(world([approval(2)], [queueItem(2, 1)]));
      assertEqual(focusSection(view.container).getAttribute('data-ghost'), null, 'no ghost for an own action');
      assertEqual(focusSection(view.container).getAttribute('data-item'), `q_${uid(2)}`, 'the next item, guarded again');
      assertEqual(allowButton(view.container).getAttribute('aria-disabled'), 'true', 'the new item starts its own guard');
    });
  }),
);

domTest(
  "T-ui-answerInRouting: a teammate's request that renders in its lead is answered in the lead (F-15)",
  scenario(async (c) => {
    await withFakeCore(answered, async (core) => {
      const member = 'm_db' as MemberKey;
      const lead = session(9, { pending: [pending('lead-r1', { onBehalfOf: member, title: 'Edit schema.sql' })] });
      const teammate = session(5, {
        title: 'db work',
        activity: { state: { kind: 'needsApproval' }, since: 0, confidence: 'high', evidence: null, episode: 'e5' as never },
      });
      const item = queueItem(5, 1, { subject: { kind: 'session', uid: teammate.uid, onBehalfOf: member, answerIn: lead.uid } });
      const view = render(<HomeScreen />);
      seed(world([lead, teammate], [item]));
      assert(view.container.textContent?.includes('answered at the lead') ?? false, 'the card says where it is answered');
      step(c, GUARD_MS + 10);
      fireEvent.click(allowButton(view.container));
      await act(async () => {});
      const req = core.calls('interaction_answer')[0]?.req as { uid: string; requestId: string };
      assertEqual(req.uid, lead.uid, 'routed to answer_in');
      assertEqual(req.requestId, 'lead-r1', "the lead's request id");
    });
  }),
);

domTest(
  'a watched approval offers Go to {owner} and never an answer button',
  scenario(async (c) => {
    await withFakeCore({ owner_focus: () => ({}) }, async (core) => {
      const watched = approval(3, {
        control: { kind: 'watching', origin: 'vsCode', owner: { appPid: 1, appName: 'VS Code', kind: 'vsCode', bundleId: 'x' } },
        pending: [pending('r3', { kind: 'watchedApproval', title: 'Edit db/seed.ts' })],
        caps: { ...session(3).caps, send: 'none', approve: 'goToOwner', ownerFocus: true },
      });
      const view = render(<HomeScreen />);
      seed(world([watched], [queueItem(3, 1, { watched: true, primary: 'goToOwner' })]));
      const labels = [...view.container.querySelectorAll('section.fh button')].map((b) => b.textContent ?? '');
      assert(!labels.some((l) => l.includes('Allow once') || l.includes('Deny')), 'no answer buttons for a watched session');
      step(c, GUARD_MS + 10);
      const go = [...view.container.querySelectorAll<HTMLButtonElement>('section.fh button')].find((b) => b.textContent?.includes('Go to'));
      assert(go, 'Go to {owner}');
      fireEvent.click(go);
      await act(async () => {});
      assertEqual((core.calls('owner_focus')[0]?.req as { uid: string }).uid, watched.uid, 'owner_focus');
    });
  }),
);

domTest(
  'calm: nothing waiting says so; nothing running offers New session',
  scenario(() => {
    const view = render(<HomeScreen />);
    seed(world([session(1, { activity: { state: { kind: 'working' }, since: 0, confidence: 'high', evidence: null, episode: 'e' as never } })], []));
    assert(view.getByText('Nothing needs you.') !== null, 'calm sentence');
    seed(world([], []));
    assert(view.getByText('Nothing is running.') !== null, 'empty sentence');
    fireEvent.click(view.getByText('New session'));
    assertEqual(appStore.getState().ui.sheet, 'newSession', 'one action');
  }),
);

// ---------------------------------------------------------------------------
// A44 — input safety on Home's Next rows (G7c-08, G7c-09)
// ---------------------------------------------------------------------------

const needsApproval = { state: { kind: 'needsApproval' as const }, since: 0, confidence: 'high' as const, evidence: null };

function worldAt(rev: number, sessions: Session[], items: AttentionItem[]): StoreSnapshot {
  return snapshot(rev, {
    sessions,
    queue: { rev, items, needYou: items.filter((i) => i.band <= 2).length, also: 0, pausedUntil: null },
  });
}

function nextRow(container: HTMLElement, n: number): HTMLElement {
  const el = container.querySelector<HTMLElement>(`.hr-list li.hr[data-item="q_${uid(n)}"] .hr-row`);
  if (!el) throw new Error(`no Next row for ${n}`);
  return el;
}

/** Home's arrival focus runs in a requestAnimationFrame: let it land, as it does long before a user tabs to a row. */
async function arrivalSettles(): Promise<void> {
  await act(async () => {
    await new Promise((r) => setTimeout(r, 50));
  });
}

const terminalCaps = { ...session(2).caps, send: 'ptyPaste' as const, approve: 'screenKeys' as const };
const terminalControl = { kind: 'inGilde' as const, mode: 'terminal' as const, pty: null, hostedTeam: false, transport: 'pty' as const };

function screenPrompt(requestId: string, confidence: 'high' | 'likely'): PendingInteraction {
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

domTest(
  'A44.1 (G7c-08): a stacked request that replaces the answered one in a Next row starts a new 700 ms guard',
  scenario(async (c) => {
    await withFakeCore(answered, async (core) => {
      const s1 = session(1, { title: 'focus', activity: { ...needsApproval, episode: 'e_1' as never }, pending: [pending('r1')] });
      const twoRequests = [pending('r2a', { title: 'Run npm test' }), pending('r2b', { title: 'Run rm -rf build' })];
      const s2 = session(2, { title: 'stacked', activity: { ...needsApproval, episode: 'e_2' as never }, pending: twoRequests });
      const view = render(<HomeScreen />);
      seed(worldAt(1, [s1, s2], [queueItem(1, 1), queueItem(2, 1)]));
      step(c, GUARD_MS + 10);
      await arrivalSettles();
      act(() => nextRow(view.container, 2).focus());
      press('Enter', { meta: true });
      await act(async () => {});
      assertEqual(core.calls('interaction_answer').length, 1, 'the first ⌘⏎ answers r2a');
      assertEqual(
        (document.activeElement as HTMLElement | null)?.closest('[data-item]')?.getAttribute('data-item'),
        `q_${uid(2)}`,
        'focus stays on the Next row',
      );

      // The core answered r2a; the session's next stacked request takes the row under the same queue key.
      seed(worldAt(2, [s1, { ...s2, pending: [twoRequests[1] as PendingInteraction] }], [queueItem(1, 1), queueItem(2, 1)]));
      step(c, 150);
      press('Enter', { meta: true });
      await act(async () => {});
      const ids = () => core.calls('interaction_answer').map((x) => (x.req as { requestId: string }).requestId);
      assertDeepEqual(ids(), ['r2a'], 'a second ⌘⏎ 150 ms later does not approve r2b, a request the row never guarded');

      // The guard is a window, not a block: once it ends, the new request answers.
      step(c, GUARD_MS);
      press('Enter', { meta: true });
      await act(async () => {});
      assertDeepEqual(ids(), ['r2a', 'r2b'], 'after its own 700 ms, r2b answers');
    });
  }),
);

domTest(
  'A44.2 (G7c-09): Home ⌘⏎ and ⌘⌫ on a Next row are inert on a screen prompt that is not High confidence; High answers',
  scenario(async (c) => {
    await withFakeCore({ screen_prompt_key: () => ({ outcome: 'sent' }) }, async (core) => {
      const s1 = session(1, { activity: { ...needsApproval, episode: 'e_1' as never }, pending: [pending('r1')] });
      const unsure = session(2, {
        control: terminalControl,
        caps: terminalCaps,
        activity: { ...needsApproval, confidence: 'likely', episode: 'e_2' as never },
        pending: [screenPrompt('sp_low', 'likely')],
      });
      const view = render(<HomeScreen />);
      seed(worldAt(1, [s1, unsure], [queueItem(1, 1), queueItem(2, 1)]));
      step(c, GUARD_MS + 10);
      await arrivalSettles();
      act(() => nextRow(view.container, 2).focus());
      press('Enter', { meta: true });
      press('Backspace', { meta: true });
      await act(async () => {});
      const calls = () => core.calls('screen_prompt_key').map((x) => x.req as ScreenPromptKeyReq);
      assertDeepEqual(calls(), [], '§8.6: ⌘⏎ and ⌘⌫ are inert on screen prompts that are not High confidence');

      // The same row at High confidence: the shared decision sends the prompt's last key for ⌘⌫.
      seed(worldAt(2, [s1, { ...unsure, pending: [screenPrompt('sp_high', 'high')] }], [queueItem(1, 1), queueItem(2, 1)]));
      step(c, GUARD_MS + 10);
      act(() => nextRow(view.container, 2).focus());
      press('Backspace', { meta: true });
      await act(async () => {});
      assertDeepEqual(
        calls(),
        [{ uid: uid(2), requestId: 'sp_high', promptRev: 'rev-sp_high', action: { kind: 'key', key: '3' } }],
        'High and permission-shaped: ⌘⌫ sends "No"',
      );
    });
  }),
);

domTest(
  'A44.2: a key on a Next row is the row\'s — it never falls through to the focus card',
  scenario(async (c) => {
    await withFakeCore(answered, async (core) => {
      const s1 = approval(1);
      const question = session(2, {
        activity: { state: { kind: 'needsAnswer' }, since: 0, confidence: 'high', evidence: null, episode: 'e_2' as never },
        pending: [pending('q2', { kind: 'question', questions: [{ text: 'Which region?', options: ['eu', 'us'], multiSelect: false }] })],
      });
      const view = render(<HomeScreen />);
      seed(world([s1, question], [queueItem(1, 1), queueItem(2, 2, { reason: { kind: 'needsAnswer' } })]));
      step(c, GUARD_MS + 10);
      await arrivalSettles();
      act(() => nextRow(view.container, 2).focus());
      press('Enter', { meta: true });
      press('Backspace', { meta: true });
      await act(async () => {});
      assertEqual(core.calls('interaction_answer').length, 0, 'the focus card (r1) is not what the user is on: nothing is answered');
    });
  }),
);
