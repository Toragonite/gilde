/**
 * System notifications for health alerts — the one module that talks to the
 * OS notification centre.
 *
 * A health alert that needs the user (`action`) is shown inside the window as a
 * sticky toast. While Gilde is in the background nobody sees that toast, so at
 * the moment the toast is created (`healthSlice.applyHealth`, once per alert
 * raise) this module also posts one macOS notification — only when:
 *
 *   - the setting is on (`config.notifications.system`, default on),
 *   - the main window does NOT have focus (focused → the toast is enough),
 *   - macOS allows it. Permission is never asked at startup: the first time a
 *     notification would be sent, `isPermissionGranted()` is checked and, if
 *     not granted, `requestPermission()` is called — at most once per app run.
 *     A refusal is remembered for the run: nothing is posted, nothing is asked
 *     again, and the settings sheet says where to turn it back on.
 *
 * The notification is the alert's own title and one-line body (the same text
 * as the toast), with no sound, no actions, no click handling — clicking it
 * brings the app forward by the OS default.
 *
 * Nothing here may break the toast or the health flow: every failure becomes a
 * `console.warn` and an outcome value; `notifyHealthAlert` never throws and
 * never rejects. In showcase mode (`src/features.ts`) the module stops before
 * any plugin call, at the same boundary as `call()` and `listenAo()`.
 *
 * The OS side is an injectable {@link NotifyBackend}, so the tests mock it.
 */

import { isTauri } from '@tauri-apps/api/core';
import { getCurrentWindow } from '@tauri-apps/api/window';
import {
  isPermissionGranted as pluginIsPermissionGranted,
  requestPermission as pluginRequestPermission,
  sendNotification as pluginSendNotification,
} from '@tauri-apps/plugin-notification';

import { showcaseMode } from '../features';
import { alertText } from '../shell/healthText';
import type { HealthAlert, Locale } from './ipc';

/** The longest title or body posted; longer text is cut with an ellipsis. */
export const NOTIFY_MAX_CHARS = 200;

/** The OS side, injectable for tests. */
export interface NotifyBackend {
  isPermissionGranted(): Promise<boolean>;
  /** `'granted'`, `'denied'` or `'default'` (the user closed the prompt). */
  requestPermission(): Promise<string>;
  /** Post one notification. */
  notify(message: { title: string; body: string }): void | Promise<void>;
  /** Whether the main window has focus. A throw falls back to `document.hasFocus()`. */
  isFocused(): Promise<boolean>;
}

/** What the module knows about the OS permission this run. */
export type NotifyPermission = 'unknown' | 'granted' | 'denied';

/** What one `notifyHealthAlert` call did — for tests and the log, never shown. */
export type NotifyOutcome =
  | 'posted'
  | 'showcase' // showcase build: never reaches the plugin
  | 'skipped' // not an active `action` alert
  | 'disabled' // the setting is off
  | 'unavailable' // no Tauri host (unit tests, a plain browser)
  | 'focused' // the window has focus: the toast is enough
  | 'denied' // macOS said no (this run)
  | 'failed'; // a plugin error, swallowed

export interface NotifyContext {
  locale: Locale;
  /** `ui.systemNotifications`. */
  enabled: boolean;
}

/** The real backend: the Tauri notification plugin and the window API. */
const tauriBackend: NotifyBackend = {
  isPermissionGranted: () => pluginIsPermissionGranted(),
  requestPermission: () => pluginRequestPermission(),
  notify: (message) => pluginSendNotification({ title: message.title, body: message.body }),
  isFocused: () => getCurrentWindow().isFocused(),
};

let injected: NotifyBackend | null = null;
let useInjected = false;
let permission: NotifyPermission = 'unknown';
/** `requestPermission()` was called this run (it never is again). */
let asked = false;
/** The permission question in flight, shared by alerts raised together. */
let pending: Promise<boolean> | null = null;
const listeners = new Set<() => void>();

/** Install a backend (tests), or `null` to go back to the real one. Also resets the run state. */
export function setNotifyBackend(backend: NotifyBackend | null): void {
  injected = backend;
  useInjected = backend !== null;
  resetNotifyState();
}

/** Forget this run's permission answer (tests). */
export function resetNotifyState(): void {
  asked = false;
  pending = null;
  setPermission('unknown');
}

/** The permission as far as this run knows it. */
export function notifyPermission(): NotifyPermission {
  return permission;
}

/** Be told when {@link notifyPermission} changes (the settings sheet). */
export function subscribeNotifyPermission(listener: () => void): () => void {
  listeners.add(listener);
  return () => {
    listeners.delete(listener);
  };
}

function setPermission(next: NotifyPermission): void {
  if (permission === next) return;
  permission = next;
  for (const listener of [...listeners]) {
    try {
      listener();
    } catch {
      // a listener's problem is not the notifier's
    }
  }
}

function backendNow(): NotifyBackend | null {
  if (useInjected) return injected;
  try {
    return isTauri() ? tauriBackend : null;
  } catch {
    return null;
  }
}

/** One line, at most `max` characters (an ellipsis marks a cut). */
export function clipLine(text: string, max: number = NOTIFY_MAX_CHARS): string {
  const line = text.replace(/\s+/g, ' ').trim();
  const chars = Array.from(line);
  if (chars.length <= max) return line;
  return `${chars.slice(0, max - 1).join('').trimEnd()}…`;
}

/** Title and body of an alert's notification: the toast's own text, one line each. */
export function notificationText(alert: HealthAlert, locale: Locale): { title: string; body: string } {
  const text = alertText(alert, locale);
  return { title: clipLine(text.title), body: clipLine(text.body) };
}

async function windowFocused(backend: NotifyBackend): Promise<boolean> {
  try {
    return (await backend.isFocused()) === true;
  } catch (e) {
    console.warn('[gilde] notify: window focus unknown, using document.hasFocus()', e);
    try {
      return typeof document !== 'undefined' && typeof document.hasFocus === 'function' && document.hasFocus();
    } catch {
      return false;
    }
  }
}

/** Whether posting is allowed; asks macOS at most once per run. Throws only plugin errors. */
async function permitted(backend: NotifyBackend): Promise<boolean> {
  if (permission === 'granted') return true;
  if (await backend.isPermissionGranted()) {
    setPermission('granted');
    return true;
  }
  if (asked) {
    // A question already in flight answers every alert raised with it.
    if (pending) return pending;
    setPermission('denied');
    return false;
  }
  asked = true;
  pending = (async () => {
    try {
      const answer = await backend.requestPermission();
      const granted = answer === 'granted';
      setPermission(granted ? 'granted' : 'denied');
      return granted;
    } catch (e) {
      // No answer is no permission; the question is not asked again this run.
      setPermission('denied');
      throw e;
    } finally {
      pending = null;
    }
  })();
  return pending;
}

/**
 * Post the OS notification for one alert raise, if it should be posted.
 * Never throws, never rejects; the outcome says what happened.
 */
export async function notifyHealthAlert(alert: HealthAlert, context: NotifyContext): Promise<NotifyOutcome> {
  try {
    if (showcaseMode()) return 'showcase';
    if (!alert || alert.severity !== 'action' || alert.dismissed) return 'skipped';
    if (!context.enabled) return 'disabled';
    const backend = backendNow();
    if (!backend) return 'unavailable';
    if (await windowFocused(backend)) return 'focused';
    if (!(await permitted(backend))) return 'denied';
    await backend.notify(notificationText(alert, context.locale));
    return 'posted';
  } catch (e) {
    console.warn('[gilde] notify: the system notification failed', e);
    return 'failed';
  }
}
