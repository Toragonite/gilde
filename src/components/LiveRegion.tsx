/**
 * The two live regions of the window (ux-ia §10), mounted once by the shell:
 * one polite (`role="status"`) and one assertive. Their text comes from
 * `lib/notify.ts` (`announce`, `announceSummary`) and is cleared a few seconds
 * after it was set, so the next announcement — even with the same words — is
 * read again. Visually hidden; never focusable.
 */

import { useEffect, useSyncExternalStore } from 'react';

import { clearAnnouncement, getAnnouncements, subscribeAnnouncements } from '../lib/notify';
import type { Announcement, Politeness } from '../lib/notify';
import './components.css';

/** How long an announcement stays in its region. */
export const ANNOUNCEMENT_TTL_MS = 7000;

function useClearLater(politeness: Politeness, entry: Announcement | null): void {
  useEffect(() => {
    if (!entry) return;
    const timer = setTimeout(() => clearAnnouncement(politeness, entry.id), ANNOUNCEMENT_TTL_MS);
    return () => clearTimeout(timer);
  }, [politeness, entry]);
}

export function LiveRegion() {
  const state = useSyncExternalStore(subscribeAnnouncements, getAnnouncements, getAnnouncements);
  useClearLater('polite', state.polite);
  useClearLater('assertive', state.assertive);
  return (
    <div className="g-sr-only">
      <div role="status" aria-live="polite" aria-atomic="true" data-live="polite">
        {state.polite ? <span key={state.polite.id}>{state.polite.text}</span> : null}
      </div>
      <div aria-live="assertive" aria-atomic="true" data-live="assertive">
        {state.assertive ? <span key={state.assertive.id}>{state.assertive.text}</span> : null}
      </div>
    </div>
  );
}
