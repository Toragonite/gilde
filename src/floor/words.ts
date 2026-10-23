/**
 * The words the Floor draws and lists: state words (the `states` area, M4),
 * ownership, durations. One place, so the canvas, the twin, the counter and
 * the name card always say the same thing.
 */

import type { Locale, MessageKey, MessageParams } from '../i18n';
import { translate } from '../i18n';
import type { Origin } from '../lib/gen/Origin';
import { formatDuration } from '../lib/format';
import type { Words } from './draw';
import type { FlSeat, FlState } from './model';

export const STATE_KEY: Readonly<Record<FlState, MessageKey>> = {
  starting: 'state.starting',
  working: 'state.working',
  needsApproval: 'state.needsApproval',
  needsAnswer: 'state.needsAnswer',
  error: 'state.error',
  crashed: 'state.crashed',
  noProgress: 'state.noProgress',
  newReply: 'state.newReply',
  ready: 'state.ready',
  ended: 'state.ended',
  stopped: 'state.stopped',
  statusUnknown: 'state.unknown',
};

const ERROR_KEY: Readonly<Record<string, MessageKey>> = {
  usageLimit: 'state.error.usageLimit',
  signInNeeded: 'state.error.signIn',
  didntStart: 'state.error.didntStart',
  agentError: 'state.error.agent',
};

const ORIGIN_KEY: Readonly<Partial<Record<Origin, MessageKey>>> = {
  terminal: 'origin.terminal',
  vsCode: 'origin.vscode',
  itermTeam: 'origin.itermTeam',
  background: 'origin.background',
  headless: 'origin.headless',
  orchestrator: 'origin.orchestrator',
  otherGilde: 'origin.otherGilde',
};

export function stateWord(locale: Locale, seat: Pick<FlSeat, 'state' | 'errorSub' | 'likely'>): string {
  const key = seat.state === 'error' && seat.errorSub ? (ERROR_KEY[seat.errorSub] ?? STATE_KEY.error) : STATE_KEY[seat.state];
  const word = translate(locale, key);
  return seat.likely ? translate(locale, 'state.likely', { state: word }) : word;
}

/** Where a Watching session runs ("VS Code", "Terminal"), or `null`. */
export function ownerWord(locale: Locale, seat: Pick<FlSeat, 'ownerApp' | 'origin'>): string | null {
  if (seat.ownerApp) return seat.ownerApp;
  const key = seat.origin ? ORIGIN_KEY[seat.origin] : undefined;
  return key ? translate(locale, key) : null;
}

/** "Gilde에서 실행" or "지켜보는 중 · {owner}에서 실행". */
export function ownershipWord(locale: Locale, seat: Pick<FlSeat, 'watch' | 'ownerApp' | 'origin'>): string {
  if (!seat.watch) return translate(locale, 'ownership.inGilde');
  const owner = ownerWord(locale, seat);
  return owner ? translate(locale, 'ownership.runsIn', { owner }) : translate(locale, 'ownership.watching');
}

/**
 * Elapsed time at the Floor's own granularity (pixel-office G5): whole
 * minutes, so a label changes at most once a minute and costs one frame.
 */
export function floorDuration(locale: Locale, ms: number): string {
  if (!Number.isFinite(ms)) return '—';
  if (ms < 60_000) return translate(locale, 'floor.underMinute');
  return formatDuration(Math.floor(ms / 60_000) * 60_000, locale);
}

export function makeWords(locale: Locale): Words {
  return {
    t: (key, params) => translate(locale, key as MessageKey, params as MessageParams | undefined),
    dur: (ms) => floorDuration(locale, ms),
    state: (seat) => stateWord(locale, seat),
  };
}
