/**
 * Pane grid — 1 / 2 / 3 columns, or 2×2 for four (DESIGN §9-1).
 *
 * The grid only lays out slots; what a slot shows is entirely up to its tab
 * descriptor. `⌘\` steps the column count, `⌘⇧\` toggles the 2×2 arrangement.
 */

import { TabBar } from './TabBar';
import { Pane } from './Pane';
import { useAppStore, useShallow } from '../store/useAppStore';

export function PaneGrid() {
  const panes = useAppStore(useShallow((s) => s.ui.panes.slots));
  const split = useAppStore((s) => s.ui.panes.split);

  return (
    <main className="main">
      <TabBar />
      <div className="pane-grid" data-split={split}>
        {panes.map((slot) => (
          <Pane key={slot.id} paneId={slot.id} tabId={slot.tabId} />
        ))}
      </div>
    </main>
  );
}
