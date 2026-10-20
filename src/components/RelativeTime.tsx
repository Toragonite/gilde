/**
 * Live relative timestamp, formatted with `Intl.RelativeTimeFormat` in the
 * active locale (ko `3분 전`, en `3 min. ago`).
 *
 * One shared ticker drives every instance, so a hundred rows cost one timer.
 */

import { useEffect, useState } from 'react';

import { useLocale } from '../i18n/useI18n';
import { formatDateTime, formatRelativeTime } from '../lib/format';

const listeners = new Set<() => void>();
let ticker: ReturnType<typeof setInterval> | null = null;

function subscribe(fn: () => void): () => void {
  listeners.add(fn);
  if (!ticker) {
    ticker = setInterval(() => {
      for (const listener of listeners) listener();
    }, 1000);
  }
  return () => {
    listeners.delete(fn);
    if (listeners.size === 0 && ticker) {
      clearInterval(ticker);
      ticker = null;
    }
  };
}

export interface RelativeTimeProps {
  /** Unix milliseconds. */
  at: number;
  className?: string;
}

export function RelativeTime({ at, className }: RelativeTimeProps) {
  const locale = useLocale();
  const [now, setNow] = useState(() => Date.now());

  useEffect(() => subscribe(() => setNow(Date.now())), []);

  if (!Number.isFinite(at) || at <= 0) return <span className={className}>—</span>;

  return (
    <span className={className} title={formatDateTime(at, locale)}>
      {formatRelativeTime(at, locale, now)}
    </span>
  );
}
