/**
 * State glyphs follow ux-ia §2.9: decorative, and never colour-only — every
 * colour group is told apart by shape.
 */

import { render } from '@testing-library/react';

import { assert, assertEqual, domTest, test } from '../test/harness';
import { GLYPHS, StateGlyph } from './StateGlyph';
import type { GlyphKind } from './StateGlyph';

test('glyphs: the §2.9 table, and states that share a colour differ in shape where the table says so', () => {
  assertEqual(GLYPHS.needsApproval.shape, 'triangle', 'triangle for needs approval');
  assertEqual(GLYPHS.needsAnswer.shape, 'question', 'question mark for needs answer');
  assertEqual(GLYPHS.noProgress.shape, 'tilde', 'tilde for no progress');
  assertEqual(GLYPHS.error.shape, GLYPHS.crashed.shape, 'the cross for error and crashed');
  assertEqual(GLYPHS.working.tone, 'strong', 'working is --n12');
  assertEqual(GLYPHS.newReply.tone, 'ok', 'new reply is --ok');
  // The three warn states and the info states each have distinct shapes.
  const warn = (['needsApproval', 'needsAnswer', 'noProgress'] as GlyphKind[]).map((k) => GLYPHS[k].shape);
  assertEqual(new Set(warn).size, 3, 'warn states distinct');
  const info = (['starting', 'ready', 'statusUnknown'] as GlyphKind[]).map((k) => GLYPHS[k].shape);
  assertEqual(new Set(info).size, 3, 'info states distinct');
  assert(GLYPHS.leftover.shape !== GLYPHS.error.shape, 'leftover (crossed circle) differs from error (cross)');
});

domTest('glyphs: rendered as decorative SVG with state, shape and tone attributes', () => {
  const view = render(<StateGlyph state="needsApproval" />);
  const svg = view.container.querySelector('svg') as SVGElement;
  assertEqual(svg.getAttribute('aria-hidden'), 'true', 'decorative: the word is the name');
  assertEqual(svg.getAttribute('data-shape'), 'triangle', 'shape');
  assertEqual(svg.getAttribute('data-tone'), 'warn', 'tone');
  assertEqual(svg.getAttribute('focusable'), 'false', 'never focusable');
});
