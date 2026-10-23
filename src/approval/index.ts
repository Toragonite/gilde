/**
 * The card component and its rules (ux-ia §3.5), shared by the Session screen
 * (T15), Split/Team tiles and Board rows (T16) and Home (T14).
 *
 *   import { CardStack } from '../approval';
 *   <CardStack uid={uid} variant="row" shortcuts={focused} onReply={() => openSession(uid)} />
 *
 * See `CardStack.tsx` for the props contract. Hosts never answer a request
 * themselves: the card calls `interaction_answer` / `screen_prompt_key` /
 * `owner_focus`, with the guard, the IME and repeat rules, and the
 * `answer_in` routing applied.
 */

export { CardStack, GONE_LINE_MS } from './CardStack';
export type { CardStackProps } from './CardStack';
export { FocusCard } from './FocusCard';
export type { FocusCardProps } from './FocusCard';
export { InteractionCard, cardTitle, ownerWord } from './InteractionCard';
export type { CardHandle, InteractionCardProps } from './InteractionCard';
export type { AnswerOutcome, AnswerResult, ShortcutRun } from './answer';
export { answerErrorLine, answerShortcut, markHandledHere, sendDecision, sendScreenAction, wasAnsweredHere } from './answer';
export {
  ARM_MS,
  answerTarget,
  composerHintKey,
  composerModeFor,
  isArmed,
  isScreen,
  isStructured,
  isWatched,
  shortcutAction,
} from './decisions';
export type { ComposerMode } from './decisions';
export { setReplyIntent, useReplyIntent } from './replyIntent';
export type { ReplyIntent, ReplyIntentKind } from './replyIntent';
