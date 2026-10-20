/**
 * The team engine's notices, in the user's language (live check G-c).
 *
 * The orchestrator's team engine writes its notices (`kind: notice`, sender
 * `engine`) in English, from a fixed set of sentence templates (the Claude Code
 * Orchestrator's `src/team/contract.ts` TEAM_NOTICE / TEAM_TEXT and the literal
 * notices in `src/team/engine.ts`). This module recognizes those KNOWN templates
 * and turns each into an i18n key plus the variable parts, so the Korean UI does
 * not show raw English.
 *
 * Rules:
 *
 *  - Pure and display-only: the stored text is never changed; a view re-renders
 *    the original whenever nothing matches.
 *  - Every pattern is anchored (`^…$`) on the engine's exact template; a notice
 *    this build does not know falls back to the raw text, unchanged.
 *  - The variable parts are kept as the engine wrote them — a member's report,
 *    a task title, a blocked reason the engine took from a CLI — and are
 *    flattened and capped by the caller like every other team string.
 *  - Order matters where a specific template is a special case of a general one
 *    (`<m> is blocked: every … account is out of quota` before `<m> is blocked: …`).
 */

import type { MessageKey } from '../i18n';
import { CAP, flat } from './model';

/** One recognized notice: the sentence key and its (raw) variable parts. */
export interface LocalizedNotice {
  key: MessageKey;
  params: Record<string, string>;
}

/** A member or team name as the engine prints it (TEAM_NAME / MEMBER_NAME, loosely). */
const NAME = '([A-Za-z0-9][A-Za-z0-9_.-]{0,63})';
/** A board task id (TASK_ID). */
const TASK = '([A-Za-z0-9_-]{1,128})';
/** A whole number. */
const NUM = '(\\d+)';
/** A local clock time `HH:MM`. */
const CLOCK = '(\\d{1,2}:\\d{2})';
/** Free text (may span lines). */
const TEXT = '([\\s\\S]*)';
/** Free text that must not be empty. */
const SOME = '([\\s\\S]+?)';

interface Template {
  key: MessageKey;
  re: RegExp;
  names: readonly string[];
}

function tpl(key: MessageKey, pattern: string, names: readonly string[]): Template {
  return { key, re: new RegExp(`^${pattern}$`), names };
}

/** Escape a literal piece of a template. */
function lit(text: string): string {
  return text.replace(/[.*+?^${}()|[\]\\]/g, '\\$&');
}

/**
 * The known templates. Each line cites the engine template it mirrors; keep the
 * literal parts byte-identical to the engine's.
 */
export const NOTICE_TEMPLATES: readonly Template[] = [
  // TEAM_NOTICE.teamIdle — `the team is idle`
  tpl('team.notice.teamIdle', lit('the team is idle'), []),

  // engine: `${m} moved ${id} "${title}" to review: ${report}`
  tpl('team.notice.movedToReview', `${NAME} moved ${TASK} "${TEXT}" to review: ${TEXT}`, ['member', 'id', 'title', 'report']),
  // engine: `${m} is blocked on ${id} "${title}": ${note}`
  tpl('team.notice.blockedOn', `${NAME} is blocked on ${TASK} "${TEXT}": ${TEXT}`, ['member', 'id', 'title', 'note']),
  // board changes by the lead or the user (engine.ts, task state changes)
  tpl('team.notice.taskDone', `${TASK} "${TEXT}" was marked done by the (lead|user)\\.`, ['id', 'title', 'by']),
  tpl('team.notice.taskUnassigned', `${TASK} "${TEXT}" was unassigned from you by the (lead|user) — stop working on it\\.`, ['id', 'title', 'by']),
  tpl('team.notice.taskCancelled', `${TASK} "${TEXT}" was cancelled by the (lead|user) — stop working on it\\.`, ['id', 'title', 'by']),
  // TEAM_NOTICE.reassigned — `${id} was reassigned to ${to} by the ${by} — stop working on it`
  tpl('team.notice.taskReassigned', `${TASK} was reassigned to ${NAME} by the (lead|user) — stop working on it`, ['id', 'to', 'by']),
  // TEAM_NOTICE.stillClaimed
  tpl('team.notice.stillClaimed', `${NAME} ended a turn with ${TASK} still claimed; its last words: ${TEXT}`, ['member', 'id', 'words']),

  // TEAM_NOTICE.blockedQuota (before the general "is blocked")
  tpl(
    'team.notice.blockedQuota',
    `${NAME} is blocked: every ${NAME} account is out of quota \\(earliest reset ${CLOCK} on ${SOME}\\)\\.`,
    ['member', 'provider', 'time', 'account'],
  ),
  // engine: `${m} is blocked: ${text}` (the reason is the engine's / the CLI's own text)
  tpl('team.notice.blocked', `${NAME} is blocked: ${SOME}`, ['member', 'reason']),
  // engine: `${m}'s lost turn is past its time limit; ${m} is free again.`
  tpl('team.notice.lostTurnFree', `${NAME}'s lost turn is past its time limit; ${NAME} is free again\\.`, ['member', 'member2']),

  // fresh conversation / hand-off (engine.ts routing notices)
  tpl(
    'team.notice.freshNextTurn',
    `${NAME} starts a fresh conversation on its next turn \\(${SOME}\\); it picks up from a hand-off note in the same worktree\\.`,
    ['member', 'why'],
  ),
  tpl(
    'team.notice.contextOut',
    `${NAME}'s conversation ran out of context; it continues in a fresh conversation from a hand-off note in the same worktree\\.`,
    ['member'],
  ),
  tpl(
    'team.notice.resumeFailed',
    `${NAME}'s CLI could not resume its conversation; it continues in a fresh conversation from a hand-off note in the same worktree\\.`,
    ['member'],
  ),
  // TEAM_NOTICE.contextLost
  tpl(
    'team.notice.contextLost',
    `${NAME}'s CLI did not find its conversation and started a new one; ${NAME} was re-briefed and continues in the same worktree`,
    ['member', 'member2'],
  ),
  // TEAM_NOTICE.failover (quota) and its auth / removed variants
  tpl(
    'team.notice.failoverLimit',
    `${NAME} continues on ${SOME} \\(${SOME} is at its limit until ${CLOCK}\\); it picks up from a hand-off note in the same worktree\\.`,
    ['member', 'account', 'old', 'time'],
  ),
  tpl(
    'team.notice.failoverLoggedOut',
    `${NAME} continues on ${SOME} \\(${SOME} is logged out\\); it picks up from a hand-off note in the same worktree\\.`,
    ['member', 'account', 'old'],
  ),
  tpl(
    'team.notice.failoverRemoved',
    `${NAME} continues on ${SOME} \\(${SOME} is no longer in the registry\\); it picks up from a hand-off note in the same worktree\\.`,
    ['member', 'account', 'old'],
  ),
  // TEAM_NOTICE.quotaWait
  tpl(
    'team.notice.quotaWait',
    `${NAME} is paused: ${SOME} is at its ${SOME} limit until ${CLOCK}; it continues then\\.`,
    ['member', 'account', 'window', 'time'],
  ),

  // budgets (engine.ts budget notices) and TEAM_NOTICE.peerWakes
  tpl('team.notice.budgetDay', `the team has used its ${NUM} turns of today; ${NAME} continues at ${CLOCK}\\.`, ['n', 'member', 'time']),
  tpl('team.notice.budgetHour', `${NAME} has used its ${NUM} turns of this hour; ${NAME} continues at ${CLOCK}\\.`, ['member', 'n', 'member2', 'time']),
  tpl(
    'team.notice.peerWakes',
    `${NAME} has been woken ${NUM} times by teammates without a message from you or board progress; teammate messages to ${NAME} now wait for a message from you or the user\\.`,
    ['member', 'n', 'member2'],
  ),

  // turns (TEAM_NOTICE.timeout / cancelled / noAnswer and engine.ts turn notices)
  tpl('team.notice.turnTimeout', `${NAME}'s turn ${NUM} stopped after ${NUM} min \\(turn limit\\); its changes are on disk`, ['member', 'n', 'minutes']),
  tpl(
    'team.notice.turnCancelled',
    `${NAME}'s turn ${NUM} was stopped from outside the team engine; it is paused — unpause it when you are ready`,
    ['member', 'n'],
  ),
  tpl('team.notice.noAnswer', `${NAME} ended turn ${NUM} with no answer`, ['member', 'n']),
  tpl(
    'team.notice.turnCrashed',
    `${NAME}'s turn ${NUM} ended unexpectedly; it is idle and waits for its next message\\.`,
    ['member', 'n'],
  ),
  tpl(
    'team.notice.turnMemoryStopped',
    `${NAME}'s turn ${NUM} was stopped at ${NUM} MB of memory \\(the limit is 8 GiB\\); its changes are on disk\\.`,
    ['member', 'n', 'mb'],
  ),
  tpl('team.notice.turnMemory', `${NAME}'s turn ${NUM} uses ${NUM} MB of memory\\.`, ['member', 'n', 'mb']),
  tpl(
    'team.notice.turnSilent',
    `${NAME}'s turn ${NUM} has printed nothing for ${NUM} minutes; it is still running\\.`,
    ['member', 'n', 'minutes'],
  ),

  // messages (TEAM_NOTICE.memberWroteUser / userWrote)
  tpl('team.notice.memberWroteUser', `${NAME} wrote to the user \\((m\\d+)\\)`, ['member', 'msg']),
  tpl('team.notice.userWrote', `the user wrote to ${NAME} \\((m\\d+)\\): ${TEXT}`, ['to', 'msg', 'text']),

  // the host (TEAM_TEXT.suspendedAfterHostEnded)
  tpl(
    'team.notice.hostEnded',
    `Team ${NAME} was suspended after its host ended; resume it with (\\S+) team_resume ${NAME}`,
    ['team', 'cco', 'team2'],
  ),

  // --- the team's warnings (TeamState.warnings[].text: engine.ts / store.ts warning lines) ---
  // TEAM_TEXT.leadInboxWarning — `lead inbox: ${n} unread`
  tpl('team.notice.leadInbox', `lead inbox: ${NUM} unread`, ['n']),
  // store.ts — `${m} is blocked` (no reason; with one it is `team.notice.blocked`)
  tpl('team.notice.blockedBare', `${NAME} is blocked`, ['member']),
  // store.ts — `${m} is turns-only` / `${m} is turns-only (${mailboxReason})`
  tpl('team.notice.turnsOnly', `${NAME} is turns-only`, ['member']),
  tpl('team.notice.turnsOnlyWhy', `${NAME} is turns-only \\(${SOME}\\)`, ['member', 'why']),
  // store.ts — the ping-pong breaker
  tpl('team.notice.pingPong', lit('teammate wakes are paused for the team (ping-pong breaker)'), []),
  // store.ts — `${m}'s CLI did not find its conversation and started a new one`
  tpl('team.notice.contextLostShort', `${NAME}'s CLI did not find its conversation and started a new one`, ['member']),
  // store.ts — `the repository's stash changed during ${name}'s turn`
  tpl('team.notice.stashChanged', `the repository's stash changed during ${NAME}'s turn`, ['member']),
  // store.ts — `${members.join(', ')} changed the same files: ${paths.join(', ')}`
  tpl('team.notice.overlap', `(${NAME.slice(1, -1)}(?:, ${NAME.slice(1, -1)})*) changed the same files: ${SOME}`, ['members', 'paths']),
  // engine.ts — `${m} is taken over in a terminal since ${HH:MM}`
  tpl('team.notice.attached', `${NAME} is taken over in a terminal since ${CLOCK}`, ['member', 'time']),
  // store.ts — a budget warning is `${m}: ${reason.text}` (the two budget reasons below)
  tpl('team.notice.budgetDayWarn', `${NAME}: the team has used its ${NUM} turns of today`, ['member', 'n']),
  tpl('team.notice.budgetHourWarn', `${NAME}: ${NAME} has used its ${NUM} turns of this hour`, ['member', 'member2', 'n']),

  // --- a member's reason (MemberState.reason.text: engine.ts setState reasons) ---
  tpl('team.notice.budgetDayReason', `the team has used its ${NUM} turns of today`, ['n']),
  tpl('team.notice.budgetHourReason', `${NAME} has used its ${NUM} turns of this hour`, ['member', 'n']),
  // engine.ts quota-wait — `${account} is at its ${window} limit`
  tpl('team.notice.atLimit', `${SOME} is at its ${SOME} limit`, ['account', 'window']),
  // TEAM_TEXT.cancelledFromTaskList / cancelledByCco
  tpl('team.notice.stoppedFromTaskList', lit('stopped from the task list'), []),
  tpl('team.notice.stoppedByCancel', `stopped by (\\S+) cancel`, ['cco']),
  // engine.ts unknown-turn
  tpl('team.notice.unknownTurn', lit('a turn of this member may still be running; it could not be found'), []),

  // --- why a host lists a team as it does (TeamListEntry.reason) ---
  tpl('team.notice.hostGone', lit('host gone'), []),
];

/** Recognize one engine notice. `null` when it is not a known template. */
export function localizeNotice(text: string | null | undefined): LocalizedNotice | null {
  if (typeof text !== 'string' || text === '') return null;
  const trimmed = text.trim();
  for (const template of NOTICE_TEMPLATES) {
    const match = template.re.exec(trimmed);
    if (!match) continue;
    const params: Record<string, string> = {};
    template.names.forEach((name, i) => {
      params[name] = match[i + 1] ?? '';
    });
    return { key: template.key, params };
  }
  return null;
}

/** The sender the engine posts as. */
export const ENGINE_SENDER = 'engine';

/** Whether a message is the engine's (a notice, or anything sent as `engine`). */
export function isEngineMessage(message: { from?: string | null; kind?: string | null }): boolean {
  return message.from === ENGINE_SENDER || message.kind === 'notice';
}

type Translate = (key: MessageKey, params?: Record<string, string | number>) => string;

/**
 * A notice as displayed: the localized sentence (every variable part flattened
 * and capped) when the template is known, else the raw text flattened.
 */
export function noticeDisplay(text: string | null | undefined, t: Translate, max: number = CAP.message): string {
  const known = localizeNotice(text);
  if (known === null) return flat(text, max);
  const params: Record<string, string> = {};
  for (const [name, value] of Object.entries(known.params)) params[name] = flat(value, CAP.message);
  // `by the lead` / `by the user` is a closed set: worded in the UI's language.
  if (known.params.by === 'lead') params.by = t('team.notice.by.lead');
  else if (known.params.by === 'user') params.by = t('team.notice.by.user');
  return flat(t(known.key, params), max);
}

/**
 * A warning kind whose topic word already says all a notice says: the notice is
 * not repeated after it ("쉬는 중", never "쉬는 중 · 팀이 쉬고 있습니다").
 */
const SAME_AS_TOPIC: Readonly<Record<string, MessageKey>> = {
  idle: 'team.notice.teamIdle',
};

/**
 * A team warning's line after its topic word: the localized notice (or the raw
 * text, flattened, when the template is unknown), or `''` when the notice only
 * restates the topic word — then the topic word stands alone.
 */
export function warningDisplay(
  kind: string | null | undefined,
  text: string | null | undefined,
  t: Translate,
  max: number = CAP.warning,
): string {
  const known = localizeNotice(text);
  if (known !== null && typeof kind === 'string' && Object.prototype.hasOwnProperty.call(SAME_AS_TOPIC, kind) && SAME_AS_TOPIC[kind] === known.key) {
    return '';
  }
  return noticeDisplay(text, t, max);
}

/** A sender as displayed: the engine is "Orchestrator", everyone else is their own name. */
export function senderDisplay(from: string | null | undefined, t: Translate): string {
  return from === ENGINE_SENDER ? t('team.sender.engine') : flat(from, CAP.name);
}
