/**
 * Toast stack (T26; DESIGN §4 `gilde://toast`, §8-6).
 *
 * Two things make this more than a notification list:
 *
 *  1. **Rust never sends prose.** A toast arrives as `{ level, key, params }`
 *     and is rendered with `t()`, so the same event reads in Korean or English
 *     without a backend rebuild. Raw technical detail (git/CLI stderr) is shown
 *     verbatim in a mono block instead — never translated.
 *  2. **An undo toast is a live 10 s window, not a receipt.** The reaper and the
 *     memory watchdog *schedule* a kill and publish the token; nothing has been
 *     signalled yet. Pressing 실행 취소 calls `undo_cancel(token)` and the kill
 *     never happens. The countdown is therefore real and the toast must not be
 *     auto-dismissed before the window closes.
 *
 * A spent token answers `false` — "too late or unknown". That is rendered as a
 * spent undo ("이미 종료되었습니다"), not as a failure, because from the user's
 * point of view nothing went wrong: they were simply late.
 */

import { useCallback, useEffect, useMemo, useState } from 'react';

import { CloseIcon } from '../components/icons';
import { StatusDot } from '../components/StatusDot';
import { Mono } from '../components/Mono';
import { useI18n } from '../i18n/useI18n';
import type { MessageKey } from '../i18n';
import { cmd, isUnavailable } from '../lib/commands';
import { formatNumber } from '../lib/format';
import type { HealthAlert, NodeStatus, ToastLevel } from '../lib/ipc';
import { useAppStore } from '../store/useAppStore';
import type { Toast } from '../store/toastSlice';
import { AlertActions } from './HealthPopover';
import { alertPidsTitle, alertText } from './healthText';
import '../dialogs/dialogs.css';

/** How often the countdown repaints while a toast is expiring. */
export const TOAST_TICK_MS = 500;

/**
 * How long the answer to an undo stays on screen after the window would have
 * closed. Without it, undoing at 9 s would flash the confirmation for 1 s.
 */
export const UNDO_ANSWER_MS = 4000;

/** What happened to an undo token after the user pressed it. */
export type UndoState = 'armed' | 'sending' | 'undone' | 'spent';

// ---------------------------------------------------------------------------
// Pure helpers (unit tested)
// ---------------------------------------------------------------------------

/** Milliseconds left before auto-dismiss; `null` for a toast that stays. */
export function toastRemainingMs(toast: Toast, now: number): number | null {
  if (!toast.ttlMs) return null;
  return Math.max(0, toast.at + toast.ttlMs - now);
}

/** Whole seconds shown next to an undo link — rounded up, so `0s` means gone. */
export function toastSecondsLeft(toast: Toast, now: number): number {
  const remaining = toastRemainingMs(toast, now);
  return remaining === null ? 0 : Math.ceil(remaining / 1000);
}

/** A toast is expired once its window has closed; sticky toasts never are. */
export function toastExpired(toast: Toast, now: number): boolean {
  return toastRemainingMs(toast, now) === 0;
}

/** The status dot beside a toast reuses the four semantic tokens (§2-1a). */
export function toastStatus(level: ToastLevel): NodeStatus {
  switch (level) {
    case 'error':
      return 'error';
    case 'warn':
      return 'stalled';
    case 'success':
      return 'done';
    default:
      return 'idle';
  }
}

export function toastLevelKey(level: ToastLevel): MessageKey {
  return `toast.level.${level}` as MessageKey;
}

/** `undo_cancel` answers a bool; `false` means the window had already closed. */
export function undoStateOf(cancelled: boolean): UndoState {
  return cancelled ? 'undone' : 'spent';
}

export function undoStatusKey(state: UndoState): MessageKey | null {
  if (state === 'undone') return 'toast.undo.done';
  if (state === 'spent') return 'toast.undo.spent';
  return null;
}

// ---------------------------------------------------------------------------
// One toast
// ---------------------------------------------------------------------------

export interface ToastRowProps {
  toast: Toast;
  now: number;
  undo: UndoState;
  onUndo: (toast: Toast) => void;
  onDismiss: (id: number) => void;
}

/** The body of a health alert's toast: the alert as it is now, with its actions. */
function HealthToastBody({ alertId }: { alertId: string }) {
  const { locale } = useI18n();
  const alert = useAppStore((s) => s.health.snapshot?.alerts.find((a) => a.id === alertId) ?? null);
  if (!alert) return null;
  const text = alertText(alert, locale);
  return (
    <>
      <span className="strong" title={alertPidsTitle(alert, locale)}>
        {text.title}
      </span>
      <span>{text.body}</span>
      <div className="toast-actions">
        <AlertActions alert={alert} inPopover={false} />
      </div>
    </>
  );
}

export function ToastRow({ toast, now, undo, onUndo, onDismiss }: ToastRowProps) {
  const { t, td, locale } = useI18n();
  const seconds = toastSecondsLeft(toast, now);
  const undoStatus = undoStatusKey(undo);

  return (
    <div className="toast" role="status" data-level={toast.level}>
      <StatusDot status={toastStatus(toast.level)} pulse={false} title={t(toastLevelKey(toast.level))} />
      <div className="toast-body">
        {toast.healthAlertId ? (
          <HealthToastBody alertId={toast.healthAlertId} />
        ) : (
          <span>{td(toast.key, toast.params)}</span>
        )}
        {toast.detail ? <pre className="toast-detail">{toast.detail}</pre> : null}
        {toast.undoToken ? (
          <div className="row gap-8">
            {undoStatus ? (
              <span className="muted s11">{t(undoStatus)}</span>
            ) : (
              <>
                <button
                  type="button"
                  className="toast-undo"
                  disabled={undo === 'sending' || seconds === 0}
                  onClick={() => onUndo(toast)}
                >
                  {t('toast.undo')}
                </button>
                <Mono small className="muted">
                  {t('toast.undoSeconds', { n: formatNumber(seconds, locale) })}
                </Mono>
              </>
            )}
          </div>
        ) : null}
      </div>
      <button
        type="button"
        className="toast-close"
        aria-label={t('toast.dismiss')}
        title={t('toast.dismiss')}
        onClick={() => onDismiss(toast.id)}
      >
        <CloseIcon className="i sm" />
      </button>
    </div>
  );
}

// ---------------------------------------------------------------------------
// The stack
// ---------------------------------------------------------------------------

/** An undo the user has already pressed, and when the answer came back. */
interface UndoEntry {
  state: UndoState;
  answeredAt: number;
}

/**
 * A toast is dismissed when its window closes — unless the user pressed undo,
 * in which case the answer stays up for {@link UNDO_ANSWER_MS} first.
 */
export function toastDue(toast: Toast, entry: UndoEntry | undefined, now: number): boolean {
  if (entry) {
    if (entry.state === 'sending') return false;
    return now - entry.answeredAt >= UNDO_ANSWER_MS;
  }
  return toastExpired(toast, now);
}

/**
 * Whether a toast is drawn now. A health alert's toast is hidden — not
 * dismissed, not removed from the store — while the health popover is open:
 * the popover shows the same alert with the same buttons, and a toast on top of
 * it would cover it. It is drawn again when the popover closes. A toast whose
 * alert is dismissed is never drawn (the health slice removes it, too).
 */
export function toastVisible(toast: Toast, popoverOpen: boolean, alerts: readonly HealthAlert[] | undefined): boolean {
  if (!toast.healthAlertId) return true;
  if (popoverOpen) return false;
  const alert = alerts?.find((a) => a.id === toast.healthAlertId);
  return !alert?.dismissed;
}

export function ToastHost() {
  const toasts = useAppStore((s) => s.toasts);
  const popoverOpen = useAppStore((s) => s.health.popoverOpen);
  const alerts = useAppStore((s) => s.health.snapshot?.alerts);
  const dismissToast = useAppStore((s) => s.dismissToast);
  const dismissHealthAlert = useAppStore((s) => s.dismissHealthAlert);
  // Closing a health alert's toast dismisses the alert (Rust keeps it quiet
  // for ten minutes unless it escalates); every other toast just goes.
  const onClose = useCallback(
    (id: number) => {
      const toast = useAppStore.getState().toasts.find((x) => x.id === id);
      if (toast?.healthAlertId) void dismissHealthAlert(toast.healthAlertId);
      else dismissToast(id);
    },
    [dismissToast, dismissHealthAlert],
  );
  const [now, setNow] = useState(() => Date.now());
  const [undoEntries, setUndoEntries] = useState<Record<number, UndoEntry>>({});

  // One timer for the whole stack, and only while something is actually
  // counting down: a sticky error toast must not keep a 2 Hz interval alive.
  const ticking = useMemo(
    () => toasts.some((toast) => toast.ttlMs > 0 || undoEntries[toast.id] !== undefined),
    [toasts, undoEntries],
  );

  useEffect(() => {
    if (!ticking) return;
    const timer = setInterval(() => setNow(Date.now()), TOAST_TICK_MS);
    return () => clearInterval(timer);
  }, [ticking]);

  // Expiry is a render-time fact, applied in an effect so the store write does
  // not happen during render.
  useEffect(() => {
    for (const toast of toasts) {
      if (toastDue(toast, undoEntries[toast.id], now)) dismissToast(toast.id);
    }
  }, [toasts, undoEntries, now, dismissToast]);

  const onUndo = useCallback((toast: Toast) => {
    const token = toast.undoToken;
    if (!token) return;
    setUndoEntries((prev) => ({ ...prev, [toast.id]: { state: 'sending', answeredAt: 0 } }));
    const answer = (state: UndoState) =>
      setUndoEntries((prev) => ({ ...prev, [toast.id]: { state, answeredAt: Date.now() } }));
    void cmd
      .undoCancel(token)
      .then((cancelled) => answer(undoStateOf(cancelled)))
      .catch((e) => {
        // The command lands with T11; an unavailable backend is a spent undo,
        // not an error dialog.
        if (!isUnavailable(e)) console.warn('[gilde] undo_cancel failed', e);
        answer('spent');
      });
  }, []);

  const shown = toasts.filter((toast) => toastVisible(toast, popoverOpen, alerts));
  if (shown.length === 0) return null;

  return (
    <div className="toast-stack">
      {shown.map((toast) => (
        <ToastRow
          key={toast.id}
          toast={toast}
          now={now}
          undo={undoEntries[toast.id]?.state ?? 'armed'}
          onUndo={onUndo}
          onDismiss={onClose}
        />
      ))}
    </div>
  );
}
