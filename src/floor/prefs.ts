/**
 * The Floor's persisted preferences (`ui_state_set` key `floorPrefs`, contract
 * §4.2.1): picture or list, message lines, legend. The value is untrusted JSON
 * from `app_bootstrap`, so it is validated field by field.
 */

import { useCallback } from 'react';

import type { JsonValue } from '../lib/gen/serde_json/JsonValue';
import { useAppStore } from '../store/useAppStore';

export interface FloorPrefs {
  /** Show as list (the twin visible, no canvas). */
  list: boolean;
  /** Recent message lines (`M`). */
  traffic: boolean;
  /** The legend line under the floor (`L`). */
  legend: boolean;
}

export const DEFAULT_PREFS: FloorPrefs = { list: false, traffic: true, legend: true };

export function parsePrefs(v: JsonValue | null | undefined): FloorPrefs {
  const out = { ...DEFAULT_PREFS };
  if (v && typeof v === 'object' && !Array.isArray(v)) {
    const o = v as Record<string, JsonValue | undefined>;
    if (typeof o.list === 'boolean') out.list = o.list;
    if (typeof o.traffic === 'boolean') out.traffic = o.traffic;
    if (typeof o.legend === 'boolean') out.legend = o.legend;
  }
  return out;
}

export function useFloorPrefs(): [FloorPrefs, (patch: Partial<FloorPrefs>) => void] {
  const raw = useAppStore((s) => s.ui.floorPrefs);
  const prefs = parsePrefs(raw);
  const set = useCallback((patch: Partial<FloorPrefs>) => {
    const s = useAppStore.getState();
    const next = { ...parsePrefs(s.ui.floorPrefs), ...patch };
    s.setFloorPrefs({ list: next.list, traffic: next.traffic, legend: next.legend });
  }, []);
  return [prefs, set];
}
