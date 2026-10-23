/**
 * The main zone's top bar: the sidebar toggle (with the Needs-you count while
 * the sidebar is hidden, "menu n", ux-ia §2.8), the screen's name, and three
 * global controls — Needs you (⌘J; not on Home, which shows the queue at full
 * width), Search (⌘K) and New session (⌘N). It is not a landmark: the page
 * has one `nav` (the sidebar) and one `main`.
 */

import { StateGlyph } from '../components/StateGlyph';
import { PanelLeftIcon, PlusIcon, SearchIcon } from '../components/icons';
import { useI18n } from '../i18n/useI18n';
import { useAppStore } from '../store/useAppStore';
import type { ShellLayout } from './layout';
import { currentProject, stepQueue } from './useShellKeys';

export interface TopBarProps {
  layout: ShellLayout;
  title: string;
  onToggleSidebar: () => void;
}

export function TopBar({ layout, title, onToggleSidebar }: TopBarProps) {
  const { t } = useI18n();
  const needYou = useAppStore((s) => s.world.queue.needYou);
  const onHome = useAppStore((s) => s.ui.view.screen === 'home');
  const setPaletteOpen = useAppStore((s) => s.setPaletteOpen);
  const openSheet = useAppStore((s) => s.openSheet);
  const compact = layout.band === 'narrow';

  return (
    <header className="topbar" aria-label={t('shell.topBar')} data-compact={compact ? 'true' : undefined}>
      <button
        type="button"
        className="btn ghost topbar-toggle"
        aria-label={t('shell.toggleSidebar')}
        aria-expanded={layout.sidebarShown}
        aria-keyshortcuts="Meta+B"
        onClick={onToggleSidebar}
      >
        <PanelLeftIcon />
        {!layout.sidebarShown && needYou > 0 ? <span className="topbar-count mono">{needYou}</span> : null}
      </button>
      <span className="topbar-title ell">{title}</span>
      <span className="grow" />
      {onHome ? null : (
        <button
          type="button"
          className="btn topbar-needs"
          data-zero={needYou === 0 ? 'true' : undefined}
          aria-label={compact ? t('shell.needsYouButton', { n: needYou }) : undefined}
          aria-keyshortcuts="Meta+J"
          onClick={() => stepQueue(1)}
        >
          <StateGlyph state={needYou > 0 ? 'needsApproval' : 'ready'} />
          {compact ? null : <span>{t('nav.needsYou')}</span>}
          <span className="mono topbar-needs-n">{needYou}</span>
          {compact ? null : <span className="g-sr-only">, {t('shell.openNext')}</span>}
          {compact ? null : (
            <span className="kbd" aria-hidden="true">
              ⌘J
            </span>
          )}
        </button>
      )}
      <button
        type="button"
        className="btn"
        aria-label={compact ? t('shell.search') : undefined}
        aria-keyshortcuts="Meta+K"
        onClick={() => setPaletteOpen(true)}
      >
        <SearchIcon size="sm" />
        {compact ? null : <span>{t('shell.search')}</span>}
        {compact ? null : (
          <span className="kbd" aria-hidden="true">
            ⌘K
          </span>
        )}
      </button>
      <button
        type="button"
        className="btn primary"
        aria-label={compact ? t('action.newSession') : undefined}
        aria-keyshortcuts="Meta+N"
        onClick={() => openSheet('newSession', currentProject())}
      >
        <PlusIcon size="sm" />
        {compact ? null : <span>{t('action.newSession')}</span>}
        {compact ? null : (
          <span className="kbd" aria-hidden="true">
            ⌘N
          </span>
        )}
      </button>
    </header>
  );
}
