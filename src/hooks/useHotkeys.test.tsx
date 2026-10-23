/**
 * The keyboard layer: combo matching, the keymap lock, IME and repeat guards,
 * text entries, focus-scoped card digits and the sheet trap (ux-ia §7,
 * contract §8.6, T-ui-ime).
 */

import { fireEvent, render } from '@testing-library/react';
import { useRef } from 'react';

import { assert, assertDeepEqual, assertEqual, domTest, test } from '../test/harness';
import {
  KEYMAP,
  ariaKeyshortcuts,
  codeFor,
  formatCombo,
  handleHotkey,
  isImeComposing,
  matchesCombo,
  parseCombo,
  registerHotkeys,
  resetHotkeysForTests,
  useHotkeys,
} from './useHotkeys';
import type { HotkeyBinding } from './useHotkeys';
import { Sheet } from '../components/Sheet';

function key(code: string, mods: Partial<Pick<KeyboardEvent, 'metaKey' | 'ctrlKey' | 'altKey' | 'shiftKey'>> = {}) {
  return { code, metaKey: false, ctrlKey: false, altKey: false, shiftKey: false, ...mods };
}

test('hotkeys: codeFor and parseCombo read display keys as physical codes', () => {
  assertEqual(codeFor('j'), 'KeyJ', 'letters');
  assertEqual(codeFor('1'), 'Digit1', 'digits');
  assertEqual(codeFor('f6'), 'F6', 'function keys');
  assertEqual(codeFor('bracketleft'), 'BracketLeft', 'punctuation');
  assertEqual(codeFor('backspace'), 'Backspace', 'backspace');
  const p = parseCombo('mod+alt+shift+arrowup');
  assert(p.mod && p.alt && p.shift && !p.ctrl, 'every modifier read');
  assertEqual(p.code, 'ArrowUp', 'arrow alias');
});

test('hotkeys: matching is exact on modifiers and never claims Ctrl', () => {
  assert(matchesCombo(key('KeyJ', { metaKey: true }), 'mod+j'), '⌘J matches');
  assert(!matchesCombo(key('KeyJ', { metaKey: true, shiftKey: true }), 'mod+j'), '⇧⌘J is not ⌘J');
  assert(matchesCombo(key('KeyJ', { metaKey: true, shiftKey: true }), 'mod+shift+j'), '⇧⌘J matches');
  assert(!matchesCombo(key('KeyJ', { ctrlKey: true }), 'mod+j'), 'Ctrl is not ⌘');
  assert(!matchesCombo(key('KeyJ', { metaKey: true, ctrlKey: true }), 'mod+j'), 'any Ctrl event is left alone');
  assert(!matchesCombo(key('KeyB', { ctrlKey: true }), 'ctrl+b'), 'a Ctrl combo never matches');
  // ⌥ rewrites event.key on macOS (⌘⌥B arrives as ∫) and the Korean layout
  // rewrites letters (ㅓ for J): only the code matters.
  const korean = { ...key('KeyJ', { metaKey: true }), key: 'ㅓ' };
  assert(matchesCombo(korean, 'mod+j'), 'the Korean layout still matches by code');
});

test('hotkeys: formatCombo and ariaKeyshortcuts spell combos for people and AT', () => {
  assertEqual(formatCombo('mod+shift+j'), '⇧⌘J', 'shift before command');
  assertEqual(formatCombo('mod+alt+1'), '⌥⌘1', 'option first');
  assertEqual(formatCombo('mod+enter'), '⌘⏎', 'return symbol');
  assertEqual(formatCombo('mod+shift+backspace'), '⇧⌘⌫', 'delete symbol');
  assertEqual(ariaKeyshortcuts('mod+enter'), 'Meta+Enter', 'aria spelling');
  assertEqual(ariaKeyshortcuts('mod+backspace'), 'Meta+Backspace', 'aria deny');
  assertEqual(ariaKeyshortcuts('1'), '1', 'a digit');
  assertEqual(ariaKeyshortcuts('mod+shift+j'), 'Shift+Meta+J', 'modifiers then key');
});

test('hotkeys: the keymap is locked — ids unique, combos parse, no Ctrl, no collision within a scope', () => {
  const ids = new Set<string>();
  for (const entry of KEYMAP) {
    assert(!ids.has(entry.id), `id ${entry.id} is unique`);
    ids.add(entry.id);
    assert(entry.combos.length > 0, `${entry.id} has a combo`);
    for (const combo of entry.combos) {
      const p = parseCombo(combo);
      assert(!p.ctrl, `${entry.id}: ${combo} never uses Ctrl`);
      assert(/^(Key[A-Z]|Digit\d|F\d+|[A-Z][A-Za-z]+)$/.test(p.code), `${entry.id}: ${combo} parses to a code (${p.code})`);
    }
  }
  const seen = new Map<string, string>();
  for (const entry of KEYMAP) {
    for (const scope of entry.scopes) {
      for (const combo of entry.combos) {
        const p = parseCombo(combo);
        const slot = `${scope}/${entry.context ?? ''}/${p.mod}${p.alt}${p.shift}${p.code}`;
        const other = seen.get(slot);
        assert(other === undefined, `${entry.id} and ${other} collide on ${combo} in ${scope}`);
        seen.set(slot, entry.id);
      }
    }
  }
  // A global binding may not be reused by a narrower scope, except where the
  // keymap says it is off there (⌥⌘↑/↓: sidebar outside Split/Team, tile focus inside).
  const global = KEYMAP.filter((e) => e.scopes.includes('G'));
  for (const g of global) {
    for (const combo of g.combos) {
      for (const other of KEYMAP) {
        if (other === g) continue;
        for (const scope of other.scopes) {
          if (scope === 'G' || g.notIn?.includes(scope)) continue;
          for (const c of other.combos) {
            assert(
              JSON.stringify(parseCombo(c)) !== JSON.stringify(parseCombo(combo)),
              `${other.id} (${scope}) reuses the global ${g.id} ${combo}`,
            );
          }
        }
      }
    }
  }
  // Spot checks against ux-ia §7.2.
  const byId = (id: string) => KEYMAP.find((e) => e.id === id);
  assertDeepEqual(byId('needsYou.next')?.combos, ['mod+j'], '⌘J is next');
  assertDeepEqual(byId('stop')?.combos, ['mod+shift+backspace'], 'Stop… moved to ⇧⌘⌫ (K1)');
  assertDeepEqual(byId('answer.deny')?.combos, ['mod+backspace'], '⌘⌫ denies');
  assertDeepEqual(byId('card.allowOnce')?.scopes, ['C'], 'digits live on a focused card');
});

test('hotkeys: IME composition and keyCode 229 are recognised', () => {
  assert(isImeComposing({ isComposing: true, keyCode: 13 }), 'isComposing');
  assert(isImeComposing({ isComposing: false, keyCode: 229 }), 'keyCode 229');
  assert(!isImeComposing({ isComposing: false, keyCode: 13 }), 'a plain Enter');
});

function Bindings({ bindings }: { bindings: HotkeyBinding[] }) {
  useHotkeys(bindings);
  return (
    <div>
      <textarea aria-label="composer" />
      <button type="button">elsewhere</button>
    </div>
  );
}

domTest('T-ui-ime: nothing fires during composition; event.repeat never fires a binding', () => {
  const runs: string[] = [];
  const view = render(
    <Bindings
      bindings={[
        { id: 'next', combo: 'mod+j', run: () => runs.push('next') },
        { id: 'send', combo: 'enter', allowInTextEntry: true, run: () => runs.push('send') },
        { id: 'nav', combo: 'mod+bracketleft', allowRepeat: true, run: () => runs.push('nav') },
      ]}
    />,
  );
  try {
    const composer = view.getByLabelText('composer');
    composer.focus();
    fireEvent.keyDown(composer, { code: 'Enter', key: 'Enter', isComposing: true });
    fireEvent.keyDown(composer, { code: 'Enter', key: 'Process', keyCode: 229 });
    assertDeepEqual(runs, [], 'Enter during a Korean composition never sends');
    fireEvent.keyDown(composer, { code: 'Enter', key: 'Enter' });
    assertDeepEqual(runs, ['send'], 'a committed Enter sends');
    fireEvent.keyDown(composer, { code: 'KeyJ', key: 'j', metaKey: true, repeat: true });
    assertDeepEqual(runs, ['send'], 'a repeated ⌘J does nothing');
    fireEvent.keyDown(composer, { code: 'BracketLeft', key: '[', metaKey: true, repeat: true });
    assertDeepEqual(runs, ['send', 'nav'], 'navigation may opt in to repeat');
  } finally {
    resetHotkeysForTests();
  }
});

domTest('hotkeys: plain keys type in text entries; ⌘ chords and Escape still work there', () => {
  const runs: string[] = [];
  const view = render(
    <Bindings
      bindings={[
        { id: 'later', combo: 'l', run: () => runs.push('later') },
        { id: 'next', combo: 'mod+j', run: () => runs.push('next') },
        { id: 'esc', combo: 'escape', run: () => runs.push('esc') },
      ]}
    />,
  );
  try {
    const composer = view.getByLabelText('composer');
    composer.focus();
    fireEvent.keyDown(composer, { code: 'KeyL', key: 'l' });
    assertDeepEqual(runs, [], 'L types into the composer');
    fireEvent.keyDown(composer, { code: 'KeyJ', key: 'j', metaKey: true });
    fireEvent.keyDown(composer, { code: 'Escape', key: 'Escape' });
    assertDeepEqual(runs, ['next', 'esc'], '⌘J and Escape fire in a text entry');
    const button = view.getByText('elsewhere');
    button.focus();
    fireEvent.keyDown(button, { code: 'KeyL', key: 'l' });
    assertDeepEqual(runs, ['next', 'esc', 'later'], 'outside a text entry L fires');
    fireEvent.keyDown(button, { code: 'KeyJ', key: 'j', ctrlKey: true });
    assertEqual(runs.length, 3, 'Ctrl+J is left to whatever has focus');
  } finally {
    resetHotkeysForTests();
  }
});

function Card({ onAnswer }: { onAnswer: (n: string) => void }) {
  const card = useRef<HTMLDivElement>(null);
  useHotkeys(
    [
      { id: 'card.allowOnce', combo: '1', run: () => onAnswer('1') },
      { id: 'card.deny', combo: '3', run: () => onAnswer('3') },
    ],
    { scope: 'C', element: card },
  );
  return (
    <div>
      <div ref={card} role="group" aria-label="approval" tabIndex={0}>
        <button type="button">Allow once</button>
      </div>
      <textarea aria-label="composer" />
      <button type="button">outside</button>
    </div>
  );
}

domTest('hotkeys: digits answer a card only while the card has focus, never in the composer', () => {
  const answers: string[] = [];
  const view = render(<Card onAnswer={(n) => answers.push(n)} />);
  try {
    const outside = view.getByText('outside');
    outside.focus();
    fireEvent.keyDown(outside, { code: 'Digit1', key: '1' });
    assertDeepEqual(answers, [], 'digit with focus outside the card');
    const composer = view.getByLabelText('composer');
    composer.focus();
    fireEvent.keyDown(composer, { code: 'Digit1', key: '1' });
    assertDeepEqual(answers, [], 'a digit typed in the composer never answers');
    const inside = view.getByText('Allow once');
    inside.focus();
    fireEvent.keyDown(inside, { code: 'Digit1', key: '1' });
    assertDeepEqual(answers, ['1'], 'digit on the focused card answers');
    fireEvent.keyDown(inside, { code: 'Digit3', key: '3', repeat: true });
    assertDeepEqual(answers, ['1'], 'a held digit does not repeat an answer');
    fireEvent.keyDown(inside, { code: 'Digit1', key: '1', isComposing: true });
    assertDeepEqual(answers, ['1'], 'nothing during composition');
  } finally {
    resetHotkeysForTests();
  }
});

domTest('hotkeys: an open sheet suspends global bindings and runs its own', () => {
  const runs: string[] = [];
  function App({ open }: { open: boolean }) {
    useHotkeys([{ id: 'palette', combo: 'mod+k', run: () => runs.push('palette') }]);
    return (
      <div>
        <button type="button">invoker</button>
        <Sheet open={open} title="Sheet" onCancel={() => runs.push('cancel')} onConfirm={() => runs.push('confirm')}>
          <p>body</p>
        </Sheet>
      </div>
    );
  }
  const view = render(<App open={false} />);
  try {
    fireEvent.keyDown(document.body, { code: 'KeyK', key: 'k', metaKey: true });
    assertDeepEqual(runs, ['palette'], 'global binding fires with no sheet');
    view.rerender(<App open />);
    const dialog = view.getByRole('dialog');
    fireEvent.keyDown(dialog, { code: 'KeyK', key: 'k', metaKey: true });
    assertDeepEqual(runs, ['palette'], '⌘K is suspended while the sheet is open');
    fireEvent.keyDown(dialog, { code: 'Enter', key: 'Enter', metaKey: true });
    fireEvent.keyDown(dialog, { code: 'Escape', key: 'Escape' });
    assertDeepEqual(runs, ['palette', 'confirm', 'cancel'], 'the sheet keys run');
    view.rerender(<App open={false} />);
    fireEvent.keyDown(document.body, { code: 'KeyK', key: 'k', metaKey: true });
    assertDeepEqual(runs, ['palette', 'confirm', 'cancel', 'palette'], 'global binding back after close');
  } finally {
    resetHotkeysForTests();
  }
});

test('hotkeys: registerHotkeys works outside React and the newest registration wins a tie', () => {
  const runs: string[] = [];
  const offA = registerHotkeys(() => [{ id: 'a', combo: 'mod+j', run: () => runs.push('a') }]);
  const offB = registerHotkeys(() => [{ id: 'b', combo: 'mod+j', run: () => runs.push('b') }]);
  try {
    const event = new KeyboardEvent('keydown', { code: 'KeyJ', key: 'j', metaKey: true, cancelable: true });
    assert(handleHotkey(event), 'handled');
    assertDeepEqual(runs, ['b'], 'the newest of two equal registrations runs');
    assert(event.defaultPrevented, 'a handled key is consumed');
    offB();
    handleHotkey(new KeyboardEvent('keydown', { code: 'KeyJ', key: 'j', metaKey: true }));
    assertDeepEqual(runs, ['b', 'a'], 'after unregistering, the other runs');
  } finally {
    offA();
    offB();
    resetHotkeysForTests();
  }
});
