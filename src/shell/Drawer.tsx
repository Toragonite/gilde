/**
 * Bottom drawer — the Process Monitor (DESIGN §9-1). Experimental lanes only:
 * `AppShell` mounts it only when `src/features.ts` turns the lanes on.
 *
 * The monitor panel is a mount point: `monitor/ProcessMonitorTable.tsx` (T23)
 * registers itself with `registerDrawerPanel('monitor', …)` and this file needs
 * no edit. Until then the drawer shows its empty state — never a fake table.
 */

import type { ComponentType } from 'react';

import { useI18n } from '../i18n/useI18n';
import { EmptyState } from '../components/EmptyState';
import { CloseIcon } from '../components/icons';
import { useAppStore } from '../store/useAppStore';
import type { DrawerTab } from '../store/uiSlice';

// -- panel registry ----------------------------------------------------------

const panels = new Map<DrawerTab, ComponentType>();

/** Later waves plug their panel in here instead of editing this file. */
export function registerDrawerPanel(tab: DrawerTab, component: ComponentType): () => void {
  panels.set(tab, component);
  return () => {
    if (panels.get(tab) === component) panels.delete(tab);
  };
}

export function Drawer() {
  const { t } = useI18n();
  const drawerTab = useAppStore((s) => s.ui.drawerTab);
  const toggleDrawer = useAppStore((s) => s.toggleDrawer);

  const Registered = panels.get(drawerTab);

  return (
    <section className="drawer" aria-label={t('drawer.monitor')}>
      <div className="tabbar">
        <span className="tab" aria-selected="true">
          {t('drawer.monitor')}
        </span>
        <div className="grow" />
        <button
          type="button"
          className="btn icon ghost"
          onClick={toggleDrawer}
          title={t('drawer.toggle')}
          aria-label={t('drawer.toggle')}
        >
          <CloseIcon size="sm" />
        </button>
      </div>
      <div className="pane-body">
        {Registered ? <Registered /> : <EmptyState message={t('drawer.monitor.empty')} />}
      </div>
    </section>
  );
}
