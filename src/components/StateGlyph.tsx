/**
 * State glyphs (ux-ia §2.9, §10): the shape that goes with every state word.
 *
 * Colour never stands alone — each state has a distinct shape (dashed ring,
 * ring with a centre, triangle, question mark, cross, tilde, filled dot, open
 * dot, crossed circle, dotted circle), so the states stay apart for colour-
 * blind users, and the screens always print the state word next to it. The
 * glyph itself is decorative (`aria-hidden`): the word is the accessible name.
 * Working pulses slowly and Starting turns, except under reduced motion.
 */

import type { ActivityState } from '../lib/gen/ActivityState';
import './components.css';

export type GlyphKind = ActivityState['kind'] | 'leftover';

export type GlyphShape =
  | 'dashedRing'
  | 'ringDot'
  | 'triangle'
  | 'question'
  | 'cross'
  | 'tilde'
  | 'filledDot'
  | 'openDot'
  | 'crossedCircle'
  | 'centreDot';

export type GlyphTone = 'info' | 'strong' | 'warn' | 'error' | 'ok';

/** The table of ux-ia §2.9. */
export const GLYPHS: Readonly<Record<GlyphKind, { shape: GlyphShape; tone: GlyphTone }>> = {
  starting: { shape: 'dashedRing', tone: 'info' },
  ended: { shape: 'dashedRing', tone: 'info' },
  stopped: { shape: 'dashedRing', tone: 'info' },
  working: { shape: 'ringDot', tone: 'strong' },
  needsApproval: { shape: 'triangle', tone: 'warn' },
  needsAnswer: { shape: 'question', tone: 'warn' },
  error: { shape: 'cross', tone: 'error' },
  crashed: { shape: 'cross', tone: 'error' },
  noProgress: { shape: 'tilde', tone: 'warn' },
  newReply: { shape: 'filledDot', tone: 'ok' },
  ready: { shape: 'openDot', tone: 'info' },
  leftover: { shape: 'crossedCircle', tone: 'error' },
  statusUnknown: { shape: 'centreDot', tone: 'info' },
};

function Shape({ shape }: { shape: GlyphShape }) {
  switch (shape) {
    case 'dashedRing':
      return <circle cx="8" cy="8" r="5.5" fill="none" strokeDasharray="2.4 2" />;
    case 'ringDot':
      return (
        <>
          <circle cx="8" cy="8" r="5.5" fill="none" />
          <circle cx="8" cy="8" r="2.5" stroke="none" fill="currentColor" />
        </>
      );
    case 'triangle':
      return <path d="M8 2.5 14 13.5H2Z" fill="currentColor" stroke="none" />;
    case 'question':
      return (
        <>
          <path d="M5.5 5.8a2.6 2.6 0 1 1 3.6 2.4c-.7.3-1.1.9-1.1 1.6v.6" fill="none" />
          <circle cx="8" cy="13" r="1" stroke="none" fill="currentColor" />
        </>
      );
    case 'cross':
      return <path d="M3.5 3.5 12.5 12.5M12.5 3.5 3.5 12.5" fill="none" />;
    case 'tilde':
      return <path d="M2.5 9.2c1.4-2.4 3-2.4 4.4-.9s3 1.5 4.6-.9" fill="none" />;
    case 'filledDot':
      return <circle cx="8" cy="8" r="4.5" stroke="none" fill="currentColor" />;
    case 'openDot':
      return <circle cx="8" cy="8" r="4.5" fill="none" />;
    case 'crossedCircle':
      return (
        <>
          <circle cx="8" cy="8" r="5.5" fill="none" />
          <path d="M4.2 4.2 11.8 11.8M11.8 4.2 4.2 11.8" fill="none" />
        </>
      );
    case 'centreDot':
      return (
        <>
          <circle cx="8" cy="8" r="5.5" fill="none" strokeDasharray="2.4 2" />
          <circle cx="8" cy="8" r="1.6" stroke="none" fill="currentColor" />
        </>
      );
  }
}

export interface StateGlyphProps {
  state: GlyphKind;
  /** Low-confidence states are drawn lighter (the word adds "likely"). */
  likely?: boolean;
  className?: string;
}

export function StateGlyph({ state, likely, className }: StateGlyphProps) {
  const glyph = GLYPHS[state] ?? GLYPHS.statusUnknown;
  return (
    <svg
      className={['g-glyph', className ?? ''].filter(Boolean).join(' ')}
      viewBox="0 0 16 16"
      width="16"
      height="16"
      aria-hidden="true"
      focusable="false"
      data-state={state}
      data-shape={glyph.shape}
      data-tone={glyph.tone}
      data-likely={likely ? 'true' : undefined}
      stroke="currentColor"
      strokeWidth="1.5"
      strokeLinecap="round"
      strokeLinejoin="round"
    >
      <Shape shape={glyph.shape} />
    </svg>
  );
}
