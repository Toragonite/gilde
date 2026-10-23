/**
 * Unit test entry point. `src/test/run.mjs` bundles this with Vite and runs it
 * on Node. `./env` installs the DOM first; `virtual:gilde-tests` then imports
 * every discovered `*.test.ts(x)` file (see run.mjs), and every `test()` they
 * register runs in file order.
 */

import './env';
import 'virtual:gilde-tests';
import { runAll } from './harness';

export async function main(options: { greps?: readonly string[] } = {}): Promise<number> {
  const summary = await runAll((line) => console.log(line), options.greps ?? []);
  console.log(`\n${summary.passed} passed, ${summary.failed} failed`);
  if (summary.failed > 0) {
    console.log('failed:');
    for (const f of summary.failures) console.log(`  - ${f.name}`);
  }
  return summary.failed === 0 ? 0 : 1;
}
