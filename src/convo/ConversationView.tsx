/**
 * The conversation of one session (ux-ia §3.4, §3.6; contract §4.2.4).
 *
 *  - Blocks come from this view's own subscription (`useConversation`), never
 *    from the store; cards come from the store (`Session.pending`) and either
 *    may arrive first (`interactionDisplay`).
 *  - The list follows the tail only while it is at the bottom; otherwise a
 *    "{n} new ↓" pill counts the rows that arrived below and jumps back
 *    (T-ui-followTail). Scrolling to the top loads older blocks.
 *  - An evicted view (the 9th subscription took its slot) offers Resume; a
 *    conversation that cannot be read here says so (ux-ia #83).
 *  - The scroll container is `role="log"` with `aria-live="off"`: streaming
 *    text is never read aloud (ux-ia §10).
 */

import { useCallback, useEffect, useMemo, useRef, useState } from 'react';
import type { ReactNode } from 'react';

import { VirtualList } from '../components/VirtualList';
import type { VirtualListHandle } from '../components/VirtualList';
import { useConversation } from '../hooks/useConversation';
import { useI18n } from '../i18n/useI18n';
import { commands } from '../lib/commands';
import type { PendingInteraction } from '../lib/gen/PendingInteraction';
import type { SessionUid } from '../lib/gen/SessionUid';
import type { IpcError } from '../lib/ipcError';
import { vendorBadge } from '../lib/vendor';
import { useAppStore } from '../store/useAppStore';
import { sessionOf } from '../store/worldSlice';
import { BlockRow } from './BlockRow';
import type { RowContext } from './BlockRow';
import { toRows } from './rows';
import type { Row } from './rows';
import './convo.css';

export interface ConversationViewProps {
  uid: SessionUid;
  /** Tests: row measurement and viewport (the test DOM has no layout). */
  measure?: (element: HTMLElement, index: number) => number;
  viewportHeight?: number;
  /** A line shown above the first block (e.g. the follow note of a watched session). */
  className?: string;
}

const EMPTY: readonly PendingInteraction[] = [];

/** `path:line` → the path and its line (1..=10^7), else the path alone. */
export function splitPathLine(raw: string): { path: string; line: number | null } {
  const m = /^(.*?):(\d{1,7})(?::\d+)?$/.exec(raw);
  if (!m) return { path: raw, line: null };
  const line = Number(m[2]);
  return line >= 1 ? { path: m[1] as string, line } : { path: raw, line: null };
}

function estimate(row: Row | undefined): number {
  if (!row) return 40;
  switch (row.kind) {
    case 'assistant':
      return Math.min(600, 40 + Math.ceil(row.block.kind.markdown.length / 110) * 20);
    case 'user':
      return Math.min(400, 48 + Math.ceil(row.block.kind.text.length / 110) * 20);
    case 'tools':
      return row.category === 'edit' ? 28 + 26 * row.blocks.length : 28;
    default:
      return 28;
  }
}

export function ConversationView({ uid, measure, viewportHeight, className }: ConversationViewProps) {
  const { t, td, te, locale } = useI18n();
  const { view, error, loadOlder, resubscribe } = useConversation(uid, { viewLabel: 'convo' });
  const session = useAppStore((s) => sessionOf(s.world, uid));
  const pending = session?.pending ?? EMPTY;
  const agent = vendorBadge(session?.vendor ?? 'claude');
  const listRef = useRef<VirtualListHandle>(null);
  const [atBottom, setAtBottom] = useState(true);
  const [ideError, setIdeError] = useState<IpcError | null>(null);

  const rows = useMemo(() => toRows(view.blocks), [view.blocks]);

  // Rows that arrived while the reader was scrolled up (the pill's count).
  const seenRows = useRef(0);
  useEffect(() => {
    if (atBottom) seenRows.current = rows.length;
  }, [atBottom, rows.length]);
  useEffect(() => {
    seenRows.current = rows.length;
    setAtBottom(true);
    // A Reset replaces every block: start at the tail again.
  }, [view.resetCount]); // eslint-disable-line react-hooks/exhaustive-deps
  const newBelow = atBottom ? 0 : Math.max(0, rows.length - seenRows.current);

  const timeFormat = useMemo(() => new Intl.DateTimeFormat(locale, { hour: '2-digit', minute: '2-digit', hourCycle: 'h23' }), [locale]);
  const resolved = session?.uid ?? uid;
  const ctx = useMemo<RowContext>(
    () => ({
      agent,
      pending,
      timeOf: (at) => (Number.isFinite(at) && at > 0 ? timeFormat.format(new Date(at)) : ''),
      openPath: (raw, line) => {
        const p = splitPathLine(raw);
        commands
          .ide_open({ target: { kind: 'file', uid: resolved, path: p.path, line: line ?? p.line } })
          .then(() => setIdeError(null), (e: IpcError) => setIdeError(e));
      },
      openDiff: (path) => {
        commands.ide_open({ target: { kind: 'diff', uid: resolved, path } }).then(() => setIdeError(null), (e: IpcError) => setIdeError(e));
      },
    }),
    [agent, pending, resolved, timeFormat],
  );

  // Reaching the top loads older blocks (one request at a time).
  const loading = useRef(false);
  const onRangeChange = useCallback(
    (first: number) => {
      if (first !== 0 || !view.hasOlder || loading.current) return;
      loading.current = true;
      void loadOlder().finally(() => {
        loading.current = false;
      });
    },
    [loadOlder, view.hasOlder],
  );

  let status: ReactNode = null;
  if (view.status === 'evicted') {
    status = (
      <p className="g-convo-status">
        {t('convo.paused')}{' '}
        <button type="button" className="g-cbtn" onClick={resubscribe}>
          {t('convo.resume')}
        </button>
      </p>
    );
  } else if (view.status === 'unavailable') {
    const unreadable = error?.code === 'UNSUPPORTED' && error.params?.cap === 'noReadableConversation';
    status = (
      <p className="g-convo-status">
        {unreadable ? t('observed.unreadable') : error ? te(error) : t('convo.unavailable')}
      </p>
    );
  } else if (view.status === 'connecting') {
    status = <p className="g-convo-status">{t('common.loading')}</p>;
  }

  return (
    <section className={`g-convo${className ? ` ${className}` : ''}`} aria-label={t('convo.region')}>
      {view.note && <p className="g-convo-note">{td(view.note.key, view.note.params)}</p>}
      {status}
      {view.hasOlder && view.status === 'live' && (
        <div className="g-convo-older">
          <button type="button" className="g-cbtn g-cbtn-ghost" onClick={() => void loadOlder()}>
            {t('convo.loadOlder')}
          </button>
        </div>
      )}
      {view.status !== 'unavailable' && (
        <VirtualList
          ref={listRef}
          className="g-convo-list"
          role="log"
          aria-live="off"
          aria-label={t('convo.region')}
          tabIndex={0}
          count={rows.length}
          getKey={(i) => rows[i]?.key ?? String(i)}
          estimateSize={(i) => estimate(rows[i])}
          renderRow={(i) => {
            const row = rows[i];
            return row ? <BlockRow row={row} ctx={ctx} /> : null;
          }}
          followTail
          onAtBottomChange={setAtBottom}
          onRangeChange={onRangeChange}
          measure={measure}
          viewportHeight={viewportHeight}
          empty={view.status === 'live' ? <p className="g-convo-status">{t('convo.empty')}</p> : null}
        />
      )}
      {newBelow > 0 && (
        <button
          type="button"
          className="g-convo-pill"
          data-follow-pill=""
          onClick={() => {
            listRef.current?.scrollToBottom();
            setAtBottom(true);
          }}
        >
          {t('convo.newBelow', { n: newBelow })} ↓
        </button>
      )}
      {ideError && (
        <p className="g-convo-status" role="status">
          {te(ideError)}
          {ideError.detail ? <span className="mono"> {ideError.detail}</span> : null}
        </p>
      )}
    </section>
  );
}
