/**
 * Reading a History session that World does not hold (contract A29.2(c);
 * ux-ia §3.14: `⌘↓` = "read it in the Session screen (Not running,
 * read-only)").
 *
 * The Session screen renders this when its uid is absent from the store. It
 * asks `history_row{uid}`:
 *  - found → a read-only Not-running view: the header from the row (title,
 *    else "{vendor} conversation · date"; vendor; project; started/ended), the
 *    Conversation through `convo_subscribe` (the core falls back to the
 *    history index), and the Continue bar from `row.continueOptions` with
 *    History's `⏎` action. No Changes, no Terminal/Log, no composer;
 *  - `NOT_FOUND`, a core without the command yet (`notBuilt`), or any other
 *    failure → "This session is no longer available." (`session.gone`).
 * When the uid appears in the store (a resume, a discovery pass), the Session
 * screen shows the store's entity instead: this view simply unmounts.
 */

import { useEffect, useId, useState } from 'react';

import { Kbd } from '../components/Badge';
import { StateGlyph } from '../components/StateGlyph';
import { ConversationView } from '../convo/ConversationView';
import { useHotkeys } from '../hooks/useHotkeys';
import { useI18n } from '../i18n/useI18n';
import { clock } from '../lib/clock';
import { commands } from '../lib/commands';
import type { HistoryRow } from '../lib/gen/HistoryRow';
import type { SessionUid } from '../lib/gen/SessionUid';
import { newClientRequestId } from '../lib/ids';
import type { IpcError } from '../lib/ipcError';
import { vendorBadge } from '../lib/vendor';
import { clockTime } from '../shell/words';
import { useAppStore } from '../store/useAppStore';
import * as M from './model';

type State = { kind: 'loading' } | { kind: 'found'; row: HistoryRow } | { kind: 'gone' };

export function HistoryReadView({ uid }: { uid: SessionUid }) {
  const { t } = useI18n();
  const titleId = useId();
  const [state, setState] = useState<State>({ kind: 'loading' });

  useEffect(() => {
    let live = true;
    setState({ kind: 'loading' });
    commands.history_row({ uid }).then(
      (row) => live && setState({ kind: 'found', row }),
      () => live && setState({ kind: 'gone' }),
    );
    return () => {
      live = false;
    };
  }, [uid]);

  if (state.kind === 'found') return <ReadOnly row={state.row} />;
  return (
    <section className="g-session" aria-labelledby={titleId} aria-busy={state.kind === 'loading'}>
      <h1 id={titleId} className="sr-text">
        {t('session.header')}
      </h1>
      {state.kind === 'gone' ? <p className="g-session-gone">{t('session.gone')}</p> : null}
    </section>
  );
}

function ReadOnly({ row }: { row: HistoryRow }) {
  const { t, te, locale } = useI18n();
  const titleId = useId();
  const title = M.rowTitle(t, row, locale);
  const setReadTitle = useAppStore((s) => s.setReadTitle);

  // The top bar and the main landmark name this read (T31); cleared on unmount.
  useEffect(() => {
    setReadTitle({ uid: row.uid, title });
    return () => setReadTitle(null, row.uid);
  }, [row.uid, title, setReadTitle]);
  const openSession = useAppStore((s) => s.openSession);
  const project = useAppStore((s) => (row.projectId ? (s.world.projects.get(row.projectId)?.name ?? null) : null));
  const sheetOpen = useAppStore((s) => s.ui.sheet !== null);
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<IpcError | null>(null);
  const action = M.rowAction(row, null);
  const now = clock.wall();

  const run = (how: 'resume' | 'fork') => {
    if (busy) return;
    setBusy(true);
    setError(null);
    commands
      .session_continue({ clientRequestId: newClientRequestId(), uid: row.uid, how: { kind: how }, controlMode: null, model: null, permissionMode: null })
      .then(
        (res) => {
          setBusy(false);
          openSession(res.uid);
        },
        (e: IpcError) => {
          setBusy(false);
          setError(e);
        },
      );
  };
  const primary = action.kind === 'resume' || action.kind === 'fork' ? () => run(action.kind as 'resume' | 'fork') : null;

  useHotkeys([{ id: 'history.read.continue', combo: 'mod+enter', run: () => primary?.() }], { scope: 'S', enabled: primary !== null && !sheetOpen });

  const meta = [
    vendorBadge(row.vendor),
    t('history.ranIn', { owner: t(M.ORIGIN_KEY[row.origin] ?? 'origin.unknown') }),
    M.rowWhen(t, row, locale, now),
    t('state.ended'),
  ].join(' · ');
  const sentence =
    action.kind === 'none'
      ? t('history.cantContinue')
      : row.endedAt !== null && !row.running
        ? t('ended.banner', { time: clockTime(row.endedAt, locale) })
        : t('history.readOnly');
  const offerFork = error !== null && error.code === 'SESSION_LIVE_ELSEWHERE';

  return (
    <section className="g-session" aria-labelledby={titleId} data-history-read={row.uid}>
      <div className="g-session-head">
        <div className="g-session-row">
          <StateGlyph state="ended" />
          <h1 id={titleId} className="g-session-title">
            {title}
          </h1>
          <span className="g-session-where">{project ?? ''}</span>
        </div>
        <div className="g-session-row">
          <span className="g-session-meta">{meta}</span>
        </div>
      </div>
      <div className="g-session-body" aria-label={t('session.tab.conversation')} role="region">
        {row.titleOnly ? <p className="g-session-gone">{t('history.noContent')}</p> : <ConversationView uid={row.uid} />}
        <div className="g-session-dock">
          <div className="g-bar" role="region" aria-label={t('history.continueRegion')} data-bar="history-read">
            <p className="g-bar-text">{sentence}</p>
            <div className="g-bar-actions">
              {primary ? (
                <button type="button" className="g-sbtn g-sbtn-primary" aria-keyshortcuts="Meta+Enter" disabled={busy} onClick={primary}>
                  {t(M.ACTION_KEY[action.kind as 'resume' | 'fork'])} <Kbd>⌘⏎</Kbd>
                </button>
              ) : null}
              {offerFork ? (
                <button type="button" className="g-sbtn" disabled={busy} onClick={() => run('fork')}>
                  {t('action.continueCopy')}
                </button>
              ) : null}
              <button
                type="button"
                className="g-sbtn"
                onClick={() => commands.ide_reveal({ target: { kind: 'location', uid: row.uid } }).then(() => setError(null), (e: IpcError) => setError(e))}
              >
                {t('history.openFolder')}
              </button>
            </div>
            {error ? (
              <p className="g-bar-line" role="status">
                {te(error)}
                {error.detail ? <span className="mono"> {error.detail}</span> : null}
              </p>
            ) : null}
          </div>
        </div>
      </div>
    </section>
  );
}
