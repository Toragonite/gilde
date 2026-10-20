/**
 * Feature flags — what the build shows.
 *
 * The first public preview of Gilde is a read-only cockpit over the Claude Code
 * Orchestrator: the Team view (Teammate mode) and the Worker accounts view.
 * Everything else built during the experiments — Gilde's own terminal sessions,
 * the dispatch board, the status graph, the process monitor and the bottom
 * drawer — is behind ONE flag. The code stays and its tests keep running; with
 * the flag off nothing of it is reachable or visible.
 *
 *   VITE_GILDE_EXPERIMENTAL_LANES=1 npm run tauri dev   → every lane
 *   (unset)                                             → the preview
 *
 * Gate at registration points (the register modules, the rail list, the
 * hotkey and palette lists, the settings sections), not in leaf components.
 */

import type { ViewKey } from './store/uiSlice';

/**
 * Build-time switch: `VITE_GILDE_SHOWCASE=1` builds the SHOWCASE — the real UI,
 * fed neutral demo data, running in a plain browser with no Tauri runtime, for
 * the landing page's screenshots (`site/README.md` § Screenshots). Off by
 * default. The demo code (`src/showcase/`) is loaded only through a dynamic
 * `import()` guarded by this constant (`main.tsx`), so a normal build drops it;
 * `scripts/check-showcase-tree-shaking.mjs` proves that. The IPC boundary
 * (`lib/commands.ts` `call`, `lib/events.ts` `listenAo`) never reaches Tauri
 * while it is on. A showcase is always the preview: the lanes stay off.
 */
export const SHOWCASE: boolean = import.meta.env.VITE_GILDE_SHOWCASE === '1';

/** Build-time switch: `VITE_GILDE_EXPERIMENTAL_LANES=1` turns the experimental lanes on (never in a showcase). */
export const EXPERIMENTAL_LANES: boolean = import.meta.env.VITE_GILDE_EXPERIMENTAL_LANES === '1' && !SHOWCASE;

let showcaseOverride: boolean | null = null;

/**
 * Whether the IPC boundary answers from the showcase instead of Tauri. Equal to
 * `SHOWCASE` except inside the unit tests, which turn it on to drive the scenes.
 */
export function showcaseMode(): boolean {
  return showcaseOverride ?? SHOWCASE;
}

/** Tests only: force showcase mode on or off; `null` restores the build value. */
export function setShowcaseForTests(value: boolean | null): void {
  showcaseOverride = value;
}

let override: boolean | null = null;

/**
 * The flag as the running code should read it. Equal to `EXPERIMENTAL_LANES`
 * except inside the unit tests, which flip it to cover both builds.
 */
export function experimentalLanes(): boolean {
  return override ?? EXPERIMENTAL_LANES;
}

/** Tests only: force the flag on or off; `null` restores the build value. */
export function setExperimentalLanesForTests(value: boolean | null): void {
  override = value;
}

/** The two views of the preview, in rail order. Always present. */
export const PREVIEW_VIEWS: readonly ViewKey[] = ['team', 'workers'];

/** The experimental lanes, in rail order after the preview views. */
export const EXPERIMENTAL_VIEWS: readonly ViewKey[] = ['sessions', 'dispatch', 'graph', 'monitor'];

/** The rail, top to bottom, for the current build. */
export function railViews(): readonly ViewKey[] {
  return experimentalLanes() ? [...PREVIEW_VIEWS, ...EXPERIMENTAL_VIEWS] : PREVIEW_VIEWS;
}

/** Whether a view can be reached in the current build. */
export function isViewEnabled(view: string): view is ViewKey {
  return (railViews() as readonly string[]).includes(view);
}

/** Pane views (tab descriptors) the current build may show. */
export function isPaneViewEnabled(view: string): boolean {
  if (view === 'team' || view === 'workers') return true;
  return experimentalLanes() && ['terminal', 'dispatch', 'graph', 'monitor'].includes(view);
}
