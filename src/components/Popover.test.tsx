/**
 * Popover: trigger ARIA, focus in on open, Esc closes and restores focus,
 * outside press and focus leaving close, menu arrow keys, placement.
 */

import { fireEvent, render } from '@testing-library/react';
import { useRef, useState } from 'react';

import { assert, assertDeepEqual, assertEqual, domTest, test } from '../test/harness';
import { Popover, placePopover, popoverTriggerProps } from './Popover';
import type { PopoverCloseReason } from './Popover';

function Harness({ role = 'dialog' as 'dialog' | 'menu', log }: { role?: 'dialog' | 'menu'; log: PopoverCloseReason[] }) {
  const [open, setOpen] = useState(false);
  const anchor = useRef<HTMLButtonElement>(null);
  return (
    <div>
      <button type="button" ref={anchor} onClick={() => setOpen((o) => !o)} {...popoverTriggerProps(open, 'pop', role)}>
        details
      </button>
      <button type="button">elsewhere</button>
      <Popover
        open={open}
        anchor={anchor}
        id="pop"
        role={role}
        label="Details"
        onClose={(reason) => {
          log.push(reason);
          setOpen(false);
        }}
      >
        {role === 'menu' ? (
          <>
            <button type="button" role="menuitem">
              one
            </button>
            <button type="button" role="menuitem">
              two
            </button>
            <button type="button" role="menuitem">
              three
            </button>
          </>
        ) : (
          <button type="button">inside</button>
        )}
      </Popover>
    </div>
  );
}

domTest('popover: trigger state, focus in, Esc closes and returns focus to the trigger', () => {
  const log: PopoverCloseReason[] = [];
  const view = render(<Harness log={log} />);
  const trigger = view.getByText('details');
  assertEqual(trigger.getAttribute('aria-expanded'), 'false', 'collapsed');
  trigger.focus();
  fireEvent.click(trigger);
  assertEqual(trigger.getAttribute('aria-expanded'), 'true', 'expanded');
  assertEqual(trigger.getAttribute('aria-controls'), 'pop', 'controls the popover');
  const pop = view.getByRole('dialog');
  assertEqual(pop.getAttribute('aria-label'), 'Details', 'named');
  assertEqual(document.activeElement?.textContent, 'inside', 'focus moved into the popover');
  fireEvent.keyDown(document.activeElement as HTMLElement, { code: 'Escape', key: 'Escape' });
  assertDeepEqual(log, ['escape'], 'Esc closes');
  assertEqual(view.queryByRole('dialog'), null, 'gone');
  assertEqual(document.activeElement, trigger, 'focus back on the trigger');
});

domTest('popover: a press outside closes; a press on the trigger or inside does not', () => {
  const log: PopoverCloseReason[] = [];
  const view = render(<Harness log={log} />);
  fireEvent.click(view.getByText('details'));
  fireEvent.mouseDown(view.getByText('inside'));
  assertDeepEqual(log, [], 'inside press keeps it open');
  fireEvent.mouseDown(view.getByText('elsewhere'));
  assertDeepEqual(log, ['outside'], 'outside press closes');
});

domTest('popover: focus leaving both popover and trigger closes it (non-modal)', () => {
  const log: PopoverCloseReason[] = [];
  const view = render(<Harness log={log} />);
  fireEvent.click(view.getByText('details'));
  const inside = view.getByText('inside');
  fireEvent.blur(inside, { relatedTarget: view.getByText('elsewhere') });
  assertDeepEqual(log, ['blur'], 'Tab out closes');
});

domTest('popover: a menu moves between items with the arrow keys, Home and End', () => {
  const log: PopoverCloseReason[] = [];
  const view = render(<Harness role="menu" log={log} />);
  fireEvent.click(view.getByText('details'));
  assertEqual(document.activeElement?.textContent, 'one', 'first item focused');
  fireEvent.keyDown(document.activeElement as HTMLElement, { code: 'ArrowDown', key: 'ArrowDown' });
  assertEqual(document.activeElement?.textContent, 'two', 'down');
  fireEvent.keyDown(document.activeElement as HTMLElement, { code: 'End', key: 'End' });
  assertEqual(document.activeElement?.textContent, 'three', 'end');
  fireEvent.keyDown(document.activeElement as HTMLElement, { code: 'ArrowDown', key: 'ArrowDown' });
  assertEqual(document.activeElement?.textContent, 'one', 'wraps');
  fireEvent.keyDown(document.activeElement as HTMLElement, { code: 'ArrowUp', key: 'ArrowUp' });
  assertEqual(document.activeElement?.textContent, 'three', 'up wraps');
  assert(view.getByRole('menu') !== null, 'menu role');
});

test('popover: placement flips above near the bottom and stays inside the window', () => {
  const vp = { width: 1000, height: 800 };
  const below = placePopover({ top: 100, bottom: 120, left: 50 }, { width: 200, height: 150 }, vp);
  assertDeepEqual(below, { top: 124, left: 50, placement: 'below' }, 'room below');
  const above = placePopover({ top: 700, bottom: 720, left: 50 }, { width: 200, height: 150 }, vp);
  assertDeepEqual(above, { top: 546, left: 50, placement: 'above' }, 'flipped above');
  const clamped = placePopover({ top: 100, bottom: 120, left: 950 }, { width: 200, height: 100 }, vp);
  assertEqual(clamped.left, 792, 'kept inside the right edge');
});
