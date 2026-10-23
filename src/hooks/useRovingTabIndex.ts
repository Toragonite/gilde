/**
 * Roving tabindex (ux-ia §7.5, §10): a list or tree is one Tab stop; the arrow
 * keys, Home and End move focus inside it. Used by the sidebar tree, the queue,
 * Board columns and the Floor's accessible twin.
 *
 * Only the item at `activeIndex` has `tabIndex=0`; every other item has -1. A
 * click or a focus that lands on an item makes it the active one, so Tab
 * returns to where the user left. Keys are matched on `event.code` and ignored
 * during IME composition and with ⌘/Ctrl/⌥ held (those belong to hotkeys).
 */

import { useCallback, useRef, useState } from 'react';
import type { KeyboardEvent as ReactKeyboardEvent } from 'react';

import { isImeComposing } from './useHotkeys';

export type RovingOrientation = 'vertical' | 'horizontal' | 'both';

export interface RovingOptions {
  /** Which arrows move: vertical ↑↓ (default; trees keep ←→ for expand/collapse), horizontal ←→, or both. */
  orientation?: RovingOrientation;
  /** Wrap from the last item to the first. Default false. */
  loop?: boolean;
  initialIndex?: number;
  /** Called when the active item changes through the keyboard or focus. */
  onActiveChange?: (index: number) => void;
}

export interface RovingItemProps {
  tabIndex: 0 | -1;
  ref: (el: HTMLElement | null) => void;
  onKeyDown: (event: ReactKeyboardEvent<HTMLElement>) => void;
  onFocus: () => void;
  'data-roving-index': number;
}

export interface Roving {
  activeIndex: number;
  setActiveIndex: (index: number, focus?: boolean) => void;
  getItemProps: (index: number) => RovingItemProps;
}

function clamp(i: number, count: number): number {
  if (count <= 0) return 0;
  return Math.min(Math.max(i, 0), count - 1);
}

export function useRovingTabIndex(count: number, options: RovingOptions = {}): Roving {
  const { orientation = 'vertical', loop = false, initialIndex = 0, onActiveChange } = options;
  const [active, setActive] = useState(() => clamp(initialIndex, count));
  const items = useRef(new Map<number, HTMLElement>());
  const activeIndex = clamp(active, count);

  const setActiveIndex = useCallback(
    (index: number, focus = false) => {
      const next = clamp(index, count);
      setActive(next);
      onActiveChange?.(next);
      if (focus) items.current.get(next)?.focus();
    },
    [count, onActiveChange],
  );

  const move = useCallback(
    (from: number, delta: number) => {
      if (count === 0) return;
      let next = from + delta;
      if (loop) next = (next + count) % count;
      setActiveIndex(clamp(next, count), true);
    },
    [count, loop, setActiveIndex],
  );

  const getItemProps = useCallback(
    (index: number): RovingItemProps => ({
      tabIndex: index === activeIndex ? 0 : -1,
      ref: (el: HTMLElement | null) => {
        if (el) items.current.set(index, el);
        else items.current.delete(index);
      },
      onFocus: () => {
        if (index !== activeIndex) {
          setActive(index);
          onActiveChange?.(index);
        }
      },
      onKeyDown: (event: ReactKeyboardEvent<HTMLElement>) => {
        if (isImeComposing(event.nativeEvent) || event.metaKey || event.ctrlKey || event.altKey) return;
        const vertical = orientation !== 'horizontal';
        const horizontal = orientation !== 'vertical';
        let handled = true;
        switch (event.code) {
          case 'ArrowDown':
            if (vertical) move(index, 1);
            else handled = false;
            break;
          case 'ArrowUp':
            if (vertical) move(index, -1);
            else handled = false;
            break;
          case 'ArrowRight':
            if (horizontal) move(index, 1);
            else handled = false;
            break;
          case 'ArrowLeft':
            if (horizontal) move(index, -1);
            else handled = false;
            break;
          case 'Home':
            setActiveIndex(0, true);
            break;
          case 'End':
            setActiveIndex(count - 1, true);
            break;
          default:
            handled = false;
        }
        if (handled) event.preventDefault();
      },
      'data-roving-index': index,
    }),
    [activeIndex, count, move, onActiveChange, orientation, setActiveIndex],
  );

  return { activeIndex, setActiveIndex, getItemProps };
}
