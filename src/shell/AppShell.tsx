/**
 * AppShell — the CSS grid from DESIGN §9-1.
 *
 *   rail(48) │ sidebar(260–420, resizable) │ main(tabs + 1..4 panes) │ inspector(300–480)
 *   ─────────┴─────────────────────────────┴─────────────────────────┴──────────────────
 *   drawer (collapsible)
 *   status bar (24)
 *
 * Widths, the drawer and the split are persisted through `app_settings_set`
 * (stored in `settings_kv`), so the window comes back the way it was left.
 *
 * The sidebar and the overlays are mount points: T21 registers the sidebar,
 * T26/T27 register their sheets and the palette. With nothing registered the
 * column is simply empty — never a placeholder.
 *
 * Preview build (`src/features.ts`): no drawer, and the inspector follows the
 * view (F5) — on Team it opens for a selected member, on Worker accounts it
 * stays closed, and the sidebar column is not shown on Worker accounts (F4).
 */

import { useCallback, useEffect, useRef } from 'react';
import type { ComponentType, CSSProperties, PointerEvent as ReactPointerEvent } from 'react';

import { experimentalLanes } from '../features';
import { useI18n } from '../i18n/useI18n';
import { useShellHotkeys } from '../hooks/useHotkeys';
import { KillConfirmSheet } from '../monitor/KillConfirmSheet';
import { TeamMemberInspector } from '../team/TeamMemberInspector';
import { useAppStore, useShallow } from '../store/useAppStore';
import type { ViewKey } from '../store/uiSlice';
import {
  INSPECTOR_MAX,
  INSPECTOR_MIN,
  SIDEBAR_MAX,
  SIDEBAR_MIN,
} from '../store/uiSlice';
import { Rail, VIEW_LABEL } from './Rail';
import { TitleBar } from './TitleBar';
import { Drawer } from './Drawer';
import { Inspector } from './Inspector';
import { StatusBar } from './StatusBar';
import { PaneGrid } from '../panes/PaneGrid';

// ---------------------------------------------------------------------------
// Mount points for later waves
// ---------------------------------------------------------------------------

let sidebarComponent: ComponentType | null = null;

/** T21 registers the sidebar (it switches on `ui.activeView` itself). */
export function registerSidebar(component: ComponentType): () => void {
  sidebarComponent = component;
  return () => {
    if (sidebarComponent === component) sidebarComponent = null;
  };
}

const overlays = new Map<string, ComponentType>();

/** Sheets, dialogs and the command palette mount here (T26, T27). */
export function registerOverlay(id: string, component: ComponentType): () => void {
  overlays.set(id, component);
  return () => {
    if (overlays.get(id) === component) overlays.delete(id);
  };
}

// ---------------------------------------------------------------------------
// Theme
// ---------------------------------------------------------------------------

function useThemeAttribute(): void {
  const theme = useAppStore((s) => s.ui.theme);
  useEffect(() => {
    const root = document.documentElement;
    if (theme === 'system') root.removeAttribute('data-theme');
    else root.setAttribute('data-theme', theme);
  }, [theme]);
}

function useLangAttribute(): void {
  const locale = useAppStore((s) => s.ui.locale);
  useEffect(() => {
    document.documentElement.lang = locale;
  }, [locale]);
}

// ---------------------------------------------------------------------------
// Drag-to-resize
// ---------------------------------------------------------------------------

interface ResizerProps {
  side: 'left' | 'right';
  label: string;
  onResize: (delta: number) => void;
}

function Resizer({ side, label, onResize }: ResizerProps) {
  const dragging = useRef(false);
  const origin = useRef(0);

  const onPointerDown = useCallback((event: ReactPointerEvent<HTMLDivElement>) => {
    dragging.current = true;
    origin.current = event.clientX;
    event.currentTarget.setPointerCapture(event.pointerId);
    event.currentTarget.dataset.dragging = 'true';
  }, []);

  const onPointerMove = useCallback(
    (event: ReactPointerEvent<HTMLDivElement>) => {
      if (!dragging.current) return;
      const delta = event.clientX - origin.current;
      if (delta === 0) return;
      origin.current = event.clientX;
      onResize(delta);
    },
    [onResize],
  );

  const stop = useCallback((event: ReactPointerEvent<HTMLDivElement>) => {
    dragging.current = false;
    delete event.currentTarget.dataset.dragging;
  }, []);

  return (
    <div
      className={`resizer ${side}`}
      role="separator"
      aria-orientation="vertical"
      aria-label={label}
      onPointerDown={onPointerDown}
      onPointerMove={onPointerMove}
      onPointerUp={stop}
      onPointerCancel={stop}
    />
  );
}

/**
 * Reconcile the restored layout against what the backend still has (M1.5-6).
 *
 * `hydrateFromConfig` brings the tabs back before anything is known about the
 * sessions behind them, so exactly once — when hydration lands — the shell asks
 * for the session list. `loadSessions` is what marks a restored terminal tab
 * exited when the session it names did not survive the crash, and what puts the
 * host-crash rows into the tree. Without this the tabs would come back looking
 * alive until something else happened to ask.
 */
function useSessionsAfterHydrate(): void {
  const hydrated = useAppStore((s) => s.ui.hydrated);
  const loadSessions = useAppStore((s) => s.loadSessions);
  const asked = useRef(false);
  useEffect(() => {
    // Terminal sessions are an experimental lane: the preview never lists them.
    if (!experimentalLanes()) return;
    if (!hydrated || asked.current) return;
    asked.current = true;
    void loadSessions();
  }, [hydrated, loadSessions]);
}

// ---------------------------------------------------------------------------
// Layout rules per view (F4, F5)
// ---------------------------------------------------------------------------

/** Worker accounts has no sidebar: the pane is the account list. */
export function sidebarShownFor(view: ViewKey, visible: boolean): boolean {
  return view === 'workers' ? false : visible;
}

/**
 * Team shows the inspector exactly while a member is selected (its details);
 * Worker accounts never does; the experimental lanes keep the user's toggle.
 */
export function inspectorShownFor(view: ViewKey, open: boolean, memberSelected: boolean): boolean {
  if (view === 'team') return memberSelected;
  if (view === 'workers') return false;
  return open;
}

// ---------------------------------------------------------------------------
// Shell
// ---------------------------------------------------------------------------

export function AppShell() {
  const { t } = useI18n();
  useThemeAttribute();
  useLangAttribute();
  useShellHotkeys();
  useSessionsAfterHydrate();

  const ui = useAppStore(
    useShallow((s) => ({
      sidebarVisible: s.ui.sidebarVisible,
      sidebarWidth: s.ui.sidebarWidth,
      inspectorOpen: s.ui.inspectorOpen,
      inspectorWidth: s.ui.inspectorWidth,
      drawerOpen: s.ui.drawerOpen,
      drawerHeight: s.ui.drawerHeight,
    })),
  );
  const setSidebarWidth = useAppStore((s) => s.setSidebarWidth);
  const setInspectorWidth = useAppStore((s) => s.setInspectorWidth);
  // The sidebar's landmark name says which view it belongs to, so it is never
  // the rail's name again — and an assistive client (or the GUI smoke driver)
  // can address the two groups apart (PINNED §23-3).
  const activeView = useAppStore((s) => s.ui.activeView);
  const memberSelected = useAppStore((s) => s.team.selectedMember !== null);
  const sidebarLabel = t('sidebar.label', { view: t(VIEW_LABEL[activeView]) });

  const Sidebar = sidebarComponent;
  const lanes = experimentalLanes();
  const sidebarShown = sidebarShownFor(activeView, ui.sidebarVisible);
  const inspectorShown = inspectorShownFor(activeView, ui.inspectorOpen, memberSelected);
  const drawerShown = lanes && ui.drawerOpen;

  const style = {
    '--gilde-sidebar-w': `${ui.sidebarWidth}px`,
    '--gilde-inspector-w': inspectorShown ? `${ui.inspectorWidth}px` : '0px',
    '--gilde-drawer-h': drawerShown ? `${ui.drawerHeight}px` : '0px',
  } as CSSProperties;

  const resizer = (
    <Resizer
      side="left"
      label={t('inspector.width')}
      onResize={(delta) =>
        setInspectorWidth(
          Math.min(INSPECTOR_MAX, Math.max(INSPECTOR_MIN, ui.inspectorWidth - delta)),
        )
      }
    />
  );

  return (
    <div
      className="shell"
      style={style}
      data-view={activeView}
      data-sidebar={sidebarShown ? 'shown' : 'hidden'}
      data-inspector={inspectorShown ? 'open' : 'closed'}
      data-drawer={drawerShown ? 'open' : 'closed'}
    >
      <TitleBar />
      <Rail />

      <aside className="sidebar" aria-label={sidebarLabel}>
        {Sidebar && sidebarShown ? <Sidebar /> : null}
        <Resizer
          side="right"
          label={t('sidebar.width')}
          onResize={(delta) =>
            setSidebarWidth(
              Math.min(SIDEBAR_MAX, Math.max(SIDEBAR_MIN, ui.sidebarWidth + delta)),
            )
          }
        />
      </aside>

      <PaneGrid />

      {inspectorShown ? (
        activeView === 'team' ? (
          <TeamMemberInspector>{resizer}</TeamMemberInspector>
        ) : (
          <Inspector>{resizer}</Inspector>
        )
      ) : null}

      {drawerShown ? <Drawer /> : null}

      <StatusBar />

      {/* Mounted once, in both builds: the monitor's kill actions and the
          health alerts' Clean up both confirm here. */}
      <KillConfirmSheet />

      {[...overlays.entries()].map(([id, Overlay]) => (
        <Overlay key={id} />
      ))}
    </div>
  );
}
