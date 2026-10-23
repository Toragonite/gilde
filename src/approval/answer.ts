/**
 * Sending an answer (contract §4.2.3, §8.6).
 *
 *  - One call per request at a time: a second press while the first is in
 *    flight does nothing (Rust is idempotent by `requestId` as well, C12).
 *  - Never retried automatically, whatever the error (§4.1 rule 6).
 *  - The call names the exact `requestId` the card showed, and goes to the
 *    `answer_in` session for a mirrored card (F-15).
 *  - Requests answered from this window are remembered (bounded), so a card
 *    that leaves the store because *we* answered it is not reported as
 *    "Answered elsewhere." (ux-ia §3.5 Withdrawn).
 */

import { commands } from '../lib/commands';
import type { AnswerVia } from '../lib/gen/AnswerVia';
import type { Decision } from '../lib/gen/Decision';
import type { PendingInteraction } from '../lib/gen/PendingInteraction';
import type { RequestId } from '../lib/gen/RequestId';
import type { ScreenPromptAction } from '../lib/gen/ScreenPromptAction';
import type { SessionUid } from '../lib/gen/SessionUid';
import type { IpcError } from '../lib/ipcError';
import { isIpcError } from '../lib/ipcError';
import { shortcutAction } from './decisions';

/** What an answer call came to. `sent`/`aborted` are screen-prompt outcomes. */
export type AnswerResult = 'answered' | 'alreadyAnswered' | 'withdrawn' | 'sent' | 'aborted';

export type AnswerOutcome = { ok: true; result: AnswerResult } | { ok: false; error: IpcError } | { ok: false; busy: true };

const MEMORY = 500;
const answeredHere = new Set<string>();
const inFlight = new Set<string>();

function remember(id: string): void {
  answeredHere.delete(id);
  answeredHere.add(id);
  if (answeredHere.size > MEMORY) {
    const oldest = answeredHere.values().next().value;
    if (oldest !== undefined) answeredHere.delete(oldest);
  }
}

/** Whether this window sent (or is sending) an answer for `requestId`. */
export function wasAnsweredHere(requestId: string): boolean {
  return answeredHere.has(requestId) || inFlight.has(requestId);
}

/**
 * Requests this window ended in another way (a stop, which withdraws the
 * session's pending requests): their cards leave without the
 * "Answered elsewhere." line, because the user did it here.
 */
export function markHandledHere(requestIds: readonly string[]): void {
  for (const id of requestIds) remember(id);
}

/** Whether an answer for `requestId` is on its way. */
export function isAnswerInFlight(requestId: string): boolean {
  return inFlight.has(requestId);
}

/** Tests only. */
export function resetAnswersForTests(): void {
  answeredHere.clear();
  inFlight.clear();
}

async function once(requestId: RequestId, run: () => Promise<AnswerResult>): Promise<AnswerOutcome> {
  if (inFlight.has(requestId)) return { ok: false, busy: true };
  inFlight.add(requestId);
  // Remember before the call resolves: the store patch that removes the card
  // can arrive before the response.
  remember(requestId);
  try {
    return { ok: true, result: await run() };
  } catch (e) {
    if (isIpcError(e)) return { ok: false, error: e };
    return {
      ok: false,
      error: { code: 'INTERNAL', key: 'error.internal', params: {}, detail: null, retryable: false } as IpcError,
    };
  } finally {
    inFlight.delete(requestId);
  }
}

/** `interaction_answer` for a structured card. */
export function sendDecision(uid: SessionUid, pending: Pick<PendingInteraction, 'requestId'>, decision: Decision, via: AnswerVia): Promise<AnswerOutcome> {
  return once(pending.requestId, async () => {
    const res = await commands.interaction_answer({ uid, requestId: pending.requestId, decision, via });
    return res.outcome;
  });
}

/** `screen_prompt_key` for a screen or setup card (TOCTOU guard: the card's own `promptRev`). */
export function sendScreenAction(
  uid: SessionUid,
  pending: Pick<PendingInteraction, 'requestId' | 'promptRev'>,
  action: ScreenPromptAction,
): Promise<AnswerOutcome> {
  return once(pending.requestId, async () => {
    if (!pending.promptRev) throw { code: 'STALE_PROMPT', key: 'error.stalePrompt', params: {}, detail: null, retryable: false };
    const res = await commands.screen_prompt_key({ uid, requestId: pending.requestId, promptRev: pending.promptRev, action });
    return res.outcome;
  });
}

/**
 * `⌘⏎` / `⌘⌫` on a card from a surface that is not the card itself (Home's
 * rows, A44.2). The decision is the card stack's own `shortcutAction` — inert
 * on `default_to_no` for allow and on screen prompts that are not
 * permission-shaped at High confidence — so no surface carries a copy of the
 * rule. The caller applies its 700 ms guard first (keyed on the request,
 * A44.1); this sends at most what the card would send for the same key.
 *
 *  - `sent`: the answer is on its way (`outcome` settles it);
 *  - `inert`: nothing was sent (with the line the card would show);
 *  - `open`: the card itself is needed (a question's `⌘⏎`, a plan's reply).
 */
export type ShortcutRun =
  | { kind: 'sent'; outcome: Promise<AnswerOutcome> }
  | { kind: 'inert'; line: 'approval.defaultNo' | 'approval.useOwnKeys' | null }
  | { kind: 'open' };

export function answerShortcut(
  uid: SessionUid,
  pending: PendingInteraction,
  which: 'primary' | 'deny',
  via: AnswerVia = 'shortcut',
): ShortcutRun {
  const action = shortcutAction(pending, which);
  switch (action.kind) {
    case 'decide':
      return { kind: 'sent', outcome: sendDecision(uid, pending, action.decision, via) };
    case 'screen':
      return { kind: 'sent', outcome: sendScreenAction(uid, pending, action.action) };
    case 'inert':
      return { kind: 'inert', line: action.line };
    case 'focus':
    case 'reply':
      return { kind: 'open' };
    default:
      return { kind: 'inert', line: null };
  }
}

/**
 * The line a failed answer shows on the card. Errors raised by the guard
 * rules map to the same lines the WebView shows itself: `CONFLICT{tooFresh}`
 * is the 700 ms guard's Rust half; `REFUSED{defaultToNo}` is the
 * `default_to_no` rule.
 */
export function answerErrorLine(error: IpcError): { key: string; params: Readonly<Record<string, unknown>> } {
  if (error.code === 'CONFLICT' && error.params?.changed === 'tooFresh') return { key: 'guard.justChanged', params: {} };
  if (error.code === 'REFUSED' && error.params?.reason === 'defaultToNo') return { key: 'approval.defaultNo', params: {} };
  return { key: error.key, params: error.params ?? {} };
}
