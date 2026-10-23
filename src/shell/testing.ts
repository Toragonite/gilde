/**
 * Test helpers for the shell, sidebar, Home and palette tests (imported by
 * `*.test.tsx` files only, so never part of the app bundle).
 */

import { act } from '@testing-library/react';

import { resetAnswerGuardForTests, refreshGuards } from '../hooks/useAnswerGuard';
import { resetHotkeysForTests } from '../hooks/useHotkeys';
import { manualClock, setClockForTests } from '../lib/clock';
import type { ManualClock } from '../lib/clock';
import type { StoreSnapshot } from '../lib/gen/StoreSnapshot';
import { initialUiState } from '../store/uiSlice';
import type { UiState } from '../store/uiSlice';
import { appStore } from '../store/useAppStore';
import { emptyWorld } from '../store/worldSlice';
import { resetFakeCore } from '../test/backend';
import { resetAgesForTests } from './useAges';
import { setSlotsForTests } from './slots';

/** Put the one store back to its first state (data only; the actions stay bound). */
export function resetStore(ui: Partial<UiState> = {}): void {
  appStore.setState({ world: emptyWorld(), ui: { ...initialUiState(), locale: 'en', hydrated: true, ...ui }, toasts: [] });
}

/** Seed the World mirror from a snapshot. */
export function seed(snapshot: StoreSnapshot): void {
  act(() => {
    appStore.getState().applyStoreSnapshot(snapshot);
  });
}

/** Run a DOM test with a manual clock and a clean store, hotkeys and guards. */
export function scenario(run: (c: ManualClock) => void | Promise<void>, ui: Partial<UiState> = {}) {
  return async () => {
    const c = manualClock(1_000_000);
    setClockForTests(c);
    resetAnswerGuardForTests();
    resetAgesForTests();
    resetHotkeysForTests();
    resetStore(ui);
    try {
      await run(c);
    } finally {
      setClockForTests(null);
      resetAnswerGuardForTests();
      resetHotkeysForTests();
      resetFakeCore();
      setSlotsForTests(null);
      resetStore();
    }
  };
}

/** Move the manual clock and let guards and holds re-evaluate. */
export function step(c: ManualClock, ms: number): void {
  act(() => {
    c.advance(ms);
    refreshGuards();
  });
}

/** A keydown on `target` (or the active element), the way the capture-phase hotkey layer sees it. */
export function press(
  code: string,
  init: { meta?: boolean; shift?: boolean; alt?: boolean; repeat?: boolean; isComposing?: boolean; target?: Element | null } = {},
): KeyboardEvent {
  const target = init.target ?? document.activeElement ?? document.body;
  const key = code.startsWith('Key') ? code.slice(3).toLowerCase() : code.startsWith('Digit') ? code.slice(5) : code;
  const event = new KeyboardEvent('keydown', {
    code,
    key,
    metaKey: init.meta ?? false,
    shiftKey: init.shift ?? false,
    altKey: init.alt ?? false,
    repeat: init.repeat ?? false,
    isComposing: init.isComposing ?? false,
    bubbles: true,
    cancelable: true,
  });
  act(() => {
    target.dispatchEvent(event);
  });
  return event;
}
