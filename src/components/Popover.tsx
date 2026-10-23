/**
 * Popover (ux-ia §3 Details popover, row menus, the health popover).
 *
 * Non-modal: it does not trap focus. Opening moves focus into it (its first
 * focusable, or the popover itself); `Esc` closes it and returns focus to the
 * trigger; a pointer press outside it and its trigger, or focus leaving both,
 * closes it. The trigger gets `aria-expanded`/`aria-controls`/`aria-haspopup`
 * from `popoverTriggerProps`. With `role="menu"` the arrow keys, Home and End
 * move between its `role="menuitem"` children.
 *
 * Placement is below the trigger, flipped above when it would leave the
 * window, and kept inside the window horizontally.
 */

import { useCallback, useEffect, useLayoutEffect, useRef, useState } from 'react';
import type { FocusEvent as ReactFocusEvent, KeyboardEvent as ReactKeyboardEvent, ReactNode, RefObject } from 'react';
import { createPortal } from 'react-dom';

import { focusableIn } from '../hooks/useFocusTrap';
import { isImeComposing } from '../hooks/useHotkeys';
import './components.css';

export type PopoverCloseReason = 'escape' | 'outside' | 'blur';

export interface PopoverProps {
  open: boolean;
  /** The element the popover hangs from (its trigger). */
  anchor: RefObject<HTMLElement | null>;
  onClose: (reason: PopoverCloseReason) => void;
  /** `dialog` (default) or `menu`. */
  role?: 'dialog' | 'menu';
  /** Must match the trigger's `aria-controls`. */
  id: string;
  /** Accessible name, unless `labelledBy` is given. */
  label?: string;
  labelledBy?: string;
  children: ReactNode;
  className?: string;
  /** Gap between trigger and popover, px. */
  offset?: number;
}

/** Spread on the trigger button. */
export function popoverTriggerProps(open: boolean, id: string, role: 'dialog' | 'menu' = 'dialog') {
  return {
    'aria-expanded': open,
    'aria-controls': open ? id : undefined,
    'aria-haspopup': role,
  } as const;
}

export interface PopoverPosition {
  top: number;
  left: number;
  placement: 'below' | 'above';
}

/** Pure placement rule (tested without layout). */
export function placePopover(
  anchor: { top: number; bottom: number; left: number },
  size: { width: number; height: number },
  viewport: { width: number; height: number },
  offset = 4,
  margin = 8,
): PopoverPosition {
  let placement: PopoverPosition['placement'] = 'below';
  let top = anchor.bottom + offset;
  if (top + size.height > viewport.height - margin && anchor.top - offset - size.height >= margin) {
    placement = 'above';
    top = anchor.top - offset - size.height;
  }
  const maxLeft = Math.max(margin, viewport.width - size.width - margin);
  const left = Math.min(Math.max(anchor.left, margin), maxLeft);
  return { top, left, placement };
}

export function Popover(props: PopoverProps) {
  if (!props.open) return null;
  return <OpenPopover {...props} />;
}

function OpenPopover({
  anchor,
  onClose,
  role = 'dialog',
  id,
  label,
  labelledBy,
  children,
  className,
  offset = 4,
}: PopoverProps) {
  const panel = useRef<HTMLDivElement>(null);
  const [position, setPosition] = useState<PopoverPosition | null>(null);
  const closeRef = useRef(onClose);
  closeRef.current = onClose;

  useLayoutEffect(() => {
    const a = anchor.current;
    const p = panel.current;
    if (!a || !p) return;
    const r = a.getBoundingClientRect();
    setPosition(
      placePopover(
        { top: r.top, bottom: r.bottom, left: r.left },
        { width: p.offsetWidth, height: p.offsetHeight },
        { width: window.innerWidth, height: window.innerHeight },
        offset,
      ),
    );
  }, [anchor, offset]);

  // Focus moves in on open; Esc returns it to the trigger.
  useEffect(() => {
    const p = panel.current;
    if (!p) return;
    const first = role === 'menu' ? p.querySelector<HTMLElement>('[role="menuitem"]') : focusableIn(p)[0];
    (first ?? p).focus();
  }, [role]);

  // A press outside both the popover and its trigger closes it.
  useEffect(() => {
    const onPointerDown = (event: Event) => {
      const target = event.target;
      if (!(target instanceof Node)) return;
      if (panel.current?.contains(target) || anchor.current?.contains(target)) return;
      closeRef.current('outside');
    };
    document.addEventListener('mousedown', onPointerDown, true);
    document.addEventListener('pointerdown', onPointerDown, true);
    return () => {
      document.removeEventListener('mousedown', onPointerDown, true);
      document.removeEventListener('pointerdown', onPointerDown, true);
    };
  }, [anchor]);

  const onKeyDown = useCallback(
    (event: ReactKeyboardEvent<HTMLDivElement>) => {
      if (isImeComposing(event.nativeEvent)) return;
      if (event.code === 'Escape' && !event.metaKey && !event.ctrlKey) {
        event.preventDefault();
        event.stopPropagation();
        closeRef.current('escape');
        anchor.current?.focus();
        return;
      }
      if (role !== 'menu' || event.metaKey || event.ctrlKey || event.altKey) return;
      const items = Array.from(panel.current?.querySelectorAll<HTMLElement>('[role="menuitem"]') ?? []);
      if (items.length === 0) return;
      const i = items.indexOf(document.activeElement as HTMLElement);
      let next = -1;
      if (event.code === 'ArrowDown') next = i < 0 ? 0 : (i + 1) % items.length;
      else if (event.code === 'ArrowUp') next = i <= 0 ? items.length - 1 : i - 1;
      else if (event.code === 'Home') next = 0;
      else if (event.code === 'End') next = items.length - 1;
      if (next >= 0) {
        event.preventDefault();
        items[next]?.focus();
      }
    },
    [anchor, role],
  );

  const onBlur = useCallback(
    (event: ReactFocusEvent<HTMLDivElement>) => {
      const next = event.relatedTarget;
      if (next === null) return; // focus went nowhere (window blur): keep it open
      if (next instanceof Node && (panel.current?.contains(next) || anchor.current?.contains(next))) return;
      closeRef.current('blur');
    },
    [anchor],
  );

  return createPortal(
    <div
      ref={panel}
      id={id}
      role={role}
      aria-label={labelledBy ? undefined : label}
      aria-labelledby={labelledBy}
      tabIndex={-1}
      className={['g-popover', className ?? ''].filter(Boolean).join(' ')}
      data-placement={position?.placement ?? 'below'}
      style={position ? { top: position.top, left: position.left } : { visibility: 'hidden' }}
      onKeyDown={onKeyDown}
      onBlur={onBlur}
    >
      {children}
    </div>,
    document.body,
  );
}
