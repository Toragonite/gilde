/**
 * Ages of queue items that keep counting between queue updates.
 *
 * `AttentionItem.waitedMs` is the core's uptime-based age at publish time; the
 * UI adds its own monotonic time elapsed since that queue arrived (contract
 * §3.4.9), so "4분" becomes "5분" without a new patch and a sleeping Mac does
 * not inflate it. A slow tick (30 s) re-renders the surfaces that show ages —
 * minutes are the finest unit they display.
 */

import { useEffect, useState } from 'react';

import { clock } from '../lib/clock';
import type { AttentionItem } from '../lib/gen/AttentionItem';

export const AGE_TICK_MS = 30_000;

let lastRev = Number.NaN;
let receivedAt = 0;

/** When the queue with this `rev` first reached the WebView (monotonic). */
export function queueReceivedAt(rev: number): number {
  if (rev !== lastRev) {
    lastRev = rev;
    receivedAt = clock.now();
  }
  return receivedAt;
}

/** The item's age now. */
export function itemAge(item: AttentionItem, queueRev: number): number {
  return item.waitedMs + Math.max(0, clock.now() - queueReceivedAt(queueRev));
}

/** Re-render every `AGE_TICK_MS` while mounted. */
export function useAgeTick(): void {
  const [, setTick] = useState(0);
  useEffect(() => {
    const timer = setInterval(() => setTick((n) => n + 1), AGE_TICK_MS);
    return () => clearInterval(timer);
  }, []);
}

/** Tests only: forget the last queue receipt. */
export function resetAgesForTests(): void {
  lastRev = Number.NaN;
  receivedAt = 0;
}
