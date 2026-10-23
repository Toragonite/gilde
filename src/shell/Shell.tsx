/**
 * The shell: one window, two zones [M1] — the sidebar ("where things are")
 * and the main zone ("what you work on") — plus two layers: the palette
 * (⌘K) and the sheets (ux-ia §2.4). No tab strip, no icon rail, no
 * inspector, no status bar.
 *
 * It also applies the window-wide settings the UI slice holds: the theme
 * (`data-theme` on the root; `system` follows macOS), the language
 * (`lang`), and the UI zoom (80–200 %; breakpoints are evaluated after zoom,
 * ux-ia §2.8). Every view change goes through the store, so T13's reporter
 * sends `app_view_state` for each one [F-04].
 */

import { useCallback, useEffect, useRef, useState, useSyncExternalStore } from 'react';

import { LiveRegion } from '../components/LiveRegion';
import { useI18n } from '../i18n/useI18n';
import { announce } from '../lib/notify';
import { Palette } from '../palette/Palette';
import { Sidebar } from '../sidebar/Sidebar';
import { useAppStore } from '../store/useAppStore';
import { shellLayout } from './layout';
import { MainView } from './MainView';
import { screenTitle } from './screenTitle';
import { SheetHost } from './SheetHost';
import { slots } from './slots';
import { Toasts } from './Toasts';
import { TopBar } from './TopBar';
import { useShellKeys } from './useShellKeys';
import './shell.css';

function subscribeResize(listener: () => void): () => void {
  window.addEventListener('resize', listener);
  return () => window.removeEventListener('resize', listener);
}

const windowWidth = () => window.innerWidth;

/** Theme, language and zoom on the document root; a zoom change is announced (ux-ia §10 Text scaling). */
function useDocumentSettings(): void {
  const { t } = useI18n();
  const theme = useAppStore((s) => s.ui.theme);
  const locale = useAppStore((s) => s.ui.locale);
  const zoom = useAppStore((s) => s.ui.zoom);

  useEffect(() => {
    const root = document.documentElement;
    if (theme === 'light' || theme === 'dark') root.setAttribute('data-theme', theme);
    else root.removeAttribute('data-theme');
  }, [theme]);

  useEffect(() => {
    document.documentElement.setAttribute('lang', locale);
  }, [locale]);

  const firstZoom = useRef(true);
  useEffect(() => {
    const style = document.documentElement.style as CSSStyleDeclaration & { zoom?: string };
    style.zoom = zoom === 1 ? '' : String(zoom);
    if (firstZoom.current) firstZoom.current = false;
    else announce(t('shell.zoomed', { pct: Math.round(zoom * 100) }));
    // `t` changes with the language only; the announcement follows the zoom.
  }, [zoom]);
}

export function Shell() {
  const { t, locale } = useI18n();
  useDocumentSettings();

  const width = useSyncExternalStore(subscribeResize, windowWidth, windowWidth);
  const zoom = useAppStore((s) => s.ui.zoom);
  const view = useAppStore((s) => s.ui.view);
  const title = useAppStore((s) => screenTitle(t, s.ui.view, s.world, locale, s.ui.readTitle));
  const sidebarVisible = useAppStore((s) => s.ui.sidebarVisible);
  const toggleSidebarPref = useAppStore((s) => s.toggleSidebar);
  const paletteOpen = useAppStore((s) => s.ui.paletteOpen);
  const [overlayOpen, setOverlayOpen] = useState(false);

  const layout = shellLayout(width, zoom, view.screen, sidebarVisible, overlayOpen);

  const toggleSidebar = useCallback(() => {
    if (layout.sidebarMode === 'pinned') toggleSidebarPref();
    else setOverlayOpen((open) => !open);
  }, [layout.sidebarMode, toggleSidebarPref]);
  const closeOverlay = useCallback(() => setOverlayOpen(false), []);

  // The overlay closes when the user goes somewhere (a row click navigates).
  useEffect(() => {
    setOverlayOpen(false);
  }, [view]);

  useShellKeys({ toggleSidebar, overlayOpen: overlayOpen && layout.sidebarMode === 'overlay', closeOverlay });

  const Dialogs = slots().Dialogs;

  return (
    <div
      className="shell"
      data-band={layout.band}
      data-sidebar={layout.sidebarPinned ? 'pinned' : layout.sidebarShown ? 'overlay' : 'hidden'}
      style={{ ['--sidebar-current' as string]: `${layout.sidebarWidth}px` }}
    >
      {layout.sidebarPinned ? <Sidebar layout={layout} /> : null}
      <div className="shell-main">
        <TopBar layout={layout} title={title} onToggleSidebar={toggleSidebar} />
        <MainView label={title} sidebarShown={layout.sidebarShown} />
      </div>
      {layout.sidebarMode === 'overlay' && layout.sidebarShown ? (
        <div className="shell-overlay">
          <div className="shell-scrim" aria-hidden="true" onClick={closeOverlay} />
          <Sidebar layout={layout} overlay onRequestClose={closeOverlay} />
        </div>
      ) : null}
      {paletteOpen ? <Palette /> : null}
      <SheetHost />
      <Toasts />
      <LiveRegion />
      {Dialogs ? <Dialogs /> : null}
    </div>
  );
}
