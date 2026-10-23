/**
 * Roving tabindex: one Tab stop; arrows, Home and End move focus; focus by
 * click makes an item the stop; IME and modifiers are ignored (ux-ia §7.5).
 */

import { fireEvent, render } from '@testing-library/react';

import { assertDeepEqual, assertEqual, domTest } from '../test/harness';
import { useRovingTabIndex } from './useRovingTabIndex';
import type { RovingOrientation } from './useRovingTabIndex';

function List({ count, orientation, loop }: { count: number; orientation?: RovingOrientation; loop?: boolean }) {
  const roving = useRovingTabIndex(count, { orientation, loop });
  return (
    <div role="tree" aria-label="sessions">
      {Array.from({ length: count }, (_, i) => (
        <div key={i} role="treeitem" aria-selected={i === roving.activeIndex} {...roving.getItemProps(i)}>
          {`row ${i}`}
        </div>
      ))}
    </div>
  );
}

function tabIndexes(container: HTMLElement): number[] {
  return Array.from(container.querySelectorAll<HTMLElement>('[role="treeitem"]')).map((el) => el.tabIndex);
}

domTest('roving: one Tab stop; ↓/↑/Home/End move focus and the stop', () => {
  const view = render(<List count={4} />);
  assertDeepEqual(tabIndexes(view.container), [0, -1, -1, -1], 'only the first item is a Tab stop');
  const first = view.getByText('row 0');
  first.focus();
  fireEvent.keyDown(first, { code: 'ArrowDown', key: 'ArrowDown' });
  assertEqual(document.activeElement?.textContent, 'row 1', 'down');
  assertDeepEqual(tabIndexes(view.container), [-1, 0, -1, -1], 'the stop moved');
  fireEvent.keyDown(document.activeElement as HTMLElement, { code: 'End', key: 'End' });
  assertEqual(document.activeElement?.textContent, 'row 3', 'end');
  fireEvent.keyDown(document.activeElement as HTMLElement, { code: 'ArrowDown', key: 'ArrowDown' });
  assertEqual(document.activeElement?.textContent, 'row 3', 'no wrap by default');
  fireEvent.keyDown(document.activeElement as HTMLElement, { code: 'Home', key: 'Home' });
  assertEqual(document.activeElement?.textContent, 'row 0', 'home');
  fireEvent.keyDown(document.activeElement as HTMLElement, { code: 'ArrowRight', key: 'ArrowRight' });
  assertEqual(document.activeElement?.textContent, 'row 0', '→ is left to the tree (expand) in vertical mode');
});

domTest('roving: focus by pointer makes that item the stop; modifiers and IME are ignored', () => {
  const view = render(<List count={3} loop />);
  const third = view.getByText('row 2');
  fireEvent.focus(third);
  assertDeepEqual(tabIndexes(view.container), [-1, -1, 0], 'focused item is the stop');
  third.focus();
  fireEvent.keyDown(third, { code: 'ArrowDown', key: 'ArrowDown' });
  assertEqual(document.activeElement?.textContent, 'row 0', 'loop wraps');
  fireEvent.keyDown(document.activeElement as HTMLElement, { code: 'ArrowDown', key: 'ArrowDown', metaKey: true });
  assertEqual(document.activeElement?.textContent, 'row 0', '⌘↓ belongs to hotkeys');
  fireEvent.keyDown(document.activeElement as HTMLElement, { code: 'ArrowDown', key: 'ArrowDown', isComposing: true });
  assertEqual(document.activeElement?.textContent, 'row 0', 'ignored during composition');
});

domTest('roving: horizontal lists use ←/→', () => {
  const view = render(<List count={3} orientation="horizontal" />);
  const first = view.getByText('row 0');
  first.focus();
  fireEvent.keyDown(first, { code: 'ArrowRight', key: 'ArrowRight' });
  assertEqual(document.activeElement?.textContent, 'row 1', 'right');
  fireEvent.keyDown(document.activeElement as HTMLElement, { code: 'ArrowDown', key: 'ArrowDown' });
  assertEqual(document.activeElement?.textContent, 'row 1', 'down does nothing');
});
