/**
 * Canvas legend (T25).
 *
 * Five statuses, each a dot *and* its word — the legend is the proof that the
 * canvas never asks anyone to read colour alone (UI-GUIDELINES §2-1a). It sits
 * bottom-left, out of the rank flow, exactly as in the reference mockup.
 */

import { StatusDot, statusMessageKey } from '../components/StatusDot';
import { useI18n } from '../i18n/useI18n';
import type { NodeStatus } from '../lib/ipc';

export const LEGEND_STATUSES: readonly NodeStatus[] = [
  'working',
  'waiting',
  'done',
  'error',
  'idle',
];

export function GraphLegend() {
  const { t } = useI18n();
  return (
    <div className="gilde-legend" aria-label={t('graph.legend')}>
      {LEGEND_STATUSES.map((status) => (
        <span key={status} className="row gap-6 s11 muted">
          <StatusDot status={status} pulse={false} />
          {t(statusMessageKey(status))}
        </span>
      ))}
    </div>
  );
}
