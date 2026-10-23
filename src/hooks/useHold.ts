/**
 * Hold-while-hovered and ghost rows (contract §8.6 [M15], T-ui-hold).
 *
 * The attention queue re-ranks live, and its rows carry answer buttons. So
 * while the pointer or keyboard focus is on a focus card or a list:
 *  - the focus card keeps its item even if the ranking changes (a new arrival
 *    waits its turn);
 *  - an item handled elsewhere leaves a "handled" ghost for 1.5 s instead of
 *    sliding the next item under the pointer — unless the user handled it here
 *    (their own action, marked within 1.5 s, leaves no ghost);
 *  - the rows of a hovered list keep their order; a removed row stays as a
 *    ghost while the list is engaged, and new rows are appended at the end.
 * Rust's queue stays authoritative for counts; this only decides what sits
 * under the user's hands. The pure functions below hold all the rules; the
 * hooks keep their state between renders.
 */

import { useCallback, useEffect, useLayoutEffect, useRef, useState } from 'react';
import type { FocusEvent as ReactFocusEvent } from 'react';

import { clock } from '../lib/clock';

export const GHOST_MS = 1500;
/** The user's own action counts as "handled here" for this long. */
export const OWN_ACTION_MS = 1500;

export interface Ghost {
  key: string;
  /** The name the item had, shown in the ghost ("…, handled"). */
  name: string;
  /** Clock time after which the ghost may go. */
  until: number;
}

export interface OwnAction {
  key: string;
  at: number;
}

function isOwn(own: OwnAction | null, key: string, now: number): boolean {
  return own !== null && own.key === key && now - own.at < OWN_ACTION_MS;
}

// ---------------------------------------------------------------------------
// Focus card
// ---------------------------------------------------------------------------

export interface FocusHoldState {
  focusKey: string | null;
  focusName: string;
  ghost: Ghost | null;
}

export const EMPTY_FOCUS_HOLD: FocusHoldState = { focusKey: null, focusName: '', ghost: null };

export interface FocusPick<T> {
  state: FocusHoldState;
  /** The item the card shows (null while a ghost is shown, or when nothing waits). */
  item: T | null;
  ghost: Ghost | null;
}

/**
 * Which item the focus card shows. `ranked` is the queue order, `engaged`
 * whether the pointer or focus is on the card.
 */
export function pickFocus<T>(
  prev: FocusHoldState,
  ranked: readonly T[],
  keyOf: (item: T) => string,
  nameOf: (item: T) => string,
  engaged: boolean,
  own: OwnAction | null,
  now: number,
): FocusPick<T> {
  const find = (key: string) => ranked.find((x) => keyOf(x) === key);

  if (prev.ghost) {
    const back = find(prev.ghost.key) !== undefined;
    if (engaged && now < prev.ghost.until && !back) {
      return { state: prev, item: null, ghost: prev.ghost };
    }
    // The ghost ends: hold the item again if it came back, else move on.
    prev = { focusKey: back ? prev.ghost.key : null, focusName: prev.focusName, ghost: null };
  }

  let item: T | null = ranked[0] ?? null;
  const held = prev.focusKey;
  if (held !== null && engaged) {
    const still = find(held);
    if (still !== undefined) {
      item = still;
    } else if (!isOwn(own, held, now)) {
      const ghost: Ghost = { key: held, name: prev.focusName, until: now + GHOST_MS };
      return { state: { ...prev, ghost }, item: null, ghost };
    }
  }

  const state: FocusHoldState = {
    focusKey: item === null ? null : keyOf(item),
    focusName: item === null ? '' : nameOf(item),
    ghost: null,
  };
  return { state, item, ghost: null };
}

// ---------------------------------------------------------------------------
// Lists
// ---------------------------------------------------------------------------

export type HeldRow<T> = { kind: 'item'; key: string; item: T } | { kind: 'ghost'; key: string; name: string };

export interface ListHoldState {
  order: readonly string[];
  names: ReadonlyMap<string, string>;
}

export const EMPTY_LIST_HOLD: ListHoldState = { order: [], names: new Map() };

export interface ListPick<T> {
  state: ListHoldState;
  rows: HeldRow<T>[];
}

/**
 * The rows a list shows. While engaged, the previous order holds: rows still
 * present stay in place, removed rows become ghosts (except the user's own),
 * new rows go to the end. Not engaged, the list is simply `items`.
 */
export function holdOrder<T>(
  prev: ListHoldState,
  items: readonly T[],
  keyOf: (item: T) => string,
  nameOf: (item: T) => string,
  engaged: boolean,
  own: OwnAction | null,
  now: number,
): ListPick<T> {
  const byKey = new Map<string, T>();
  for (const item of items) byKey.set(keyOf(item), item);
  const names = new Map(prev.names);
  for (const item of items) names.set(keyOf(item), nameOf(item));

  let rows: HeldRow<T>[];
  if (engaged && prev.order.length > 0) {
    rows = [];
    const placed = new Set<string>();
    for (const key of prev.order) {
      const item = byKey.get(key);
      if (item !== undefined) {
        rows.push({ kind: 'item', key, item });
        placed.add(key);
      } else if (!isOwn(own, key, now)) {
        rows.push({ kind: 'ghost', key, name: names.get(key) ?? key });
        placed.add(key);
      }
    }
    for (const item of items) {
      const key = keyOf(item);
      if (!placed.has(key)) rows.push({ kind: 'item', key, item });
    }
  } else {
    rows = items.map((item) => ({ kind: 'item' as const, key: keyOf(item), item }));
  }

  // Names of rows no longer shown are dropped so the map stays bounded.
  const shown = new Set(rows.map((r) => r.key));
  for (const key of [...names.keys()]) if (!shown.has(key)) names.delete(key);
  return { state: { order: rows.map((r) => r.key), names }, rows };
}

// ---------------------------------------------------------------------------
// Hooks
// ---------------------------------------------------------------------------

export interface Engagement {
  engaged: boolean;
  /** Spread on the container: pointer over it or focus inside it engages it. */
  props: {
    onMouseEnter: () => void;
    onMouseLeave: () => void;
    onFocus: () => void;
    onBlur: (event: ReactFocusEvent<HTMLElement>) => void;
  };
}

/** Pointer over or focus inside a container. */
export function useEngagement(): Engagement {
  const [hovered, setHovered] = useState(false);
  const [focused, setFocused] = useState(false);
  const props = {
    onMouseEnter: useCallback(() => setHovered(true), []),
    onMouseLeave: useCallback(() => setHovered(false), []),
    onFocus: useCallback(() => setFocused(true), []),
    onBlur: useCallback((event: ReactFocusEvent<HTMLElement>) => {
      const next = event.relatedTarget;
      if (next instanceof Node && event.currentTarget.contains(next)) return;
      setFocused(false);
    }, []),
  };
  return { engaged: hovered || focused, props };
}

function useOwnAction(): [OwnAction | null, (key: string) => void] {
  const [own, setOwn] = useState<OwnAction | null>(null);
  const mark = useCallback((key: string) => setOwn({ key, at: clock.now() }), []);
  return [own, mark];
}

/** Re-render at `until` (clock time), so a ghost leaves by itself. */
function useWakeAt(until: number | null): void {
  const [, setTick] = useState(0);
  useEffect(() => {
    if (until === null) return;
    const delay = Math.max(0, until - clock.now()) + 5;
    const timer = setTimeout(() => setTick((n) => n + 1), delay);
    return () => clearTimeout(timer);
  }, [until]);
}

export interface HeldFocus<T> {
  item: T | null;
  ghost: Ghost | null;
  /** Call when the user answers or dismisses an item here: it leaves no ghost. */
  markOwnAction: (key: string) => void;
}

/** The focus card's item (Home), held while `engaged`. */
export function useHeldFocus<T>(
  ranked: readonly T[],
  keyOf: (item: T) => string,
  nameOf: (item: T) => string,
  engaged: boolean,
): HeldFocus<T> {
  const stateRef = useRef<FocusHoldState>(EMPTY_FOCUS_HOLD);
  const [own, markOwnAction] = useOwnAction();
  const pick = pickFocus(stateRef.current, ranked, keyOf, nameOf, engaged, own, clock.now());
  useLayoutEffect(() => {
    stateRef.current = pick.state;
  });
  useWakeAt(pick.ghost ? pick.ghost.until : null);
  return { item: pick.item, ghost: pick.ghost, markOwnAction };
}

export interface HeldList<T> {
  rows: HeldRow<T>[];
  markOwnAction: (key: string) => void;
}

/** A list whose order holds while `engaged`. */
export function useHeldOrder<T>(
  items: readonly T[],
  keyOf: (item: T) => string,
  nameOf: (item: T) => string,
  engaged: boolean,
): HeldList<T> {
  const stateRef = useRef<ListHoldState>(EMPTY_LIST_HOLD);
  const [own, markOwnAction] = useOwnAction();
  const pick = holdOrder(stateRef.current, items, keyOf, nameOf, engaged, own, clock.now());
  useLayoutEffect(() => {
    stateRef.current = pick.state;
  });
  return { rows: pick.rows, markOwnAction };
}
