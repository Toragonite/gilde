/**
 * Adding a sidebar session to the Split (ux-ia §3.8 step 1: "⌘D on a focused
 * row or an open session, or drag a sidebar row onto Main"; KEYMAP `split.add`,
 * scope L; contract A30.5). T32.
 *
 *  - ⌘D on a focused session row (the project tree and Needs you) calls
 *    `addToSplitOrAsk`, so a full Split asks which tile to replace. On any
 *    other row (a project header, a team row, "n ended today") ⌘D does
 *    nothing — and it is still consumed, so it never falls through to the
 *    open Session screen's own ⌘D. Focus stays on the row.
 *  - Session rows are draggable (native drag and drop, a Gilde-only MIME type
 *    carrying the uid and nothing else); other rows are not. Dropping on Main
 *    adds the session the same way; on any screen but Split it then opens the
 *    Split. The keyboard path is the accessible equivalent.
 */

import { useCallback, useEffect, useState } from 'react';
import type { DragEvent as ReactDragEvent, RefObject } from 'react';

import { useHotkeys } from '../hooks/useHotkeys';
import type { SessionUid } from '../lib/gen/SessionUid';
import { addToSplitOrAsk } from '../split/replace';
import { appStore } from '../store/useAppStore';
import { sessionOf } from '../store/worldSlice';

/** The drag payload type: only Gilde's own rows carry it. */
export const SESSION_DRAG_TYPE = 'application/x-gilde-session';

/** The attribute a session row carries so ⌘D knows which session it is. */
export const SPLIT_UID_ATTR = 'data-split-uid';

const UID = /^s_[0-9a-f-]{36}$/;

/** A uid the World holds (a stale or foreign payload is ignored). */
function knownSession(raw: string | null | undefined): SessionUid | null {
  if (!raw || !UID.test(raw)) return null;
  const s = sessionOf(appStore.getState().world, raw as SessionUid);
  return s ? s.uid : null;
}

/** Add a sidebar session to the Split. `open` also shows the Split (a drop outside it). */
export function addSessionToSplit(uid: SessionUid, open: boolean): void {
  const outcome = addToSplitOrAsk(uid);
  // `ask` has already opened the Split with the replace choice.
  if (open && outcome !== 'ask' && appStore.getState().ui.view.screen !== 'split') {
    appStore.getState().navigate({ screen: 'split' });
  }
}

/**
 * ⌘D (scope L) on the rows inside `list`: a session row adds its session; any
 * other row consumes the key and does nothing.
 */
export function useSplitAddKey(list: RefObject<HTMLElement | null>): void {
  useHotkeys(
    [
      {
        id: 'split.add',
        combo: 'mod+d',
        run: (event) => {
          const target = event.target instanceof Element ? event.target : null;
          const row = target?.closest(`[${SPLIT_UID_ATTR}]`) ?? null;
          const uid = knownSession(row?.getAttribute(SPLIT_UID_ATTR));
          if (uid) addSessionToSplit(uid, false);
        },
      },
    ],
    { scope: 'L', element: list, requireFocus: true },
  );
}

/** Props that make a session row draggable onto Main. */
export function sessionDragProps(uid: SessionUid) {
  return {
    [SPLIT_UID_ATTR]: uid,
    draggable: true,
    onDragStart: (event: ReactDragEvent<HTMLElement>) => {
      event.dataTransfer.setData(SESSION_DRAG_TYPE, uid);
      event.dataTransfer.effectAllowed = 'copy';
    },
  } as const;
}

function carriesSession(event: ReactDragEvent<HTMLElement>): boolean {
  const types = event.dataTransfer?.types;
  return !!types && Array.from(types).includes(SESSION_DRAG_TYPE);
}

/**
 * Main as a drop target for sidebar session rows: the handlers to spread on
 * the `main` element and whether a session is over it (the drop affordance).
 */
export function useMainDrop() {
  const [over, setOver] = useState(false);

  // A drag that ends anywhere (Esc, a drop outside the window) clears the affordance.
  useEffect(() => {
    if (!over) return;
    const clear = () => setOver(false);
    window.addEventListener('dragend', clear);
    window.addEventListener('drop', clear);
    return () => {
      window.removeEventListener('dragend', clear);
      window.removeEventListener('drop', clear);
    };
  }, [over]);

  const onDragOver = useCallback((event: ReactDragEvent<HTMLElement>) => {
    if (!carriesSession(event)) return;
    event.preventDefault();
    event.dataTransfer.dropEffect = 'copy';
    setOver(true);
  }, []);

  const onDragLeave = useCallback((event: ReactDragEvent<HTMLElement>) => {
    const next = event.relatedTarget;
    if (next instanceof Node && event.currentTarget.contains(next)) return;
    setOver(false);
  }, []);

  const onDrop = useCallback((event: ReactDragEvent<HTMLElement>) => {
    setOver(false);
    if (!carriesSession(event)) return;
    event.preventDefault();
    const uid = knownSession(event.dataTransfer.getData(SESSION_DRAG_TYPE));
    if (uid) addSessionToSplit(uid, true);
  }, []);

  return { over, props: { onDragOver, onDragEnter: onDragOver, onDragLeave, onDrop } };
}
