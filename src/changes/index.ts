/**
 * Session → Changes (T15). The shell mounts the three sheets (A23.5): they
 * take no props, act on the main view's focused session and render only
 * while `ui.sheet` is 'commit' / 'merge' / 'discardWorktree'.
 */

export { ChangesView, changesCountOf, subscribeChangesCount } from './ChangesView';
export type { ChangesViewProps } from './ChangesView';
export { CommitSheet, DiscardWorktreeSheet, MergeSheet } from './sheets';
