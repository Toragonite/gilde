/**
 * The clock the UI's timing rules read (the 700 ms guard, holds, throttles).
 * Tests replace it to step time without waiting.
 */

export interface Clock {
  /** Monotonic milliseconds (for durations). */
  now(): number;
  /** Wall-clock epoch milliseconds (for "since" and dates). */
  wall(): number;
}

const systemClock: Clock = {
  now: () => (typeof performance !== 'undefined' ? performance.now() : Date.now()),
  wall: () => Date.now(),
};

let current: Clock = systemClock;

export const clock: Clock = {
  now: () => current.now(),
  wall: () => current.wall(),
};

/** A clock that only moves when told to (tests). */
export interface ManualClock extends Clock {
  advance(ms: number): void;
  set(ms: number): void;
}

export function manualClock(start = 1_000_000): ManualClock {
  let t = start;
  return {
    now: () => t,
    wall: () => t,
    advance: (ms) => {
      t += ms;
    },
    set: (ms) => {
      t = ms;
    },
  };
}

/** Tests only: install a clock; `null` restores the system clock. */
export function setClockForTests(next: Clock | null): void {
  current = next ?? systemClock;
}
