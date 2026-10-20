/**
 * Pane — a content-agnostic tile (DESIGN §9-1).
 *
 * A pane holds a tab id; the tab carries a descriptor that names a view. The
 * pane looks the view up in a registry and renders it. That is the whole reason
 * a terminal, the dispatch board, the graph and the process monitor can share
 * one grid and one keyboard model.
 *
 * Views register themselves:
 *
 * ```ts
 * registerPaneView('terminal', TerminalPane);   // terminal/TerminalPane.tsx (T20)
 * ```
 */

import type { ComponentType } from 'react';

import { useI18n } from '../i18n/useI18n';
import { EmptyState } from '../components/EmptyState';
import { useAppStore } from '../store/useAppStore';
import type { PaneDescriptor, Tab } from '../store/uiSlice';

export type PaneViewKind = PaneDescriptor['view'];

export interface PaneViewProps {
  descriptor: PaneDescriptor;
  tab: Tab;
  paneId: string;
  active: boolean;
}

const views = new Map<PaneViewKind, ComponentType<PaneViewProps>>();

export function registerPaneView(
  kind: PaneViewKind,
  component: ComponentType<PaneViewProps>,
): () => void {
  views.set(kind, component);
  return () => {
    if (views.get(kind) === component) views.delete(kind);
  };
}

export function paneViewRegistered(kind: PaneViewKind): boolean {
  return views.has(kind);
}

export interface PaneProps {
  paneId: string;
  tabId: string | null;
}

export function Pane({ paneId, tabId }: PaneProps) {
  const { t } = useI18n();
  const tab = useAppStore((s) => s.ui.tabs.find((candidate) => candidate.id === tabId));
  const active = useAppStore((s) => s.ui.activePaneId === paneId);
  const setActivePane = useAppStore((s) => s.setActivePane);

  const View = tab ? views.get(tab.descriptor.view) : undefined;

  return (
    <section
      className="pane"
      data-active={active ? 'true' : undefined}
      data-pane-id={paneId}
      onMouseDown={() => {
        if (!active) setActivePane(paneId);
      }}
    >
      <div className="pane-body">
        {/* A tab whose view is not registered in this build (a hidden lane)
            renders as an empty pane — never as a "not available" notice. */}
        {tab && View ? (
          <View descriptor={tab.descriptor} tab={tab} paneId={paneId} active={active} />
        ) : (
          <EmptyState message={t('pane.empty')} />
        )}
      </div>
    </section>
  );
}
