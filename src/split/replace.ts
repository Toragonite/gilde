/**
 * Adding to a full Split (ux-ia §3.7, J4 step 1): "A 7th asks which tile to
 * replace and preselects the least recently focused one; the message is 'Up
 * to 6 sessions side by side.'"
 *
 * `addToSplitOrAsk(uid)` adds when there is room; otherwise it remembers the
 * session (memory only) and opens the Split, which shows the replace choice.
 * Nothing is replaced until the user picks a tile.
 */

import { useSyncExternalStore } from 'react';

import type { SessionUid } from '../lib/gen/SessionUid';
import { appStore } from '../store/useAppStore';
import { focusOrder } from './focus';
import { leastRecent } from './layout';

let pending: SessionUid | null = null;
const listeners = new Set<() => void>();

function emit(): void {
  for (const fn of [...listeners]) fn();
}

function subscribe(fn: () => void): () => void {
  listeners.add(fn);
  return () => listeners.delete(fn);
}

export type AddOutcome = 'added' | 'present' | 'ask';

/** Add to the Split, or ask which tile to replace when it already holds six. */
export function addToSplitOrAsk(uid: SessionUid): AddOutcome {
  const s = appStore.getState();
  if (s.ui.split.uids.includes(uid)) return 'present';
  if (s.addToSplit(uid)) return 'added';
  pending = uid;
  emit();
  if (s.ui.view.screen !== 'split') s.navigate({ screen: 'split' });
  return 'ask';
}

/** The session waiting for a tile, if any. */
export function usePendingAdd(): SessionUid | null {
  return useSyncExternalStore(subscribe, () => pending, () => pending);
}

/** The session waiting for a tile (non-React readers). */
export function currentPendingAdd(): SessionUid | null {
  return pending;
}

export function cancelPendingAdd(): void {
  if (pending === null) return;
  pending = null;
  emit();
}

/** Replace `old` with the waiting session. */
export function replaceWithPending(old: SessionUid): boolean {
  const next = pending;
  if (next === null) return false;
  const s = appStore.getState();
  if (!s.ui.split.uids.includes(old)) return false;
  s.removeFromSplit(old);
  const added = appStore.getState().addToSplit(next);
  if (added) appStore.getState().setSplitFocus(next);
  pending = null;
  emit();
  return added;
}

/** The tile the replace choice preselects: the least recently focused one. */
export function preselectedTile(uids: readonly SessionUid[]): SessionUid | null {
  return leastRecent(focusOrder('split') as readonly SessionUid[], uids);
}

/** Tests only. */
export function resetReplaceForTests(): void {
  pending = null;
  emit();
}
