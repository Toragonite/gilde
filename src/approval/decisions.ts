/**
 * Pure card logic (ux-ia §3.5, contract §6.1.4, §6.2.4, §8.6).
 *
 * Everything here is a function of the `PendingInteraction` the store holds,
 * so the rules that decide what a key does are testable without a DOM:
 *
 *  - which digits a card answers and with what (`digitAction`);
 *  - what `⌘⏎` and `⌘⌫` do on the top card (`shortcutAction`), which is
 *    inert on `default_to_no` cards and on screen prompts that are not
 *    permission-shaped at High confidence;
 *  - what the composer's `⏎` sends while a card is pending (`composerModeFor`,
 *    F-11, N-12);
 *  - where an answer goes (`answerTarget`, F-15: a mirrored card answers in
 *    the `answer_in` session).
 */

import type { Decision } from '../lib/gen/Decision';
import type { InteractionKind } from '../lib/gen/InteractionKind';
import type { InteractionOption } from '../lib/gen/InteractionOption';
import type { PendingInteraction } from '../lib/gen/PendingInteraction';
import type { QuestionAnswer } from '../lib/gen/QuestionAnswer';
import type { ScreenPromptAction } from '../lib/gen/ScreenPromptAction';
import type { SessionUid } from '../lib/gen/SessionUid';
import type { ReplyIntentKind } from './replyIntent';

// ---------------------------------------------------------------------------
// Kinds
// ---------------------------------------------------------------------------

/** Cards answered through `interaction_answer`. */
export function isStructured(kind: InteractionKind): boolean {
  return kind === 'toolApproval' || kind === 'question' || kind === 'planApproval';
}

/** Cards answered through `screen_prompt_key`. */
export function isScreen(kind: InteractionKind): boolean {
  return kind === 'screenPrompt' || kind === 'setupPrompt';
}

/** Cards Gilde cannot answer: the only action is Go to {owner} [M8]. */
export function isWatched(kind: InteractionKind): boolean {
  return kind === 'watchedApproval' || kind === 'watchedQuestion';
}

/** The session a card's answers go to: `answer_in` when set (a mirrored card), else its own [F-15]. */
export function answerTarget(pending: Pick<PendingInteraction, 'answerIn'>, own: SessionUid): SessionUid {
  return pending.answerIn ?? own;
}

// ---------------------------------------------------------------------------
// Screen prompts
// ---------------------------------------------------------------------------

/** The digit options of a screen prompt, in key order. */
export function digitOptions(pending: Pick<PendingInteraction, 'options'>): InteractionOption[] {
  return pending.options
    .filter((o) => /^[1-9]$/.test(o.key))
    .sort((a, b) => Number(a.key) - Number(b.key));
}

/**
 * The option that allows: `allowOnce`, or `allowWithNote` — the same allow
 * digit on a prompt whose footer offers "Tab to amend" (A25.1).
 */
export function isAllowRole(role: InteractionOption['role'] | undefined): boolean {
  return role === 'allowOnce' || role === 'allowWithNote';
}

/**
 * Permission-shaped (§6.2.4): the producer marks option 1 as the allow
 * (`allowOnce` or `allowWithNote`) and the last option `deny` exactly when
 * option 1 starts with "Yes" and the last with "No"; anything else is a
 * question-shaped prompt.
 */
export function isPermissionShaped(pending: Pick<PendingInteraction, 'options'>): boolean {
  const digits = digitOptions(pending);
  if (digits.length < 2) return false;
  return isAllowRole(digits[0]?.role) && digits[digits.length - 1]?.role === 'deny';
}

/** The option that answers "No" on a permission-shaped prompt (its digit), if any. */
export function denyOption(pending: Pick<PendingInteraction, 'options'>): InteractionOption | null {
  if (!isPermissionShaped(pending)) return null;
  const digits = digitOptions(pending);
  return digits[digits.length - 1] ?? null;
}

/**
 * Labels after which the TUI takes typing in its own composer: picking one
 * sends the digit and then shows the Terminal sub-view (§6.2.4). Compared on
 * the lowercased label without trailing punctuation.
 */
const TYPE_IN_TERMINAL = ['type something', 'chat about this'];

export function opensTerminal(option: Pick<InteractionOption, 'label'>): boolean {
  const label = option.label.trim().toLowerCase().replace(/[.…]+$/, '');
  return TYPE_IN_TERMINAL.includes(label);
}

/**
 * The folder-trust option of a setup card (A6, A25.1): the producer marks the
 * recognised trust dialog's trust item with role `trustFolder`, and the card
 * then shows "Trust this folder and continue" (the action is
 * `screen_prompt_key{TrustFolder}`; the option's key is never sent). Every
 * other setup card offers only "Continue in the terminal".
 */
export function trustOption(pending: Pick<PendingInteraction, 'kind' | 'options'>): InteractionOption | null {
  if (pending.kind !== 'setupPrompt') return null;
  return pending.options.find((o) => o.role === 'trustFolder') ?? null;
}

/**
 * "Allow with a note" is offered only for a screen prompt whose allow option
 * the producer marks `allowWithNote` — the footer says "Tab to amend" and the
 * option is highlighted (§6.2.4, A25.1); otherwise the action is hidden (the
 * core would answer `UNSUPPORTED{allowWithNote}`).
 */
export function allowsNote(pending: Pick<PendingInteraction, 'kind' | 'options'>): boolean {
  return pending.kind === 'screenPrompt' && pending.options.some((o) => o.role === 'allowWithNote');
}

// ---------------------------------------------------------------------------
// Keys
// ---------------------------------------------------------------------------

/** What one answering control does: a structured decision, a screen action, or a move. */
export type CardAction =
  | { kind: 'decide'; decision: Decision }
  | { kind: 'screen'; action: ScreenPromptAction; thenTerminal: boolean }
  /**
   * Plan "Keep planning", "Deny and say why…" and "Allow with a note": the
   * reply is typed in the composer. `intent` is the choice the composer then
   * sends exactly (A44.3); without it the composer keeps its own mapping.
   */
  | { kind: 'reply'; intent?: ReplyIntentKind }
  /** A question option: select it (single-question, single-select cards send at once). */
  | { kind: 'pick'; questionIndex: number; label: string }
  /** Focus the card without answering. */
  | { kind: 'focus' }
  /** Inert, with the line to show (e.g. `approval.defaultNo`). */
  | { kind: 'inert'; line: 'approval.defaultNo' | 'approval.useOwnKeys' | null };

/**
 * The action of digit `d` ("1".."9") while the card has focus. `null` = the
 * digit is not an option of this card (it is not consumed, so it does nothing).
 * `currentQuestion` is the question digits apply to on a question card.
 */
export function digitAction(pending: PendingInteraction, d: string, currentQuestion = 0): CardAction | null {
  switch (pending.kind) {
    case 'toolApproval':
      if (d === '1') return { kind: 'decide', decision: { kind: 'allowOnce' } };
      if (d === '2') return pending.alwaysLabel !== null ? { kind: 'decide', decision: { kind: 'allowSession' } } : null;
      if (d === '3') return { kind: 'decide', decision: { kind: 'deny' } };
      return null;
    case 'planApproval':
      if (d === '1') return { kind: 'decide', decision: { kind: 'approvePlan' } };
      if (d === '3') return { kind: 'reply' };
      return null;
    case 'question': {
      const q = pending.questions[currentQuestion];
      if (!q) return null;
      const label = q.options[Number(d) - 1];
      return label === undefined ? null : { kind: 'pick', questionIndex: currentQuestion, label };
    }
    case 'screenPrompt': {
      const option = digitOptions(pending).find((o) => o.key === d);
      if (!option) return null;
      return { kind: 'screen', action: { kind: 'key', key: option.key }, thenTerminal: opensTerminal(option) };
    }
    default:
      return null;
  }
}

/**
 * The intent "Deny and say why… ⇥" records for a card (A44.3): a reply that
 * denies a tool approval or a screen prompt. Other kinds keep the composer's
 * own mapping (a question's free answer, a plan's "keep planning").
 */
export function denyWhyIntent(kind: InteractionKind): ReplyIntentKind | undefined {
  return kind === 'toolApproval' || kind === 'screenPrompt' ? 'denyWithReply' : undefined;
}

/**
 * `⌘⏎` (`primary`) and `⌘⌫` (`deny`) on the session's top card (§8.6).
 * Every surface decides these keys here — the card stack, Home's rows, the
 * sidebar, `⌘J` and the tiles (A44.2):
 *  - tool approvals: allow once / deny; on `default_to_no`, `⌘⏎` is inert with
 *    the `approval.defaultNo` line (`[1]` still works);
 *  - plans: approve / move to the composer for the "keep planning" reply;
 *  - questions: never answered by a shortcut; `⌘⏎` focuses the card;
 *  - screen prompts: option 1 / the last option, only when permission-shaped
 *    **and** High confidence (the producer's `default_to_no` is false exactly
 *    then); otherwise inert with `approval.useOwnKeys`;
 *  - setup and watched cards: inert (watched `⌘⏎` = Go to {owner}, handled by the stack).
 */
export function shortcutAction(pending: PendingInteraction, which: 'primary' | 'deny'): CardAction {
  switch (pending.kind) {
    case 'toolApproval':
      if (which === 'deny') return { kind: 'decide', decision: { kind: 'deny' } };
      if (pending.defaultToNo) return { kind: 'inert', line: 'approval.defaultNo' };
      return { kind: 'decide', decision: { kind: 'allowOnce' } };
    case 'planApproval':
      return which === 'primary' ? { kind: 'decide', decision: { kind: 'approvePlan' } } : { kind: 'reply' };
    case 'question':
      return which === 'primary' ? { kind: 'focus' } : { kind: 'inert', line: null };
    case 'screenPrompt': {
      const live = isPermissionShaped(pending) && pending.confidence === 'high' && !pending.defaultToNo;
      if (!live) return { kind: 'inert', line: 'approval.useOwnKeys' };
      const digits = digitOptions(pending);
      const option = which === 'primary' ? digits[0] : digits[digits.length - 1];
      if (!option) return { kind: 'inert', line: 'approval.useOwnKeys' };
      return { kind: 'screen', action: { kind: 'key', key: option.key }, thenTerminal: false };
    }
    default:
      return { kind: 'inert', line: null };
  }
}

// ---------------------------------------------------------------------------
// Questions
// ---------------------------------------------------------------------------

/**
 * Indexes of the questions the card can answer: a question whose text repeats
 * an earlier one is not answerable (the vendor shape keys answers by text, so
 * only the first can be answered; F-37 — the core adds the
 * `card.duplicateQuestion` line).
 */
export function answerableQuestions(pending: Pick<PendingInteraction, 'questions'>): number[] {
  const seen = new Set<string>();
  const out: number[] = [];
  pending.questions.forEach((q, i) => {
    if (seen.has(q.text)) return;
    seen.add(q.text);
    out.push(i);
  });
  return out;
}

/** Selections per question index (labels in the order offered). */
export type Selections = Readonly<Record<number, readonly string[]>>;

/** Toggle `label` for a question: multi-select adds/removes, single-select replaces. */
export function toggleSelection(selections: Selections, questionIndex: number, label: string, multiSelect: boolean): Selections {
  const current = selections[questionIndex] ?? [];
  let next: string[];
  if (multiSelect) next = current.includes(label) ? current.filter((l) => l !== label) : [...current, label];
  else next = [label];
  return { ...selections, [questionIndex]: next };
}

/** The `Answer` decision for the selections, or null while an answerable question has none. */
export function answerFromSelections(pending: Pick<PendingInteraction, 'questions'>, selections: Selections): Decision | null {
  const answers: QuestionAnswer[] = [];
  for (const i of answerableQuestions(pending)) {
    const q = pending.questions[i];
    const picked = selections[i] ?? [];
    if (!q || picked.length === 0) return null;
    const labels = q.options.filter((o) => picked.includes(o));
    if (labels.length === 0) return null;
    answers.push({ questionIndex: i, labels: q.multiSelect ? labels : labels.slice(0, 1), freeText: null });
  }
  return answers.length > 0 ? { kind: 'answer', answers } : null;
}

/** The first answerable question without a selection (where digits apply), else the last answerable one. */
export function currentQuestion(pending: Pick<PendingInteraction, 'questions'>, selections: Selections): number {
  const answerable = answerableQuestions(pending);
  for (const i of answerable) if ((selections[i] ?? []).length === 0) return i;
  return answerable[answerable.length - 1] ?? 0;
}

/** A single question, single-select: a digit or a click answers at once. */
export function answersOnPick(pending: Pick<PendingInteraction, 'questions'>): boolean {
  const answerable = answerableQuestions(pending);
  return answerable.length === 1 && pending.questions[answerable[0] as number]?.multiSelect === false;
}

// ---------------------------------------------------------------------------
// The composer while a card is pending (§8.6, F-11, F-38, N-12)
// ---------------------------------------------------------------------------

/** How long a card must have been on screen before typing is armed as a reply (F-38). */
export const ARM_MS = 700;

/**
 * What the composer's `⏎` does while `pending` is the session's top card:
 *  - `denyWithMessage` (tool approval) — hint `approval.composerHint`;
 *  - `freeAnswer` (a question with exactly one unanswered question) — hint
 *    `approval.composerAnswerHint`;
 *  - `focusCard` (a question with several unanswered questions: `⏎` moves
 *    focus to the card instead of sending a partial answer map) — same hint;
 *  - `keepPlanning` (plan) — hint `plan.composerHint`;
 *  - `screenDenyWithReply` (screen prompt) — hint `approval.composerHint`;
 *  - `keepDraft` (setup prompt, watched card): nothing is sent, the draft stays;
 *  - `send` — no card: an ordinary message.
 */
export type ComposerMode =
  | { kind: 'send' }
  | { kind: 'denyWithMessage'; requestId: PendingInteraction['requestId'] }
  | { kind: 'freeAnswer'; requestId: PendingInteraction['requestId']; questionIndex: number }
  | { kind: 'focusCard'; requestId: PendingInteraction['requestId'] }
  | { kind: 'keepPlanning'; requestId: PendingInteraction['requestId'] }
  | { kind: 'screenDenyWithReply'; requestId: PendingInteraction['requestId']; promptRev: string }
  | { kind: 'keepDraft'; requestId: PendingInteraction['requestId'] };

export function composerModeFor(top: PendingInteraction | null): ComposerMode {
  if (!top) return { kind: 'send' };
  switch (top.kind) {
    case 'toolApproval':
      return { kind: 'denyWithMessage', requestId: top.requestId };
    case 'question': {
      const answerable = answerableQuestions(top);
      if (answerable.length === 1) return { kind: 'freeAnswer', requestId: top.requestId, questionIndex: answerable[0] as number };
      return { kind: 'focusCard', requestId: top.requestId };
    }
    case 'planApproval':
      return { kind: 'keepPlanning', requestId: top.requestId };
    case 'screenPrompt':
      return top.promptRev
        ? { kind: 'screenDenyWithReply', requestId: top.requestId, promptRev: top.promptRev }
        : { kind: 'keepDraft', requestId: top.requestId };
    default:
      return { kind: 'keepDraft', requestId: top.requestId };
  }
}

/** The hint key the armed composer shows for a mode (null = no hint). */
export function composerHintKey(
  mode: ComposerMode,
): 'approval.composerHint' | 'approval.composerAnswerHint' | 'plan.composerHint' | null {
  switch (mode.kind) {
    case 'denyWithMessage':
    case 'screenDenyWithReply':
      return 'approval.composerHint';
    case 'freeAnswer':
    case 'focusCard':
      return 'approval.composerAnswerHint';
    case 'keepPlanning':
      return 'plan.composerHint';
    default:
      return null;
  }
}

/** Decision text caps (§4.2.3): `DenyWithMessage`/`KeepPlanning` 1..=8 KiB, free answers ≤ 8 KiB. */
export const REPLY_MAX_BYTES = 8 * 1024;

export function utf8Length(text: string): number {
  return new TextEncoder().encode(text).length;
}

/** Whether `text` fits a decision message (non-blank, ≤ 8 KiB). */
export function fitsReply(text: string): boolean {
  return text.trim() !== '' && utf8Length(text) <= REPLY_MAX_BYTES;
}

/**
 * Whether typing in the composer is armed as a reply to `top`: the card had
 * been rendered at least `ARM_MS` before the first keystroke of this draft
 * (F-38). `cardShownAt` is when this view first rendered the card;
 * `draftStartedAt` is the clock time of the draft's first keystroke (null =
 * nothing typed yet).
 */
export function isArmed(cardShownAt: number | null, draftStartedAt: number | null, now: number): boolean {
  if (cardShownAt === null) return false;
  const start = draftStartedAt ?? now;
  return start - cardShownAt >= ARM_MS;
}
