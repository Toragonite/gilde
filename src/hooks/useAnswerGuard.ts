/**
 * The 700 ms answer guard (contract §8.6, PM-02, T-ui-guard).
 *
 * For 700 ms after the item under the user's hands changes — a new focus card
 * on Home, a `⌘J` arrival, a new top card — and after the window is activated
 * or a `gilde://focus-request` arrives (a notification click, F-30), answering
 * keys **and** button clicks on that item are ignored, visibly. A card that
 * replaced another under the pointer can therefore never be approved blind.
 * Key repeat never answers either. Rust adds its own `CONFLICT{tooFresh}` for
 * requests younger than 300 ms; this guard is the WebView half.
 *
 * Times come from `lib/clock` so tests can step them.
 */

import { useCallback, useEffect, useRef, useState, useSyncExternalStore } from 'react';

import { clock } from '../lib/clock';

export const GUARD_MS = 700;

// ---------------------------------------------------------------------------
// App-wide triggers: window activation and focus requests
// ---------------------------------------------------------------------------

let globalTriggerAt = Number.NEGATIVE_INFINITY;
let version = 0;
const subscribers = new Set<() => void>();
let windowListening = false;

function bump(): void {
  version++;
  for (const fn of [...subscribers]) fn();
}

/** A `gilde://focus-request` arrived (the event bridge calls this). */
export function noteFocusRequest(): void {
  globalTriggerAt = clock.now();
  bump();
}

/** The window became active or visible again. */
export function noteWindowActivation(): void {
  globalTriggerAt = clock.now();
  bump();
}

/**
 * Re-evaluate every mounted guard against the clock. The guards schedule this
 * themselves when their window ends; tests call it after stepping a manual clock.
 */
export function refreshGuards(): void {
  bump();
}

function onWindowFocus(): void {
  noteWindowActivation();
}

function onVisibilityChange(): void {
  if (typeof document !== 'undefined' && document.visibilityState === 'visible') noteWindowActivation();
}

function listenToWindow(): void {
  if (windowListening || typeof window === 'undefined') return;
  window.addEventListener('focus', onWindowFocus);
  document.addEventListener('visibilitychange', onVisibilityChange);
  windowListening = true;
}

function subscribe(fn: () => void): () => void {
  subscribers.add(fn);
  listenToWindow();
  return () => {
    subscribers.delete(fn);
    if (subscribers.size === 0 && windowListening) {
      window.removeEventListener('focus', onWindowFocus);
      document.removeEventListener('visibilitychange', onVisibilityChange);
      windowListening = false;
    }
  };
}

function getVersion(): number {
  return version;
}

/** Tests only: forget the last app-wide trigger (mounted guards see it on their next render). */
export function resetAnswerGuardForTests(): void {
  globalTriggerAt = Number.NEGATIVE_INFINITY;
}

// ---------------------------------------------------------------------------
// The hook
// ---------------------------------------------------------------------------

export interface AnswerGuard {
  /** As of the last render: answering is ignored right now. */
  guarded: boolean;
  /** Clock time at which the guard ends. */
  guardedUntil: number;
  /** Live check against the clock (what `guard` uses). */
  isGuarded: () => boolean;
  /**
   * Wrap an answering handler (a click or a key): it does nothing while the
   * guard holds, and nothing for an auto-repeated key.
   */
  guard: <A extends unknown[]>(handler: (...args: A) => void) => (...args: A) => void;
  /** Spread on the answering controls so the state is visible and announced as unavailable. */
  props: { 'data-guarded'?: 'true'; 'aria-disabled'?: true };
}

function isRepeatEvent(value: unknown): boolean {
  return typeof value === 'object' && value !== null && (value as { repeat?: unknown }).repeat === true;
}

/**
 * Guard the item identified by `itemKey`. A new key (and the first render)
 * starts the 700 ms window; so do window activation and focus requests.
 * `null` means nothing to answer (never guarded).
 */
export function useAnswerGuard(itemKey: string | null, guardMs: number = GUARD_MS): AnswerGuard {
  useSyncExternalStore(subscribe, getVersion, getVersion);
  const keyRef = useRef<{ key: string | null; at: number } | null>(null);
  if (keyRef.current === null || keyRef.current.key !== itemKey) {
    keyRef.current = { key: itemKey, at: clock.now() };
  }
  const startedAt = Math.max(keyRef.current.at, globalTriggerAt);
  const guardedUntil = itemKey === null ? Number.NEGATIVE_INFINITY : startedAt + guardMs;
  const guarded = clock.now() < guardedUntil;

  const untilRef = useRef(guardedUntil);
  untilRef.current = guardedUntil;

  // Re-render once the window ends, so the visible state clears by itself.
  const [, setTick] = useState(0);
  useEffect(() => {
    if (!guarded) return;
    const delay = Math.max(0, guardedUntil - clock.now()) + 5;
    const timer = setTimeout(() => setTick((n) => n + 1), delay);
    return () => clearTimeout(timer);
  }, [guarded, guardedUntil]);

  const isGuarded = useCallback(() => clock.now() < untilRef.current, []);

  const guard = useCallback(
    <A extends unknown[]>(handler: (...args: A) => void) =>
      (...args: A) => {
        if (clock.now() < untilRef.current) return;
        if (args.some(isRepeatEvent)) return;
        handler(...args);
      },
    [],
  );

  const props: AnswerGuard['props'] = guarded ? { 'data-guarded': 'true', 'aria-disabled': true } : {};
  return { guarded, guardedUntil, isGuarded, guard, props };
}
