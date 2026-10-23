/**
 * The Recovered dialog (contract §10.2 step 6, §10.3, A29.3; R1 §2).
 *
 * Two phases, one surface, both non-modal — the dialog never takes focus:
 * these facts arrive while the user may already be working, and an event
 * never moves focus (UI-GUIDELINES §2-5).
 *
 *  1. **The undo window** (`world.boot.recovery`): the boot sweep scheduled
 *     identity-fenced stops for what the last run left behind (a crashed
 *     instance's children, the quit's survivors) under one undo token. The
 *     dialog reads "{n} processes from the last run will stop in {s} s" with
 *     [Keep them] (`undo_cancel{token}`, which cancels every stop of the
 *     group). It closes at `until`, after the undo, or when the core clears
 *     the window. A WebView that subscribes late still shows it, with less
 *     time left.
 *  2. **Afterwards**, what still runs is a leftover (`GildeCrashLeftover`,
 *     `QuitSurvivor`) and shows through `world.leftovers`: the dialog names
 *     those once, with Clean up… (the Clean-up sheet for exactly them, with
 *     its own undo) and Dismiss. Dismissed stays dismissed for this window.
 */

import { useEffect, useId, useState } from 'react';

import { Mono } from '../components/Mono';
import { StateGlyph } from '../components/StateGlyph';
import { REASON_KEY, requestCleanUp } from '../accounts/StopSheet';
import { useI18n } from '../i18n/useI18n';
import { clock } from '../lib/clock';
import { commands } from '../lib/commands';
import { formatBytes } from '../lib/format';
import type { ProcessKey } from '../lib/gen/ProcessKey';
import type { ProcessRecord } from '../lib/gen/ProcessRecord';
import type { RecoveryWindow } from '../lib/gen/RecoveryWindow';
import type { UndoToken } from '../lib/gen/UndoToken';
import type { IpcError } from '../lib/ipcError';
import { vendorBadge } from '../lib/vendor';
import { useAppStore, useShallow } from '../store/useAppStore';
import type { WorldState } from '../store/worldSlice';

const SHOWN = 5;
/** The countdown's step. */
const TICK_MS = 250;

let dismissedKeys = new Set<ProcessKey>();
/** Windows the user kept (undone) or that ran out here; never shown again. */
let closedWindows = new Set<UndoToken>();

/** Tests only. */
export function resetRecoveredForTests(): void {
  dismissedKeys = new Set();
  closedWindows = new Set();
}

/** The leftovers of the last run: crash leftovers and quit survivors. */
export function recoveredLeftovers(world: Pick<WorldState, 'leftovers'>): ProcessRecord[] {
  return [...world.leftovers.values()]
    .filter((p) => p.leftover !== null && (p.leftover.reason === 'gildeCrashLeftover' || p.leftover.reason === 'quitSurvivor'))
    .sort((a, b) => b.rssBytes - a.rssBytes || a.key.localeCompare(b.key));
}

/** Whole seconds left in a window (never below 0). */
export function secondsLeft(until: number, now: number): number {
  return Math.max(0, Math.ceil((until - now) / 1000));
}

function UndoWindow({ recovery, onClosed }: { recovery: RecoveryWindow; onClosed: () => void }) {
  const { t, te } = useI18n();
  const titleId = useId();
  const [now, setNow] = useState(() => clock.wall());
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<IpcError | null>(null);
  const left = secondsLeft(recovery.until, now);

  useEffect(() => {
    const id = setInterval(() => setNow(clock.wall()), TICK_MS);
    return () => clearInterval(id);
  }, []);

  useEffect(() => {
    if (left === 0) {
      closedWindows.add(recovery.undoToken);
      onClosed();
    }
  }, [left, recovery.undoToken, onClosed]);

  const keep = () => {
    if (busy) return;
    setBusy(true);
    setError(null);
    commands.undo_cancel({ undoToken: recovery.undoToken }).then(
      () => {
        closedWindows.add(recovery.undoToken);
        onClosed();
      },
      (e: IpcError) => {
        setBusy(false);
        // Too late: the window ended while the click was on its way.
        if (e.code === 'NOT_FOUND') {
          closedWindows.add(recovery.undoToken);
          onClosed();
          return;
        }
        setError(e);
      },
    );
  };

  if (left === 0) return null;
  return (
    <div className="rc" role="dialog" aria-modal="false" aria-labelledby={titleId} data-phase="window">
      <h2 id={titleId} className="rc-title">
        {t('recovered.title')}
      </h2>
      <p className="rc-text" role="timer" aria-live="off">
        {t('recovered.stopping', { n: recovery.stopping, s: left })}
      </p>
      {error ? (
        <p className="rc-text rc-error" role="status">
          {te(error)}
        </p>
      ) : null}
      <div className="rc-actions">
        <button type="button" className="btn" disabled={busy} onClick={keep}>
          {t('recovered.keep')}
        </button>
      </div>
    </div>
  );
}

function LeftoversNotice({ list }: { list: readonly ProcessRecord[] }) {
  const { t, locale } = useI18n();
  const titleId = useId();
  const [, setVersion] = useState(0);
  const visible = list.filter((p) => !dismissedKeys.has(p.key));
  if (visible.length === 0) return null;

  const crash = visible.some((p) => p.leftover?.reason === 'gildeCrashLeftover');
  const dismiss = () => {
    for (const p of visible) dismissedKeys.add(p.key);
    setVersion((v) => v + 1);
  };

  return (
    <div className="rc" role="dialog" aria-modal="false" aria-labelledby={titleId} data-phase="leftovers">
      <h2 id={titleId} className="rc-title">
        {t('recovered.title')}
      </h2>
      <p className="rc-text">{t(crash ? 'recovered.afterCrash' : 'recovered.afterQuit', { n: visible.length })}</p>
      <ul className="rc-list">
        {visible.slice(0, SHOWN).map((p) => (
          <li key={p.key} className="rc-row">
            <StateGlyph state="leftover" />
            <span className="rc-why">{p.leftover ? t(REASON_KEY[p.leftover.reason]) : t('state.leftover')}</span>
            <span className="rc-sub">{p.vendor ? vendorBadge(p.vendor) : ''}</span>
            <Mono className="rc-mem">{formatBytes(p.rssBytes, locale)}</Mono>
          </li>
        ))}
        {visible.length > SHOWN ? <li className="rc-more">{t('accounts.more', { n: visible.length - SHOWN })}</li> : null}
      </ul>
      <div className="rc-actions">
        <button type="button" className="btn ghost" onClick={dismiss}>
          {t('common.dismiss')}
        </button>
        <button
          type="button"
          className="btn"
          onClick={() => {
            const keys = visible.map((p) => p.key);
            dismiss();
            requestCleanUp(keys);
          }}
        >
          {t('action.cleanUp')}
        </button>
      </div>
    </div>
  );
}

export function RecoveredDialog() {
  const recovery = useAppStore((s) => s.world.boot.recovery);
  const list = useAppStore(useShallow((s) => recoveredLeftovers(s.world)));
  const [, setVersion] = useState(0);
  if (recovery && !closedWindows.has(recovery.undoToken) && recovery.stopping > 0) {
    return <UndoWindow key={recovery.undoToken} recovery={recovery} onClosed={() => setVersion((v) => v + 1)} />;
  }
  return <LeftoversNotice list={list} />;
}
