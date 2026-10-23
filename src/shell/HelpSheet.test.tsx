/**
 * The keyboard help sheet is generated from the KEYMAP (ux-ia §7.1 Help):
 * every binding has a label in both languages, every scope a title, and the
 * sheet renders each binding once.
 */

import { render } from '@testing-library/react';

import { KEYMAP } from '../hooks/useHotkeys';
import { translate } from '../i18n';
import type { MessageKey } from '../i18n';
import { assert, assertEqual, domTest, test } from '../test/harness';
import { HELP_SCOPES, HelpSheet, helpGroups, helpLabelKey } from './HelpSheet';
import { resetStore } from './testing';

// Floor rows another area adds (A23.4) bring their own labels in that area.
const LABELLED_ELSEWHERE = new Set(['floor.replay', 'floor.stepBack', 'floor.stepForward']);

test('every KEYMAP binding has a help label (keys.<id>) in Korean and English', () => {
  for (const entry of KEYMAP) {
    const key = helpLabelKey(entry.id);
    if (key === null) {
      assert(LABELLED_ELSEWHERE.has(entry.id), `keys.${entry.id} is missing`);
      continue;
    }
    assert(translate('ko', key) !== key, `ko label for ${entry.id}`);
    assert(translate('en', key) !== key, `en label for ${entry.id}`);
  }
});

test('every scope with a binding has a title, and each binding is listed once', () => {
  const groups = helpGroups();
  for (const g of groups) assert(HELP_SCOPES.includes(g.scope), `scope ${g.scope} is ordered`);
  for (const scope of HELP_SCOPES) {
    const key = `help.scope.${scope}` as MessageKey;
    assert(translate('en', key) !== key, `title for ${scope}`);
  }
  const listed = groups.reduce((n, g) => n + g.entries.length, 0);
  assertEqual(listed, KEYMAP.length, 'one row per binding');
});

domTest('the help sheet is a dialog listing the bindings', () => {
  resetStore();
  const view = render(<HelpSheet onClose={() => {}} />);
  const dialog = view.getByRole('dialog');
  assert(dialog.textContent?.includes('Palette: go to anything') ?? false, 'a label renders');
  assert(dialog.textContent?.includes('⌘K') ?? false, 'its combo renders');
});
