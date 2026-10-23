/**
 * The event bridge — mounted exactly once by the app root. It is the only place
 * that listens to `gilde://` events and the only place that starts the
 * long-lived links to the core:
 *
 *  1. `app_bootstrap` → locale, theme, launch kind, persisted UI state; the
 *     build-id check of §4.7 (reload once, then "Restart Gilde to finish
 *     updating.");
 *  2. `store_subscribe` → the World mirror (`worldSlice`), resubscribing on any
 *     rev gap or Resync;
 *  3. the four global events (§4.3): focus-request (opens the target and
 *     starts the 700 ms answer guard, F-30), quit-requested, toast, visibility
 *     (window activation also starts the guard);
 *  4. the reporters: `app_view_state` on every change (F-04), `ui_state_set`
 *     for the persisted keys, `app_input_activity` at most once per 2 s while
 *     the user types (notification rule 5, carries no content), and the
 *     `app_mark` boot marks.
 *
 * `startEventBridge` is the plain function (tests drive it); `useEventBridge`
 * mounts it in React.
 */

import { useEffect } from 'react';

import { clock } from '../lib/clock';
import { commands } from '../lib/commands';
import { listenAll } from '../lib/events';
import { showcaseMode } from '../lib/mode';
import type { FocusTarget } from '../lib/gen/FocusTarget';
import type { IpcError } from '../lib/ipcError';
import { startUiPersistence } from '../store/persist';
import type { AppStore } from '../store/useAppStore';
import { appStore } from '../store/useAppStore';
import { startViewStateReporter } from '../store/viewState';
import { noteFocusRequest, noteWindowActivation } from './useAnswerGuard';
import { isTextEntryTarget } from './useHotkeys';

type Store = typeof appStore;

/**
 * The WebView's own build id (§4.7, A8.1), defined by vite.config.ts on every
 * build. A bundle without it (never produced by our config) reads as `null`,
 * which can never equal the core's id, so the check below then fails loudly.
 */
export function embeddedBuildId(): string | null {
  const id = import.meta.env.VITE_GILDE_BUILD_ID as string | undefined;
  return typeof id === 'string' && id !== '' ? id : null;
}

const RELOADED_FLAG = 'gilde.buildReloaded';

/** `app_input_activity` at most once per this many ms. */
export const INPUT_ACTIVITY_MS = 2000;

function readFlag(): boolean {
  try {
    return globalThis.sessionStorage?.getItem(RELOADED_FLAG) === '1';
  } catch {
    return false;
  }
}

function writeFlag(on: boolean): void {
  try {
    if (on) globalThis.sessionStorage?.setItem(RELOADED_FLAG, '1');
    else globalThis.sessionStorage?.removeItem(RELOADED_FLAG);
  } catch {
    // Storage unavailable: the mismatch line shows instead of a second reload.
  }
}

/** Open what a `gilde://focus-request` names. */
export function applyFocusTarget(store: Store, target: FocusTarget): void {
  const s = store.getState();
  switch (target.kind) {
    case 'session':
      s.openSession(target.uid, { focusCard: target.focusCard });
      break;
    case 'home':
      s.navigate({ screen: 'home' });
      break;
    case 'team':
      s.navigate({ screen: 'team', teamKey: target.teamKey, mode: target.mode });
      break;
    case 'floor':
      s.navigate({ screen: 'floor' });
      break;
  }
}

export interface BridgeOptions {
  /** Reload the WebView (the build-id rule); tests pass a spy. */
  reload?: () => void;
  /** The WebView's build id; defaults to the embedded one (tests override it). */
  buildId?: string | null;
  /**
   * Whether to run the build-id check. Default: on every app boot, off only in
   * showcase mode (there is no core to compare with). Tests that drive the
   * bridge through the showcase boundary turn it on explicitly.
   */
  checkBuild?: boolean;
}

/** Start everything; returns the stop function. Safe to call once per window. */
export function startEventBridge(store: Store = appStore, options: BridgeOptions = {}): () => void {
  const stops: (() => void)[] = [];
  let stopped = false;
  const reload = options.reload ?? (() => globalThis.location?.reload());

  // 1. Bootstrap, then the world.
  void commands.app_bootstrap({}).then(
    (res) => {
      if (stopped) return;
      const check = options.checkBuild ?? !showcaseMode();
      const mine = options.buildId === undefined ? embeddedBuildId() : options.buildId;
      if (check && res.buildId !== mine) {
        if (!readFlag()) {
          writeFlag(true);
          reload();
          return;
        }
        store.getState().setBuildMismatch(true);
      } else {
        writeFlag(false);
      }
      store.getState().hydrateFromBootstrap(res);
      stops.push(
        startUiPersistence(
          (listener) => store.subscribe((s) => s.ui, listener),
          store.getState().ui,
        ),
      );
    },
    (error: IpcError) => {
      // Bootstrap never fails after B4 (§4.2.1); before that, keep the
      // defaults and let the world subscription show what the core can do.
      void error;
    },
  );
  void store
    .getState()
    .subscribeWorld()
    .then(() => {
      if (stopped || store.getState().world.status !== 'live') return;
      void commands.app_mark({ mark: 'interactive', perfMs: Math.min(600000, Math.round(clock.now())) }).catch(() => {});
    });

  // 2. The four events.
  stops.push(
    listenAll((add) => {
      add('focusRequest', (event) => {
        noteFocusRequest();
        applyFocusTarget(store, event.target);
      });
      add('quitRequested', () => store.getState().setQuitRequested(true));
      add('toast', (event) => {
        store.getState().pushToastEvent(event);
      });
      add('visibility', (event) => {
        const before = store.getState().ui.visibility;
        store.getState().setVisibility(event);
        if (event.frontmost && !before.frontmost) noteWindowActivation();
      });
    }),
  );

  // 3. Reporters.
  stops.push(
    startViewStateReporter(
      (listener) =>
        store.subscribe(
          (s: AppStore) => [s.ui, s.world] as const,
          ([ui, world]) => listener(ui, world),
          { equalityFn: (a, b) => a[0] === b[0] && a[1] === b[1] },
        ),
      { ui: store.getState().ui, world: store.getState().world },
    ),
  );

  if (typeof document !== 'undefined') {
    const onVisibility = () => store.getState().setDocumentVisible(document.visibilityState !== 'hidden');
    document.addEventListener('visibilitychange', onVisibility);
    stops.push(() => document.removeEventListener('visibilitychange', onVisibility));
    onVisibility();

    let lastActivity = -Infinity;
    const onInput = (event: Event) => {
      if (!isTextEntryTarget(event.target)) return;
      const now = clock.now();
      if (now - lastActivity < INPUT_ACTIVITY_MS) return;
      lastActivity = now;
      void commands.app_input_activity({}).catch(() => {});
    };
    document.addEventListener('input', onInput, { capture: true });
    stops.push(() => document.removeEventListener('input', onInput, { capture: true }));
  }

  if (typeof requestAnimationFrame === 'function') {
    requestAnimationFrame(() => {
      if (stopped) return;
      void commands.app_mark({ mark: 'firstPaint', perfMs: Math.min(600000, Math.round(clock.now())) }).catch(() => {});
    });
  }

  return () => {
    stopped = true;
    for (const stop of stops.splice(0).reverse()) stop();
    store.getState().unsubscribeWorld();
  };
}

/** Mount the bridge once (the app root). */
export function useEventBridge(): void {
  useEffect(() => startEventBridge(appStore), []);
}
