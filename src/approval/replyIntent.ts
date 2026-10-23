/**
 * What the composer's next `⏎` carries for a card, when the card asked for it.
 *
 * There is one place to type, not two (ux-ia §3.5): "Deny and say why… ⇥",
 * "Keep planning" and "Allow with a note" all move focus to a composer. The
 * card records the user's choice here, on **the composer that receives focus**
 * (the host: the tile's or the screen's own session), with the session the
 * answer goes to (`target`: `answer_in` for a mirrored card, F-15) and the
 * action kind (A44.3). The composer sends exactly that; an intent that does
 * not match its top card is never turned into a Deny (A44.3). Memory only, one
 * intent per host; it ends with its request.
 */

import { useSyncExternalStore } from 'react';

import type { RequestId } from '../lib/gen/RequestId';
import type { SessionUid } from '../lib/gen/SessionUid';

/** What the user chose on the card: amend the allow, or deny with the typed reason. */
export type ReplyIntentKind = 'allowWithNote' | 'denyWithReply';

export interface ReplyIntent {
  requestId: RequestId;
  kind: ReplyIntentKind;
  /** The session the answer goes to: the card's `answer_in` (a mirrored card), else the host itself. */
  target: SessionUid;
}

const intents = new Map<string, ReplyIntent>();
const listeners = new Set<() => void>();
let version = 0;

function emit(): void {
  version++;
  for (const fn of [...listeners]) fn();
}

function same(a: ReplyIntent | null, b: ReplyIntent | null): boolean {
  if (a === null || b === null) return a === b;
  return a.requestId === b.requestId && a.kind === b.kind && a.target === b.target;
}

/** Set (or clear, with null) the intent of the composer of session `host`. */
export function setReplyIntent(host: SessionUid, intent: ReplyIntent | null): void {
  if (same(intents.get(host) ?? null, intent)) return;
  if (intent) intents.set(host, intent);
  else intents.delete(host);
  emit();
}

/** The intent the composer of session `host` holds. */
export function replyIntentOf(host: SessionUid): ReplyIntent | null {
  return intents.get(host) ?? null;
}

function subscribe(fn: () => void): () => void {
  listeners.add(fn);
  return () => listeners.delete(fn);
}

/** The intent of the composer of session `host` (re-renders when it changes). */
export function useReplyIntent(host: SessionUid | null): ReplyIntent | null {
  useSyncExternalStore(subscribe, () => version, () => version);
  return host ? replyIntentOf(host) : null;
}

/** Tests only. */
export function resetReplyIntentsForTests(): void {
  intents.clear();
  emit();
}
