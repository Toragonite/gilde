/**
 * Typed `listen()` helpers.
 *
 * DESIGN §9-3 rule 3: **every** `gilde://` subscription lives in `useEventBridge()`,
 * mounted exactly once by `App.tsx`. Components never subscribe on their own, so
 * there is exactly one listener per event and exactly one place that unsubscribes.
 * `listenAo` is the only wrapper around Tauri's `listen`, and `hooks/useEventBridge.ts`
 * is its only caller — `grep -rn "listenAo(" src` proves the rule holds.
 */

import { listen } from '@tauri-apps/api/event';
import type { UnlistenFn } from '@tauri-apps/api/event';

import { showcaseMode } from '../features';
import type { GildeEventName, GildeEventPayload } from './ipc';

export type GildeEventHandler<K extends GildeEventName> = (payload: GildeEventPayload<K>) => void;

/**
 * Subscribe to one `gilde://` event with its payload type bound to its name.
 * In showcase mode (`src/features.ts`) there is no Tauri host to send events:
 * nothing is subscribed and the teardown is a no-op.
 */
export function listenAo<K extends GildeEventName>(
  name: K,
  handler: GildeEventHandler<K>,
): Promise<UnlistenFn> {
  if (showcaseMode()) return Promise.resolve(() => {});
  return listen<GildeEventPayload<K>>(name, (event) => handler(event.payload));
}

/**
 * Subscribe to several events at once and get a single teardown that awaits
 * every pending registration — including ones that resolve after the component
 * already unmounted, which is what leaks listeners in StrictMode otherwise.
 */
export function listenAll(
  register: (add: <K extends GildeEventName>(name: K, handler: GildeEventHandler<K>) => void) => void,
): () => void {
  const pending: Promise<UnlistenFn>[] = [];
  let disposed = false;

  const noop: UnlistenFn = () => {};
  register((name, handler) => {
    // A registration that fails (no Tauri host, window torn down) must not
    // surface as an unhandled rejection; it degrades to "this event is silent".
    pending.push(
      listenAo(name, handler).catch((e) => {
        console.warn('[gilde] event subscription failed', name, e);
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
