/**
 * Health — what the Accounts & health screen and the sidebar footer summarise
 * (ux-ia §3.15, §2.4 footer). Health alerts and notifications are Rust's now
 * (contract §2.2.4); the WebView keeps only its popover state and derives the
 * figures from the World mirror, so every surface shows the same numbers.
 */

import type { StateCreator } from 'zustand';

import type { SourceStatus } from '../lib/gen/SourceStatus';
import type { WorldState } from './worldSlice';

export interface HealthState {
  popoverOpen: boolean;
}

export interface HealthSlice {
  health: HealthState;
  setHealthPopover: (open: boolean) => void;
}

export const createHealthSlice: StateCreator<HealthSlice, [['zustand/subscribeWithSelector', never]], [], HealthSlice> = (
  set,
) => ({
  health: { popoverOpen: false },
  setHealthPopover: (open) => set({ health: { popoverOpen: open } }),
});

/** Leftover processes (§3.4.10 `leftover`): count and resident bytes. */
export function leftoverSummary(world: WorldState): { count: number; bytes: number } {
  let count = 0;
  let bytes = 0;
  for (const p of world.leftovers.values()) {
    if (p.leftover === null) continue;
    count += 1;
    bytes += p.rssBytes;
  }
  return { count, bytes };
}

/** Memory of the agents shown (sum of known session memory; unknown stays out, never 0-filled). */
export function agentMemory(world: WorldState): { bytes: number; known: number; unknown: number } {
  let bytes = 0;
  let known = 0;
  let unknown = 0;
  for (const s of world.sessions.values()) {
    if (s.control.kind === 'notRunning') continue;
    if (s.memoryBytes === null) unknown += 1;
    else {
      bytes += s.memoryBytes;
      known += 1;
    }
  }
  return { bytes, known, unknown };
}

/** Running sessions (any control other than NotRunning). */
export function runningCount(world: WorldState): number {
  let n = 0;
  for (const s of world.sessions.values()) if (s.control.kind !== 'notRunning') n += 1;
  return n;
}

/** Discovery sources macOS denied (drives the TCC notice, §5.8). */
export function deniedSources(world: WorldState): SourceStatus[] {
  return world.sources.filter((s) => s.state.kind === 'permissionDenied');
}
