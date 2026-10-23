/**
 * Responsive rules of the two-zone shell (ux-ia §2.8, M11), as a pure function.
 *
 * Breakpoints are evaluated on the window width **after UI zoom** (ux-ia §10
 * Text scaling): a 1440 px window at 150 % gets the 900–1199 layout instead of
 * clipped text.
 *
 *   ≥ 1200 px    sidebar 300 px, pinned (⌘B hides it)
 *   900–1199 px  sidebar 248 px, pinned, no key hints on the nav rows;
 *                Split, Team and Floor auto-hide it so tiles get the width,
 *                and ⌘B brings it back as an overlay
 *   < 900 px     overlay drawer (⌘B or the menu button); the top bar keeps
 *                "menu {n}" with the Needs-you count
 */

import type { MainView } from '../store/uiSlice';

export const WIDE_MIN = 1200;
export const MEDIUM_MIN = 900;
export const SIDEBAR_WIDE = 300;
export const SIDEBAR_MEDIUM = 248;

export type WidthBand = 'wide' | 'medium' | 'narrow';

export type SidebarMode = 'pinned' | 'overlay';

export interface ShellLayout {
  band: WidthBand;
  /** Width the main zone is laid out in (CSS px after zoom). */
  effectiveWidth: number;
  sidebarWidth: number;
  sidebarMode: SidebarMode;
  /** The sidebar takes space next to the main zone. */
  sidebarPinned: boolean;
  /** The sidebar is on screen (pinned and visible, or an open overlay). */
  sidebarShown: boolean;
  /** Key hints on the sidebar's nav rows (wide only). */
  navHints: boolean;
}

/** Screens that want the full width below 1200 px (ux-ia §2.8). */
const WIDE_SCREENS: ReadonlySet<MainView['screen']> = new Set(['split', 'team', 'floor']);

export function widthBand(effectiveWidth: number): WidthBand {
  if (effectiveWidth >= WIDE_MIN) return 'wide';
  if (effectiveWidth >= MEDIUM_MIN) return 'medium';
  return 'narrow';
}

/** The window width in layout pixels at a UI zoom (0.8–2.0; anything else counts as 1). */
export function effectiveWidth(windowWidth: number, zoom: number): number {
  const z = Number.isFinite(zoom) && zoom > 0 ? zoom : 1;
  const w = Number.isFinite(windowWidth) && windowWidth > 0 ? windowWidth : WIDE_MIN;
  return Math.floor(w / z);
}

/**
 * The layout for a window. `sidebarVisible` is the user's pinned preference
 * (`ui.sidebarVisible`, toggled by ⌘B while pinned); `overlayOpen` is the
 * transient overlay state (⌘B, the menu button) used whenever the sidebar is not pinned.
 */
export function shellLayout(
  windowWidth: number,
  zoom: number,
  screen: MainView['screen'],
  sidebarVisible: boolean,
  overlayOpen: boolean,
): ShellLayout {
  const width = effectiveWidth(windowWidth, zoom);
  const band = widthBand(width);
  const autoHide = band === 'medium' && WIDE_SCREENS.has(screen);
  const sidebarMode: SidebarMode = band === 'narrow' || autoHide ? 'overlay' : 'pinned';
  const sidebarPinned = sidebarMode === 'pinned' && sidebarVisible;
  const sidebarShown = sidebarMode === 'pinned' ? sidebarVisible : overlayOpen;
  return {
    band,
    effectiveWidth: width,
    sidebarWidth: band === 'medium' ? SIDEBAR_MEDIUM : SIDEBAR_WIDE,
    sidebarMode,
    sidebarPinned,
    sidebarShown,
    navHints: band === 'wide',
  };
}
