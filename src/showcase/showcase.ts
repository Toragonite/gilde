/**
 * The showcase — the real UI on the demo backend, in a plain browser
 * (`VITE_GILDE_SHOWCASE=1 npx vite`, then `?scene=…`; see `scenes.ts`).
 *
 * `main.tsx` loads this module only through its guarded dynamic import and
 * awaits `startShowcase` before the first paint. Nothing here renders: it
 * builds the scene's world, installs the demo backend at the IPC boundary
 * (`installShowcaseBackend`, `lib/commands.ts`), and lets the app boot exactly
 * as it does against the core — `app_bootstrap` restores the scene's view from
 * `uiState`, `store_subscribe` delivers the snapshot and then the patch
 * script. Once the app has hydrated and the World is live, the scene's sheet
 * opens (with the setters a click would use) and the page is marked ready.
 *
 * The page hook `window.__gildeShowcase` is what the browser gates
 * (`scripts/gates/browser-gates.mjs`) read: `ready`, `advanceTo(seconds)` to
 * freeze the world at a sample time, and the marker.
 */

import { installShowcaseBackend } from '../lib/commands';
import type { CommandName } from '../lib/gen/commands';
import type { JsonValue } from '../lib/gen/serde_json/JsonValue';
import type { QuitRecordId } from '../lib/gen/QuitRecordId';
import { useAppStore } from '../store/useAppStore';
import { SHOWCASE_MARKER, createShowcaseBackend } from './backend';
import type { ShowcaseBackendHandle } from './backend';
import { SCENES, parseShowcaseQuery } from './scenes';
import type { SceneName, ShowcaseOptions } from './scenes';
import { SAMPLE_TIMES_S, buildTimeline } from './timeline';
import { buildWorld } from './world';
import type { ShowcaseWorld } from './world';

export { SHOWCASE_MARKER };
export type { ShowcaseOptions };

/** What the gates read from the page. */
export interface ShowcaseHook {
  marker: string;
  scene: SceneName;
  /** The app hydrated, the World is live and the scene's sheet is open. */
  ready: boolean;
  /** Deliver the patch script up to `seconds`; returns how many steps have run. */
  advanceTo: (seconds: number) => number;
  sampleTimes: readonly number[];
  /** Commands the app asked so far. */
  asked: () => readonly CommandName[];
}

export interface SeededShowcase {
  options: ShowcaseOptions;
  world: ShowcaseWorld;
  handle: ShowcaseBackendHandle;
  /** The persisted UI keys `app_bootstrap` hands the app. */
  uiState: Record<string, JsonValue>;
}

/** The quit record of the `home-notice` scene (`r_<UUIDv7>`). */
const QUIT_RECORD = 'r_0192f000-0000-7000-8000-000000000001' as QuitRecordId;

/** The embedded build id (A8.1), echoed by the demo `app_bootstrap`. */
function buildId(): string {
  const id = import.meta.env.VITE_GILDE_BUILD_ID as string | undefined;
  return typeof id === 'string' && id !== '' ? id : 'showcase';
}

/** Build the scene's world and backend and install it. `now` anchors every time in the data. */
export function seedShowcase(options: ShowcaseOptions, now: number = Date.now()): SeededShowcase {
  const spec = SCENES[options.scene];
  const world = buildWorld(options.world, now);
  const steps = buildTimeline(world);
  const uiState: Record<string, JsonValue> = {
    lastView: spec.view(world.roles),
    homeMode: 'focus',
  };
  if (spec.split > 0) {
    const uids = world.roles.split.slice(0, spec.split);
    uiState.splitSet = { uids, focused: uids[0] ?? null };
  }
  if (spec.teamMode && world.roles.team) {
    uiState[`teamLayout:${world.roles.team}`] = { mode: spec.teamMode, focused: null, zoomed: null };
  }
  const handle = createShowcaseBackend(world, steps, {
    locale: options.locale,
    theme: options.theme,
    startAt: Math.round(options.t * 1000),
    live: options.live,
    uiState,
    launch: spec.notice ? { kind: 'afterQuit', recordId: QUIT_RECORD, count: 2 } : { kind: 'restore' },
    buildId: buildId(),
  });
  installShowcaseBackend(handle.backend);
  return { options, world, handle, uiState };
}

/** Open the scene's sheet once the app hydrated and the World is live; then mark the page ready. */
export function whenReady(seeded: SeededShowcase, store: typeof useAppStore, onReady: () => void): () => void {
  const spec = SCENES[seeded.options.scene];
  let done = false;
  let unsubscribe: () => void = () => {};
  const check = () => {
    const s = store.getState();
    if (done || !s.ui.hydrated || s.world.status !== 'live') return;
    done = true;
    unsubscribe();
    if (spec.sheet) s.openSheet(spec.sheet);
    if (spec.quit) s.setQuitRequested(true);
    onReady();
  };
  unsubscribe = store.subscribe(check);
  check();
  return () => unsubscribe();
}

/** The showcase build's start: the scene the URL names. */
export async function startShowcase(search: string): Promise<void> {
  const seeded = seedShowcase(parseShowcaseQuery(search));
  const hook: ShowcaseHook = {
    marker: SHOWCASE_MARKER,
    scene: seeded.options.scene,
    ready: false,
    advanceTo: (seconds) => seeded.handle.advanceTo(Math.round(seconds * 1000)),
    sampleTimes: SAMPLE_TIMES_S,
    asked: () => seeded.handle.asked,
  };
  (globalThis as { __gildeShowcase?: ShowcaseHook }).__gildeShowcase = hook;
  whenReady(seeded, useAppStore, () => {
    hook.ready = true;
    globalThis.document?.documentElement.setAttribute('data-showcase-ready', seeded.options.scene);
  });
}
