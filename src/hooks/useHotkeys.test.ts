/**
 * Hotkey matching tests (T05 acceptance 7): the listed ⌘-combos fire, and
 * nothing fires while a text entry has focus.
 */

import { assert, assertEqual, test } from '../test/harness';
import { codeFor, matchesCombo, parseCombo } from './useHotkeys';

interface FakeKey {
  code: string;
  metaKey?: boolean;
  ctrlKey?: boolean;
  altKey?: boolean;
  shiftKey?: boolean;
}

function key(f: FakeKey): KeyboardEvent {
  return {
    code: f.code,
    metaKey: f.metaKey ?? false,
    ctrlKey: f.ctrlKey ?? false,
    altKey: f.altKey ?? false,
    shiftKey: f.shiftKey ?? false,
  } as KeyboardEvent;
}

test('codeFor maps display keys onto physical codes', () => {
  assertEqual(codeFor('b'), 'KeyB', 'letters');
  assertEqual(codeFor('1'), 'Digit1', 'digits');
  assertEqual(codeFor('backslash'), 'Backslash', 'punctuation alias');
  assertEqual(codeFor('arrowleft'), 'ArrowLeft', 'arrow alias');
});

test('parseCombo reads the modifier set', () => {
  const combo = parseCombo('mod+alt+b');
  assert(combo.mod && combo.alt && !combo.shift, 'mod and alt only');
  assertEqual(combo.code, 'KeyB', 'the key becomes a code');
});

test('the acceptance combos match exactly', () => {
  assert(matchesCombo(key({ code: 'KeyB', metaKey: true }), 'mod+b'), 'cmd+B');
  assert(
    matchesCombo(key({ code: 'KeyB', metaKey: true, altKey: true }), 'mod+alt+b'),
    'cmd+opt+B',
  );
  assert(matchesCombo(key({ code: 'Digit1', metaKey: true }), 'mod+1'), 'cmd+1');
  assert(matchesCombo(key({ code: 'Digit9', metaKey: true }), 'mod+9'), 'cmd+9');
  assert(matchesCombo(key({ code: 'Backslash', metaKey: true }), 'mod+backslash'), 'cmd+backslash');
  assert(
    matchesCombo(key({ code: 'Backslash', metaKey: true, shiftKey: true }), 'mod+shift+backslash'),
    'cmd+shift+backslash',
  );
  assert(matchesCombo(key({ code: 'KeyW', metaKey: true }), 'mod+w'), 'cmd+W');
  assert(matchesCombo(key({ code: 'KeyT', metaKey: true }), 'mod+t'), 'cmd+T');
  assert(matchesCombo(key({ code: 'Slash', metaKey: true }), 'mod+slash'), 'cmd+slash');
});

test('modifier mismatches do not match', () => {
  assert(!matchesCombo(key({ code: 'KeyB' }), 'mod+b'), 'plain B is not cmd+B');
  assert(
    !matchesCombo(key({ code: 'KeyB', metaKey: true, altKey: true }), 'mod+b'),
    'cmd+opt+B is not cmd+B',
  );
  assert(
    !matchesCombo(key({ code: 'Backslash', metaKey: true }), 'mod+shift+backslash'),
    'cmd+backslash is not cmd+shift+backslash',
  );
});

test('option rewrites event.key but never event.code', () => {
  // ⌘⌥B arrives with key "∫" on macOS; matching on `code` is what keeps this working.
  const event = { ...key({ code: 'KeyB', metaKey: true, altKey: true }), key: '∫' } as KeyboardEvent;
  assert(matchesCombo(event, 'mod+alt+b'), 'the rewritten key is irrelevant');
});

test('Ctrl chords are left to the terminal', () => {
  assert(
    !matchesCombo(key({ code: 'KeyB', ctrlKey: true }), 'mod+b'),
    'ctrl+B is not the app shortcut — a TUI owns it',
  );
  assert(
    !matchesCombo(key({ code: 'KeyW', ctrlKey: true, metaKey: true }), 'mod+w'),
    'a stray ctrl disqualifies the match',
  );
});
