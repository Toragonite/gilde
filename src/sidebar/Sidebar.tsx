/**
 * The sidebar — "where things are" (ux-ia §2.4): three nav rows (Home ⌘0,
 * History ⇧⌘F, Accounts & health ⇧⌘A), the Needs-you queue (not on Home),
 * the project tree, and the footer. It is the page's one `nav` landmark,
 * "Sessions" (ux-ia §10). Pinned at 300/248 px, or an overlay drawer below
 * 900 px and in Split/Team/Floor below 1200 px (`shell/layout.ts`).
 */

import { useEffect, useRef } from 'react';

import { GaugeIcon, HistoryIcon, HomeIcon } from '../components/icons';
import { useHotkeys } from '../hooks/useHotkeys';
import { useI18n } from '../i18n/useI18n';
import type { MessageKey } from '../i18n';
import type { SessionUid } from '../lib/gen/SessionUid';
import type { MainView } from '../store/uiSlice';
import { appStore, useAppStore } from '../store/useAppStore';
import { sessionOf } from '../store/worldSlice';
import type { ShellLayout } from '../shell/layout';
import { NeedsYouList } from './NeedsYouList';
import { ProjectTree } from './ProjectTree';
import { SidebarFooter } from './SidebarFooter';
import { SPLIT_UID_ATTR } from './splitAdd';
import './sidebar.css';

interface NavRow {
  screen: 'home' | 'history' | 'accounts';
  label: MessageKey;
  keys: string;
  aria: string;
  icon: typeof HomeIcon;
  view: MainView;
}

const NAV: readonly NavRow[] = [
  { screen: 'home', label: 'nav.home', keys: '⌘0', aria: 'Meta+0', icon: HomeIcon, view: { screen: 'home' } },
  { screen: 'history', label: 'nav.history', keys: '⇧⌘F', aria: 'Meta+Shift+F', icon: HistoryIcon, view: { screen: 'history' } },
  {
    screen: 'accounts',
    label: 'nav.accountsHealth',
    keys: '⇧⌘A',
    aria: 'Meta+Shift+A',
    icon: GaugeIcon,
    view: { screen: 'accounts', section: 'accounts' },
  },
];

export interface SidebarProps {
  layout: ShellLayout;
  overlay?: boolean;
  onRequestClose?: () => void;
}

/**
 * Whether `⌘⏎` / `⌘⌫` pressed on `target` inside the sidebar may go on to the
 * card in front (the Session screen's, or the focused Split tile's): only
 * when the focused row is a session row (Needs you, the project tree) whose
 * top request is the very request that card shows. That card then decides
 * the key through `shortcutAction` behind its own guard, keyed on the request
 * (A44.1, A44.2).
 */
export function sidebarKeyReachesFrontCard(target: EventTarget | null): boolean {
  const row = target instanceof Element ? target.closest(`[${SPLIT_UID_ATTR}]`) : null;
  const raw = row?.getAttribute(SPLIT_UID_ATTR);
  if (!raw) return false;
  const s = appStore.getState();
  const view = s.ui.view;
  const frontUid = view.screen === 'session' ? view.uid : view.screen === 'split' ? s.ui.split.focused : null;
  if (!frontUid) return false;
  const rowTop = sessionOf(s.world, raw as SessionUid)?.pending[0]?.requestId ?? null;
  const frontTop = sessionOf(s.world, frontUid)?.pending[0]?.requestId ?? null;
  return rowTop !== null && rowTop === frontTop;
}

export function Sidebar({ layout, overlay = false }: SidebarProps) {
  const { t } = useI18n();
  const screen = useAppStore((s) => s.ui.view.screen);
  const navigate = useAppStore((s) => s.navigate);
  const first = useRef<HTMLButtonElement>(null);
  const root = useRef<HTMLElement>(null);

  // With focus in the sidebar, ⌘⏎ / ⌘⌫ never answer a request other than the
  // one the focused row names: they are inert here unless that row's request
  // is the card in front (then that card takes the key). Without this they
  // would fall through to whatever card the main view holds — another
  // session's, or Home's focus card (A44.2).
  useHotkeys(
    [
      { id: 'answer.primary', combo: 'mod+enter', when: (event) => !sidebarKeyReachesFrontCard(event.target), run: () => {} },
      { id: 'answer.deny', combo: 'mod+backspace', when: (event) => !sidebarKeyReachesFrontCard(event.target), run: () => {} },
    ],
    { scope: 'L', element: root, requireFocus: true },
  );

  // An opened overlay takes focus (the user asked for it with ⌘B or the menu button).
  useEffect(() => {
    if (overlay) first.current?.focus();
  }, [overlay]);

  return (
    <nav
      ref={root}
      className="sidebar"
      aria-label={t('shell.sidebar')}
      data-region="sidebar"
      data-overlay={overlay ? 'true' : undefined}
      style={{ width: layout.sidebarWidth }}
    >
      <div className="sb-head">
        <span className="sb-brand">Gilde</span>
      </div>
      <div className="sb-scroll">
        <div className="sb-nav">
          {NAV.map((row, i) => {
            const Icon = row.icon;
            return (
              <button
                key={row.screen}
                ref={i === 0 ? first : undefined}
                type="button"
                className="sb-nav-row"
                aria-current={screen === row.screen ? 'page' : undefined}
                aria-keyshortcuts={row.aria}
                onClick={() => navigate(row.view)}
              >
                <Icon size="sm" />
                <span className="ell grow">{t(row.label)}</span>
                {layout.navHints ? (
                  <span className="kbd" aria-hidden="true">
                    {row.keys}
                  </span>
                ) : null}
              </button>
            );
          })}
        </div>
        {screen === 'home' ? null : <NeedsYouList />}
        <ProjectTree />
      </div>
      <SidebarFooter />
    </nav>
  );
}
