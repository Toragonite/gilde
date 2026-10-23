/**
 * The visible half of the 700 ms answer guard (`hooks/useAnswerGuard`): a short
 * line next to a card's buttons while answering is paused. It is not a live
 * region (it would speak on every new card); the buttons carry `aria-disabled`.
 */

import { useI18n } from '../i18n/useI18n';
import type { AnswerGuard } from '../hooks/useAnswerGuard';
import './components.css';

export function GuardNote({ guard }: { guard: Pick<AnswerGuard, 'guarded'> }) {
  const { t } = useI18n();
  if (!guard.guarded) return null;
  return (
    <span className="g-guard-note" data-guard-note="">
      {t('guard.justChanged')}
    </span>
  );
}
