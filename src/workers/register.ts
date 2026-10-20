/**
 * Registration into the T05 shell seams — the worker accounts view's only side
 * effect (Gilde plan #3c). Kept out of the component module for the same reason
 * `team/register.ts` is: importing `WorkersPane` must not mutate a global
 * registry. `App.tsx` imports this once, after the store exists.
 *
 * One seam: the pane view `workers`. The rail entry and the palette's entry come
 * from `uiSlice` (`RAIL_VIEWS`, `VIEW_PANE`), which open or focus this tab.
 */

import { registerPaneView } from '../panes/Pane';
import { WorkersPane } from './WorkersPane';

registerPaneView('workers', WorkersPane);
