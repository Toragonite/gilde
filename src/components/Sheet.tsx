/**
 * Modal sheet (ux-ia §7.1, §7.5, §10; UI-GUIDELINES §2-5).
 *
 * - A dialog in a body portal: `role="dialog"`, `aria-modal`, named by its title.
 * - Focus is trapped inside and restored to the invoker on close
 *   (`useFocusTrap`). Initial focus: `initialFocus`, else the first enabled
 *   `[data-autofocus]`, else the first enabled action (Cancel, by the rule that
 *   Cancel is rendered first), else the first focusable. The quit sheet passes
 *   its Cancel button.
 * - Keys: `Esc` and `⌘.` cancel (macOS convention); `⌘⏎` confirms only on a
 *   non-destructive sheet with `onConfirm` — a destructive sheet has no `⌘⏎`,
 *   the user must Tab to the destructive button (ux-ia §7.1, §10.2).
 * - While open it pushes the `X` hotkey scope: global bindings are suspended
 *   and only bindings registered inside the sheet fire.
 * - The backdrop does not close the sheet: a stray click never cancels work.
 */

import { useId, useRef } from 'react';
import type { ReactNode, RefObject } from 'react';
import { createPortal } from 'react-dom';

import { useI18n } from '../i18n/useI18n';
import { useFocusTrap } from '../hooks/useFocusTrap';
import { useHotkeyScope, useHotkeys } from '../hooks/useHotkeys';
import type { HotkeyBinding } from '../hooks/useHotkeys';
import './components.css';

export interface SheetProps {
  open: boolean;
  title: ReactNode;
  children?: ReactNode;
  /** Buttons, right-aligned. Cancel goes first. */
  actions?: ReactNode;
  /** Esc, ⌘., the close button. */
  onCancel: () => void;
  /** ⌘⏎ — honoured only when the sheet is not `destructive`. */
  onConfirm?: () => void;
  /** The primary action stops, deletes or discards something: no ⌘⏎. */
  destructive?: boolean;
  /** The control focused on open (the quit sheet passes Cancel). */
  initialFocus?: RefObject<HTMLElement | null>;
  /** Id of an element describing the consequence, for `aria-describedby`. */
  describedBy?: string;
  /** Show the × button in the header. Default true. */
  closeButton?: boolean;
  size?: 'md' | 'lg';
  className?: string;
}

export function Sheet(props: SheetProps) {
  if (!props.open) return null;
  return <OpenSheet {...props} />;
}

function OpenSheet({
  title,
  children,
  actions,
  onCancel,
  onConfirm,
  destructive = false,
  initialFocus,
  describedBy,
  closeButton = true,
  size = 'md',
  className,
}: SheetProps) {
  const { t } = useI18n();
  const dialog = useRef<HTMLDivElement>(null);
  const titleId = useId();

  useFocusTrap(dialog, true, { initialFocus });
  useHotkeyScope('X', true, dialog);

  const bindings: HotkeyBinding[] = [
    { id: 'sheet.cancel', combo: ['escape', 'mod+period'], run: () => onCancel() },
  ];
  if (onConfirm && !destructive) {
    bindings.push({ id: 'sheet.confirm', combo: 'mod+enter', run: () => onConfirm() });
  }
  useHotkeys(bindings, { scope: 'X', element: dialog });

  return createPortal(
    <div className="g-sheet-backdrop">
      <div
        ref={dialog}
        className={['g-sheet', `g-sheet-${size}`, className ?? ''].filter(Boolean).join(' ')}
        role="dialog"
        aria-modal="true"
        aria-labelledby={titleId}
        aria-describedby={describedBy}
        data-destructive={destructive ? 'true' : undefined}
      >
        <div className="g-sheet-header">
          <h2 className="g-sheet-title" id={titleId}>
            {title}
          </h2>
          {closeButton ? (
            <button type="button" className="g-sheet-close" aria-label={t('common.close')} onClick={onCancel}>
              <svg viewBox="0 0 24 24" aria-hidden="true" focusable="false">
                <path d="M18 6 6 18M6 6l12 12" />
              </svg>
            </button>
          ) : null}
        </div>
        {children !== undefined ? <div className="g-sheet-body">{children}</div> : null}
        {actions ? (
          <div className="g-sheet-actions" data-sheet-actions="">
            {actions}
          </div>
        ) : null}
      </div>
    </div>,
    document.body,
  );
}
