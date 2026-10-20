/**
 * Pure presentation rules of the worker accounts view (Gilde plan #3c), kept
 * out of the component so the tests can pin them.
 *
 * Nothing here decides anything about an account: no quota arithmetic, no
 * "is it usable", no cooldown timer. Each rule maps one value the ENGINE sent
 * to how it is shown — a login state to a pill, a severity to a bar tone, a
 * time to a local clock — and every string from the engine goes through
 * `flat()` (one line, no controls or bidi overrides, capped) and is rendered
 * by React as text, never as markup.
 */

import type { MessageKey } from '../i18n';
import type { CcoUsageWindow, CcoWorkerAccount, CcoWorkersUnavailableReason, Locale, NodeStatus } from '../lib/ipc';
// The one runtime value of the vendored uiTypes.ts (CCO 0b7b1b8): imported, never copied.
import { RUN_LOGIN_FAILURE_DETAIL } from '../../src-tauri/sidecar/src/vendored/cco/uiTypes';

export { flat } from '../team/model';

/** Display caps (characters) — the engine caps already; these fit the layout. */
export const WCAP = {
  name: 40,
  label: 40,
  plan: 40,
  model: 64,
  detail: 200,
  spend: 40,
  schema: 64,
} as const;

/**
 * `loginDetail` of an account the engine shows logged out because a RUN failed on login
 * (CCO uiTypes.ts RUN_LOGIN_FAILURE_DETAIL, exported there so a front end can localize it
 * without copying the literal). A fixed marker, not prose: recognized exactly, shown as
 * this build's own sentence; any other detail is shown as it came.
 */
export { RUN_LOGIN_FAILURE_DETAIL };

export interface LoginPill {
  status: NodeStatus;
  key: MessageKey | null;
  /** Shown when `key` is null (a state this build does not know). */
  raw: string;
}

const LOGIN_KEYS: Record<string, MessageKey> = {
  ok: 'workers.login.ok',
  expired: 'workers.login.expired',
  'no-plan-limits': 'workers.login.noPlanLimits',
  error: 'workers.login.error',
  refreshing: 'workers.login.refreshing',
};

const LOGIN_STATUS: Record<string, NodeStatus> = {
  ok: 'ok',
  expired: 'error',
  'no-plan-limits': 'idle',
  error: 'error',
  refreshing: 'idle',
};

/** The account's login pill, by the engine's `login`. */
export function loginPill(account: CcoWorkerAccount): LoginPill {
  return {
    status: LOGIN_STATUS[account.login] ?? 'idle',
    key: LOGIN_KEYS[account.login] ?? null,
    raw: String(account.login).slice(0, 24),
  };
}

/** The `loginDetail` line: this build's sentence for the run-failure marker, else null (show the text). */
export function loginDetailKey(detail: string | null): MessageKey | null {
  return detail === RUN_LOGIN_FAILURE_DETAIL ? 'workers.login.runFailed' : null;
}

/** The engine's own severity → the bar's tone (no threshold of Gilde's). */
export function windowTone(severity: CcoUsageWindow['severity'] | string): 'warn' | 'error' | undefined {
  if (severity === 'critical') return 'error';
  if (severity === 'warning') return 'warn';
  return undefined;
}

/** A cooldown is SHOWN while its end is in the future — the engine's time, compared to the clock. */
export function isCoolingDown(account: CcoWorkerAccount, now: number): boolean {
  return account.cooldownUntil !== null && account.cooldownUntil > now;
}

/** An account on another vendor's CLI is labelled with that CLI's product name. */
export function isOtherVendor(account: CcoWorkerAccount): boolean {
  return account.provider !== 'claude';
}

export const UNAVAILABLE_KEYS: Record<CcoWorkersUnavailableReason, MessageKey> = {
  'no-host': 'workers.reason.noHost',
  'host-gone': 'workers.reason.hostGone',
  'host-too-old': 'workers.reason.hostTooOld',
  'schema-unknown': 'workers.reason.schemaUnknown',
  'host-error': 'workers.reason.hostError',
};

export function unavailableKey(reason: string): MessageKey {
  return UNAVAILABLE_KEYS[reason as CcoWorkersUnavailableReason] ?? 'workers.reason.unknown';
}

const clockCache = new Map<string, Intl.DateTimeFormat>();
function clockFormat(locale: Locale, withDate: boolean): Intl.DateTimeFormat {
  const key = `${locale}:${withDate}`;
  let f = clockCache.get(key);
  if (!f) {
    f = new Intl.DateTimeFormat(locale, {
      ...(withDate ? { month: 'numeric', day: 'numeric' } : {}),
      hour: '2-digit',
      minute: '2-digit',
      hourCycle: 'h23',
    });
    clockCache.set(key, f);
  }
  return f;
}

/** Local wall-clock time `HH:MM`, with the date when it is not today. */
export function formatClock(at: number, locale: Locale, now: number = Date.now()): string {
  if (!Number.isFinite(at)) return '—';
  const sameDay = new Date(at).toDateString() === new Date(now).toDateString();
  return clockFormat(locale, !sameDay).format(new Date(at));
}
