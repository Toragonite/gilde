#!/usr/bin/env node
/**
 * Front-end unit test runner.
 *
 * T00 froze the dependency set and no test runner is in it, so instead of
 * adding one this bundles `src/test/all.ts` with the Vite that ships with the
 * project and runs the bundle on Node.
 *
 *   node src/test/run.mjs
 *
 * Exit code 0 means every test passed.
 */

import { mkdtemp, rm } from 'node:fs/promises';
import { tmpdir } from 'node:os';
import { join, resolve, dirname } from 'node:path';
import { fileURLToPath, pathToFileURL } from 'node:url';
import { build } from 'vite';

const here = dirname(fileURLToPath(import.meta.url));
const root = resolve(here, '..', '..');

const outDir = await mkdtemp(join(tmpdir(), 'gilde-fe-tests-'));

try {
  await build({
    configFile: false,
    root,
    logLevel: 'warn',
    // Bundle dependencies too: the output lives in a temp directory, so nothing
    // may be left as a bare import for Node to resolve.
    ssr: { noExternal: true },
    resolve: { conditions: ['node', 'module', 'import', 'default'] },
    build: {
      outDir,
      emptyOutDir: true,
      ssr: resolve(here, 'all.ts'),
      target: 'node20',
      minify: false,
      sourcemap: false,
      rollupOptions: {
        output: { entryFileNames: 'all.mjs', format: 'es' },
      },
    },
  });

  const module = await import(pathToFileURL(join(outDir, 'all.mjs')).href);
  const code = await module.main();
  process.exitCode = code;
} finally {
  await rm(outDir, { recursive: true, force: true });
}
