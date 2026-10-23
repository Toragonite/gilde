/**
 * The Clean-up sheet — "Stop…" on one leftover, "Clean up all…" on all
 * (ux-ia §3.15, §5.9; contract §4.2.7, §9.8, A23.5). It replaces the old
 * monitor's kill sheet.
 *
 * - It names each target: what it is (the core's display label), why it is a
 *   leftover, its memory, and how it will be stopped; anything the core cannot
 *   identify is listed as refused and never stopped (ux-ia §9.2 row 114).
 * - Destructive: focus starts on Cancel and `⌘⏎` never confirms.
 * - The stop runs behind the core's 10 s undo window; the toast that reports
 *   it carries Undo (`undo_cancel`, toastSlice).
 * - `CONFLICT{changed}` (the leftovers moved since the preview) re-reads the
 *   preview and says so; nothing is stopped on stale facts.
 *
 * Sheet convention (A23.5): `CleanUpSheet` takes no props. Which processes it
 * covers comes from `requestCleanUp(keys)` (one leftover's Stop…); opened any
 * other way (Home's leftover line, the queue) it covers every leftover.
 */

import { useCallback, useEffect, useRef, useState } from 'react';

import { Mono } from '../components/Mono';
import { Sheet } from '../components/Sheet';
import { StateGlyph } from '../components/StateGlyph';
import type { MessageKey } from '../i18n';
import { useI18n } from '../i18n/useI18n';
import { commands } from '../lib/commands';
import { formatBytes } from '../lib/format';
import type { HealthStopPreviewRes } from '../lib/gen/HealthStopPreviewRes';
import type { LeftoverReason } from '../lib/gen/LeftoverReason';
import type { ProcessKey } from '../lib/gen/ProcessKey';
import type { StopMethod } from '../lib/gen/StopMethod';
import type { IpcError } from '../lib/ipcError';
import { appStore, useAppStore } from '../store/useAppStore';
import { leftoverList } from './model';
import './accounts.css';

/** `health_stop_preview` takes 1..=50 keys. */
export const MAX_STOP_KEYS = 50;

export const REASON_KEY: Readonly<Record<LeftoverReason, MessageKey>> = {
  teammateOfGoneLead: 'leftover.teammateOfGoneLead',
  paneServerOfDeadLead: 'leftover.paneServerOfDeadLead',
  toolServerOfEndedSession: 'leftover.toolServerOfEndedSession',
  gildeCrashLeftover: 'leftover.gildeCrashLeftover',
  quitSurvivor: 'leftover.quitSurvivor',
};

const HOW_KEY: Readonly<Record<StopMethod, MessageKey>> = {
  fenced: 'stopHow.fenced',
  claudeStop: 'stopHow.claudeStop',
  termAfterIdentity: 'stopHow.termAfterIdentity',
};

let requested: ProcessKey[] | null = null;

/** Open the Clean-up sheet for these leftovers (`null` = all of them). */
export function requestCleanUp(keys: readonly ProcessKey[] | null): void {
  requested = keys ? [...keys] : null;
  appStore.getState().openSheet('cleanUp');
}

/** Tests only. */
export function resetCleanUpRequestForTests(): void {
  requested = null;
}

export function CleanUpSheet() {
  const open = useAppStore((s) => s.ui.sheet === 'cleanUp');
  return open ? <CleanUpBody /> : null;
}

function CleanUpBody() {
  const { t, te, locale } = useI18n();
  const closeSheet = useAppStore((s) => s.closeSheet);
  const [keys] = useState<ProcessKey[]>(() => {
    const keys = requested ?? leftoverList(appStore.getState().world).map((p) => p.key);
    requested = null;
    return keys.slice(0, MAX_STOP_KEYS);
  });
  const [preview, setPreview] = useState<HealthStopPreviewRes | null>(null);
  const [error, setError] = useState<IpcError | null>(null);
  const [changed, setChanged] = useState(false);
  const [busy, setBusy] = useState(false);
  const [round, setRound] = useState(0);
  const cancelRef = useRef<HTMLButtonElement>(null);

  useEffect(() => {
    if (keys.length === 0) return;
    let live = true;
    commands.health_stop_preview({ processKeys: keys }).then(
      (res) => live && setPreview(res),
      (e: IpcError) => live && setError(e),
    );
    return () => {
      live = false;
    };
  }, [keys, round]);

  const confirm = useCallback(() => {
    if (!preview || busy || preview.targets.length === 0) return;
    setBusy(true);
    setError(null);
    commands.health_stop({ confirmToken: preview.confirmToken }).then(
      (res) => {
        const store = appStore.getState();
        if (res.stopped > 0) store.pushToastEvent({ key: 'toast.cleanup.scheduled', params: { n: res.stopped }, undoToken: res.undoToken });
        if (res.survivors > 0) store.pushToastEvent({ key: 'toast.cleanup.survivors', params: { n: res.survivors }, undoToken: null });
        closeSheet();
      },
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
  }, [preview, busy, closeSheet]);

  const targets = preview?.targets ?? [];
  const memory = targets.reduce((sum, x) => sum + (Number.isFinite(x.memoryBytes) ? x.memoryBytes : 0), 0);
  const refused = preview?.refused.length ?? 0;

  return (
    <Sheet
      open
      destructive
      size="lg"
      title={t('sheet.cleanup.title')}
      onCancel={closeSheet}
      initialFocus={cancelRef}
      actions={
        <>
          <button ref={cancelRef} type="button" className="btn" onClick={closeSheet}>
            {t('common.cancel')}
          </button>
          <button type="button" className="btn danger" disabled={!preview || busy || targets.length === 0} onClick={confirm}>
            {t('sheet.cleanup.stopN', { n: targets.length })}
          </button>
        </>
      }
    >
      <div className="cu-body">
        {keys.length === 0 ? (
          <p className="cu-line">{t('accounts.noLeftovers')}</p>
        ) : preview ? (
          targets.length > 0 ? (
            <p className="cu-line">{t('sheet.cleanup', { n: targets.length, memory: formatBytes(memory, locale) })}</p>
          ) : (
            <p className="cu-line">{t('sheet.cleanup.nothing')}</p>
          )
        ) : !error ? (
          <p className="cu-line muted">{t('common.loading')}</p>
        ) : null}
        {changed ? <p className="cu-line muted">{t('error.conflict')}</p> : null}
        {targets.length > 0 ? (
          <ul className="cu-targets" aria-label={t('accounts.heading.leftovers')} tabIndex={0}>
            {targets.map((x) => (
              <li key={x.processKey} className="cu-target">
                <StateGlyph state="leftover" />
                <span className="cu-what">
                  <Mono ellipsis>{x.what}</Mono>
                  <span className="cu-why">{t(REASON_KEY[x.why] ?? 'state.leftover')}</span>
                </span>
                <Mono className="cu-mem">{formatBytes(x.memoryBytes, locale)}</Mono>
                <span className="cu-how">{t(HOW_KEY[x.how] ?? 'stopHow.termAfterIdentity')}</span>
              </li>
            ))}
          </ul>
        ) : null}
        {refused > 0 ? <p className="cu-line warn">{t('sheet.cleanup.refused', { n: refused })}</p> : null}
        {error ? (
          <p className="cu-line error" role="status">
            {te(error)}
            {error.detail ? <span className="mono cu-detail">{error.detail}</span> : null}
          </p>
        ) : null}
        <p className="cu-line muted">{t('health.never')}</p>
      </div>
    </Sheet>
  );
}
