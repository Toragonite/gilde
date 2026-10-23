/**
 * Accounts & health (`⇧⌘A`, ux-ia §3.15, §5.8, §5.9; contract §4.2.7, A23.5).
 *
 * Two columns at 1200 px and wider; below that one section at a time, picked
 * with the segmented control or `⌥⌘1`/`⌥⌘2` (scope A). `section` is the view
 * the shell holds, so Back/Forward and the palette land on the right one.
 *
 * Usage bars refresh when this screen opens (`quota_refresh` per account; the
 * core rate-limits it) and from the header's ↻; otherwise the core refreshes
 * them in the background while the screen is shown (≤ 1 per 5 min, F-35).
 */

import { useCallback, useEffect, useId, useState } from 'react';

import { RefreshIcon } from '../components/icons';
import { useHotkeys } from '../hooks/useHotkeys';
import { useI18n } from '../i18n/useI18n';
import { clock } from '../lib/clock';
import { commands } from '../lib/commands';
import { formatDateTime, formatRelativeTime } from '../lib/format';
import type { AccountId } from '../lib/gen/AccountId';
import { appStore, useAppStore } from '../store/useAppStore';
import type { AccountsSection } from '../store/uiSlice';
import { AccountsPanel } from './AccountsPanel';
import { HealthPanel } from './HealthPanel';
import { lastQuotaFetch } from './model';
import './accounts.css';

/** Below this window width the screen shows one section at a time (ux-ia §7.2 scope A). */
export const NARROW_WIDTH = 1200;

function useNarrow(): boolean {
  const read = () => (typeof window === 'undefined' ? false : window.innerWidth < NARROW_WIDTH);
  const [narrow, setNarrow] = useState(read);
  useEffect(() => {
    const onResize = () => setNarrow(read());
    window.addEventListener('resize', onResize);
    return () => window.removeEventListener('resize', onResize);
  }, []);
  return narrow;
}

/** Ask the core for fresh quota readings (it answers cached values inside its rate limit). */
function refreshQuotas(ids: readonly AccountId[]): Promise<void> {
  return Promise.allSettled(ids.map((accountId) => commands.quota_refresh({ accountId }))).then(() => undefined);
}

export function AccountsScreen({ section }: { section: AccountsSection }) {
  const { t } = useI18n();
  const navigate = useAppStore((s) => s.navigate);
  const accounts = useAppStore((s) => s.world.accounts);
  const narrow = useNarrow();
  const [refreshing, setRefreshing] = useState(false);
  const accountsHeading = useId();
  const healthHeading = useId();
  const updated = lastQuotaFetch(accounts.values());

  const refresh = useCallback(() => {
    const ids = [...appStore.getState().world.accounts.keys()];
    if (ids.length === 0) return;
    setRefreshing(true);
    void refreshQuotas(ids).finally(() => setRefreshing(false));
  }, []);

  // Refresh once when the screen opens.
  useEffect(() => {
    refresh();
  }, [refresh]);

  const show = useCallback((next: AccountsSection) => navigate({ screen: 'accounts', section: next }), [navigate]);

  useHotkeys(
    [
      { id: 'accounts.section.accounts', combo: 'mod+alt+1', run: () => show('accounts') },
      { id: 'accounts.section.health', combo: 'mod+alt+2', run: () => show('health') },
    ],
    { scope: 'A', enabled: narrow },
  );

  return (
    <div className="ah" data-narrow={narrow ? 'true' : undefined}>
      <div className="ah-head">
        <h1>{t('nav.accountsHealth')}</h1>
        {narrow ? (
          <div className="seg ah-tabs" role="group" aria-label={t('accounts.sections')}>
            <button type="button" aria-pressed={section === 'accounts'} aria-keyshortcuts="Meta+Alt+1" onClick={() => show('accounts')}>
              {t('accounts.heading.accounts')}
            </button>
            <button type="button" aria-pressed={section === 'health'} aria-keyshortcuts="Meta+Alt+2" onClick={() => show('health')}>
              {t('accounts.heading.health')}
            </button>
          </div>
        ) : null}
        <span className="grow" />
        {updated !== null ? <UpdatedLine at={updated} /> : null}
        <button type="button" className="btn ghost icon" aria-label={t('accounts.refresh')} title={t('accounts.refresh')} disabled={refreshing} onClick={refresh}>
          <RefreshIcon size="sm" />
        </button>
      </div>
      <div className="ah-cols">
        {!narrow || section === 'accounts' ? <AccountsPanel headingId={accountsHeading} /> : null}
        {!narrow || section === 'health' ? <HealthPanel headingId={healthHeading} /> : null}
      </div>
    </div>
  );
}

/** "Updated 40 s ago", refreshed every 15 s (calm: no per-second churn in the header). */
function UpdatedLine({ at }: { at: number }) {
  const { t, locale } = useI18n();
  const [now, setNow] = useState(() => clock.wall());
  useEffect(() => {
    const id = setInterval(() => setNow(clock.wall()), 15_000);
    return () => clearInterval(id);
  }, []);
  return (
    <span className="ah-updated" title={formatDateTime(at, locale)}>
      {t('accounts.updated', { time: formatRelativeTime(at, locale, Math.max(now, at)) })}
    </span>
  );
}
