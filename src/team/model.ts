/**
 * Pure presentation rules of the team preview (team contract v2 §8.2), kept out
 * of the components so the tests can pin them.
 *
 * Two of them are safety rules rather than styling:
 *
 *  1. **Every string from a team is data.** Members and the lead write messages,
 *     roles, notes and tool lines; any of it may hold control characters, a
 *     bidirectional override that reorders what the eye reads, or a megabyte of
 *     text. `flat()` makes one display line of it and caps it — React escapes the
 *     rest (contract §8: "flatten + cap before rendering").
 *  2. **A pill is the ENGINE's member state**, never a guess from a terminal
 *     screen — and a team that is not `believed` (L7: read from `state.json`
 *     while its host is gone) shows `was running` instead of `running`.
 */

import type { MessageKey } from '../i18n';
import { agentDisplayName } from '../lib/agents';
import type {
  CcoBoardTask,
  CcoMemberState,
  CcoTeamState,
  CcoTeamView,
  NodeStatus,
} from '../lib/ipc';

// eslint-disable-next-line no-control-regex
const CONTROL_OR_SPACE = /[\s\u0000-\u001f\u007f-\u009f\u2028\u2029]+/g;
/** Bidi embeddings, overrides and isolates: they reorder what is displayed. */
const BIDI_CONTROLS = /[\u202A-\u202E\u2066-\u2069\u200E\u200F\u061C]/g;

/** One display line of a team string: no controls, no bidi tricks, at most `max` characters. */
export function flat(value: string | null | undefined, max: number): string {
  if (typeof value !== 'string' || value === '') return '';
  const line = value.replace(BIDI_CONTROLS, '').replace(CONTROL_OR_SPACE, ' ').trim();
  if (line.length <= max) return line;
  let cut = Math.max(0, max - 1);
  const code = line.charCodeAt(cut - 1);
  if (cut > 0 && code >= 0xd800 && code <= 0xdbff) cut -= 1;
  return `${line.slice(0, cut)}…`;
}

/** Display caps (characters). */
export const CAP = {
  name: 40,
  role: 80,
  title: 120,
  goal: 600,
  reason: 200,
  /** CCO TeamListEntry.reason: "one line, ≤ 300 chars as rendered". */
  listReason: 300,
  /** The same reason on a team chip (the full line is the chip's title and the header's). */
  chipReason: 24,
  activity: 200,
  message: 400,
  note: 160,
  path: 120,
  /** A full path in a tooltip (not cut for the eye, still bounded). */
  tip: 1024,
  warning: 200,
  label: 60,
} as const;

/**
 * Agents are text badges, never logos (UI-GUIDELINES §2-3), named by product
 * (F13): the engine calls the Antigravity CLI `gemini`.
 */
export function providerBadge(provider: string): string {
  return flat(agentDisplayName(provider), 16);
}

const MEMBER_STATE_KEYS: Record<string, MessageKey> = {
  starting: 'team.member.starting',
  idle: 'team.member.idle',
  running: 'team.member.running',
  pausing: 'team.member.pausing',
  paused: 'team.member.paused',
  attach_pending: 'team.member.attach_pending',
  attached: 'team.member.attached',
  blocked: 'team.member.blocked',
  stopped: 'team.member.stopped',
  ended: 'team.member.ended',
};

const MEMBER_STATE_STATUS: Record<string, NodeStatus> = {
  starting: 'working',
  running: 'working',
  idle: 'idle',
  pausing: 'detached',
  paused: 'detached',
  attach_pending: 'waiting',
  attached: 'waiting',
  blocked: 'error',
  stopped: 'exited',
  ended: 'exited',
};

/** States that claim a live process — not shown as current when the team is not believed. */
const LIVE_STATES = new Set(['starting', 'running', 'pausing', 'attach_pending', 'attached']);

export interface MemberPill {
  status: NodeStatus;
  /** The message key of the label, or null when `raw` is shown (a state this build does not know). */
  key: MessageKey | null;
  raw: string;
}

/** The pill of one member, by the engine's state — and L7 (`believed`). */
export function memberPill(member: CcoMemberState, believed: boolean): MemberPill {
  if (!believed && LIVE_STATES.has(member.state)) {
    return { status: 'idle', key: 'team.wasRunning', raw: member.state };
  }
  return {
    status: MEMBER_STATE_STATUS[member.state] ?? 'idle',
    key: MEMBER_STATE_KEYS[member.state] ?? null,
    raw: flat(member.state, 24),
  };
}

export interface TeamPill {
  status: NodeStatus;
  key: MessageKey | null;
  raw: string;
}

const TEAM_STATUS_KEYS: Record<string, MessageKey> = {
  active: 'team.status.active',
  suspended: 'team.status.suspended',
  ended: 'team.status.ended',
  readonly: 'team.status.readonly',
  damaged: 'team.status.damaged',
};

const TEAM_STATUS_STATUS: Record<string, NodeStatus> = {
  active: 'ok',
  suspended: 'stalled',
  ended: 'exited',
  readonly: 'waiting',
  damaged: 'error',
};

/** The team's status pill. An `active` team that is not believed is `host gone` (L7). */
export function teamPill(state: CcoTeamState, believed: boolean): TeamPill {
  if (!believed && state.status === 'active') {
    return { status: 'stalled', key: 'team.hostGone', raw: state.status };
  }
  return {
    status: TEAM_STATUS_STATUS[state.status] ?? 'idle',
    key: TEAM_STATUS_KEYS[state.status] ?? null,
    raw: flat(state.status, 24),
  };
}

/** The five board columns of §8.2, in order. */
export const BOARD_COLUMNS = ['open', 'claimed', 'blocked', 'review', 'done'] as const;
export type BoardColumn = (typeof BOARD_COLUMNS)[number];

export const BOARD_COLUMN_KEYS: Record<BoardColumn, MessageKey> = {
  open: 'team.board.open',
  claimed: 'team.board.claimed',
  blocked: 'team.board.blocked',
  review: 'team.board.review',
  done: 'team.board.done',
};

/**
 * The board grouped by state. `cancelled` is finished work and sits under Done
 * (tagged); a state this build does not know is shown under Open rather than lost.
 */
export function boardColumns(board: CcoBoardTask[]): Record<BoardColumn, CcoBoardTask[]> {
  const out: Record<BoardColumn, CcoBoardTask[]> = { open: [], claimed: [], blocked: [], review: [], done: [] };
  for (const task of board) {
    const state = task.state === 'cancelled' ? 'done' : task.state;
    const column = (BOARD_COLUMNS as readonly string[]).includes(state) ? (state as BoardColumn) : 'open';
    out[column].push(task);
  }
  return out;
}

/** The member's kept activity lines (oldest first), from the view. */
export function activityOf(view: CcoTeamView, member: string) {
  return view.activity.find((a) => a.member === member)?.lines ?? [];
}

/** Messages to or from one member, newest first. */
export function messagesOf(view: CcoTeamView, member: string) {
  return view.messages.filter((m) => m.from === member || m.to === member);
}

/** The hint line a team's status calls for, if any. */
export function statusHint(view: CcoTeamView): MessageKey | null {
  const { status } = view.state;
  if (status === 'suspended') return 'team.suspendedHint';
  if (status === 'readonly') return 'team.readonlyHint';
  if (status === 'damaged') return 'team.damagedHint';
  return null;
}

// ---------------------------------------------------------------------------
// Plain language (F7): engine codes → sentences a person reads
// ---------------------------------------------------------------------------

/**
 * A known code's sentence, or null for a code this build does not know. A null
 * is shown as the generic sentence with the raw code in a tooltip — a new code
 * from a newer engine is never hidden and never shown as the main text.
 */
export interface PlainLabel {
  key: MessageKey;
  /** The engine's own code, for a tooltip; never the main text. */
  code: string;
  known: boolean;
}

const REASON_KEYS: Record<string, MessageKey> = {
  quota: 'team.reason.quota',
  login: 'team.reason.login',
  'account-removed': 'team.reason.account-removed',
  crash: 'team.reason.crash',
  context: 'team.reason.context',
  resume: 'team.reason.resume',
  worktree: 'team.reason.worktree',
  'worktree-missing': 'team.reason.worktree-missing',
  'worktree-branch': 'team.reason.worktree-branch',
  setup: 'team.reason.setup',
  cap: 'team.reason.cap',
  memory: 'team.reason.memory',
  cli: 'team.reason.cli',
  error: 'team.reason.error',
  'unknown-turn': 'team.reason.unknown-turn',
  budget: 'team.reason.budget',
  'quota-wait': 'team.reason.quota-wait',
  cancelled: 'team.reason.cancelled',
};

/** `MemberState.reason.code` → a sentence; unknown codes → `team.reason.other`. */
export function reasonLabel(code: string | null | undefined): PlainLabel {
  const raw = flat(code, 32);
  const key = REASON_KEYS[raw];
  return key ? { key, code: raw, known: true } : { key: 'team.reason.other', code: raw, known: false };
}

const WARNING_KEYS: Record<string, MessageKey> = {
  overlap: 'team.warning.overlap',
  budget: 'team.warning.budget',
  blocked: 'team.warning.blocked',
  idle: 'team.warning.idle',
  attach: 'team.warning.attach',
  store: 'team.warning.store',
  mailbox: 'team.warning.mailbox',
  'context-lost': 'team.warning.context-lost',
  stash: 'team.warning.stash',
  inbox: 'team.warning.inbox',
};

/** `TeamState.warnings[].kind` → a short topic; unknown kinds → `team.warning.other`. */
export function warningLabel(kind: string | null | undefined): PlainLabel {
  const raw = flat(kind, 24);
  const key = WARNING_KEYS[raw];
  return key ? { key, code: raw, known: true } : { key: 'team.warning.other', code: raw, known: false };
}

const MAILBOX_KEYS: Record<string, MessageKey> = {
  tools: 'team.mailbox.tools',
  'turns-only': 'team.mailbox.turnsOnly',
  unverified: 'team.mailbox.unverified',
};

/** How a member gets its messages, in words. An unknown value reads as "not checked yet". */
export function mailboxKey(mailbox: string | null | undefined): MessageKey {
  return MAILBOX_KEYS[flat(mailbox, 16)] ?? 'team.mailbox.unverified';
}

const MESSAGE_KIND_KEYS: Record<string, MessageKey> = {
  question: 'team.messageKind.question',
  answer: 'team.messageKind.answer',
  assignment: 'team.messageKind.assignment',
  report: 'team.messageKind.report',
  notice: 'team.messageKind.notice',
};

/**
 * A message's kind as a badge word, or null when no badge is shown: a plain
 * `text` message needs none, and a kind this build does not know gets none
 * (its raw value goes in the row's tooltip).
 */
export function messageKindKey(kind: string | null | undefined): MessageKey | null {
  return MESSAGE_KIND_KEYS[flat(kind, 16)] ?? null;
}

const OUTCOME_KEYS: Record<string, MessageKey> = {
  ok: 'team.outcome.ok',
  error: 'team.outcome.error',
  quota: 'team.outcome.quota',
  auth: 'team.outcome.auth',
  timeout: 'team.outcome.timeout',
  interrupted: 'team.outcome.interrupted',
  crashed: 'team.outcome.crashed',
  cancelled: 'team.outcome.cancelled',
  context: 'team.outcome.context',
  'resume-failed': 'team.outcome.resume-failed',
  memory: 'team.outcome.memory',
};

/** How a turn ended (`lastTurn.outcome`), in words; unknown → `team.outcome.other`. */
export function outcomeLabel(outcome: string | null | undefined): PlainLabel {
  const raw = flat(outcome, 24);
  const key = OUTCOME_KEYS[raw];
  return key ? { key, code: raw, known: true } : { key: 'team.outcome.other', code: raw, known: false };
}
