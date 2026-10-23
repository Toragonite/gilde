/**
 * What the composer's `⏎` does, as data and as calls (contract §4.2.3, §8.6).
 *
 * `composerIntent` picks the mode: an ordinary message, or — only when typing
 * is armed (the card had been on screen ≥ 700 ms before the draft began,
 * F-38) — the reply that answers the session's top card (F-11, N-12), or the
 * intent a card recorded on this composer ("Allow with a note", "Deny and say
 * why", A44.3). `submit` performs it once and reports what the composer
 * should do next. Nothing here retries.
 */

import { answerTarget, composerModeFor, isArmed, markHandledHere, sendDecision, sendScreenAction } from '../approval';
import type { ComposerMode, ReplyIntent } from '../approval';
import { utf8Length } from '../approval/decisions';
import { commands } from '../lib/commands';
import type { PendingInteraction } from '../lib/gen/PendingInteraction';
import type { SendPriority } from '../lib/gen/SendPriority';
import type { SessionUid } from '../lib/gen/SessionUid';
import { newClientMsgId } from '../lib/ids';
import type { IpcError } from '../lib/ipcError';
import { isIpcError } from '../lib/ipcError';

/** `turn_send` text cap (§4.2.3): 1..=256 KiB, no NUL. */
export const MESSAGE_MAX_BYTES = 256 * 1024;
/** Decision messages (DenyWithMessage, KeepPlanning, free answers): ≤ 8 KiB. */
export const REPLY_MAX_BYTES = 8 * 1024;

export type ComposerIntent = ComposerMode | { kind: 'allowWithNote'; requestId: PendingInteraction['requestId']; promptRev: string };

/**
 * The mode `⏎` uses right now in the composer of session `host`.
 *
 *  - Not armed (F-38) or no card: an ordinary message.
 *  - The intent the card recorded on this composer (A44.3) — for this request
 *    and this answer target — is sent exactly as chosen: `allowWithNote` or
 *    `denyWithReply` (a screen prompt's `screen_prompt_key`, a tool
 *    approval's `DenyWithMessage`).
 *  - An intent that does not match (another request, another target, a card
 *    it cannot apply to) never turns into a Deny: an ordinary message.
 *  - A mirrored card (`answer_in` ≠ `host`, F-15) is answered only through an
 *    intent; typing without one is an ordinary message to `host`.
 *  - Otherwise the per-kind mapping of the composer's own card (F-11, N-12).
 *
 * `live` is the answering session's copy of `top` (its fresh `promptRev`).
 */
export function composerIntent(
  host: SessionUid,
  top: PendingInteraction | null,
  intent: ReplyIntent | null,
  cardShownAt: number | null,
  draftStartedAt: number | null,
  now: number,
  live: PendingInteraction | null = top,
): ComposerIntent {
  if (!top || !isArmed(cardShownAt, draftStartedAt, now)) return { kind: 'send' };
  const target = answerTarget(top, host);
  const card = live && live.requestId === top.requestId ? live : top;
  if (intent) {
    if (intent.requestId !== top.requestId || intent.target !== target) return { kind: 'send' };
    if (card.kind === 'screenPrompt' && card.promptRev) {
      return intent.kind === 'allowWithNote'
        ? { kind: 'allowWithNote', requestId: card.requestId, promptRev: card.promptRev }
        : { kind: 'screenDenyWithReply', requestId: card.requestId, promptRev: card.promptRev };
    }
    if (card.kind === 'toolApproval' && intent.kind === 'denyWithReply') return { kind: 'denyWithMessage', requestId: card.requestId };
    return { kind: 'send' };
  }
  if (target !== host) return { kind: 'send' };
  return composerModeFor(top);
}

/** The hint the armed composer shows (null = none). */
export function intentHintKey(
  intent: ComposerIntent,
): 'approval.composerHint' | 'approval.composerAnswerHint' | 'plan.composerHint' | 'approval.noteHint' | null {
  switch (intent.kind) {
    case 'denyWithMessage':
    case 'screenDenyWithReply':
      return 'approval.composerHint';
    case 'freeAnswer':
    case 'focusCard':
      return 'approval.composerAnswerHint';
    case 'keepPlanning':
      return 'plan.composerHint';
    case 'allowWithNote':
      return 'approval.noteHint';
    default:
      return null;
  }
}

/** Strip NUL (the core refuses it) without touching anything else the user typed. */
export function cleanText(text: string): string {
  return text.includes('\u0000') ? text.replace(/\u0000/g, '') : text;
}

export type SubmitResult =
  /** Sent (or answered): clear the draft. */
  | { kind: 'done'; queued: boolean }
  /** Nothing sent; keep the draft and show a line. */
  | { kind: 'kept'; line: { key: string; params: Readonly<Record<string, unknown>> }; error?: IpcError }
  /** Several unanswered questions: move focus to the card. */
  | { kind: 'focusCard' }
  /** An answer for this request is already on its way. */
  | { kind: 'busy' };

function keptError(error: IpcError): SubmitResult {
  if (error.code === 'REFUSED' && error.params?.reason === 'screenPromptPending') {
    return { kind: 'kept', line: { key: 'composer.keptDraft', params: {} } };
  }
  return { kind: 'kept', line: { key: error.key, params: error.params ?? {} }, error };
}

/** Perform `intent` with `text` for session `uid` (the card's session; a mirrored card is never the composer's). */
export async function submit(uid: SessionUid, intent: ComposerIntent, rawText: string, priority: SendPriority): Promise<SubmitResult> {
  const text = cleanText(rawText);
  if (text.trim() === '') return { kind: 'kept', line: { key: '', params: {} } };
  const tooLongReply = { kind: 'kept' as const, line: { key: 'composer.replyTooLong', params: {} } };
  const answer = async (run: () => ReturnType<typeof sendDecision>): Promise<SubmitResult> => {
    const outcome = await run();
    if (outcome.ok) return { kind: 'done', queued: false };
    if ('busy' in outcome) return { kind: 'busy' };
    return keptError(outcome.error);
  };
  switch (intent.kind) {
    case 'send': {
      if (utf8Length(text) > MESSAGE_MAX_BYTES) return { kind: 'kept', line: { key: 'composer.tooLong', params: {} } };
      try {
        const res = await commands.turn_send({ uid, clientMsgId: newClientMsgId(), text, priority });
        return { kind: 'done', queued: res.delivery === 'queued' };
      } catch (e) {
        if (isIpcError(e)) return keptError(e);
        throw e;
      }
    }
    case 'denyWithMessage':
      if (utf8Length(text) > REPLY_MAX_BYTES) return tooLongReply;
      return answer(() => sendDecision(uid, { requestId: intent.requestId }, { kind: 'denyWithMessage', message: text }, 'shortcut'));
    case 'keepPlanning':
      if (utf8Length(text) > REPLY_MAX_BYTES) return tooLongReply;
      return answer(() => sendDecision(uid, { requestId: intent.requestId }, { kind: 'keepPlanning', message: text }, 'shortcut'));
    case 'freeAnswer':
      if (utf8Length(text) > REPLY_MAX_BYTES) return tooLongReply;
      return answer(() =>
        sendDecision(
          uid,
          { requestId: intent.requestId },
          { kind: 'answer', answers: [{ questionIndex: intent.questionIndex, labels: [], freeText: text }] },
          'shortcut',
        ),
      );
    case 'focusCard':
      return { kind: 'focusCard' };
    case 'screenDenyWithReply':
      if (utf8Length(text) > MESSAGE_MAX_BYTES) return { kind: 'kept', line: { key: 'composer.tooLong', params: {} } };
      return answer(() => sendScreenAction(uid, { requestId: intent.requestId, promptRev: intent.promptRev }, { kind: 'denyWithReply', text }));
    case 'allowWithNote':
      if (utf8Length(text) > MESSAGE_MAX_BYTES) return { kind: 'kept', line: { key: 'composer.tooLong', params: {} } };
      return answer(() => sendScreenAction(uid, { requestId: intent.requestId, promptRev: intent.promptRev }, { kind: 'allowWithNote', text }));
    case 'keepDraft':
      return { kind: 'kept', line: { key: 'composer.keptDraft', params: {} } };
  }
}

/**
 * Stop the running turn (`⌘.`). The session's pending requests are withdrawn
 * by the stop; they were ended here, so their cards leave without the
 * "Answered elsewhere." line.
 */
export async function stopTurn(uid: SessionUid, pending: readonly PendingInteraction[], discardQueued: boolean) {
  markHandledHere(pending.map((p) => p.requestId));
  return commands.turn_interrupt({ uid, discardQueued });
}
