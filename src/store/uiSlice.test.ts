/**
 * UI slice tests — the layout contract (T05 acceptance 3) and the tab/pane
 * model that the keyboard layer drives (acceptance 4, 7).
 */

import { assert, assertEqual, test } from '../test/harness';
import { setExperimentalLanesForTests } from '../features';
import { resolveLocale } from '../i18n';
import type { AppConfig } from '../lib/ipc';
import { useAppStore } from './useAppStore';
import {
  cancelPersistLayout,
  INSPECTOR_MAX,
  INSPECTOR_MIN,
  SIDEBAR_MAX,
  SIDEBAR_MIN,
  initialUiState,
  pickPersistedLayout,
  viewTabTitleKey,
} from './uiSlice';

/** Run `body` with the experimental lanes forced on or off, then restore. */
function withLanes(on: boolean, body: () => void): void {
  setExperimentalLanesForTests(on);
  try {
    body();
  } finally {
    setExperimentalLanesForTests(null);
  }
}

function reset(): void {
  // Layout writes are debounced; drop anything a previous test queued so no
  // `app_settings_set` fires after the run finishes.
  cancelPersistLayout();
  useAppStore.setState({ ui: initialUiState() });
}

function configWith(layout: unknown): AppConfig {
  return { layout } as unknown as AppConfig;
}

test('sidebar and inspector widths are clamped to the design range', () => {
  reset();
  const store = useAppStore.getState();
  store.setSidebarWidth(10);
  assertEqual(useAppStore.getState().ui.sidebarWidth, SIDEBAR_MIN, 'below the minimum clamps up');
  store.setSidebarWidth(9999);
  assertEqual(useAppStore.getState().ui.sidebarWidth, SIDEBAR_MAX, 'above the maximum clamps down');
  store.setInspectorWidth(0);
  assertEqual(useAppStore.getState().ui.inspectorWidth, INSPECTOR_MIN, 'inspector minimum');
  store.setInspectorWidth(10_000);
  assertEqual(useAppStore.getState().ui.inspectorWidth, INSPECTOR_MAX, 'inspector maximum');
});

test('the layout round-trips through config.layout (experimental lanes on)', () => withLanes(true, () => {
  reset();
  const store = useAppStore.getState();
  store.setSidebarWidth(340);
  store.setInspectorWidth(400);
  store.toggleDrawer();
  store.setDrawerTab('monitor');
  store.setActiveView('graph');
  store.setSplit(4);
  store.toggleSidebar();

  const saved = pickPersistedLayout(useAppStore.getState().ui);
  assertEqual(saved.sidebarWidth, 340, 'width is a declared LayoutConfig field');
  assertEqual(saved.panes.split, 4, 'the split lives in the free-form panes slot');
  assertEqual(saved.panes.drawerTab, 'monitor', 'the drawer tab is remembered');
  assertEqual(saved.panes.sidebarVisible, false, 'sidebar visibility is remembered');

  // A fresh launch reads it back.
  reset();
  useAppStore.getState().hydrateFromConfig(configWith(saved));
  const ui = useAppStore.getState().ui;
  assertEqual(ui.sidebarWidth, 340, 'width restored');
  assertEqual(ui.inspectorWidth, 400, 'inspector width restored');
  assertEqual(ui.drawerOpen, true, 'drawer restored');
  assertEqual(ui.drawerTab, 'monitor', 'drawer tab restored');
  assertEqual(ui.activeView, 'graph', 'active view restored');
  assertEqual(ui.panes.split, 4, 'split restored');
  assertEqual(ui.panes.slots.length, 4, 'four pane slots exist');
  assertEqual(ui.sidebarVisible, false, 'sidebar visibility restored');
  assertEqual(ui.hydrated, true, 'the store is marked hydrated');
}));

test('the app opens on Team, with the inspector closed and no drawer', () => {
  const ui = initialUiState();
  assertEqual(ui.activeView, 'team', 'the default view is Team');
  assertEqual(ui.inspectorOpen, false, 'the inspector starts closed (F5)');
  assertEqual(ui.drawerOpen, false, 'no drawer');
  assertEqual(ui.localePref, 'system', 'the language follows the system until chosen');
  assertEqual(viewTabTitleKey({ view: 'team' }), 'view.team', 'a view tab is titled from its key');
  assertEqual(viewTabTitleKey({ view: 'terminal', sessionId: 's' }), null, 'a terminal tab keeps its own title');
});

test('the preview never enters or restores a hidden lane', () => withLanes(false, () => {
  reset();
  useAppStore.getState().setActiveView('graph');
  assertEqual(useAppStore.getState().ui.activeView, 'team', 'setActiveView refuses a hidden lane');
  useAppStore.getState().toggleDrawer();
  assertEqual(useAppStore.getState().ui.drawerOpen, false, 'the drawer never opens in the preview');

  // A layout saved by an experimental build: a graph view, a terminal tab, an
  // open drawer on the old Events tab.
  reset();
  useAppStore.getState().hydrateFromConfig(
    configWith({
      panes: {
        activeView: 'graph',
        drawerOpen: true,
        drawerTab: 'events',
        tabs: [
          { id: 'tab-3', title: 'claude', descriptor: { view: 'terminal', sessionId: 's1' }, closable: true },
          { id: 'tab-4', title: 'team', descriptor: { view: 'team' }, closable: true },
          { id: 'tab-5', title: 'settings', descriptor: { view: 'settings' }, closable: true },
        ],
        slots: ['tab-3'],
      },
    }),
  );
  const ui = useAppStore.getState().ui;
  assertEqual(ui.activeView, 'team', 'a hidden saved view falls back to Team');
  assertEqual(ui.drawerOpen, false, 'the drawer stays closed');
  assertEqual(ui.drawerTab, 'monitor', 'the removed Events tab is not restored');
  assertEqual(ui.tabs.map((tab) => tab.id).join(','), 'tab-4', 'only the Team tab survives');
  assertEqual(ui.panes.slots[0].tabId, null, 'a slot that pointed at a dropped tab is empty');
}));

test('the experimental flag restores the lanes', () => withLanes(true, () => {
  reset();
  useAppStore.getState().setActiveView('graph');
  assertEqual(useAppStore.getState().ui.activeView, 'graph', 'with the lanes on the graph is reachable');
  useAppStore.getState().hydrateFromConfig(
    configWith({
      panes: {
        activeView: 'sessions',
        tabs: [{ id: 'tab-3', title: 'claude', descriptor: { view: 'terminal', sessionId: 's1' }, closable: true }],
        slots: ['tab-3'],
      },
    }),
  );
  assertEqual(useAppStore.getState().ui.activeView, 'sessions', 'a saved lane view is restored');
  assertEqual(useAppStore.getState().ui.tabs.length, 1, 'and its terminal tab');
}));

test('locale "system" resolves from the OS language; an explicit choice wins (F1)', () => {
  assertEqual(resolveLocale('system', 'ko-KR'), 'ko', 'ko-KR → Korean');
  assertEqual(resolveLocale('system', 'ko'), 'ko', 'bare ko → Korean');
  assertEqual(resolveLocale('system', 'en-US'), 'en', 'en-US → English');
  assertEqual(resolveLocale('system', 'de-DE'), 'en', 'anything else → English');
  assertEqual(resolveLocale(undefined, 'ko-KR'), 'ko', 'a missing value follows the system');
  assertEqual(resolveLocale('fr', 'en-US'), 'en', 'an unknown value follows the system');
  assertEqual(resolveLocale('en', 'ko-KR'), 'en', 'an explicit en beats a Korean system');
  assertEqual(resolveLocale('ko', 'en-US'), 'ko', 'an explicit ko beats an English system');

  reset();
  // The test environment's navigator says ko-KR.
  useAppStore.getState().hydrateFromConfig({ locale: 'system' } as unknown as AppConfig);
  assertEqual(useAppStore.getState().ui.locale, 'ko', 'system → the OS language');
  assertEqual(useAppStore.getState().ui.localePref, 'system', 'and the preference stays system');
  useAppStore.getState().hydrateFromConfig({ locale: 'en' } as unknown as AppConfig);
  assertEqual(useAppStore.getState().ui.locale, 'en', 'an old config with an explicit en keeps it');
  useAppStore.getState().hydrateFromConfig({} as unknown as AppConfig);
  assertEqual(useAppStore.getState().ui.locale, 'ko', 'no locale at all → the OS language');
  reset();
});

test('hydrating from a config without a layout keeps the defaults', () => {
  reset();
  useAppStore.getState().hydrateFromConfig(configWith(undefined));
  const ui = useAppStore.getState().ui;
  assertEqual(ui.sidebarWidth, 300, 'default width survives');
  assertEqual(ui.panes.split, 1, 'default split survives');
});

test('tabs open into the active pane and survive a split change', () => {
  reset();
  const store = useAppStore.getState();
  const a = store.openTab({ title: 'a', descriptor: { view: 'graph' }, closable: true });
  const b = store.openTab({ title: 'b', descriptor: { view: 'monitor' }, closable: true });

  let ui = useAppStore.getState().ui;
  assertEqual(ui.tabs.length, 2, 'both tabs exist');
  assertEqual(ui.panes.slots[0].tabId, b, 'the newest tab took the active pane');

  useAppStore.getState().setSplit(2);
  ui = useAppStore.getState().ui;
  assertEqual(ui.panes.slots.length, 2, 'a second slot appeared');
  assertEqual(ui.panes.slots[0].tabId, b, 'the first slot kept its tab');
  assertEqual(ui.panes.slots[1].tabId, null, 'the new slot starts empty');

  useAppStore.getState().assignTab('pane-2', a);
  ui = useAppStore.getState().ui;
  assertEqual(ui.panes.slots[1].tabId, a, 'the tab moved into the second pane');
  assertEqual(ui.activePaneId, 'pane-2', 'assigning also focuses the pane');
});

test('focusTabIndex and cycleTab drive the cmd+1..9 and cmd+alt+arrow bindings', () => {
  reset();
  const store = useAppStore.getState();
  const a = store.openTab({ title: 'a', descriptor: { view: 'graph' }, closable: true });
  const b = store.openTab({ title: 'b', descriptor: { view: 'monitor' }, closable: true });

  useAppStore.getState().focusTabIndex(0);
  assertEqual(useAppStore.getState().ui.panes.slots[0].tabId, a, 'cmd+1 focuses the first tab');

  useAppStore.getState().cycleTab(1);
  assertEqual(useAppStore.getState().ui.panes.slots[0].tabId, b, 'cycling forward moves on');

  useAppStore.getState().cycleTab(1);
  assertEqual(useAppStore.getState().ui.panes.slots[0].tabId, a, 'cycling wraps around');

  useAppStore.getState().focusTabIndex(8);
  assertEqual(useAppStore.getState().ui.panes.slots[0].tabId, a, 'an out-of-range index is a no-op');
});

test('closing the active pane closes its tab and re-points the slot', () => {
  reset();
  const store = useAppStore.getState();
  const a = store.openTab({ title: 'a', descriptor: { view: 'graph' }, closable: true });
  store.openTab({ title: 'b', descriptor: { view: 'monitor' }, closable: true });

  useAppStore.getState().closeActivePane();
  const ui = useAppStore.getState().ui;
  assertEqual(ui.tabs.length, 1, 'one tab was closed');
  assertEqual(ui.tabs[0].id, a, 'the remaining tab is the other one');
  assertEqual(ui.panes.slots[0].tabId, a, 'the slot fell back to the remaining tab');
});

test('a session status update reaches every tab bound to that session', () => {
  reset();
  const store = useAppStore.getState();
  store.openTab({
    title: 'claude',
    descriptor: { view: 'terminal', sessionId: 's-1' },
    closable: true,
  });
  store.openTab({
    title: 'codex',
    descriptor: { view: 'terminal', sessionId: 's-2' },
    closable: true,
  });

  useAppStore.getState().updateSessionTabs('s-1', { status: 'waiting' });
  const tabs = useAppStore.getState().ui.tabs;
  assertEqual(tabs[0].status, 'waiting', 'the matching tab took the status');
  assert(tabs[1].status === undefined, 'the other session was untouched');
});

test('escape-driven overlays open and close as one group', () => {
  reset();
  const store = useAppStore.getState();
  store.setPaletteOpen(true);
  store.setHelpOpen(true);
  assert(useAppStore.getState().ui.paletteOpen, 'the palette is open');
  useAppStore.getState().closeOverlays();
  const ui = useAppStore.getState().ui;
  assert(
    !ui.paletteOpen && !ui.helpOpen && !ui.switcherOpen && !ui.settingsOpen,
    'closeOverlays clears all of them',
  );
});

test('pending layout writes can be cancelled on teardown', () => {
  cancelPersistLayout();
  assert(true, 'cancelling a pending debounce is safe when nothing is queued');
});
