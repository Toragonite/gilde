/**
 * Empty state: one sentence and at most one action button, no illustration and
 * no icon (UI-GUIDELINES §2-2). "실행 중인 세션이 없습니다. [새 세션]".
 */

export interface EmptyStateProps {
  message: string;
  actionLabel?: string;
  onAction?: () => void;
  /** Raw technical detail (stderr, a path) shown verbatim under the sentence. */
  detail?: string | null;
}

export function EmptyState({ message, actionLabel, onAction, detail }: EmptyStateProps) {
  return (
    <div className="empty">
      <span>{message}</span>
      {detail ? <pre className="mono s11 muted empty-detail">{detail}</pre> : null}
      {actionLabel && onAction ? (
        <button type="button" className="btn" onClick={onAction}>
          {actionLabel}
        </button>
      ) : null}
    </div>
  );
}
