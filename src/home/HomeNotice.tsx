/**
 * Home's one notice [M15 "at most one notice"], in priority order:
 *
 *  1. the last quit or crash stopped sessions Gilde ran (`LaunchInfo`
 *     afterQuit/afterCrash): "{n} sessions stopped when Gilde quit ·
 *     [Continue all] [Dismiss]" (ux-ia §2.7, M10);
 *  2. the first notice the core reports (`world.notices`, keyed lines);
 *  3. on the first launch: "Looking for sessions on this Mac…" until the first
 *     discovery pass is done, then "Found {n} sessions…" with [Turn on]
 *     notifications and [Dismiss].
 *
 * A notice the user dismissed stays dismissed for this window.
 */

import { useState } from 'react';

import { StateGlyph } from '../components/StateGlyph';
import { useI18n } from '../i18n/useI18n';
import { commands } from '../lib/commands';
import type { IpcError } from '../lib/ipcError';
import { appStore, useAppStore } from '../store/useAppStore';
import { runningCount } from '../shell/words';

function fail(error: unknown): void {
  appStore.getState().pushErrorToast(error as IpcError);
}

export function HomeNotice() {
  const { t, td } = useI18n();
  const launch = useAppStore((s) => s.ui.launch);
  const notice = useAppStore((s) => s.world.notices[0] ?? null);
  const firstDone = useAppStore((s) => s.world.boot.firstDiscoveryDone);
  const found = useAppStore((s) => runningCount(s.world));
  const [dismissed, setDismissed] = useState<ReadonlySet<string>>(new Set());
  const dismiss = (id: string) => setDismissed((d) => new Set(d).add(id));

  if (launch && (launch.kind === 'afterQuit' || launch.kind === 'afterCrash') && launch.count > 0 && !dismissed.has('quit')) {
    const recordId = launch.recordId;
    return (
      <div className="fh-notice" role="note">
        <StateGlyph state="stopped" />
        <span className="fh-notice-text">
          {t(launch.kind === 'afterQuit' ? 'home.stoppedOnQuit' : 'home.stoppedOnCrash', { n: launch.count })}
        </span>
        <button
          type="button"
          className="btn primary"
          onClick={() => {
            dismiss('quit');
            void commands.continue_all({ recordId, uids: null }).catch(fail);
          }}
        >
          {t('action.continueAll')}
        </button>
        <button
          type="button"
          className="btn ghost"
          onClick={() => {
            dismiss('quit');
            void commands.quit_record_dismiss({ recordId }).catch(fail);
          }}
        >
          {t('common.dismiss')}
        </button>
      </div>
    );
  }

  if (notice) {
    return (
      <div className="fh-notice" role="note">
        <span className="fh-notice-text">{td(notice.key, notice.params)}</span>
      </div>
    );
  }

  if (launch?.kind === 'firstLaunch' && !dismissed.has('first')) {
    if (!firstDone) {
      return (
        <div className="fh-notice" role="note">
          <span className="fh-notice-text">{t('firstRun.looking')}</span>
        </div>
      );
    }
    return (
      <div className="fh-notice" role="note">
        <span className="fh-notice-text">
          {t('firstRun.found', { n: found })} {t('firstRun.notify')}
        </span>
        <button
          type="button"
          className="btn"
          onClick={() => {
            dismiss('first');
            void commands.notify_permission_request({}).catch(fail);
          }}
        >
          {t('firstRun.turnOn')}
        </button>
        <button type="button" className="btn ghost" onClick={() => dismiss('first')}>
          {t('common.dismiss')}
        </button>
      </div>
    );
  }

  return null;
}
