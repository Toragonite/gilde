/**
 * Session → Changes (ux-ia §3.13, contract §4.2.5, A17.4–A17.6).
 *
 *  - No code is rendered: the file list, +/−, the changed parts (hunk
 *    names) and who made each change. `⏎` on a file = Diff in IDE, `⌘O` =
 *    open it; Gilde has no editor or diff viewer.
 *  - Commit / Merge / Discard follow the core's `ActionState`s: enabled,
 *    disabled with the reason key (`changes.ownerWriting` while the owner is
 *    alive, `changes.stopToDiscard` while sessions run here), or hidden.
 *  - Every action goes preview → sheet → confirm token. Merge and Discard are
 *    destructive sheets (focus on Cancel, no `⌘⏎`); Discard lists each loss
 *    with its own checkbox and never deletes the branch (A17.5).
 *  - Errors are the manifest's codes (A17.4), shown as sentences with the
 *    detail in mono; nothing is retried.
 */

import { useCallback, useEffect, useState } from 'react';
import type { KeyboardEvent as ReactKeyboardEvent, ReactNode } from 'react';

import { useI18n } from '../i18n/useI18n';
import { commands } from '../lib/commands';
import { formatDiffStat } from '../lib/format';
import type { ActionState } from '../lib/gen/ActionState';
import type { ChangesView as ChangesData } from '../lib/gen/ChangesView';
import type { FileRow } from '../lib/gen/FileRow';
import type { SessionUid } from '../lib/gen/SessionUid';
import type { IpcError } from '../lib/ipcError';
import { vendorBadge } from '../lib/vendor';
import { ownerWord } from '../approval';
import { useAppStore } from '../store/useAppStore';
import { sessionOf } from '../store/worldSlice';
import { errorLine, useChangesResult } from './sheets';
import type { Line } from './sheets';
import './changes.css';

export interface ChangesViewProps {
  uid: SessionUid;
  /** "Ask {agent} to commit": the host puts the text in the composer (never sends it). */
  onAskCommit?: (text: string) => void;
}

type Load = { kind: 'loading' } | { kind: 'ready'; data: ChangesData } | { kind: 'error'; error: IpcError };

/** Files per session uid, so the session tab can show "Changes 3" after a first load. */
const fileCounts = new Map<string, number>();
const countListeners = new Set<() => void>();
export function changesCountOf(uid: SessionUid): number | null {
  return fileCounts.get(uid) ?? null;
}
export function subscribeChangesCount(fn: () => void): () => void {
  countListeners.add(fn);
  return () => countListeners.delete(fn);
}
function setCount(uid: SessionUid, n: number) {
  if (fileCounts.get(uid) === n) return;
  fileCounts.set(uid, n);
  for (const fn of [...countListeners]) fn();
}

export function ChangesView({ uid, onAskCommit }: ChangesViewProps) {
  const { t, td, te, locale } = useI18n();
  const session = useAppStore((s) => sessionOf(s.world, uid));
  const [load, setLoad] = useState<Load>({ kind: 'loading' });
  const [line, setLine] = useState<Line>(null);
  const openSheet = useAppStore((s) => s.openSheet);
  const resolved = session?.uid ?? uid;
  // A sheet's outcome (committed, merged, discarded) refreshes the list.
  const result = useChangesResult(resolved);
  // Reload when the session's state changes (a turn that ended may have changed files).
  const stateKind = session?.activity.state.kind ?? null;

  const refresh = useCallback(() => {
    commands.changes_get({ uid: resolved }).then(
      (data) => {
        setLoad({ kind: 'ready', data });
        setCount(resolved, data.files.length);
      },
      (error: IpcError) => setLoad({ kind: 'error', error }),
    );
  }, [resolved]);

  useEffect(() => {
    refresh();
  }, [refresh, stateKind, result.seq]);

  useEffect(() => {
    if (result.line) setLine(result.line);
  }, [result.seq]); // eslint-disable-line react-hooks/exhaustive-deps

  const openFile = useCallback(
    (path: string) => {
      commands.ide_open({ target: { kind: 'file', uid: resolved, path, line: null } }).then(undefined, (e) => setLine(errorLine(e)));
    },
    [resolved],
  );
  const openDiff = useCallback(
    (path: string) => {
      commands.ide_open({ target: { kind: 'diff', uid: resolved, path } }).then(undefined, (e) => setLine(errorLine(e)));
    },
    [resolved],
  );

  if (!session) return null;
  const agent = vendorBadge(session.vendor);

  if (load.kind === 'loading') return <p className="g-changes-status">{t('common.loading')}</p>;
  if (load.kind === 'error') {
    const notGit = load.error.code === 'UNSUPPORTED' && load.error.params?.cap === 'notGit';
    return (
      <div className="g-changes">
        <p className="g-changes-status">{notGit ? t('error.unsupported') : te(load.error)}</p>
        {load.error.detail && <p className="g-changes-detail mono">{load.error.detail}</p>}
        <button type="button" className="g-cbtn" onClick={refresh}>
          {t('changes.refresh')}
        </button>
      </div>
    );
  }

  const data = load.data;
  const readOnly = session.caps.changes !== 'full';
  const added = data.files.reduce((n, f) => n + f.added, 0);
  const removed = data.files.reduce((n, f) => n + f.removed, 0);
  const owner = ownerWord(session, t) || agent;

  const reasonText = (state: ActionState) =>
    state.kind === 'disabled' ? td(state.reason, state.reason === 'changes.ownerWriting' ? { owner } : {}) : null;
  const reasons = new Set(
    [data.actions.commit, data.actions.merge, data.actions.discard].map(reasonText).filter((x): x is string => x !== null),
  );

  const action = (state: ActionState, label: string, open: () => void, extra = '') =>
    state.kind === 'hidden' || readOnly ? null : (
      <button type="button" className={`g-cbtn${extra}`} disabled={state.kind === 'disabled'} onClick={open}>
        {label}
      </button>
    );

  return (
    <div className="g-changes">
      <div className="g-changes-head">
        {data.base && (
          <p className="g-changes-meta">
            {t('changes.base', { branch: data.base.branch, commit: data.base.commitShort })}
            {data.base.newerOnMain > 0 && <> · {t('changes.newer', { n: data.base.newerOnMain, branch: data.base.branch })}</>}
          </p>
        )}
        <p className="g-changes-meta">
          {t('changes.fileCount', { n: data.files.length })}
          {data.files.length > 0 && <span className="mono"> · {formatDiffStat(added, removed, locale)}</span>}
        </p>
        {data.sharedFolder && <p className="g-changes-note">{t('changes.sharedFolder', { n: data.sharedFolder.otherLiveSessions })}</p>}
        <button type="button" className="g-cbtn g-cbtn-ghost g-changes-refresh" onClick={refresh}>
          {t('changes.refresh')}
        </button>
      </div>

      {data.files.length === 0 ? (
        <p className="g-changes-status">{t('changes.empty')}</p>
      ) : (
        <FileTable files={data.files} onOpen={openFile} onDiff={openDiff} />
      )}

      {data.commits.length > 0 && (
        <section className="g-changes-commits" aria-label={t('changes.commits')}>
          <h3 className="g-changes-h">{t('changes.commits')}</h3>
          <ul>
            {data.commits.map((c) => (
              <li key={c.commitShort}>
                <span className="mono">{c.commitShort}</span>
                <span className="g-changes-subject">{c.subject}</span>
              </li>
            ))}
          </ul>
        </section>
      )}

      {!readOnly && (
        <div className="g-changes-actions">
          {session.caps.send !== 'none' && onAskCommit && data.files.length > 0 && (
            <button type="button" className="g-cbtn" onClick={() => onAskCommit(t('changes.askCommitText'))}>
              {t('action.askCommit', { agent })}
            </button>
          )}
          {action(data.actions.commit, t('changes.commit'), () => openSheet('commit'))}
          {action(data.actions.merge, t('changes.merge'), () => openSheet('merge'))}
          <span className="g-changes-spacer" />
          {action(data.actions.discard, t('changes.discard'), () => openSheet('discardWorktree'), ' g-cbtn-danger')}
        </div>
      )}
      {[...reasons].map((r) => (
        <p key={r} className="g-changes-note">
          {r}
        </p>
      ))}
      {line && (
        <p className="g-changes-note" role="status">
          {td(line.key, line.params)}
          {line.detail ? <span className="g-changes-detail mono"> {line.detail}</span> : null}
        </p>
      )}

    </div>
  );
}

function FileTable({ files, onOpen, onDiff }: { files: readonly FileRow[]; onOpen: (p: string) => void; onDiff: (p: string) => void }) {
  const { t, locale } = useI18n();
  const openSession = useAppStore((s) => s.openSession);
  const sessions = useAppStore((s) => s.world.sessions);
  const titleOf = (other: SessionUid) => sessions.get(other)?.title ?? null;
  const onKey = (event: ReactKeyboardEvent<HTMLLIElement>, path: string) => {
    if (event.nativeEvent.isComposing || event.repeat) return;
    if (event.key === 'Enter' && !event.metaKey) {
      event.preventDefault();
      onDiff(path);
    } else if (event.key.toLowerCase() === 'o' && event.metaKey && !event.shiftKey) {
      event.preventDefault();
      onOpen(path);
    }
  };
  return (
    <section aria-label={t('changes.files')}>
      <ul className="g-changes-files">
        {files.map((f) => {
          let madeBy: ReactNode = null;
          switch (f.madeBy.kind) {
            case 'thisSession':
              madeBy = t('changes.madeBy.this');
              break;
            case 'otherSession': {
              const other = f.madeBy.uid;
              madeBy = (
                <button type="button" className="g-linkbtn" onClick={() => openSession(other)}>
                  {titleOf(other) ?? t('changes.madeBy.other')}
                </button>
              );
              break;
            }
            case 'notFromKnownSession':
              madeBy = t('changes.unknownSource');
              break;
            default:
              madeBy = t('common.unknown');
          }
          const mark = f.change === 'added' ? 'A' : f.change === 'deleted' ? 'D' : 'M';
          return (
            <li key={f.pathRel} className="g-changes-file" tabIndex={0} onKeyDown={(e) => onKey(e, f.pathRel)}>
              <span className="g-changes-mark mono" aria-hidden="true">
                {mark}
              </span>
              <span className="g-changes-path mono">{f.pathRel}</span>
              <span className="g-changes-stat mono">{formatDiffStat(f.added, f.removed, locale)}</span>
              <span className="g-changes-parts">{f.parts.join(', ')}</span>
              <span className="g-changes-by">{madeBy}</span>
              <span className="g-changes-fileacts">
                <button type="button" className="g-cbtn g-cbtn-ghost" aria-keyshortcuts="Enter" onClick={() => onDiff(f.pathRel)}>
                  {t('convo.diff')}
                </button>
                {f.change !== 'deleted' && (
                  <button type="button" className="g-cbtn g-cbtn-ghost" aria-keyshortcuts="Meta+O" onClick={() => onOpen(f.pathRel)}>
                    {t('convo.open')}
                  </button>
                )}
              </span>
            </li>
          );
        })}
      </ul>
    </section>
  );
}
