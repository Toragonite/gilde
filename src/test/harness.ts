/**
 * A 60-line test harness.
 *
 * T00 froze the dependency set and a test runner is not in it, so the unit
 * tests here are plain functions collected by this harness and executed by
 * `src/test/run.mjs` (which bundles them with the Vite that is already
 * installed). Assertions throw; the runner reports and sets the exit code.
 */

export interface TestCase {
  name: string;
  run: () => void | Promise<void>;
}

const cases: TestCase[] = [];

export function test(name: string, run: () => void | Promise<void>): void {
  cases.push({ name, run });
}

export function assert(condition: unknown, message: string): void {
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

export interface RunSummary {
  passed: number;
  failed: number;
  failures: { name: string; error: string }[];
}

export async function runAll(log: (line: string) => void): Promise<RunSummary> {
  const summary: RunSummary = { passed: 0, failed: 0, failures: [] };
  for (const testCase of cases) {
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
