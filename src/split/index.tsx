/**
 * Split (contract §15.2 T16, ux-ia §3.7). The shell finds this module by its
 * `src/<area>/index.{ts,tsx}` glob (A23.5) and mounts `SplitScreen()` for the
 * main view `{ screen: 'split' }`.
 *
 * Also exported for the Team screen and for any surface that adds a session:
 *  - `Tile` / `TileMenu` / `TileStrip` — one session as a tile (Team › Panes);
 *  - `addToSplitOrAsk(uid)` — add, or ask which tile to replace when six are open;
 *  - `requestTileFocus` / `requestComposerFocus` — move focus into a tile on
 *    the user's request (never for an arriving card or tile).
 */

export { SplitScreen } from './SplitScreen';
export type { SplitScreenProps } from './SplitScreen';
export { Tile, TileMenu } from './Tile';
export type { TileMenuItem, TileNotice, TileProps } from './Tile';
export { TileStrip } from './TileStrip';
export type { StripItem } from './TileStrip';
export { addToSplitOrAsk } from './replace';
export { requestComposerFocus, requestTileFocus } from './focus';
