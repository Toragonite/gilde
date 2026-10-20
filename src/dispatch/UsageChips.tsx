/**
 * The footer chips of a result card (DESIGN §9-7): `in/out/cached` tokens,
 * cost, a copyable `conversationId`, the route note and — verbatim — whatever
 * note the adapter set.
 *
 * Two rules:
 *  - a chip that has no value is not rendered. A row of `—` placeholders reads
 *    as "we measured zero", which is a different claim from "the vendor did not
 *    report it";
 *  - the adapter's `note` is never rewritten or translated. "permission
 *    downgraded to acceptEdits" is the adapter's sentence and the user needs
 *    the exact words to search for them.
 */

import { Badge, Chip } from '../components/Badge';
import { Mono } from '../components/Mono';
import { useI18n } from '../i18n/useI18n';
import { formatCostUsd, formatNumber } from '../lib/format';
import type { DispatchRun, TokenUsage } from '../lib/ipc';

export interface UsageChipsProps {
  run: DispatchRun;
  usage: TokenUsage | null;
  costUsd: number | null;
  onCopyConversationId?: (conversationId: string) => void;
}

function tokenCount(value: unknown): number | null {
  return typeof value === 'number' && Number.isFinite(value) && value > 0 ? value : null;
}

export function UsageChips({ run, usage, costUsd, onCopyConversationId }: UsageChipsProps) {
  const { t, locale } = useI18n();

  const input = tokenCount(usage?.inputTokens);
  const output = tokenCount(usage?.outputTokens);
  const cached = tokenCount(usage?.cachedInputTokens);
  const total = tokenCount(usage?.totalTokens);
  const cost = typeof costUsd === 'number' ? costUsd : tokenCount(usage?.costUsd);
  const conversationId = run.conversationId ?? '';

  const nothing =
    input === null &&
    output === null &&
    cached === null &&
    total === null &&
    cost === null &&
    conversationId === '' &&
    !run.note &&
    !run.routeNote;

  if (nothing) return null;

  return (
    <div className="row gap-6 dsp-chips">
      {input !== null ? (
        <Mono small className="muted">
          {t('dispatch.usage.input', { n: formatNumber(input, locale) })}
        </Mono>
      ) : null}
      {output !== null ? (
        <Mono small className="muted">
          {t('dispatch.usage.output', { n: formatNumber(output, locale) })}
        </Mono>
      ) : null}
      {cached !== null ? (
        <Mono small className="muted">
          {t('dispatch.usage.cached', { n: formatNumber(cached, locale) })}
        </Mono>
      ) : null}
      {input === null && output === null && total !== null ? (
        <Mono small className="muted">
          {t('dispatch.usage.total', { n: formatNumber(total, locale) })}
        </Mono>
      ) : null}
      {cost !== null ? (
        <Mono small className="muted">
          {formatCostUsd(cost, locale)}
        </Mono>
      ) : null}

      {conversationId ? (
        <Chip
          title={t('dispatch.card.conversation', { id: conversationId })}
          onClick={onCopyConversationId ? () => onCopyConversationId(conversationId) : undefined}
        >
          <Mono small ellipsis>
            {conversationId}
          </Mono>
        </Chip>
      ) : null}

      {run.routeNote ? (
        <Badge title={t('dispatch.card.routeNote')}>{run.routeNote}</Badge>
      ) : null}

      {/* The adapter's own words, unchanged (DESIGN §9-7). */}
      {run.note ? <Badge tone="warn" title={t('dispatch.card.note')}>{run.note}</Badge> : null}
    </div>
  );
}
