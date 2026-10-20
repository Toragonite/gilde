/**
 * Test helpers for the two builds (`src/features.ts`).
 *
 * The unit-test bundle is built without `VITE_GILDE_EXPERIMENTAL_LANES`, so by
 * default every test sees the PREVIEW. The suites of the experimental lanes
 * (terminal, sidebar tree, dispatch, monitor, graph) keep running — they
 * register with `laneTest`, which turns the lanes on for that one test.
 */

import { setExperimentalLanesForTests } from '../features';
import { initialUiState } from '../store/uiSlice';
import type { UiState } from '../store/uiSlice';
import { test } from './harness';

/** Run `body` with the lanes forced on (or off), then restore the build value. */
export async function withLanes<T>(on: boolean, body: () => T | Promise<T>): Promise<T> {
  setExperimentalLanesForTests(on);
  try {
    return await body();
  } finally {
    setExperimentalLanesForTests(null);
  }
}

/** A test that runs with the experimental lanes on. */
export function laneTest(name: string, run: () => void | Promise<void>): void {
  test(name, () => withLanes(true, run));
}

/** A test that runs as the preview build (lanes off, whatever the build says). */
export function previewTest(name: string, run: () => void | Promise<void>): void {
  test(name, () => withLanes(false, run));
}

/** The UI state an experimental-lanes session starts from: the Sessions view, inspector open. */
export function laneUiState(): UiState {
  return { ...initialUiState(), activeView: 'sessions', inspectorOpen: true };
}
