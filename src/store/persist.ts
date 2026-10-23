/**
 * Writes the persisted UI keys through `ui_state_set` (contract §4.2.1 `UiKey`:
 * sidebarOrder, sidebarFolds, splitSet, teamLayout:<TeamKey>, lastView,
 * homeMode, floorPrefs). Values are debounced per key and checked against the
 * command's caps (≤ 64 KiB, JSON depth ≤ 16) before they are sent; a value
 * over a cap is dropped, never truncated.
 *
 * A45.2: the core accepts each key only in its own shape — ids and enum
 * values, no free text — so these are exactly the shapes sent: `lastView` is
 * the `MainView`, `splitSet` `{ uids, focused }`, `teamLayout:<key>` the
 * `TeamLayout`, `floorPrefs` its three flags, `sidebarOrder` project ids and
 * `sidebarFolds` `{ [nodeKey]: boolean }`. The two sidebar values stay within
 * the core's 500 entries: the order keeps its first 500 ids (as `parseOrder`
 * reads it), the folds their 500 most recently added keys.
 */

import { commands } from '../lib/commands';
import type { JsonValue } from '../lib/gen/serde_json/JsonValue';
import type { UiKey } from '../lib/gen/UiKey';
import type { UiState } from './uiSlice';

export const PERSIST_DEBOUNCE_MS = 300;
export const UI_VALUE_MAX_BYTES = 64 * 1024;
export const UI_VALUE_MAX_DEPTH = 16;
/** `sidebarOrder` ids / `sidebarFolds` entries the core keeps (A45.2). */
export const UI_SIDEBAR_MAX_ENTRIES = 500;

/** The first `max` items of a list value (the top of the user's order); anything else unchanged. */
function firstItems(value: JsonValue, max: number): JsonValue {
  return Array.isArray(value) && value.length > max ? value.slice(0, max) : value;
}

/** The last `max` entries (insertion order) of an object value; anything else unchanged. */
function lastEntries(value: JsonValue, max: number): JsonValue {
  if (typeof value !== 'object' || value === null || Array.isArray(value)) return value;
  const entries = Object.entries(value);
  return entries.length > max ? (Object.fromEntries(entries.slice(-max)) as JsonValue) : value;
}

/** JSON nesting depth (a scalar is depth 0). */
export function jsonDepth(value: unknown, limit = 64): number {
  if (value === null || typeof value !== 'object') return 0;
  if (limit <= 0) return Infinity;
  let max = 0;
  for (const child of Array.isArray(value) ? value : Object.values(value as Record<string, unknown>)) {
    max = Math.max(max, jsonDepth(child, limit - 1));
  }
  return 1 + max;
}

/** True when `value` may be sent as a `ui_state_set` value. */
export function fitsUiCaps(value: JsonValue): boolean {
  if (jsonDepth(value) > UI_VALUE_MAX_DEPTH) return false;
  const bytes = new TextEncoder().encode(JSON.stringify(value)).length;
  return bytes <= UI_VALUE_MAX_BYTES;
}

/** The persisted values of a UI state, by key. */
export function persistedValues(ui: UiState): Map<UiKey, JsonValue> {
  const out = new Map<UiKey, JsonValue>();
  out.set('lastView', ui.view as unknown as JsonValue);
  out.set('splitSet', { uids: ui.split.uids, focused: ui.split.focused } as unknown as JsonValue);
  out.set('homeMode', ui.homeMode);
  if (ui.sidebarOrder !== null) out.set('sidebarOrder', firstItems(ui.sidebarOrder, UI_SIDEBAR_MAX_ENTRIES));
  if (ui.sidebarFolds !== null) out.set('sidebarFolds', lastEntries(ui.sidebarFolds, UI_SIDEBAR_MAX_ENTRIES));
  if (ui.floorPrefs !== null) out.set('floorPrefs', ui.floorPrefs);
  for (const [teamKey, layout] of Object.entries(ui.teamLayouts)) {
    out.set(`teamLayout:${teamKey}`, layout as unknown as JsonValue);
  }
  return out;
}

/**
 * Start persisting: every change of a persisted value is sent once per
 * debounce window. Returns the stop function. Hydration from `app_bootstrap`
 * is not echoed back (the values start as "already sent").
 */
export function startUiPersistence(
  subscribe: (listener: (ui: UiState) => void) => () => void,
  initial: UiState,
): () => void {
  const sent = new Map<UiKey, string>();
  for (const [k, v] of persistedValues(initial)) sent.set(k, JSON.stringify(v));
  const timers = new Map<UiKey, ReturnType<typeof setTimeout>>();
  const latest = new Map<UiKey, JsonValue>();

  const flush = (key: UiKey) => {
    timers.delete(key);
    const value = latest.get(key);
    if (value === undefined) return;
    const text = JSON.stringify(value);
    if (sent.get(key) === text) return;
    if (!fitsUiCaps(value)) return;
    sent.set(key, text);
    void commands.ui_state_set({ key, value }).catch(() => {
      // Not saved this time (BUSY, or the command is not built yet): forget
      // that it was sent so the next change tries again.
      if (sent.get(key) === text) sent.delete(key);
    });
  };

  const unsubscribe = subscribe((ui) => {
    if (!ui.hydrated) return;
    for (const [key, value] of persistedValues(ui)) {
      const text = JSON.stringify(value);
      if (sent.get(key) === text && !timers.has(key)) continue;
      latest.set(key, value);
      if (!timers.has(key)) timers.set(key, setTimeout(() => flush(key), PERSIST_DEBOUNCE_MS));
    }
  });

  return () => {
    unsubscribe();
    for (const [key, timer] of timers) {
      clearTimeout(timer);
      flush(key);
    }
    timers.clear();
  };
}
