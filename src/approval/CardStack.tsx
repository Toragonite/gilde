/**
 * The session's pending requests as a stack of cards — the one component the
 * Session screen, Split/Team tiles, Home rows and Board member rows use
 * (ux-ia §3.5 "One component serves all four").
 *
 * Props API (stable; T14 and T16 build against it):
 *
 *   <CardStack
 *     uid={uid}              // session whose `pending` is shown (tombstones resolved)
 *     variant="full"         // 'full' (Session, tiles: whole stack) | 'row' (Home, Board: top card only)
 *     shortcuts={focused}    // ⌘⏎ / ⌘⌫ answer the top card from anywhere in the view
 *     focusToken={token}     // bump to move focus into the top card (⌘J arrival)
 *     onReply={focusComposer}// "Deny and say why… ⇥": where the reply is typed
 *     onLeave={…}            // Esc in a card (Esc never denies)
 *     onShowTerminal={…}     // setup card / "Type something." options
 *     onAnswered={(id, outcome) => …}
 *   />
 *
 * Rules kept here (contract §8.6):
 *  - cards stack oldest first with "{i} of {n}"; `⌘⏎`/`⌘⌫` answer the top one;
 *  - a new card never takes focus; it is announced once, assertively, in the
 *    focused view (§2-12); focus moves only on `focusToken`;
 *  - the 700 ms guard restarts when the top card changes or a `focusToken`
 *    arrives (each card owns its guard, keyed on its `requestId`, A44.1), so
 *    a stacked request that replaces the answered one is never answered by
 *    a second key press inside 700 ms;
 *  - a request that leaves the store without an answer from this window
 *    collapses into one muted line, "Answered elsewhere." (or "The pending
 *    request ended with the session." when the process ended) — no state
 *    flash, no announcement; nothing else ever dismisses a card.
 */

import { useEffect, useMemo, useRef, useState } from 'react';

import { useHotkeys } from '../hooks/useHotkeys';
import { useI18n } from '../i18n/useI18n';
import type { PendingInteraction } from '../lib/gen/PendingInteraction';
import type { RequestId } from '../lib/gen/RequestId';
import type { SessionUid } from '../lib/gen/SessionUid';
import { announce } from '../lib/notify';
import { vendorBadge } from '../lib/vendor';
import { useAppStore } from '../store/useAppStore';
import { sessionOf } from '../store/worldSlice';
import { wasAnsweredHere } from './answer';
import type { AnswerResult } from './answer';
import { InteractionCard, cardTitle } from './InteractionCard';
import type { CardHandle } from './InteractionCard';
import './approval.css';

export interface CardStackProps {
  /** The session whose `Session.pending` is shown (a merged uid resolves to its survivor). */
  uid: SessionUid;
  /** 'full' = Session screen and tiles (whole stack); 'row' = Home and Board rows (top card only). Default 'full'. */
  variant?: 'full' | 'row';
  /** `⌘⏎` / `⌘⌫` answer the top card from anywhere (pass true only for the focused session/tile/row). Default false. */
  shortcuts?: boolean;
  /** Bump to move focus into the top card (Deny on a `default_to_no` card). Never bump it for a new card. */
  focusToken?: number | null;
  /**
   * "Deny and say why… ⇥", "Allow with a note", "Keep planning", Tab in a
   * card: move focus to the composer of `uid` — the host's composer, which
   * receives the card's reply intent (A44.3). Absent → those controls open
   * the answering session, whose composer receives it.
   */
  onReply?: () => void;
  /** `Esc` inside a card. Absent → the card loses focus. */
  onLeave?: () => void;
  /** Setup card "Continue in the terminal", "Type something." options. Absent → opens the session's terminal sub-view. */
  onShowTerminal?: () => void;
  /** After an answer call resolved. */
  onAnswered?: (requestId: RequestId, outcome: AnswerResult) => void;
}

/** How long the collapsed "Answered elsewhere." line stays. The conversation keeps the record. */
export const GONE_LINE_MS = 8000;

interface GoneLine {
  requestId: RequestId;
  key: 'approval.answeredElsewhere' | 'card.sessionEnded';
}

const EMPTY: readonly PendingInteraction[] = [];

export function CardStack(props: CardStackProps) {
  const { uid, variant = 'full', shortcuts = false, focusToken = null, onReply, onLeave, onShowTerminal, onAnswered } = props;
  const { t } = useI18n();
  const session = useAppStore((s) => sessionOf(s.world, uid));
  const pending = session?.pending ?? EMPTY;
  const top = pending[0] ?? null;
  const topRef = useRef<CardHandle | null>(null);
  const [gone, setGone] = useState<GoneLine[]>([]);
  const resolvedUid = session?.uid ?? uid;
  const previous = useRef<{ uid: SessionUid; pending: readonly PendingInteraction[] }>({ uid: resolvedUid, pending });
  const ended = session ? session.control.kind === 'notRunning' : true;

  // Requests that left the store without an answer from this window. A host
  // that reuses the stack for another session (Home's focus card) starts over.
  useEffect(() => {
    const before = previous.current;
    previous.current = { uid: resolvedUid, pending };
    if (before.uid !== resolvedUid) {
      setGone([]);
      return;
    }
    const now = new Set(pending.map((p) => p.requestId));
    const left = before.pending.filter((p) => !now.has(p.requestId) && !wasAnsweredHere(p.requestId));
    if (left.length === 0) return;
    const key = ended ? 'card.sessionEnded' : 'approval.answeredElsewhere';
    setGone((g) => [...g.filter((x) => !left.some((l) => l.requestId === x.requestId)), ...left.map((l) => ({ requestId: l.requestId, key }) as GoneLine)].slice(-3));
  }, [pending, ended, resolvedUid]);

  useEffect(() => {
    if (gone.length === 0) return;
    const timer = setTimeout(() => setGone((g) => g.slice(1)), GONE_LINE_MS);
    return () => clearTimeout(timer);
  }, [gone]);

  // An answer that came back `withdrawn` collapses the same way.
  const onWithdrawn = (requestId: RequestId) => {
    setGone((g) => [...g.filter((x) => x.requestId !== requestId), { requestId, key: 'approval.answeredElsewhere' as const }].slice(-3));
  };

  // Announce a new top card once, in the focused view only; never move focus.
  useEffect(() => {
    if (!shortcuts || !top || !session) return;
    const title = cardTitle(top, session, vendorBadge(session.vendor), t);
    const state = top.kind === 'question' || top.kind === 'planApproval' || top.kind === 'watchedQuestion' || top.kind === 'setupPrompt'
      ? t('state.needsAnswer')
      : t('state.needsApproval');
    announce(t('card.announce', { state, title }), { politeness: 'assertive', key: `card:${top.requestId}` });
  }, [shortcuts, top, session, t]);

  // Focus on request only (⌘J arrival, opening a session with focusCard).
  const lastToken = useRef<number | null>(null);
  useEffect(() => {
    if (focusToken === null || focusToken === lastToken.current) return;
    lastToken.current = focusToken;
    // After the card has rendered.
    const id = requestAnimationFrame(() => topRef.current?.focus());
    return () => cancelAnimationFrame(id);
  }, [focusToken, top?.requestId]);

  useHotkeys(
    [
      { id: 'answer.primary', combo: 'mod+enter', run: () => topRef.current?.shortcut('primary') },
      { id: 'answer.deny', combo: 'mod+backspace', run: () => topRef.current?.shortcut('deny') },
    ],
    { scope: 'S', enabled: shortcuts && top !== null },
  );

  const shown = useMemo(() => (variant === 'row' ? pending.slice(0, 1) : pending), [pending, variant]);

  if (!session || (shown.length === 0 && gone.length === 0)) return null;

  return (
    <div className="g-cards" data-variant={variant} data-count={pending.length}>
      {gone.map((g) => (
        <p key={`gone-${g.requestId}`} className="g-card-gone" data-gone={g.key}>
          {t(g.key)}
        </p>
      ))}
      {shown.map((p, i) => (
        <InteractionCard
          key={p.requestId}
          session={session}
          pending={p}
          index={i}
          count={pending.length}
          variant={variant}
          guardKey={`${p.requestId}:${i === 0 ? 'top' : 'below'}:${i === 0 ? (focusToken ?? '') : ''}`}
          handleRef={i === 0 ? topRef : undefined}
          onReply={onReply}
          onLeave={onLeave}
          onShowTerminal={onShowTerminal}
          onAnswered={onAnswered}
          onWithdrawn={onWithdrawn}
        />
      ))}
    </div>
  );
}
