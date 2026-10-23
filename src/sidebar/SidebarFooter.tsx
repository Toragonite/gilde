/**
 * The sidebar footer (ux-ia §2.4, §2.5): "16 running · 6.1 GB" (opens
 * Accounts & health at Health) and the notification state — "Alerts on",
 * "Paused until 09:00", or "Paused until you resume" for the open-ended pause
 * (`paused_until ≥ PAUSED_UNTIL_RESUME`, contract A16.2). The bell opens a
 * small menu: pause for 1 hour, until 9:00 tomorrow, until I resume, or resume.
 */

import { useId, useRef, useState } from 'react';

import { Popover, popoverTriggerProps } from '../components/Popover';
import { BellIcon, BellOffIcon } from '../components/icons';
import { useI18n } from '../i18n/useI18n';
import { commands } from '../lib/commands';
import { formatBytes } from '../lib/format';
import type { Locale } from '../lib/gen/Locale';
import type { PauseUntil } from '../lib/gen/PauseUntil';
import type { IpcError } from '../lib/ipcError';
import { appStore, useAppStore } from '../store/useAppStore';
import type { T } from '../shell/words';
import { PAUSED_UNTIL_RESUME, clockTime, liveMemory, runningCount } from '../shell/words';

/** The footer's notification state words for a `paused_until` value. */
export function pausedText(t: T, pausedUntil: number | null, locale: Locale): string {
  if (pausedUntil === null) return t('notify.on');
  if (pausedUntil >= PAUSED_UNTIL_RESUME) return t('notify.pausedUntilResume');
  return t('notify.paused', { time: clockTime(pausedUntil, locale) });
}

function fail(error: unknown): void {
  appStore.getState().pushErrorToast(error as IpcError);
}

export function SidebarFooter() {
  const { t, locale } = useI18n();
  const running = useAppStore((s) => runningCount(s.world));
  const memory = useAppStore((s) => liveMemory(s.world));
  const pausedUntil = useAppStore((s) => s.world.queue.pausedUntil);
  const navigate = useAppStore((s) => s.navigate);
  const [menu, setMenu] = useState(false);
  const bell = useRef<HTMLButtonElement>(null);
  const menuId = useId();
  const paused = pausedUntil !== null;

  const pause = (until: PauseUntil) => {
    setMenu(false);
    void commands.notify_pause({ until }).catch(fail);
  };
  const resume = () => {
    setMenu(false);
    void commands.notify_resume({}).catch(fail);
  };

  const size = memory === null ? null : formatBytes(memory, locale);
  return (
    <div className="sb-foot">
      <button
        type="button"
        className="btn ghost sb-foot-run"
        onClick={() => navigate({ screen: 'accounts', section: 'health' })}
      >
        <span className="ell">
          {t('shell.running', { n: running })}
          {size ? ` · ${size}` : ''}
        </span>
      </button>
      <span className="grow" />
      <button
        ref={bell}
        type="button"
        className="btn ghost sb-bell"
        data-paused={paused ? 'true' : undefined}
        {...popoverTriggerProps(menu, menuId, 'menu')}
        onClick={() => setMenu((open) => !open)}
      >
        {paused ? <BellOffIcon size="sm" /> : <BellIcon size="sm" />}
        <span className="ell">{pausedText(t, pausedUntil, locale)}</span>
      </button>
      <Popover open={menu} anchor={bell} id={menuId} role="menu" label={t('notify.menu')} onClose={() => setMenu(false)}>
        {paused ? (
          <button type="button" role="menuitem" onClick={resume}>
            {t('notify.resume')}
          </button>
        ) : null}
        <button type="button" role="menuitem" onClick={() => pause('oneHour')}>
          {t('notify.pause1h')}
        </button>
        <button type="button" role="menuitem" onClick={() => pause('tomorrowNine')}>
          {t('notify.pauseTomorrow')}
        </button>
        <button type="button" role="menuitem" onClick={() => pause('untilResume')}>
          {t('notify.pauseUntilResume')}
        </button>
      </Popover>
    </div>
  );
}
