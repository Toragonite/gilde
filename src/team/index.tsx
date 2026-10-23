/**
 * The Team screen (contract §15.2 T16; ux-ia §3.8–§3.10). The shell finds
 * this module by its `src/<area>/index.{ts,tsx}` glob (A23.5) and mounts
 * `TeamScreen({ teamKey, mode })` for `{ screen: 'team' }` (its header and
 * mode switch are this module's; Team › Floor mounts T17's `TeamFloor`,
 * A23.2) and the prop-less `ClosePaneSheet` while `ui.sheet === 'closePane'`.
 */

export { TeamScreen } from './TeamScreen';
export type { TeamScreenProps } from './TeamScreen';
export { ClosePaneSheet } from './ClosePaneSheet';
