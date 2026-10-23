/**
 * One conversation view's subscription (contract §4.2.4).
 *
 * Mounting subscribes (`convo_subscribe` + a channel), unmounting or a uid
 * change unsubscribes. Events fold through `reduceConvo` (store/convo.ts), so
 * blocks stay in this view and never enter the global store. The uid is
 * resolved through World's tombstones, so a merged session keeps its view.
 *
 * At most 8 live subscriptions exist in the core; the 9th evicts the least
 * recently subscribed (status `evicted`). The view then offers `resubscribe`
 * instead of fighting for a slot.
 */

import { useCallback, useEffect, useRef, useState } from 'react';

import { createChannel, disposeChannel } from '../lib/channel';
import { commands } from '../lib/commands';
import type { ConvoEnvelope } from '../lib/gen/ConvoEnvelope';
import type { SessionUid } from '../lib/gen/SessionUid';
import type { SubId } from '../lib/gen/SubId';
import { newViewId } from '../lib/ids';
import type { IpcError } from '../lib/ipcError';
import { emptyConvo, reduceConvo } from '../store/convo';
import type { ConvoView } from '../store/convo';
import { useAppStore } from '../store/useAppStore';
import { resolveUid } from '../store/worldSlice';

export interface ConversationOptions {
  /** Blocks of the initial tail, 1..=500 (core default 200). */
  tailBlocks?: number;
  /** A short label for the view id (`^[a-z0-9-]{1,32}$` after minting). */
  viewLabel?: string;
}

export interface Conversation {
  view: ConvoView;
  /** The subscribe failure (e.g. `UNSUPPORTED{noReadableConversation}`), if any. */
  error: IpcError | null;
  /** Ask for up to `count` (1..=200) blocks before the oldest one shown. */
  loadOlder: (count?: number) => Promise<void>;
  /** Subscribe again (after `evicted`). */
  resubscribe: () => void;
}

export function useConversation(uid: SessionUid | null, options: ConversationOptions = {}): Conversation {
  const resolved = useAppStore((s) => (uid ? resolveUid(s.world, uid) : null));
  const [view, setView] = useState<ConvoView>(emptyConvo);
  const [error, setError] = useState<IpcError | null>(null);
  const [epoch, setEpoch] = useState(0);
  const subIdRef = useRef<SubId | null>(null);
  const viewRef = useRef(view);
  viewRef.current = view;
  const tail = options.tailBlocks;
  const label = options.viewLabel ?? 'convo';

  useEffect(() => {
    if (!resolved) return;
    let live = true;
    setView(emptyConvo());
    setError(null);
    subIdRef.current = null;
    const channel = createChannel<ConvoEnvelope>((envelope) => {
      if (!live) return;
      setView((v) => reduceConvo(v, envelope));
    });
    commands
      .convo_subscribe({ uid: resolved, viewId: newViewId(label), tailBlocks: tail ?? null }, channel)
      .then(
        (res) => {
          if (!live) {
            void commands.convo_unsubscribe({ subId: res.subId }).catch(() => {});
            return;
          }
          subIdRef.current = res.subId;
          setView((v) => ({ ...v, subId: v.subId ?? res.subId, source: v.source ?? res.source, note: v.note ?? res.note }));
        },
        (e: IpcError) => {
          if (!live) return;
          setError(e);
          setView((v) => ({ ...v, status: 'unavailable' }));
        },
      );
    return () => {
      live = false;
      disposeChannel(channel);
      const subId = subIdRef.current;
      subIdRef.current = null;
      if (subId) void commands.convo_unsubscribe({ subId }).catch(() => {});
    };
  }, [resolved, tail, label, epoch]);

  const loadOlder = useCallback(async (count = 100) => {
    const subId = subIdRef.current;
    const first = viewRef.current.blocks[0];
    if (!subId || !first || !viewRef.current.hasOlder) return;
    try {
      await commands.convo_load_older({ subId, beforeBlockId: first.id, count: Math.max(1, Math.min(200, count)) });
    } catch (e) {
      setError(e as IpcError);
    }
  }, []);

  const resubscribe = useCallback(() => setEpoch((n) => n + 1), []);

  return { view, error, loadOlder, resubscribe };
}
