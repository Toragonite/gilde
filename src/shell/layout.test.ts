/**
 * Responsive rules [M11, ux-ia §2.8]: breakpoints after zoom, the 300/248 px
 * sidebar, auto-hide for Split/Team/Floor below 1200 px, the overlay drawer.
 */

import { assertEqual, test } from '../test/harness';
import { effectiveWidth, shellLayout, widthBand } from './layout';

test('M11: ≥ 1200 wide, 900–1199 medium, < 900 narrow', () => {
  assertEqual(widthBand(1440), 'wide', '1440');
  assertEqual(widthBand(1200), 'wide', '1200 is wide');
  assertEqual(widthBand(1199), 'medium', '1199');
  assertEqual(widthBand(900), 'medium', '900 is medium');
  assertEqual(widthBand(899), 'narrow', '899');
  assertEqual(widthBand(760), 'narrow', '760');
});

test('M11: breakpoints are evaluated after UI zoom (1440 px at 150 % is medium)', () => {
  assertEqual(effectiveWidth(1440, 1.5), 960, '1440 / 1.5');
  assertEqual(shellLayout(1440, 1.5, 'home', true, false).band, 'medium', 'zoomed layout');
  assertEqual(shellLayout(1440, 2, 'home', true, false).band, 'narrow', '200 %');
  assertEqual(effectiveWidth(1440, Number.NaN), 1440, 'a broken zoom counts as 100 %');
});

test('M11: the sidebar is pinned at 300 px wide and 248 px medium; ⌘B hides it when pinned', () => {
  const wide = shellLayout(1440, 1, 'home', true, false);
  assertEqual(wide.sidebarMode, 'pinned', 'pinned');
  assertEqual(wide.sidebarWidth, 300, '300 px');
  assertEqual(wide.sidebarShown, true, 'shown');
  assertEqual(wide.navHints, true, 'key hints when wide');
  const medium = shellLayout(1000, 1, 'session', true, false);
  assertEqual(medium.sidebarWidth, 248, '248 px');
  assertEqual(medium.navHints, false, 'no key hints at 900–1199');
  const hidden = shellLayout(1440, 1, 'home', false, false);
  assertEqual(hidden.sidebarShown, false, 'hidden by the user');
  assertEqual(hidden.sidebarPinned, false, 'takes no space');
});

test('M11: Split, Team and Floor auto-hide the sidebar below 1200 px; ⌘B brings it back as an overlay', () => {
  for (const screen of ['split', 'team', 'floor'] as const) {
    const l = shellLayout(1000, 1, screen, true, false);
    assertEqual(l.sidebarMode, 'overlay', `${screen} medium is overlay`);
    assertEqual(l.sidebarShown, false, `${screen} starts hidden`);
    assertEqual(shellLayout(1000, 1, screen, true, true).sidebarShown, true, `${screen} overlay open`);
    assertEqual(shellLayout(1300, 1, screen, true, false).sidebarMode, 'pinned', `${screen} wide stays pinned`);
  }
});

test('M11: below 900 px the sidebar is always an overlay drawer', () => {
  const l = shellLayout(800, 1, 'home', true, false);
  assertEqual(l.sidebarMode, 'overlay', 'drawer');
  assertEqual(l.sidebarShown, false, 'closed until asked');
  assertEqual(l.sidebarPinned, false, 'never pinned');
  assertEqual(shellLayout(800, 1, 'home', true, true).sidebarShown, true, 'opened with ⌘B or the menu button');
});
