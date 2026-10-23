/**
 * Toasts — outcomes of the user's own actions only (ux-ia §6.2; T13's
 * `toastSlice`). Polite (`role="status"`), at most three, bottom right of the
 * main zone; an undo toast keeps its 10 s window. An error toast shows its
 * `IpcError.detail` as one mono line, never inside the sentence.
 */

import { useEffect } from 'react';

import { CloseIcon } from '../components/icons';
import { useI18n } from '../i18n/useI18n';
import { useAppStore } from '../store/useAppStore';

export function Toasts() {
  const { t, td } = useI18n();
  const toasts = useAppStore((s) => s.toasts);
  const dismiss = useAppStore((s) => s.dismissToast);
  const undo = useAppStore((s) => s.undoToast);
  const expire = useAppStore((s) => s.expireToasts);

  useEffect(() => {
    if (toasts.length === 0) return;
    const timer = setInterval(() => expire(), 1000);
    return () => clearInterval(timer);
  }, [toasts.length, expire]);

  if (toasts.length === 0) return null;
  return (
    <div className="toasts" role="status" aria-live="polite">
      {toasts.map((toast) => (
        <div key={toast.id} className="toast" data-level={toast.level}>
          <div className="toast-text">
            <span>{td(toast.key, toast.params)}</span>
            {toast.detail ? <span className="mono s11 muted ell">{toast.detail}</span> : null}
          </div>
          {toast.undoToken ? (
            <button type="button" className="btn ghost" onClick={() => void undo(toast.id)}>
              {t('common.undo')}
            </button>
          ) : null}
          <button type="button" className="btn ghost icon" aria-label={t('common.dismiss')} onClick={() => dismiss(toast.id)}>
            <CloseIcon size="sm" />
          </button>
        </div>
      ))}
    </div>
  );
}
