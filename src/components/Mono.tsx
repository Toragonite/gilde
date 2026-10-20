/**
 * Monospace, tabular-numeric text: pids, paths, byte counts, elapsed clocks.
 * Numbers that sit in a column must line up (UI-GUIDELINES §2-1).
 */

import type { ReactNode } from 'react';

export interface MonoProps {
  children: ReactNode;
  className?: string;
  title?: string;
  /** `11px` for dense table cells, `12px` (default) elsewhere. */
  small?: boolean;
  /** Truncate with an ellipsis instead of wrapping. */
  ellipsis?: boolean;
}

export function Mono({ children, className, title, small, ellipsis }: MonoProps) {
  const classes = ['mono', small ? 's11' : 's12', ellipsis ? 'ell' : '', className ?? '']
    .filter(Boolean)
    .join(' ');
  return (
    <span className={classes} title={title}>
      {children}
    </span>
  );
}
