/**
 * Tab bar — 32px, one row of tabs plus (experimental lanes only) the split
 * control and "New session" (DESIGN §9-1).
 *
 * Tabs are 12px text with a status dot; the active tab is the only one drawn on
 * the terminal background, with no underline (UI-GUIDELINES §2-1b). A view's
 * own tab (Team, Worker accounts, …) is titled from its message key at render
 * time, so it follows a language switch.
 */

import { experimentalLanes } from '../features';
import { useI18n } from '../i18n/useI18n';
import { StatusDot } from '../components/StatusDot';
import { CloseIcon, PlusIcon } from '../components/icons';
import { runIntent } from '../hooks/useHotkeys';
import { useAppStore, useShallow } from '../store/useAppStore';
import { viewTabTitleKey } from '../store/uiSlice';
import type { SplitCount } from '../store/uiSlice';

const SPLITS: readonly SplitCount[] = [1, 2, 3, 4];

function LaneActions() {
  const { t } = useI18n();
  const split = useAppStore((s) => s.ui.panes.split);
  const setSplit = useAppStore((s) => s.setSplit);
  return (
    <div className="row gap-8 tabbar-actions">
      <div className="seg" role="group" aria-label={t('tab.split')}>
        {SPLITS.map((value) => (
          <button
            key={value}
            type="button"
            aria-pressed={split === value}
            onClick={() => setSplit(value)}
            title={t('tab.split')}
          >
            {value}
          </button>
        ))}
      </div>
      <button type="button" className="btn" onClick={() => runIntent('newSession')}>
        <PlusIcon size="sm" />
        {t('sidebar.newSession')}
      </button>
    </div>
  );
}

export function TabBar() {
  const { t } = useI18n();
  const tabs = useAppStore((s) => s.ui.tabs);
  const activeTabId = useAppStore(
    useShallow((s) => s.ui.panes.slots.find((slot) => slot.id === s.ui.activePaneId)?.tabId ?? null),
  );
  const focusTab = useAppStore((s) => s.focusTab);
  const closeTab = useAppStore((s) => s.closeTab);
  const setActiveView = useAppStore((s) => s.setActiveView);

  return (
    <div className="tabbar" role="tablist">
      {tabs.map((tab) => {
        const titleKey = viewTabTitleKey(tab.descriptor);
        const title = titleKey ? t(titleKey) : tab.title;
        return (
          <div key={tab.id} className="tab" role="presentation" aria-selected={tab.id === activeTabId}>
            <button
              type="button"
              className="tab-label"
              role="tab"
              aria-selected={tab.id === activeTabId}
              onClick={() => {
                // A view's own tab moves the rail with it, so the sidebar and
                // the inspector follow what the main area shows.
                const view = tab.descriptor.view;
                if (view === 'team' || view === 'workers') setActiveView(view);
                focusTab(tab.id);
              }}
              title={tab.sublabel ? `${title} · ${tab.sublabel}` : title}
            >
              {tab.status ? <StatusDot status={tab.status} /> : null}
              <span className="ell">{title}</span>
              {tab.sublabel ? <span className="muted ell">{tab.sublabel}</span> : null}
            </button>
            {tab.closable ? (
              <button
                type="button"
                className="tab-close"
                aria-label={t('tab.close')}
                onClick={() => closeTab(tab.id)}
              >
                <CloseIcon size="sm" />
              </button>
            ) : null}
          </div>
        );
      })}

      <div className="grow" />

      {experimentalLanes() ? <LaneActions /> : null}
    </div>
  );
}
