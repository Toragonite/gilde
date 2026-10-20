/**
 * Registration into the T05 shell seams — the team preview's only side effect
 * (team contract v2 §8.2).
 *
 * Kept out of the component module for the same reason `monitor/register.ts` is:
 * importing `TeamPane` must not mutate a global registry, or a test that renders
 * the pane silently changes what the rest of the shell resolves. `App.tsx`
 * imports this once, after the store exists.
 *
 * One seam: the pane view `team`. The rail entry and the palette's "Team" come
 * from `uiSlice` (`RAIL_VIEWS`, `VIEW_PANE`), which open or focus this tab.
 */

import { registerPaneView } from '../panes/Pane';
import { TeamPane } from './TeamPane';

registerPaneView('team', TeamPane);
