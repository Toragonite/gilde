/**
 * Sheet: focus trap and restore, initial focus, Esc/⌘. cancel, ⌘⏎ only on
 * non-destructive sheets, nested sheets (ux-ia §7.1, §7.5, §10).
 */

import { fireEvent, render } from '@testing-library/react';
import { useRef, useState } from 'react';

import { resetHotkeysForTests } from '../hooks/useHotkeys';
import { assert, assertDeepEqual, assertEqual, domTest } from '../test/harness';
import { Sheet } from './Sheet';

function Harness({
  destructive = false,
  withInitial = false,
  log,
}: {
  destructive?: boolean;
  withInitial?: boolean;
  log: string[];
}) {
  const [open, setOpen] = useState(false);
  const cancelRef = useRef<HTMLButtonElement>(null);
  return (
    <div>
      <button type="button" onClick={() => setOpen(true)}>
        open
      </button>
      <Sheet
        open={open}
        title="Stop session"
        destructive={destructive}
        initialFocus={withInitial ? cancelRef : undefined}
        onCancel={() => {
          log.push('cancel');
          setOpen(false);
        }}
        onConfirm={() => {
          log.push('confirm');
          setOpen(false);
        }}
        actions={
          <>
            <button type="button" ref={cancelRef} onClick={() => setOpen(false)}>
              Cancel
            </button>
            <button type="button" onClick={() => log.push('primary')}>
              Stop
            </button>
          </>
        }
      >
        <label>
          Reason
          <input type="text" />
        </label>
        <button type="button" data-autofocus="">
          Field
        </button>
      </Sheet>
    </div>
  );
}

function tab(shift = false) {
  fireEvent.keyDown(document.activeElement ?? document.body, { code: 'Tab', key: 'Tab', shiftKey: shift });
}

domTest('sheet: a dialog named by its title; initial focus; trap; restore to the invoker', () => {
  const log: string[] = [];
  const view = render(<Harness log={log} />);
  try {
    const invoker = view.getByText('open');
    invoker.focus();
    fireEvent.click(invoker);
    const dialog = view.getByRole('dialog');
    assertEqual(dialog.getAttribute('aria-modal'), 'true', 'modal');
    const labelId = dialog.getAttribute('aria-labelledby') ?? '';
    assertEqual(document.getElementById(labelId)?.textContent, 'Stop session', 'named by its title');
    assertEqual(document.activeElement?.textContent, 'Field', 'the [data-autofocus] control is focused first');

    // Tab cycles inside: last → first, first ← last.
    const stop = view.getByText('Stop');
    stop.focus();
    tab();
    assert(dialog.contains(document.activeElement), 'Tab from the last control stays inside');
    const close = dialog.querySelector('.g-sheet-close') as HTMLElement;
    assertEqual(document.activeElement, close, 'wrapped to the first focusable (the close button)');
    tab(true);
    assertEqual(document.activeElement, stop, 'Shift+Tab from the first wraps to the last');

    // Focus that escapes behind the sheet is brought back.
    invoker.focus();
    assert(dialog.contains(document.activeElement), 'focus cannot leave the sheet');

    fireEvent.keyDown(dialog, { code: 'Escape', key: 'Escape' });
    assertDeepEqual(log, ['cancel'], 'Esc cancels');
    assertEqual(view.queryByRole('dialog'), null, 'closed');
    assertEqual(document.activeElement, invoker, 'focus returns to the invoker');
  } finally {
    resetHotkeysForTests();
  }
});

domTest('sheet: ⌘. cancels; ⌘⏎ confirms a non-destructive sheet only', () => {
  const log: string[] = [];
  const view = render(<Harness log={log} />);
  try {
    fireEvent.click(view.getByText('open'));
    fireEvent.keyDown(view.getByRole('dialog'), { code: 'Period', key: '.', metaKey: true });
    assertDeepEqual(log, ['cancel'], '⌘. cancels (macOS convention)');
    fireEvent.click(view.getByText('open'));
    const input = view.getByRole('dialog').querySelector('input') as HTMLInputElement;
    input.focus();
    fireEvent.keyDown(input, { code: 'Enter', key: 'Enter', metaKey: true });
    assertDeepEqual(log, ['cancel', 'confirm'], '⌘⏎ confirms, even from a text field');
  } finally {
    resetHotkeysForTests();
  }
  view.unmount();

  const log2: string[] = [];
  const destructive = render(<Harness log={log2} destructive withInitial />);
  try {
    fireEvent.click(destructive.getByText('open'));
    const dialog = destructive.getByRole('dialog');
    assertEqual(document.activeElement?.textContent, 'Cancel', 'the passed initial focus (Cancel) wins');
    assertEqual(dialog.getAttribute('data-destructive'), 'true', 'marked destructive');
    fireEvent.keyDown(dialog, { code: 'Enter', key: 'Enter', metaKey: true });
    assertDeepEqual(log2, [], 'a destructive sheet has no ⌘⏎');
    assert(destructive.queryByRole('dialog') !== null, 'still open');
  } finally {
    resetHotkeysForTests();
  }
});

domTest('sheet: without a marked control, the first action (Cancel) takes focus', () => {
  function Plain() {
    const [open, setOpen] = useState(true);
    return (
      <Sheet
        open={open}
        title="Discard"
        onCancel={() => setOpen(false)}
        actions={
          <>
            <button type="button">Cancel</button>
            <button type="button">Discard</button>
          </>
        }
      >
        <p>Text only.</p>
      </Sheet>
    );
  }
  render(<Plain />);
  try {
    assertEqual(document.activeElement?.textContent, 'Cancel', 'Cancel focused by default');
  } finally {
    resetHotkeysForTests();
  }
});

domTest('sheet: a sheet over a sheet traps and cancels only the top one', () => {
  const log: string[] = [];
  function Nested() {
    const [inner, setInner] = useState(false);
    return (
      <Sheet open title="Outer" onCancel={() => log.push('outer')}>
        <button type="button" onClick={() => setInner(true)}>
          more
        </button>
        <Sheet open={inner} title="Inner" onCancel={() => {
          log.push('inner');
          setInner(false);
        }}>
          <button type="button">inner button</button>
        </Sheet>
      </Sheet>
    );
  }
  const view = render(<Nested />);
  try {
    const more = view.getByText('more');
    more.focus();
    fireEvent.click(more);
    const dialogs = view.getAllByRole('dialog');
    assertEqual(dialogs.length, 2, 'two sheets');
    const inner = dialogs[1] as HTMLElement;
    assert(inner.contains(document.activeElement), 'focus moved into the inner sheet');
    fireEvent.keyDown(document.activeElement as HTMLElement, { code: 'Escape', key: 'Escape' });
    assertDeepEqual(log, ['inner'], 'Esc closes only the top sheet');
    assertEqual(document.activeElement, more, 'focus back on the control that opened it');
  } finally {
    resetHotkeysForTests();
  }
});
