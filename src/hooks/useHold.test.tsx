/**
 * T-ui-hold: the focus card holds its item while engaged, a handled item
 * leaves a ghost for 1.5 s (none for the user's own action), and a hovered
 * list keeps its order with ghosts (contract §8.6 [M15]).
 */

import { act, fireEvent, render } from '@testing-library/react';

import { manualClock, setClockForTests } from '../lib/clock';
import { assert, assertDeepEqual, assertEqual, domTest, test } from '../test/harness';
import {
  EMPTY_FOCUS_HOLD,
  EMPTY_LIST_HOLD,
  GHOST_MS,
  holdOrder,
  pickFocus,
  useEngagement,
  useHeldFocus,
  useHeldOrder,
} from './useHold';
import type { FocusHoldState, ListHoldState } from './useHold';

interface Item {
  id: string;
  name: string;
}
const it = (id: string): Item => ({ id, name: `name ${id}` });
const keyOf = (x: Item) => x.id;
const nameOf = (x: Item) => x.name;

test('T-ui-hold (core): not engaged, the focus card follows the ranking', () => {
  let s: FocusHoldState = EMPTY_FOCUS_HOLD;
  let p = pickFocus(s, [it('a'), it('b')], keyOf, nameOf, false, null, 0);
  assertEqual(p.item?.id, 'a', 'top item');
  s = p.state;
  p = pickFocus(s, [it('b'), it('a')], keyOf, nameOf, false, null, 10);
  assertEqual(p.item?.id, 'b', 're-ranked while not engaged: the new top shows');
});

test('T-ui-hold (core): engaged, the item holds; a handled item leaves a 1.5 s ghost', () => {
  let s = pickFocus(EMPTY_FOCUS_HOLD, [it('a'), it('b')], keyOf, nameOf, false, null, 0).state;
  // A new arrival outranks "a" while the pointer is on the card: "a" holds.
  let p = pickFocus(s, [it('c'), it('a'), it('b')], keyOf, nameOf, true, null, 100);
  assertEqual(p.item?.id, 'a', 'held under the pointer');
  s = p.state;
  // "a" is answered elsewhere: a ghost instead of sliding "c" under the pointer.
  p = pickFocus(s, [it('c'), it('b')], keyOf, nameOf, true, null, 200);
  assertEqual(p.item, null, 'no item under the pointer');
  assertEqual(p.ghost?.key, 'a', 'ghost of the handled item');
  assertEqual(p.ghost?.name, 'name a', 'ghost keeps the name');
  s = p.state;
  p = pickFocus(s, [it('c'), it('b')], keyOf, nameOf, true, null, 200 + GHOST_MS - 1);
  assertEqual(p.ghost?.key, 'a', 'ghost lasts 1.5 s');
  s = p.state;
  p = pickFocus(s, [it('c'), it('b')], keyOf, nameOf, true, null, 200 + GHOST_MS);
  assertEqual(p.ghost, null, 'ghost gone after 1.5 s');
  assertEqual(p.item?.id, 'c', 'then the next item shows');
});

test('T-ui-hold (core): the ghost ends as soon as the pointer leaves', () => {
  let s = pickFocus(EMPTY_FOCUS_HOLD, [it('a')], keyOf, nameOf, true, null, 0).state;
  s = pickFocus(s, [it('b')], keyOf, nameOf, true, null, 10).state;
  const p = pickFocus(s, [it('b')], keyOf, nameOf, false, null, 20);
  assertEqual(p.ghost, null, 'not engaged: no ghost');
  assertEqual(p.item?.id, 'b', 'next item');
});

test('T-ui-hold (core): the user’s own action leaves no ghost', () => {
  const s = pickFocus(EMPTY_FOCUS_HOLD, [it('a'), it('b')], keyOf, nameOf, true, null, 0).state;
  const p = pickFocus(s, [it('b')], keyOf, nameOf, true, { key: 'a', at: 50 }, 100);
  assertEqual(p.ghost, null, 'answered here: no ghost');
  assertEqual(p.item?.id, 'b', 'the next item shows at once');
});

test('T-ui-hold (core): a hovered list keeps its order, ghosts removed rows, appends new ones', () => {
  let s: ListHoldState = EMPTY_LIST_HOLD;
  s = holdOrder(s, [it('a'), it('b'), it('c')], keyOf, nameOf, false, null, 0).state;
  const p = holdOrder(s, [it('c'), it('d'), it('a')], keyOf, nameOf, true, null, 10);
  assertDeepEqual(
    p.rows.map((r) => `${r.kind}:${r.key}`),
    ['item:a', 'ghost:b', 'item:c', 'item:d'],
    'order held, b ghosted, d appended',
  );
  const ghost = p.rows[1];
  assert(ghost?.kind === 'ghost' && ghost.name === 'name b', 'ghost carries the name');
  const own = holdOrder(s, [it('a'), it('c')], keyOf, nameOf, true, { key: 'b', at: 5 }, 10);
  assertDeepEqual(own.rows.map((r) => r.key), ['a', 'c'], 'own action: no ghost row');
  const free = holdOrder(p.state, [it('c'), it('d'), it('a')], keyOf, nameOf, false, null, 20);
  assertDeepEqual(free.rows.map((r) => r.key), ['c', 'd', 'a'], 'pointer gone: the ranking shows');
});

function FocusCard({ ranked }: { ranked: Item[] }) {
  const engagement = useEngagement();
  const held = useHeldFocus(ranked, keyOf, nameOf, engagement.engaged);
  return (
    <section aria-label="focus" {...engagement.props}>
      {held.ghost ? <p data-ghost="">{held.ghost.name} handled</p> : null}
      {held.item ? (
        <button type="button" onClick={() => held.markOwnAction(held.item?.id ?? '')}>
          {held.item.name}
        </button>
      ) : null}
    </section>
  );
}

domTest('T-ui-hold: the focus card holds while hovered and shows a ghost, then moves on', () => {
  const c = manualClock(0);
  setClockForTests(c);
  try {
    const view = render(<FocusCard ranked={[it('a'), it('b')]} />);
    const section = view.getByLabelText('focus');
    fireEvent.mouseEnter(section);
    view.rerender(<FocusCard ranked={[it('c'), it('a'), it('b')]} />);
    assert(view.queryByText('name a') !== null, 'held while hovered');
    view.rerender(<FocusCard ranked={[it('c'), it('b')]} />);
    assert(view.container.querySelector('[data-ghost]') !== null, 'ghost shown');
    assert(view.queryByText('name c') === null, 'the next item is not pulled under the pointer');
    act(() => c.advance(GHOST_MS));
    view.rerender(<FocusCard ranked={[it('c'), it('b')]} />);
    assert(view.queryByText('name c') !== null, 'after 1.5 s the next item shows');
    fireEvent.mouseLeave(section);
  } finally {
    setClockForTests(null);
  }
});

domTest('T-ui-hold: answering the held item here moves on without a ghost', () => {
  const c = manualClock(0);
  setClockForTests(c);
  try {
    const view = render(<FocusCard ranked={[it('a'), it('b')]} />);
    const section = view.getByLabelText('focus');
    fireEvent.focus(view.getByText('name a'));
    fireEvent.click(view.getByText('name a'));
    view.rerender(<FocusCard ranked={[it('b')]} />);
    assert(view.container.querySelector('[data-ghost]') === null, 'no ghost for the own action');
    assert(view.queryByText('name b') !== null, 'next item at once');
    fireEvent.blur(section);
  } finally {
    setClockForTests(null);
  }
});

function Rows({ items }: { items: Item[] }) {
  const engagement = useEngagement();
  const held = useHeldOrder(items, keyOf, nameOf, engagement.engaged);
  return (
    <ul aria-label="rows" {...engagement.props}>
      {held.rows.map((r) => (
        <li key={r.key} data-kind={r.kind}>
          {r.kind === 'ghost' ? `${r.name} handled` : r.item.name}
        </li>
      ))}
    </ul>
  );
}

domTest('T-ui-hold: rows under the pointer keep their order', () => {
  const view = render(<Rows items={[it('a'), it('b'), it('c')]} />);
  fireEvent.mouseEnter(view.getByLabelText('rows'));
  view.rerender(<Rows items={[it('c'), it('a')]} />);
  const texts = Array.from(view.container.querySelectorAll('li')).map((li) => li.textContent);
  assertDeepEqual(texts, ['name a', 'name b handled', 'name c'], 'held order with a ghost');
  fireEvent.mouseLeave(view.getByLabelText('rows'));
  view.rerender(<Rows items={[it('c'), it('a')]} />);
  const after = Array.from(view.container.querySelectorAll('li')).map((li) => li.textContent);
  assertDeepEqual(after, ['name c', 'name a'], 'released: ranked order');
});
