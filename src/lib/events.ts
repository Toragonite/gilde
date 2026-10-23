/**
 * The four global events (contract §4.3), on top of `gen/events.ts`.
 *
 * Every subscription lives in the one event bridge (`hooks/useEventBridge.ts`),
 * mounted once; components never listen on their own. In showcase mode (and in
 * tests that turn it on) events come from a local bus that `emitLocalEvent`
 * feeds, because there is no Tauri host to send them.
 */

import type { UnlistenFn } from '@tauri-apps/api/event';

import * as gen from './gen/events';
import type { FocusRequestEvent } from './gen/FocusRequestEvent';
import type { QuitRequestedEvent } from './gen/QuitRequestedEvent';
import type { ToastEvent } from './gen/ToastEvent';
import type { VisibilityEvent } from './gen/VisibilityEvent';
import { showcaseMode } from './mode';

export const EVENT_NAMES = gen.EVENT_NAMES;

/** Payload type of each event, by its short name. */
export interface GildeEvents {
  focusRequest: FocusRequestEvent;
  quitRequested: QuitRequestedEvent;
  toast: ToastEvent;
  visibility: VisibilityEvent;
}

export type GildeEventName = keyof GildeEvents;

export type GildeEventHandler<K extends GildeEventName> = (payload: GildeEvents[K]) => void;

const TAURI: { readonly [K in GildeEventName]: (h: GildeEventHandler<K>) => Promise<UnlistenFn> } = {
  focusRequest: gen.onFocusRequest,
  quitRequested: gen.onQuitRequested,
  toast: gen.onToast,
  visibility: gen.onVisibility,
};

const local = new Map<GildeEventName, Set<(payload: never) => void>>();

/** Deliver an event on the local bus (showcase mode and tests). */
export function emitLocalEvent<K extends GildeEventName>(name: K, payload: GildeEvents[K]): void {
  const handlers = local.get(name);
  if (!handlers) return;
  for (const h of [...handlers]) (h as GildeEventHandler<K>)(payload);
}

/** Subscribe to one event. */
export function listenEvent<K extends GildeEventName>(name: K, handler: GildeEventHandler<K>): Promise<UnlistenFn> {
  if (showcaseMode()) {
    let set = local.get(name);
    if (!set) {
      set = new Set();
      local.set(name, set);
    }
    const h = handler as (payload: never) => void;
    set.add(h);
    return Promise.resolve(() => {
      set.delete(h);
    });
  }
  return TAURI[name](handler);
}

/**
 * Subscribe to several events and get one teardown that also releases
 * registrations resolving after the teardown ran (the StrictMode leak).
 * A registration that fails (no runtime, window closing) makes that event
 * silent instead of an unhandled rejection.
 */
export function listenAll(
  register: (add: <K extends GildeEventName>(name: K, handler: GildeEventHandler<K>) => void) => void,
): () => void {
  const pending: Promise<UnlistenFn>[] = [];
  let disposed = false;
  const noop: UnlistenFn = () => {};
  register((name, handler) => {
    pending.push(
      listenEvent(name, handler).catch(() => {
        console.warn('[gilde] event subscription failed', name);
        return noop;
      }),
    );
  });
  void Promise.all(pending).then((fns) => {
    if (disposed) for (const fn of fns) fn();
  });
  return () => {
    disposed = true;
    void Promise.all(pending).then((fns) => {
      for (const fn of fns) fn();
    });
  };
}
