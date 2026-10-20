/**
 * Modal sheet. Destructive actions state their target and their consequence in
 * one line and focus Cancel by default (UI-GUIDELINES §2-5) — no bare
 * `confirm()` anywhere in this app.
 *
 * A non-destructive dialog can override that default by marking one control
 * `data-default-focus="true"` (M1.5 BACKLOG 14): the new-session dialog exists
 * to open a session, so Enter should open one rather than cancel. The override
 * is ignored while that control is disabled, so a form that cannot be submitted
 * yet still falls back to Cancel instead of focusing a dead button.
 *
 * The marker is read **anywhere in the dialog, body included** (M1.5 BACKLOG
 * 15c). A form that cannot be submitted yet has a reason — a field nobody has
 * filled in — and that field is where the user has to go next, so the dialog
 * moves the marker onto it rather than parking the focus on Cancel and leaving
 * the caret to be moved by a timer racing the first render.
 */

import { useEffect, useRef } from 'react';
import type { ReactNode } from 'react';
import { createPortal } from 'react-dom';

/** The bit of a candidate `sheetFocusIndex` needs — kept tiny to test on Node. */
export interface FocusCandidate {
  marked: boolean;
  disabled: boolean;
}

/**
 * Which of a sheet's focusable controls opens focused: the first enabled
 * `data-default-focus` control, else the first enabled action (Cancel, by the
 * convention that the cancel button is rendered first). `-1` when there is
 * nothing to focus. Pure, so the rule is tested without a DOM.
 *
 * The candidates are given in document order — body first, then the action row
 * — so a dialog that marks a field the user still has to fill in wins over one
 * that also marked its primary. In practice exactly one control carries the
 * marker at a time; this is what happens if that ever stops being true.
 */
export function sheetFocusIndex(candidates: readonly FocusCandidate[]): number {
  const marked = candidates.findIndex((c) => c.marked && !c.disabled);
  if (marked >= 0) return marked;
  return candidates.findIndex((c) => !c.disabled);
}

export interface SheetProps {
  open: boolean;
  title: string;
  children: ReactNode;
  /** Buttons, rendered right-aligned. Cancel goes first and takes focus. */
  actions?: ReactNode;
  onClose: () => void;
  labelledBy?: string;
}

export function Sheet({ open, title, children, actions, onClose, labelledBy }: SheetProps) {
  const container = useRef<HTMLDivElement>(null);

  useEffect(() => {
    if (!open) return;
    // Default focus lands on the first action — Cancel, by convention — unless
    // the dialog marked another control as its default (BACKLOG 14, 15c). The
    // marked control may be a field in the body, so both are queried, in
    // document order.
    const candidates = Array.from(
      container.current?.querySelectorAll<HTMLElement>(
        '[data-default-focus="true"], .sheet-actions button',
      ) ?? [],
    );
    const index = sheetFocusIndex(
      candidates.map((el) => ({
        marked: el.getAttribute('data-default-focus') === 'true',
        disabled: (el as Partial<HTMLButtonElement>).disabled === true,
      })),
    );
    if (index >= 0) candidates[index].focus();
  }, [open]);

  useEffect(() => {
    if (!open) return;
    const onKeyDown = (event: KeyboardEvent) => {
      if (event.key === 'Escape') {
        event.stopPropagation();
        onClose();
      }
    };
    window.addEventListener('keydown', onKeyDown);
    return () => window.removeEventListener('keydown', onKeyDown);
  }, [open, onClose]);

  if (!open) return null;

  return createPortal(
    <>
      <div className="sheet-backdrop" onClick={onClose} />
      <div className="sheet-layer">
        <div
          className="sheet"
          role="dialog"
          aria-modal="true"
          aria-label={labelledBy ? undefined : title}
          aria-labelledby={labelledBy}
          ref={container}
        >
          <div className="sheet-title">{title}</div>
          <div className="sheet-body">{children}</div>
          {actions ? <div className="sheet-actions">{actions}</div> : null}
        </div>
      </div>
    </>,
    document.body,
  );
}
