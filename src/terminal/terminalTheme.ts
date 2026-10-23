/**
 * xterm theme and options, derived from the design tokens at runtime.
 *
 * `styles/tokens.css` is the only file in `src/` allowed to hold a colour
 * literal (UI-GUIDELINES §2-1, enforced by `scripts/lint-ui.sh`), so this
 * module never writes one: it reads the resolved value of `--n0`, `--n11`,
 * `--accent` … off the document element. That also means the terminal follows
 * the theme for free — light, dark and "system" all resolve through the same
 * variables.
 *
 * The sixteen ANSI colours are left to xterm's own defaults. They belong to the
 * *content*, not to the shell's palette, and inventing greys for them would
 * both break the "one neutral scale" rule and recolour what an agent
 * deliberately printed. `minimumContrastRatio: 4.5` keeps that content legible
 * on both grounds instead (the same 4.5:1 floor UI-GUIDELINES §3-8 applies to
 * body text).
 */

import type { ITerminalOptions, ITheme } from '@xterm/xterm';

/** Matches `--mono` in `styles/tokens.css`; used when config names no family. */
const FALLBACK_MONO = "'SF Mono', 'JetBrains Mono', Menlo, monospace";

/**
 * CJK-capable fixed-pitch families, tried before the Latin ones (BACKLOG 17).
 *
 * A terminal grid assumes a Hangul or Han glyph is exactly two cells wide. None
 * of the Latin monospace fonts on macOS contain those glyphs, so the browser
 * substitutes a proportional Korean face whose advance is *not* two cells, and
 * Korean output arrives visibly loose and out of column. These four are the
 * fonts that get it right; the first one installed wins, and each of them is a
 * true monospace whose Latin half-width matches its CJK full-width, so putting
 * them first does not disturb ASCII.
 *
 * Nothing is bundled and no dependency is added: an absent family simply falls
 * through. `docs/PINNED-VERSIONS.md` §28 records that installing D2Coding is
 * the recommended fix on this machine, where none of them is present.
 */
const CJK_MONO = [
  "'D2Coding'",
  "'Sarasa Mono K'",
  "'Sarasa Term K'",
  "'Noto Sans Mono CJK KR'",
].join(', ');

/**
 * The last CJK fallback, and the reason it sits *after* the Latin families
 * rather than before them: Apple SD Gothic Neo ships with every macOS and also
 * carries Latin glyphs, so listing it earlier would hand ASCII to a
 * proportional face on exactly the machines that have no CJK monospace
 * installed — the terminal would lose its grid to fix its Korean. Behind
 * `Menlo` it is only ever reached for the glyphs Menlo does not have.
 */
const CJK_LAST_RESORT = "'Apple SD Gothic Neo'";

/** `monospace` and friends match every glyph, so they may only come last. */
const GENERIC = new Set(['monospace', 'sans-serif', 'serif', 'system-ui', 'ui-monospace']);

/**
 * Build the stack: named families in order, duplicates dropped, generics pushed
 * to the end (a generic in the middle would swallow every glyph after it).
 *
 * `preferCjk` is the difference between the default stack and a configured one.
 * With no setting the CJK monospaces lead, because when one of them is
 * installed it is the best face for the whole grid — its Latin advance is
 * exactly half its CJK advance. A family the user typed into settings leads
 * instead: their choice keeps ASCII, and the CJK monospaces sit behind it for
 * the glyphs it does not carry.
 */
export function withCjkFallback(family: string, preferCjk = true): string {
  const order = preferCjk
    ? [CJK_MONO, family, CJK_LAST_RESORT]
    : [family, CJK_MONO, CJK_LAST_RESORT];
  const seen = new Set<string>();
  const named: string[] = [];
  const generic: string[] = [];
  for (const part of order.join(', ').split(',')) {
    const name = part.trim();
    if (!name) continue;
    const key = name.replace(/['"]/g, '').toLowerCase();
    if (seen.has(key)) continue;
    seen.add(key);
    (GENERIC.has(key) ? generic : named).push(name);
  }
  return [...named, ...generic].join(', ');
}

function cssVar(name: string, fallback = ''): string {
  if (typeof document === 'undefined' || typeof getComputedStyle !== 'function') return fallback;
  const value = getComputedStyle(document.documentElement).getPropertyValue(name).trim();
  return value || fallback;
}

/**
 * The terminal's own colours: surface `--n0`, body text `--n11`, caret
 * `--accent` (the one place blue is allowed besides focus rings and links).
 */
export function readTerminalTheme(): ITheme {
  return {
    background: cssVar('--n0'),
    foreground: cssVar('--n11'),
    cursor: cssVar('--accent'),
    cursorAccent: cssVar('--n0'),
    selectionBackground: cssVar('--n4'),
    selectionInactiveBackground: cssVar('--n3'),
    scrollbarSliderBackground: cssVar('--n5'),
    scrollbarSliderHoverBackground: cssVar('--n6'),
    scrollbarSliderActiveBackground: cssVar('--n7'),
  };
}

/** The terminal's font stack: `--mono` with the CJK monospaces first. */
export function terminalFontFamily(): string {
  return withCjkFallback(cssVar('--mono', FALLBACK_MONO) || FALLBACK_MONO);
}

/** Terminal text size, px (UI-GUIDELINES §2-1 secondary size). */
export const TERMINAL_FONT_SIZE = 12;

/**
 * `scrollback` is xterm's own viewport history and is independent of the core's
 * ring: the ring is the durable truth, this is just how far back the user can
 * scroll without a snapshot.
 */
export const XTERM_SCROLLBACK_LINES = 5000;

export function terminalOptions(): ITerminalOptions {
  return {
    fontFamily: terminalFontFamily(),
    fontSize: TERMINAL_FONT_SIZE,
    lineHeight: 1.2,
    // BACKLOG 17 asked for a `letterSpacing` correction as the fallback for a
    // machine with no CJK monospace installed. It is pinned at 0 instead, on
    // purpose: letter spacing is added to *every* cell, so widening it to close
    // a Hangul gap pushes ASCII apart by the same amount and the grid is wrong
    // in the other direction. The stack above is the fix; §28 says which font
    // to install.
    letterSpacing: 0,
    scrollback: XTERM_SCROLLBACK_LINES,
    cursorBlink: true,
    cursorStyle: 'bar',
    // ⌥ is a meta prefix in every agent TUI; without this macOS inserts
    // accented characters instead of sending ESC-prefixed sequences.
    macOptionIsMeta: true,
    minimumContrastRatio: 4.5,
    drawBoldTextInBrightColors: true,
    allowProposedApi: true,
    theme: readTerminalTheme(),
  };
}

// ---------------------------------------------------------------------------
// Theme changes
// ---------------------------------------------------------------------------

/**
 * Call `onChange` whenever the resolved palette can have changed: the shell
 * stamps `data-theme` on `<html>` for an explicit choice, and "system" follows
 * `prefers-color-scheme`.
 */
export function watchTerminalTheme(onChange: () => void): () => void {
  if (typeof document === 'undefined') return () => {};

  const observer = new MutationObserver(() => onChange());
  observer.observe(document.documentElement, {
    attributes: true,
    attributeFilter: ['data-theme'],
  });

  const media =
    typeof matchMedia === 'function' ? matchMedia('(prefers-color-scheme: dark)') : null;
  const onMedia = () => onChange();
  media?.addEventListener('change', onMedia);

  return () => {
    observer.disconnect();
    media?.removeEventListener('change', onMedia);
  };
}
