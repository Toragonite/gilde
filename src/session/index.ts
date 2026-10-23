/**
 * The Session screen (T15) and the pieces tiles reuse (T16):
 *
 *   import { SessionScreen } from '../session';
 *   <SessionScreen uid={view.uid} sub={view.sub} />      // the main view for `{ screen: 'session' }`
 *
 * Shell convention (A23.5): `StopSessionSheet` takes no props (renders while
 * `ui.sheet === 'stopSession'`, acts on the focused session). The Home focus
 * card is `FocusCard({ session, interaction, answerUid, autoFocus? })`. For a tile: `ConversationView`,
 * `CardStack`, `Composer`, `TerminalView`, `OwnershipBar` / `ContinueBar`.
 */

export { SessionScreen, SEEN_DWELL_MS, stateWord, thirdView } from './SessionScreen';
export type { SessionScreenProps } from './SessionScreen';
export { DetailsPopover } from './DetailsPopover';
export { ContinueBar, ErrorBar, OwnershipBar } from './SessionBars';
export { StopSessionSheet } from './StopSessionSheet';
/** The Home focus card (A23.5), in T14's shape; `CardStack` for a session's whole list. */
export { CardStack, FocusCard } from '../approval';
export type { CardStackProps, FocusCardProps } from '../approval';
export { Composer } from '../composer/Composer';
export type { ComposerProps } from '../composer/Composer';
export { ConversationView } from '../convo/ConversationView';
export { ChangesView } from '../changes/ChangesView';
export { TerminalView } from '../terminal/TerminalView';
export { LogView } from '../terminal/LogView';
