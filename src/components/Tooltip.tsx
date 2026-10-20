/**
 * Hover tooltip. Used for the reason a control is disabled, a masked argv, or a
 * full path behind an ellipsis — never for decoration.
 *
 * Rendered into a body portal so it is not clipped by a scrolling panel.
 */

import { useCallback, useRef, useState } from 'react';
import type { ReactNode } from 'react';
import { createPortal } from 'react-dom';

export interface TooltipProps {
  label: string;
  children: ReactNode;
  /** Delay before showing, in ms. */
  delayMs?: number;
}

export function Tooltip({ label, children, delayMs = 350 }: TooltipProps) {
  const [position, setPosition] = useState<{ x: number; y: number } | null>(null);
  const timer = useRef<ReturnType<typeof setTimeout> | null>(null);
  const anchor = useRef<HTMLSpanElement>(null);

  const show = useCallback(() => {
    if (timer.current) clearTimeout(timer.current);
    timer.current = setTimeout(() => {
      const rect = anchor.current?.getBoundingClientRect();
      if (rect) setPosition({ x: rect.left, y: rect.bottom + 4 });
    }, delayMs);
  }, [delayMs]);

  const hide = useCallback(() => {
    if (timer.current) clearTimeout(timer.current);
    timer.current = null;
    setPosition(null);
  }, []);

  return (
    <span
      ref={anchor}
      onMouseEnter={show}
      onMouseLeave={hide}
      onFocus={show}
      onBlur={hide}
      className="tooltip-anchor"
    >
      {children}
      {position && label
        ? createPortal(
            <div className="tooltip" style={{ left: position.x, top: position.y }} role="tooltip">
              {label}
            </div>,
            document.body,
          )
        : null}
    </span>
  );
}
