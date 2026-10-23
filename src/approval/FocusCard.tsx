/**
 * Home's focus card (ux-ia §3.1, M15; A23.5): one request, shown as a compact
 * card, in the shape T14's Home already uses —
 *
 *   <FocusCard session={s} interaction={p} answerUid={uid} autoFocus />
 *
 * It is the same `InteractionCard` the Session screen stacks, with every rule
 * it carries (digits only while focused, the 700 ms guard, `default_to_no`,
 * IME and repeat, `answer_in` routing). `answerUid` is where answers go: the
 * request's `answer_in` when it is a teammate's (F-15), else the session.
 * `⌘⏎`/`⌘⌫` answer it from anywhere on Home while `shortcuts` is on (default).
 * Focus moves into it only on mount with `autoFocus`, or when `autoFocus`
 * turns on — never because a new request arrived.
 */

import { useEffect, useRef } from 'react';

import { useHotkeys } from '../hooks/useHotkeys';
import type { PendingInteraction } from '../lib/gen/PendingInteraction';
import type { RequestId } from '../lib/gen/RequestId';
import type { Session } from '../lib/gen/Session';
import type { SessionUid } from '../lib/gen/SessionUid';
import type { AnswerResult } from './answer';
import { InteractionCard } from './InteractionCard';
import type { CardHandle } from './InteractionCard';

export interface FocusCardProps {
  session: Session;
  interaction: PendingInteraction;
  /** Where answers go (`interaction.answerIn ?? session.uid`). */
  answerUid: SessionUid;
  /** Move focus into the card on mount (Deny on a `default_to_no` card). */
  autoFocus?: boolean;
  /** `⌘⏎`/`⌘⌫` answer this card from anywhere. Default true. */
  shortcuts?: boolean;
  /** "Deny and say why…", "Keep planning": absent → those controls open the session. */
  onReply?: () => void;
  onAnswered?: (requestId: RequestId, outcome: AnswerResult) => void;
  /** "{i} of {n}" when the session has several requests (default 0 of 1: no position). */
  index?: number;
  count?: number;
}

export function FocusCard({ session, interaction, answerUid, autoFocus = false, shortcuts = true, onReply, onAnswered, index = 0, count = 1 }: FocusCardProps) {
  const handle = useRef<CardHandle | null>(null);
  // A host-given answer target that differs from the session is the F-15 route.
  const pending =
    answerUid !== session.uid && interaction.answerIn !== answerUid ? { ...interaction, answerIn: answerUid } : interaction;

  useHotkeys(
    [
      { id: 'answer.primary', combo: 'mod+enter', run: () => handle.current?.shortcut('primary') },
      { id: 'answer.deny', combo: 'mod+backspace', run: () => handle.current?.shortcut('deny') },
    ],
    { scope: 'S', enabled: shortcuts },
  );

  useEffect(() => {
    if (!autoFocus) return;
    const id = requestAnimationFrame(() => handle.current?.focus());
    return () => cancelAnimationFrame(id);
  }, [autoFocus]);

  return (
    <InteractionCard
      session={session}
      pending={pending}
      index={index}
      count={count}
      variant="row"
      guardKey={`${interaction.requestId}:focus`}
      handleRef={handle}
      onReply={onReply}
      onAnswered={onAnswered}
    />
  );
}
