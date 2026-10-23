/**
 * The test harness.
 *
 * The dependency set has no test runner, so tests are plain functions collected
 * here and executed by `src/test/run.mjs` (which bundles them with the Vite that
 * is already installed). Assertions throw; the runner reports and sets the exit
 * code. `domTest` adds the DOM cleanup that component tests need, `property`
 * runs a seeded property check (no extra dependency).
 */

import { cleanup } from '@testing-library/react';

export interface TestCase {
  name: string;
  run: () => void | Promise<void>;
}

const cases: TestCase[] = [];

export function test(name: string, run: () => void | Promise<void>): void {
  cases.push({ name, run });
}

/**
 * A test that renders into the shared happy-dom document. The rendered trees
 * are unmounted and the body emptied afterwards, pass or fail, so one test's
 * DOM can never leak into the next.
 */
export function domTest(name: string, run: () => void | Promise<void>): void {
  test(name, async () => {
    try {
      await run();
    } finally {
      cleanup();
      document.body.replaceChildren();
    }
  });
}

export function assert(condition: unknown, message: string): asserts condition {
  if (!condition) throw new Error(`assertion failed: ${message}`);
}

export function assertEqual<T>(actual: T, expected: T, message: string): void {
  if (!Object.is(actual, expected)) {
    throw new Error(`${message}\n  expected: ${String(expected)}\n  actual:   ${String(actual)}`);
  }
}

export function assertDeepEqual(actual: unknown, expected: unknown, message: string): void {
  const a = JSON.stringify(actual);
  const b = JSON.stringify(expected);
  if (a !== b) throw new Error(`${message}\n  expected: ${b}\n  actual:   ${a}`);
}

export function assertMatch(actual: string, pattern: RegExp, message: string): void {
  if (!pattern.test(actual)) {
    throw new Error(`${message}\n  pattern: ${pattern}\n  actual:  ${actual}`);
  }
}

export async function assertRejects(run: () => Promise<unknown>, message: string): Promise<unknown> {
  try {
    await run();
  } catch (error) {
    return error;
  }
  throw new Error(`assertion failed: ${message} (the promise resolved)`);
}

/** Resolve after `ms` real milliseconds. */
export function sleep(ms: number): Promise<void> {
  return new Promise((resolve) => setTimeout(resolve, ms));
}

// ---------------------------------------------------------------------------
// Property checks
// ---------------------------------------------------------------------------

/** A small seeded PRNG (mulberry32): the same seed gives the same cases. */
export interface Rng {
  /** A float in [0, 1). */
  next(): number;
  /** An integer in [min, max]. */
  int(min: number, max: number): number;
  /** One element of a non-empty array. */
  pick<T>(items: readonly T[]): T;
  /** True with probability `p`. */
  chance(p: number): boolean;
}

export function rng(seed: number): Rng {
  let a = seed >>> 0;
  const next = () => {
    a = (a + 0x6d2b79f5) >>> 0;
    let t = a;
    t = Math.imul(t ^ (t >>> 15), t | 1);
    t ^= t + Math.imul(t ^ (t >>> 7), t | 61);
    return ((t ^ (t >>> 14)) >>> 0) / 4294967296;
  };
  return {
    next,
    int: (min, max) => min + Math.floor(next() * (max - min + 1)),
    pick: (items) => items[Math.floor(next() * items.length)] as (typeof items)[number],
    chance: (p) => next() < p,
  };
}

/**
 * Run `check` on `runs` generated cases. A failure reports the seed and the case
 * index, so `property(..., { seed })` reproduces it.
 */
export async function property<T>(
  name: string,
  generate: (r: Rng, index: number) => T,
  check: (value: T, index: number) => void | Promise<void>,
  options: { runs?: number; seed?: number } = {},
): Promise<void> {
  const runs = options.runs ?? 500;
  const seed = options.seed ?? 0x5eed;
  const r = rng(seed);
  for (let i = 0; i < runs; i++) {
    const value = generate(r, i);
    try {
      await check(value, i);
    } catch (error) {
      const why = error instanceof Error ? error.message : String(error);
      throw new Error(`${name}: case ${i} (seed ${seed}) failed\n  input: ${JSON.stringify(value)}\n  ${why}`);
    }
  }
}

// ---------------------------------------------------------------------------
// Running
// ---------------------------------------------------------------------------

export interface RunSummary {
  passed: number;
  failed: number;
  failures: { name: string; error: string }[];
}

export async function runAll(log: (line: string) => void, greps: readonly string[] = []): Promise<RunSummary> {
  const summary: RunSummary = { passed: 0, failed: 0, failures: [] };
  for (const testCase of cases) {
    if (greps.length > 0 && !greps.some((g) => testCase.name.includes(g))) continue;
    try {
      await testCase.run();
      summary.passed += 1;
      log(`  ok   ${testCase.name}`);
    } catch (error) {
      summary.failed += 1;
      const message = error instanceof Error ? (error.stack ?? error.message) : String(error);
      summary.failures.push({ name: testCase.name, error: message });
      log(`  FAIL ${testCase.name}`);
      log(message.replace(/^/gm, '       '));
    }
  }
  return summary;
}
