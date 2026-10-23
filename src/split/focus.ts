/**
 * Focus requests for tiles — memory only, outside the store.
 *
 * Focus moves only when the user asks (ux-ia §7.5, UI-GUIDELINES §2-5): `⌘n`,
 * `⌥⌘` arrows, a strip click, "Show pane", or a via-lead prefill that puts
 * the cursor into the lead's composer. Each request is a token; a tile acts on
 * a token once. A new tile, a new card or a finished turn never makes one.
 *
 *  - `requestTileFocus(uid)`: the tile focuses its pending card if it has one,
 *    else its composer (In Gilde), else its body (Watching) [M7, ux-ia §3.7].
 *  - `requestComposerFocus(uid)`: the tile focuses its composer with the caret
 *    at the end (a prefilled draft, `team.prefill.*`).
 *
 * The least-recently-focused order per surface ("split", "team:<key>") lives
 * here too: it decides which tile a 7th session or a strip item replaces.
 */

import { useSyncExternalStore } from 'react';

import { clock } from '../lib/clock';
import type { SessionUid } from '../lib/gen/SessionUid';
import { touch } from './layout';

/** A request older than this is dropped: a tile that mounts later never takes focus from it. */
export const FOCUS_REQUEST_TTL_MS = 1500;

export type FocusTarget = 'tile' | 'composer';

export interface FocusRequest {
  uid: SessionUid;
  target: FocusTarget;
  token: number;
  at: number;
}

let request: FocusRequest | null = null;
let token = 0;
const listeners = new Set<() => void>();

function emit(): void {
  for (const fn of [...listeners]) fn();
}

function subscribe(fn: () => void): () => void {
  listeners.add(fn);
  return () => listeners.delete(fn);
}

/** Ask the tile of `uid` to take focus (card > composer > body). */
export function requestTileFocus(uid: SessionUid): void {
  request = { uid, target: 'tile', token: ++token, at: clock.now() };
  emit();
}

/** Ask the tile of `uid` to focus its composer, caret at the end. */
export function requestComposerFocus(uid: SessionUid): void {
  request = { uid, target: 'composer', token: ++token, at: clock.now() };
  emit();
}

/** The latest request, if it is for `uid` (re-renders when a request arrives). */
export function useFocusRequest(uid: SessionUid | null): FocusRequest | null {
  const current = useSyncExternalStore(
    subscribe,
    () => request,
    () => request,
  );
  return current && uid && current.uid === uid ? current : null;
}

export function currentFocusRequest(): FocusRequest | null {
  return request;
}

/**
 * Take the request for `uid` if it is still fresh; it is consumed, so exactly
 * one tile acts on it, once.
 */
export function takeFocusRequest(uid: SessionUid): FocusRequest | null {
  const r = request;
  if (!r || r.uid !== uid) return null;
  request = null;
  if (clock.now() - r.at > FOCUS_REQUEST_TTL_MS) return null;
  return r;
}

// ---------------------------------------------------------------------------
// Least recently focused, per surface
// ---------------------------------------------------------------------------

const recent = new Map<string, string[]>();

/** Record that `key` was focused on `surface` ("split", "team:<TeamKey>"). */
export function noteFocused(surface: string, key: string): void {
  recent.set(surface, touch(recent.get(surface) ?? [], key));
}

/** Most recently focused first. */
export function focusOrder(surface: string): readonly string[] {
  return recent.get(surface) ?? [];
}

/** Tests only. */
export function resetFocusForTests(): void {
  request = null;
  recent.clear();
  emit();
}
