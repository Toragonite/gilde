/**
 * The Changes sheets (ux-ia §3.13; contract §4.2.5, A17.4–A17.6) in the shell's
 * sheet convention (A23.5): `CommitSheet`, `MergeSheet`, `DiscardWorktreeSheet`
 * take no props. Each renders only while `ui.sheet` names it, acts on the
 * session the main view is focused on (the open session, the focused Split
 * tile or the focused team member — the `app_view_state` focus), and closes
 * with `closeSheet()`. The shell mounts them; `ChangesView` opens them.
 *
 * Every action is preview → sheet → confirm token. Merge and Discard are
 * destructive (focus on Cancel, no `⌘⏎`); Discard ticks each loss on its own
 * and never deletes the branch (A17.5). The outcome line is handed back to
 * `ChangesView` through `changesResult` (memory only).
 */

import { useEffect, useId, useRef, useState, useSyncExternalStore } from 'react';

import { Sheet } from '../components/Sheet';
import type { MessageKey } from '../i18n';
import { useI18n } from '../i18n/useI18n';
import { commands } from '../lib/commands';
import type { GitCommitPreviewRes } from '../lib/gen/GitCommitPreviewRes';
import type { GitMergePreviewRes } from '../lib/gen/GitMergePreviewRes';
import type { Session } from '../lib/gen/Session';
import type { SessionUid } from '../lib/gen/SessionUid';
import type { WorktreeDiscardPreviewRes } from '../lib/gen/WorktreeDiscardPreviewRes';
import { isIpcError } from '../lib/ipcError';
import type { SheetId } from '../store/uiSlice';
import { useAppStore } from '../store/useAppStore';
import { viewStateOf } from '../store/viewState';
import { sessionOf } from '../store/worldSlice';

export type Line = { key: string; params: Readonly<Record<string, unknown>>; detail?: string | null } | null;

export function errorLine(e: unknown): Line {
  if (isIpcError(e)) return { key: e.key, params: e.params, detail: e.detail };
  return { key: 'error.internal', params: {} };
}

// ---------------------------------------------------------------------------
// Outcome lines (sheet → ChangesView), per session, memory only
// ---------------------------------------------------------------------------

const results = new Map<string, { line: Line; seq: number }>();
const resultListeners = new Set<() => void>();
let resultSeq = 0;

export function setChangesResult(uid: SessionUid, line: Line): void {
  results.set(uid, { line, seq: ++resultSeq });
  for (const fn of [...resultListeners]) fn();
}

/** The last outcome line for a session, and a counter that changes with every outcome. */
export function useChangesResult(uid: SessionUid | null): { line: Line; seq: number } {
  useSyncExternalStore(
    (fn) => {
      resultListeners.add(fn);
      return () => resultListeners.delete(fn);
    },
    () => resultSeq,
    () => resultSeq,
  );
  return (uid && results.get(uid)) || { line: null, seq: 0 };
}

// ---------------------------------------------------------------------------
// The target session
// ---------------------------------------------------------------------------

/** The session a prop-less sheet acts on: the main view's focused session (A23.5). */
export function useSheetTarget(id: SheetId): { open: boolean; session: Session | null; close: () => void } {
  const open = useAppStore((s) => s.ui.sheet === id);
  const session = useAppStore((s) => {
    if (s.ui.sheet !== id) return null;
    const uid = viewStateOf(s.ui, s.world).focusedUid;
    return uid ? sessionOf(s.world, uid) : null;
  });
  const close = useAppStore((s) => s.closeSheet);
  return { open, session, close };
}

function SheetError({ line }: { line: Line }) {
  const { td } = useI18n();
  if (!line) return null;
  return (
    <p className="g-changes-note" role="status">
      {td(line.key, line.params)}
      {line.detail ? <span className="g-changes-detail mono"> {line.detail}</span> : null}
    </p>
  );
}

// ---------------------------------------------------------------------------
// Commit
// ---------------------------------------------------------------------------

export function CommitSheet() {
  const { open, session, close } = useSheetTarget('commit');
  if (!open || !session) return null;
  return <CommitBody key={session.uid} session={session} close={close} />;
}

function CommitBody({ session, close }: { session: Session; close: () => void }) {
  const { t } = useI18n();
  const [preview, setPreview] = useState<GitCommitPreviewRes | null>(null);
  const [message, setMessage] = useState(session.title ?? '');
  const [checked, setChecked] = useState<ReadonlySet<string>>(new Set());
  const [line, setLine] = useState<Line>(null);
  const [busy, setBusy] = useState(false);
  const messageId = useId();

  useEffect(() => {
    let live = true;
    commands
      .changes_get({ uid: session.uid })
      .then((view) => commands.git_commit_preview({ uid: session.uid, paths: view.files.map((f) => f.pathRel) }))
      .then(
        (res) => {
          if (!live) return;
          setPreview(res);
          setChecked(new Set(res.files.map((f) => f.pathRel)));
        },
        (e) => live && setLine(errorLine(e)),
      );
    return () => {
      live = false;
    };
  }, [session.uid]);

  const canCommit = preview !== null && message.trim() !== '' && checked.size > 0 && !busy;
  const confirm = () => {
    if (!preview || !canCommit) return;
    setBusy(true);
    commands
      .git_commit({ uid: session.uid, message, paths: preview.files.map((f) => f.pathRel).filter((p) => checked.has(p)), confirmToken: preview.confirmToken })
      .then(
        (res) => {
          setChangesResult(session.uid, { key: 'changes.committed', params: { commit: res.commit } });
          close();
        },
        (e) => {
          setBusy(false);
          setLine(errorLine(e));
        },
      );
  };

  return (
    <Sheet
      open
      title={t('changes.commitTitle')}
      onCancel={close}
      onConfirm={confirm}
      size="lg"
      actions={
        <>
          <button type="button" className="g-cbtn" onClick={close}>
            {t('common.cancel')}
          </button>
          <button type="button" className="g-cbtn g-cbtn-primary" disabled={!canCommit} onClick={confirm}>
            {t('changes.commitConfirm')} <span className="g-key">⌘⏎</span>
          </button>
        </>
      }
    >
      <label className="g-changes-label" htmlFor={messageId}>
        {t('changes.commitMessage')}
      </label>
      <textarea id={messageId} className="g-changes-message" data-autofocus="" value={message} rows={3} onChange={(e) => setMessage(e.target.value)} />
      {preview && <p className="g-changes-meta">{t('changes.author', { name: `${preview.author.name} <${preview.author.email}>` })}</p>}
      {preview === null && !line && <p className="g-changes-meta">{t('common.loading')}</p>}
      {preview && (
        <ul className="g-changes-checks">
          {preview.files.map((f) => (
            <li key={f.pathRel}>
              <label>
                <input
                  type="checkbox"
                  checked={checked.has(f.pathRel)}
                  onChange={() =>
                    setChecked((c) => {
                      const next = new Set(c);
                      if (next.has(f.pathRel)) next.delete(f.pathRel);
                      else next.add(f.pathRel);
                      return next;
                    })
                  }
                />
                <span className="mono">{f.pathRel}</span>
              </label>
            </li>
          ))}
        </ul>
      )}
      <SheetError line={line} />
    </Sheet>
  );
}

// ---------------------------------------------------------------------------
// Merge
// ---------------------------------------------------------------------------

export function MergeSheet() {
  const { open, session, close } = useSheetTarget('merge');
  if (!open || !session) return null;
  return <MergeBody key={session.uid} session={session} close={close} />;
}

function MergeBody({ session, close }: { session: Session; close: () => void }) {
  const { t } = useI18n();
  const [preview, setPreview] = useState<GitMergePreviewRes | null>(null);
  const [branch, setBranch] = useState('');
  const [line, setLine] = useState<Line>(null);
  const [busy, setBusy] = useState(false);
  const cancelRef = useRef<HTMLButtonElement>(null);

  useEffect(() => {
    let live = true;
    commands.changes_get({ uid: session.uid }).then((v) => live && setBranch(v.base?.branch ?? ''), () => undefined);
    commands.git_merge_preview({ uid: session.uid }).then(
      (res) => live && setPreview(res),
      (e) => live && setLine(errorLine(e)),
    );
    return () => {
      live = false;
    };
  }, [session.uid]);

  const effect = preview?.effect;
  const confirm = () => {
    if (!preview || busy || effect?.kind === 'upToDate') return;
    setBusy(true);
    commands.git_merge({ uid: session.uid, confirmToken: preview.confirmToken }).then(
      (res) => {
        setChangesResult(session.uid, { key: 'changes.merged', params: { commit: res.mergeCommit } });
        close();
      },
      (e) => {
        setBusy(false);
        setLine(errorLine(e));
      },
    );
  };

  return (
    <Sheet
      open
      destructive
      title={t('changes.mergeTitle', { branch })}
      onCancel={close}
      initialFocus={cancelRef}
      actions={
        <>
          <button ref={cancelRef} type="button" className="g-cbtn" onClick={close}>
            {t('common.cancel')}
          </button>
          <button type="button" className="g-cbtn g-cbtn-primary" disabled={!preview || busy || effect?.kind === 'upToDate'} onClick={confirm}>
            {t('changes.mergeConfirm')}
          </button>
        </>
      }
    >
      {effect ? (
        <>
          <p className="g-changes-meta">
            {effect.kind === 'fastForward'
              ? t('changes.merge.fastForward', { n: effect.commits })
              : effect.kind === 'mergeCommit'
                ? t('changes.merge.mergeCommit', { n: effect.commits })
                : t('changes.merge.upToDate')}
          </p>
          {effect.kind !== 'upToDate' && <p className="g-changes-meta">{t('changes.mergeFiles', { n: effect.filesChanged })}</p>}
        </>
      ) : (
        !line && <p className="g-changes-meta">{t('common.loading')}</p>
      )}
      <SheetError line={line} />
    </Sheet>
  );
}

// ---------------------------------------------------------------------------
// Discard worktree
// ---------------------------------------------------------------------------

export function DiscardWorktreeSheet() {
  const { open, session, close } = useSheetTarget('discardWorktree');
  if (!open || !session) return null;
  return <DiscardBody key={session.uid} session={session} close={close} />;
}

function DiscardBody({ session, close }: { session: Session; close: () => void }) {
  const { t } = useI18n();
  const [preview, setPreview] = useState<WorktreeDiscardPreviewRes | null>(null);
  const [accepted, setAccepted] = useState<ReadonlySet<string>>(new Set());
  const [line, setLine] = useState<Line>(null);
  const [busy, setBusy] = useState(false);
  const cancelRef = useRef<HTMLButtonElement>(null);

  useEffect(() => {
    let live = true;
    commands.worktree_discard_preview({ uid: session.uid }).then(
      (res) => live && setPreview(res),
      (e) => live && setLine(errorLine(e)),
    );
    return () => {
      live = false;
    };
  }, [session.uid]);

  const all = preview !== null && preview.losses.every((l) => accepted.has(l.id));
  const confirm = () => {
    if (!preview || !all || busy) return;
    setBusy(true);
    commands.worktree_discard({ uid: session.uid, confirmToken: preview.confirmToken, acceptedLosses: [...accepted] }).then(
      () => {
        setChangesResult(session.uid, { key: 'changes.discarded', params: {} });
        close();
      },
      (e) => {
        setBusy(false);
        setLine(errorLine(e));
      },
    );
  };

  return (
    <Sheet
      open
      destructive
      title={t('changes.discardTitle')}
      onCancel={close}
      initialFocus={cancelRef}
      actions={
        <>
          <button ref={cancelRef} type="button" className="g-cbtn" onClick={close}>
            {t('common.cancel')}
          </button>
          <button type="button" className="g-cbtn g-cbtn-danger" disabled={!all || busy} onClick={confirm}>
            {t('changes.discardConfirm')}
          </button>
        </>
      }
    >
      {preview ? (
        <>
          <ul className="g-changes-checks">
            {preview.losses.map((l) => (
              <li key={l.id}>
                <label>
                  <input
                    type="checkbox"
                    checked={accepted.has(l.id)}
                    onChange={() =>
                      setAccepted((a) => {
                        const next = new Set(a);
                        if (next.has(l.id)) next.delete(l.id);
                        else next.add(l.id);
                        return next;
                      })
                    }
                  />
                  <span>{t(`changes.loss.${l.kind}` as MessageKey, { n: l.count })}</span>
                </label>
              </li>
            ))}
          </ul>
          <p className="g-changes-meta">{t('changes.branchKept')}</p>
        </>
      ) : (
        !line && <p className="g-changes-meta">{t('common.loading')}</p>
      )}
      <SheetError line={line} />
    </Sheet>
  );
}
