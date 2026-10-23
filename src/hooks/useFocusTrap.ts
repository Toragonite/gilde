/**
 * Focus trap and restore for modal surfaces (ux-ia §7.5: "Sheets trap focus
 * and restore it to the invoker on close").
 *
 * While active: focus moves into the container (the `initialFocus` element,
 * else the first enabled `[data-autofocus]`, else the first enabled action
 * button, else the first focusable, else the container itself); Tab and
 * Shift+Tab cycle inside; focus that escapes (a click on the page behind) is
 * brought back. Only the innermost active trap acts, so a sheet over a sheet
 * works. On deactivation focus returns to the element that had it before.
 */

import { useEffect } from 'react';
import type { RefObject } from 'react';

const FOCUSABLE = [
  'a[href]',
  'button:not([disabled])',
  'input:not([disabled]):not([type="hidden"])',
  'select:not([disabled])',
  'textarea:not([disabled])',
  '[tabindex]:not([tabindex="-1"])',
  '[contenteditable="true"]',
].join(',');

function isHidden(el: HTMLElement, root: HTMLElement): boolean {
  for (let n: HTMLElement | null = el; n && n !== root; n = n.parentElement) {
    if (n.hidden || n.getAttribute('aria-hidden') === 'true' || n.hasAttribute('inert')) return true;
  }
  return false;
}

/** Focusable descendants in document order. */
export function focusableIn(root: HTMLElement): HTMLElement[] {
  return Array.from(root.querySelectorAll<HTMLElement>(FOCUSABLE)).filter((el) => !isHidden(el, root));
}

function enabled(el: HTMLElement): boolean {
  return !(el as Partial<HTMLButtonElement>).disabled && el.getAttribute('aria-disabled') !== 'true';
}

/** Where focus lands when the container opens. */
export function initialFocusTarget(root: HTMLElement, preferred?: HTMLElement | null): HTMLElement {
  if (preferred && root.contains(preferred) && enabled(preferred)) return preferred;
  const marked = Array.from(root.querySelectorAll<HTMLElement>('[data-autofocus]')).find(enabled);
  if (marked) return marked;
  const action = Array.from(root.querySelectorAll<HTMLElement>('[data-sheet-actions] button')).find(enabled);
  if (action) return action;
  const first = focusableIn(root).find(enabled);
  if (first) return first;
  return root;
}

const stack: HTMLElement[] = [];

export interface FocusTrapOptions {
  initialFocus?: RefObject<HTMLElement | null>;
  /** Default true. */
  restoreFocus?: boolean;
}

export function useFocusTrap(container: RefObject<HTMLElement | null>, active: boolean, options: FocusTrapOptions = {}): void {
  const { initialFocus, restoreFocus = true } = options;
  useEffect(() => {
    const root = container.current;
    if (!active || !root) return;
    const invoker = document.activeElement instanceof HTMLElement ? document.activeElement : null;
    stack.push(root);
    if (!root.hasAttribute('tabindex')) root.setAttribute('tabindex', '-1');
    initialFocusTarget(root, initialFocus?.current ?? null).focus();

    const isTop = () => stack[stack.length - 1] === root;

    const onKeyDown = (event: KeyboardEvent) => {
      if (event.code !== 'Tab' || !isTop() || event.metaKey || event.ctrlKey || event.altKey) return;
      const items = focusableIn(root);
      if (items.length === 0) {
        event.preventDefault();
        root.focus();
        return;
      }
      const first = items[0] as HTMLElement;
      const last = items[items.length - 1] as HTMLElement;
      const current = document.activeElement;
      if (event.shiftKey && (current === first || current === root || !root.contains(current))) {
        event.preventDefault();
        last.focus();
      } else if (!event.shiftKey && (current === last || !root.contains(current))) {
        event.preventDefault();
        first.focus();
      }
    };

    const onFocusIn = (event: FocusEvent) => {
      if (!isTop()) return;
      const target = event.target;
      if (target instanceof Node && root.contains(target)) return;
      // Something outside took focus (a click behind the sheet): bring it back.
      initialFocusTarget(root).focus();
    };

    document.addEventListener('keydown', onKeyDown, true);
    document.addEventListener('focusin', onFocusIn);
    return () => {
      document.removeEventListener('keydown', onKeyDown, true);
      document.removeEventListener('focusin', onFocusIn);
      const i = stack.lastIndexOf(root);
      if (i >= 0) stack.splice(i, 1);
      if (restoreFocus && invoker && invoker.isConnected) invoker.focus();
    };
    // The initial-focus ref is read once, on activation.
  }, [active, container, restoreFocus]);
}
