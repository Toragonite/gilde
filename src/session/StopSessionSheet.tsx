/**
 * Stop session… (`⇧⌘⌫`, ux-ia §4.8, §5.9; contract §4.2.2, A23.5, A24.7).
 *
 * Shell convention (A23.5): no props; it renders while `ui.sheet` is
 * 'stopSession', acts on the main view's focused session, and closes with
 * `closeSheet()`. A destructive sheet: focus starts on Cancel and `⌘⏎` never
 * confirms. The sentence is the core's `ownerText` as sent: `sheet.stopSession`
 * for a session of Gilde's own (A24.7, A25.3), `sheet.stopWatched` for one
 * that runs elsewhere (stopping ends it there too). Never a SIGKILL of a
 * vendor process; survivors are reported on the Session screen.
 */

import { useEffect, useRef, useState, useSyncExternalStore } from 'react';

import { useSheetTarget } from '../changes/sheets';
import { Sheet } from '../components/Sheet';
import { useI18n } from '../i18n/useI18n';
import { commands } from '../lib/commands';
import type { Session } from '../lib/gen/Session';
import type { SessionStopPreviewRes } from '../lib/gen/SessionStopPreviewRes';
import type { SessionUid } from '../lib/gen/SessionUid';
import type { IpcError } from '../lib/ipcError';

// The outcome line for the Session screen ("The session was stopped." /
// "2 processes are still running."), per session, memory only.
const outcomes = new Map<string, { survivors: number; seq: number }>();
const listeners = new Set<() => void>();
let seq = 0;

export function useStopOutcome(uid: SessionUid | null): { survivors: number; seq: number } | null {
  useSyncExternalStore(
    (fn) => {
      listeners.add(fn);
      return () => listeners.delete(fn);
    },
    () => seq,
    () => seq,
  );
  return uid ? (outcomes.get(uid) ?? null) : null;
}

function noteOutcome(uid: SessionUid, survivors: number) {
  outcomes.set(uid, { survivors, seq: ++seq });
  for (const fn of [...listeners]) fn();
}

export function StopSessionSheet() {
  const { open, session, close } = useSheetTarget('stopSession');
  if (!open || !session) return null;
  return <StopBody key={session.uid} session={session} close={close} />;
}

function StopBody({ session, close }: { session: Session; close: () => void }) {
  const { t, td, te } = useI18n();
  const [preview, setPreview] = useState<SessionStopPreviewRes | null>(null);
  const [error, setError] = useState<IpcError | null>(null);
  const [busy, setBusy] = useState(false);
  const cancelRef = useRef<HTMLButtonElement>(null);

  useEffect(() => {
    let live = true;
    commands.session_stop_preview({ uid: session.uid }).then(
      (res) => live && setPreview(res),
      (e: IpcError) => live && setError(e),
    );
    return () => {
      live = false;
    };
  }, [session.uid]);

  const confirm = () => {
    if (!preview || busy) return;
    setBusy(true);
    commands.session_stop({ uid: session.uid, confirmToken: preview.confirmToken }).then(
      (res) => {
        noteOutcome(session.uid, res.survivors);
        close();
      },
      (e: IpcError) => {
        setBusy(false);
        setError(e);
      },
    );
  };

  const sentence = preview?.ownerText ?? null;
  return (
    <Sheet
      open
      destructive
      title={t('action.stopSession')}
      onCancel={close}
      initialFocus={cancelRef}
      actions={
        <>
          <button ref={cancelRef} type="button" className="g-sbtn" onClick={close}>
            {t('common.cancel')}
          </button>
          <button type="button" className="g-sbtn g-sbtn-danger" disabled={!preview || busy} onClick={confirm}>
            {t('action.stop')}
          </button>
        </>
      }
    >
      {sentence ? <p className="g-bar-text">{td(sentence.key, sentence.params)}</p> : !error && <p className="g-bar-text">{t('common.loading')}</p>}
      {error && (
        <p className="g-bar-line" role="status">
          {te(error)}
          {error.detail ? <span className="mono"> {error.detail}</span> : null}
        </p>
      )}
    </Sheet>
  );
}
