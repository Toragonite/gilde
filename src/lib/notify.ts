/**
 * In-app announcements for screen readers (ux-ia §10 Live regions).
 *
 * The WebView has no notification permission (contract §9.2): macOS
 * notifications are posted by Rust. What lives here is what VoiceOver hears
 * inside the window, through the two live regions of `components/LiveRegion`
 * (mounted once by the shell):
 *
 *  - `announce(text, { politeness, key })` — a keyed announcement is made at
 *    most once ("a new approval in the focused session is announced once,
 *    assertively"; the key is its requestId).
 *  - `announceSummary(text)` — the polite summary for other sessions, at most
 *    one per 10 s; calls in between coalesce to the newest text, which is
 *    announced when the 10 s are up.
 *
 * Times come from `lib/clock` so tests can step them.
 */

import { clock } from './clock';

export type Politeness = 'polite' | 'assertive';

export interface Announcement {
  /** Increases with every announcement, so the same text can be announced twice. */
  id: number;
  text: string;
}

export interface AnnouncerState {
  polite: Announcement | null;
  assertive: Announcement | null;
}

export const SUMMARY_INTERVAL_MS = 10_000;
/** Remembered keys (oldest forgotten first). */
const KEY_MEMORY = 500;

let state: AnnouncerState = { polite: null, assertive: null };
let nextId = 1;
const listeners = new Set<() => void>();
const announcedKeys = new Set<string>();
let lastSummaryAt = Number.NEGATIVE_INFINITY;
let pendingSummary: string | null = null;
let summaryTimer: ReturnType<typeof setTimeout> | null = null;

function emit(): void {
  for (const fn of [...listeners]) fn();
}

export function subscribeAnnouncements(listener: () => void): () => void {
  listeners.add(listener);
  return () => {
    listeners.delete(listener);
  };
}

export function getAnnouncements(): AnnouncerState {
  return state;
}

function remember(key: string): void {
  announcedKeys.add(key);
  if (announcedKeys.size > KEY_MEMORY) {
    const oldest = announcedKeys.values().next().value;
    if (oldest !== undefined) announcedKeys.delete(oldest);
  }
}

export interface AnnounceOptions {
  politeness?: Politeness;
  /** Announce this key at most once. */
  key?: string;
}

/** Announce `text`. Returns false when it was not announced (empty, or the key was already used). */
export function announce(text: string, options: AnnounceOptions = {}): boolean {
  const trimmed = text.trim();
  if (trimmed === '') return false;
  if (options.key !== undefined) {
    if (announcedKeys.has(options.key)) return false;
    remember(options.key);
  }
  const entry: Announcement = { id: nextId++, text: trimmed };
  state = options.politeness === 'assertive' ? { ...state, assertive: entry } : { ...state, polite: entry };
  emit();
  return true;
}

/** Whether `key` has been announced (a withdrawn card checks before saying so). */
export function wasAnnounced(key: string): boolean {
  return announcedKeys.has(key);
}

function flushSummary(): void {
  if (pendingSummary === null) return;
  const now = clock.now();
  if (now - lastSummaryAt < SUMMARY_INTERVAL_MS) {
    scheduleSummary(lastSummaryAt + SUMMARY_INTERVAL_MS - now);
    return;
  }
  const text = pendingSummary;
  pendingSummary = null;
  lastSummaryAt = now;
  announce(text, { politeness: 'polite' });
}

function scheduleSummary(delay: number): void {
  if (summaryTimer !== null) return;
  summaryTimer = setTimeout(() => {
    summaryTimer = null;
    flushSummary();
  }, Math.max(0, delay));
}

/** The throttled polite summary ("Needs you: 4. Newest: db-agent needs approval."). */
export function announceSummary(text: string): void {
  if (text.trim() === '') return;
  pendingSummary = text;
  const now = clock.now();
  if (now - lastSummaryAt >= SUMMARY_INTERVAL_MS) flushSummary();
  else scheduleSummary(lastSummaryAt + SUMMARY_INTERVAL_MS - now);
}

/**
 * Re-check a pending summary against the clock. The announcer's own timer
 * does this; tests call it after stepping a manual clock.
 */
export function tickAnnouncer(): void {
  if (summaryTimer !== null) {
    clearTimeout(summaryTimer);
    summaryTimer = null;
  }
  flushSummary();
}

/** Clear a region's text once it has been read (LiveRegion calls this after a delay). */
export function clearAnnouncement(politeness: Politeness, id: number): void {
  const current = state[politeness];
  if (!current || current.id !== id) return;
  state = { ...state, [politeness]: null };
  emit();
}

/** Tests only: forget everything. */
export function resetAnnouncerForTests(): void {
  if (summaryTimer !== null) clearTimeout(summaryTimer);
  summaryTimer = null;
  pendingSummary = null;
  lastSummaryAt = Number.NEGATIVE_INFINITY;
  announcedKeys.clear();
  state = { polite: null, assertive: null };
  emit();
}
