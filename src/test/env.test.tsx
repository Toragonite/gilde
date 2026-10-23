/**
 * The DOM test environment itself: React renders into happy-dom, keyboard and
 * focus behave, and each `domTest` starts from an empty body.
 */

import { fireEvent, render, screen } from '@testing-library/react';
import { useState } from 'react';

import { assert, assertEqual, domTest } from './harness';

function Counter() {
  const [n, setN] = useState(0);
  return (
    <button type="button" onKeyDown={(e) => e.code === 'Enter' && setN((x) => x + 1)}>
      count {n}
    </button>
  );
}

domTest('env: React renders into the DOM and handles keyboard events', () => {
  render(<Counter />);
  const button = screen.getByRole('button');
  button.focus();
  assertEqual(document.activeElement, button, 'focus moves to the button');
  fireEvent.keyDown(button, { code: 'Enter', key: 'Enter' });
  assertEqual(button.textContent, 'count 1', 'the key handler ran');
});

domTest('env: every domTest starts from an empty body', () => {
  assertEqual(document.body.children.length, 0, 'nothing left from the previous test');
  assert(typeof window.requestAnimationFrame === 'function', 'rAF exists');
  assert(typeof KeyboardEvent === 'function', 'KeyboardEvent exists');
});
