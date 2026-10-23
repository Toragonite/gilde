/**
 * The quit sheet (`⌘Q` while sessions run in Gilde; ux-ia §3.16, §5.10;
 * contract §4.2.1, §10.2, A23.5, §15.5 T18).
 *
 * - It opens on `ui.sheet === 'quit'` and on `gilde://quit-requested`
 *   (`ui.quitRequested`); with no session in Gilde the core quits without it.
 * - It lists the In-Gilde sessions by project, marks the ones waiting for the
 *   user, names team leads with their teammates, and says that sessions
 *   started elsewhere keep running.
 * - A pending approval or answer is lost with its session
 *   (`sheet.quit.pendingLost`, R6 LP-04): Continue does not re-ask.
 * - Destructive: focus starts on Cancel and `⌘⏎` never confirms.
 * - `CONFLICT{changed}` (sessions started or ended since the preview) re-reads
 *   the preview and says so; the user confirms again.
 * - Cancel tells the core the exit is not wanted (`app_quit_cancel`).
 */

import { useCallback, useEffect, useRef, useState } from 'react';

import { Sheet } from '../components/Sheet';
import { StateGlyph } from '../components/StateGlyph';
import type { MessageKey } from '../i18n';
import { useI18n } from '../i18n/useI18n';
import { commands } from '../lib/commands';
import type { AppQuitPreviewRes } from '../lib/gen/AppQuitPreviewRes';
import type { QuitRow } from '../lib/gen/QuitRow';
import type { IpcError } from '../lib/ipcError';
import { displayName } from '../shell/words';
import { appStore, useAppStore } from '../store/useAppStore';
import { sessionOf } from '../store/worldSlice';
import './quit.css';

const WAITING_KEY: Readonly<Record<'approval' | 'answer', MessageKey>> = {
  approval: 'sheet.quit.waitingApproval',
  answer: 'sheet.quit.waitingAnswer',
};

export function QuitSheet() {
  const open = useAppStore((s) => s.ui.sheet === 'quit' || s.ui.quitRequested);
  return open ? <QuitBody /> : null;
}

/** Rows grouped by project, projects in first-seen order (the core's quit order). */
export function groupByProject(rows: readonly QuitRow[]): QuitRow[] {
  const order: string[] = [];
  const groups = new Map<string, QuitRow[]>();
  for (const r of rows) {
    const key = r.projectId ?? '';
    if (!groups.has(key)) {
      groups.set(key, []);
      order.push(key);
    }
    groups.get(key)?.push(r);
  }
  return order.flatMap((k) => groups.get(k) ?? []);
}

function QuitBody() {
  const { t, te, locale } = useI18n();
  const world = useAppStore((s) => s.world);
  const progress = useAppStore((s) => s.world.quit);
  const [preview, setPreview] = useState<AppQuitPreviewRes | null>(null);
  const [error, setError] = useState<IpcError | null>(null);
  const [changed, setChanged] = useState(false);
  const [busy, setBusy] = useState(false);
  const [round, setRound] = useState(0);
  const cancelRef = useRef<HTMLButtonElement>(null);
  const busyRef = useRef(false);
  busyRef.current = busy;

  useEffect(() => {
    let live = true;
    commands.app_quit_preview({}).then(
      (res) => live && setPreview(res),
      (e: IpcError) => live && setError(e),
    );
    return () => {
      live = false;
    };
  }, [round]);

  const cancel = useCallback(() => {
    // Once the quit runs there is nothing left to cancel.
    if (busyRef.current) return;
    const store = appStore.getState();
    if (store.ui.quitRequested) {
      void commands.app_quit_cancel({}).catch(() => undefined);
      store.setQuitRequested(false);
    }
    store.closeSheet();
  }, []);

  const confirm = useCallback(() => {
    if (!preview || busy) return;
    setBusy(true);
    setError(null);
    commands.app_quit({ confirmToken: preview.confirmToken }).then(
      () => undefined,
      (e: IpcError) => {
        setBusy(false);
        if (e.code === 'CONFLICT') {
          setChanged(true);
          setPreview(null);
          setRound((r) => r + 1);
          return;
        }
        setError(e);
      },
    );
  }, [preview, busy]);

  const all = preview ? groupByProject(preview.sessions) : [];
  const n = all.length;
  const waiting = all.some((r) => r.waiting !== null);
  // A hosted team reads as one row, its lead's ("Team auth-split · lead + 3"); a
  // waiting teammate keeps its own row so the request it loses is named.
  const leadTeams = new Set(all.filter((r) => r.teamLead).map((r) => sessionOf(world, r.uid)?.team?.teamKey).filter(Boolean));
  const rows = all.filter((r) => {
    if (r.teamLead || r.waiting !== null) return true;
    const key = sessionOf(world, r.uid)?.team?.teamKey;
    return !key || !leadTeams.has(key);
  });
  const teamOf = (row: QuitRow) => {
    const s = sessionOf(world, row.uid);
    const key = s?.team?.teamKey;
    if (!key) return null;
    const team = world.teams.get(key);
    const members = preview?.teams.find((x) => x.teamKey === key)?.members ?? null;
    // `members` counts the teammates without the lead (A29.4).
    return { name: team?.name ?? null, others: members === null ? null : Math.max(0, members) };
  };

  let progressLine: string | null = null;
  if (busy && progress) {
    if (progress.phase === 'recording') progressLine = t('sheet.quit.recording');
    else if (progress.phase === 'stopping') progressLine = t('sheet.quit.stopping', { stopped: progress.stopped, all: progress.total });
    else progressLine = t('sheet.quit.done');
  }

  return (
    <Sheet
      open
      destructive
      size="lg"
      title={t('sheet.quit.title')}
      onCancel={cancel}
      initialFocus={cancelRef}
      actions={
        <>
          <button ref={cancelRef} type="button" className="btn" onClick={cancel} disabled={busy}>
            {t('common.cancel')}
          </button>
          <button type="button" className="btn danger" disabled={!preview || busy} onClick={confirm} data-quit-confirm="">
            {n > 0 ? t('sheet.quit.confirm', { n }) : t('sheet.quit.quitNow')}
          </button>
        </>
      }
    >
      <div className="qt-body">
        {preview ? (
          <p className="qt-line">{n > 0 ? t('sheet.quit', { n }) : t('sheet.quit.none')}</p>
        ) : !error ? (
          <p className="qt-line qt-muted">{t('common.loading')}</p>
        ) : null}
        {changed ? <p className="qt-line qt-muted">{t('sheet.quit.changed')}</p> : null}
        {n > 0 ? (
          <ul className="qt-rows" aria-label={t('sheet.quit.list')} tabIndex={0}>
            {rows.map((row) => {
              const s = sessionOf(world, row.uid);
              const project = row.projectId ? world.projects.get(row.projectId)?.name : null;
              const team = row.teamLead ? teamOf(row) : null;
              const name = team?.name ? t('sheet.quit.team', { name: team.name }) : s ? displayName(t, world, s, locale) : t('common.unknown');
              return (
                <li key={row.uid} className="qt-row" data-waiting={row.waiting ?? undefined}>
                  <StateGlyph state={s ? s.activity.state.kind : row.waiting === 'approval' ? 'needsApproval' : row.waiting === 'answer' ? 'needsAnswer' : 'ready'} />
                  <span className="qt-name">{name}</span>
                  {team && team.others !== null ? <span className="qt-sub">{t('sheet.quit.lead', { others: team.others })}</span> : null}
                  {row.waiting ? <span className="qt-wait">{t(WAITING_KEY[row.waiting])}</span> : null}
                  <span className="qt-project">{project ?? t('sheet.quit.noProject')}</span>
                </li>
              );
            })}
          </ul>
        ) : null}
        {waiting ? <p className="qt-line qt-warn">{t('sheet.quit.pendingLost')}</p> : null}
        {preview && preview.elsewhere > 0 ? <p className="qt-line qt-muted">{t('sheet.quit.elsewhere', { n: preview.elsewhere })}</p> : null}
        {progressLine ? (
          <p className="qt-line qt-muted" role="status">
            {progressLine}
          </p>
        ) : null}
        {error ? (
          <p className="qt-line qt-error" role="status">
            {te(error)}
            {error.detail ? <span className="mono qt-detail">{error.detail}</span> : null}
          </p>
        ) : null}
      </div>
    </Sheet>
  );
}
