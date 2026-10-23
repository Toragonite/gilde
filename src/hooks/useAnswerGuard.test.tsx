/**
 * T-ui-guard: for 700 ms after the item changes, after window activation and
 * after a focus request, answering keys and clicks are ignored — visibly —
 * and they work afterwards (contract §8.6, F-30, PM-02).
 */

import { act, fireEvent, render } from '@testing-library/react';

import { GuardNote } from '../components/GuardNote';
import { manualClock, setClockForTests } from '../lib/clock';
import type { ManualClock } from '../lib/clock';
import { assert, assertDeepEqual, assertEqual, domTest } from '../test/harness';
import {
  GUARD_MS,
  noteFocusRequest,
  noteWindowActivation,
  refreshGuards,
  resetAnswerGuardForTests,
  useAnswerGuard,
} from './useAnswerGuard';

function Card({ itemKey, answers }: { itemKey: string | null; answers: string[] }) {
  const g = useAnswerGuard(itemKey);
  const allow = g.guard(() => answers.push(`allow ${itemKey}`));
  return (
    <div
      role="group"
      aria-label="card"
      tabIndex={0}
      onKeyDown={g.guard((e: { code: string }) => {
        if (e.code === 'Digit1') answers.push(`key ${itemKey}`);
      })}
    >
      <button type="button" onClick={allow} {...g.props}>
        Allow once
      </button>
      <GuardNote guard={g} />
    </div>
  );
}

function withClock(run: (c: ManualClock) => void | Promise<void>) {
  return async () => {
    const c = manualClock(10_000);
    setClockForTests(c);
    resetAnswerGuardForTests();
    try {
      await run(c);
    } finally {
      setClockForTests(null);
      resetAnswerGuardForTests();
    }
  };
}

function step(c: ManualClock, ms: number) {
  act(() => {
    c.advance(ms);
    refreshGuards();
  });
}

domTest(
  'T-ui-guard: a new item ignores clicks and keys for 700 ms, visibly, then answers',
  withClock((c) => {
    const answers: string[] = [];
    const view = render(<Card itemKey="r1" answers={answers} />);
    const button = view.getByText('Allow once');
    assertEqual(button.getAttribute('data-guarded'), 'true', 'guarded on arrival');
    assertEqual(button.getAttribute('aria-disabled'), 'true', 'announced as unavailable');
    assert(view.container.querySelector('[data-guard-note]') !== null, 'the guard is visible');
    fireEvent.click(button);
    fireEvent.keyDown(view.getByRole('group'), { code: 'Digit1', key: '1' });
    assertDeepEqual(answers, [], 'nothing answered during the guard');

    step(c, GUARD_MS - 1);
    fireEvent.click(button);
    assertDeepEqual(answers, [], 'still guarded at 699 ms');

    step(c, 1);
    assertEqual(button.getAttribute('data-guarded'), null, 'guard cleared');
    assert(view.container.querySelector('[data-guard-note]') === null, 'note gone');
    fireEvent.click(button);
    fireEvent.keyDown(view.getByRole('group'), { code: 'Digit1', key: '1' });
    assertDeepEqual(answers, ['allow r1', 'key r1'], 'answers work after 700 ms');

    fireEvent.keyDown(view.getByRole('group'), { code: 'Digit1', key: '1', repeat: true });
    assertEqual(answers.length, 2, 'a repeated key never answers');

    // A different item under the same card starts the guard again.
    view.rerender(<Card itemKey="r2" answers={answers} />);
    fireEvent.click(view.getByText('Allow once'));
    assertEqual(answers.length, 2, 'the replacing item is guarded');
    step(c, GUARD_MS);
    fireEvent.click(view.getByText('Allow once'));
    assertDeepEqual(answers.slice(2), ['allow r2'], 'and answers after its own 700 ms');
  }),
);

domTest(
  'T-ui-guard: window activation and a focus request restart the guard (F-30)',
  withClock((c) => {
    const answers: string[] = [];
    const view = render(<Card itemKey="r1" answers={answers} />);
    step(c, GUARD_MS);
    fireEvent.click(view.getByText('Allow once'));
    assertEqual(answers.length, 1, 'answerable');

    act(() => {
      window.dispatchEvent(new Event('focus'));
    });
    assertEqual(view.getByText('Allow once').getAttribute('data-guarded'), 'true', 'window focus guards again');
    fireEvent.click(view.getByText('Allow once'));
    assertEqual(answers.length, 1, 'click right after activation ignored');
    step(c, GUARD_MS);
    fireEvent.click(view.getByText('Allow once'));
    assertEqual(answers.length, 2, 'works again');

    act(() => noteFocusRequest());
    fireEvent.click(view.getByText('Allow once'));
    assertEqual(answers.length, 2, 'a notification click (focus request) guards');
    step(c, GUARD_MS);
    act(() => noteWindowActivation());
    fireEvent.click(view.getByText('Allow once'));
    assertEqual(answers.length, 2, 'noteWindowActivation guards');
    step(c, GUARD_MS);
    fireEvent.click(view.getByText('Allow once'));
    assertEqual(answers.length, 3, 'answers after the guard');
  }),
);

domTest(
  'T-ui-guard: nothing to answer is never guarded',
  withClock(() => {
    const answers: string[] = [];
    const view = render(<Card itemKey={null} answers={answers} />);
    assertEqual(view.getByText('Allow once').getAttribute('data-guarded'), null, 'no item, no guard');
  }),
);
