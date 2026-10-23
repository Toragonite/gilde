/**
 * A chip's menu inside the New session sheet (ux-ia §3.12 "Chip menus").
 *
 * It renders inline in the sheet, not in a portal: the sheet traps focus and
 * keys, and a portal would sit outside both. Focus moves to the checked item
 * when the menu opens (the user opened it); `↑↓ Home End` move, `⏎`/`Space`
 * pick, `Esc` closes the menu only — never the sheet (the menu's binding is
 * deeper than the sheet's, so it wins) — and returns focus to the chip.
 */

import { useEffect, useRef } from 'react';
import type { KeyboardEvent as ReactKeyboardEvent, ReactNode } from 'react';

import { isImeComposing, useHotkeys } from '../hooks/useHotkeys';

export interface ChipMenuItem {
  key: string;
  label: string;
  sub?: string | null;
  /** A choice (`menuitemradio`); `undefined` = an action (`menuitem`). */
  checked?: boolean;
  disabled?: boolean;
  onSelect: () => void;
}

export interface ChipMenuProps {
  id: string;
  label: string;
  items: readonly ChipMenuItem[];
  onClose: () => void;
  /** Controls under the list (the worktree name field). */
  extra?: ReactNode;
}

const ITEM = '[role="menuitemradio"]:not([disabled]),[role="menuitem"]:not([disabled])';

export function ChipMenu({ id, label, items, onClose, extra }: ChipMenuProps) {
  const wrap = useRef<HTMLDivElement>(null);
  const list = useRef<HTMLDivElement>(null);

  useEffect(() => {
    const el = list.current;
    if (!el) return;
    const target = el.querySelector<HTMLElement>('[aria-checked="true"]:not([disabled])') ?? el.querySelector<HTMLElement>(ITEM);
    target?.focus();
    // Focus moves in once, when the menu opens.
  }, []);

  useHotkeys([{ id: 'newSession.menu.close', combo: 'escape', allowInTextEntry: true, run: () => onClose() }], {
    scope: 'N',
    element: wrap,
    requireFocus: true,
  });

  const onKeyDown = (event: ReactKeyboardEvent<HTMLDivElement>) => {
    if (isImeComposing(event.nativeEvent) || event.metaKey || event.ctrlKey || event.altKey) return;
    const all = Array.from(list.current?.querySelectorAll<HTMLElement>(ITEM) ?? []);
    if (all.length === 0) return;
    const at = all.indexOf(document.activeElement as HTMLElement);
    let next = -1;
    if (event.code === 'ArrowDown') next = at < 0 ? 0 : Math.min(all.length - 1, at + 1);
    else if (event.code === 'ArrowUp') next = at <= 0 ? 0 : at - 1;
    else if (event.code === 'Home') next = 0;
    else if (event.code === 'End') next = all.length - 1;
    if (next < 0) return;
    event.preventDefault();
    all[next]?.focus();
  };

  return (
    <div className="ns-menu" ref={wrap}>
      <div role="menu" id={id} aria-label={label} ref={list} onKeyDown={onKeyDown} className="ns-menu-list">
        {items.map((item) => (
          <button
            key={item.key}
            type="button"
            role={item.checked === undefined ? 'menuitem' : 'menuitemradio'}
            aria-checked={item.checked === undefined ? undefined : item.checked}
            disabled={item.disabled}
            tabIndex={-1}
            className="ns-menu-item"
            onClick={item.onSelect}
          >
            <span className="ns-menu-mark" aria-hidden="true" data-on={item.checked ? 'true' : undefined} data-kind={item.checked === undefined ? 'action' : 'radio'} />
            <span className="ns-menu-label">{item.label}</span>
            {item.sub ? <span className="ns-menu-sub">{item.sub}</span> : null}
          </button>
        ))}
      </div>
      {extra}
    </div>
  );
}
