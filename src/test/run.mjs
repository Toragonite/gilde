#!/usr/bin/env node
/**
 * Front-end unit test runner.
 *
 * The dependency set is frozen and has no test runner, so this bundles
 * `src/test/all.ts` with the project's own Vite and runs the bundle on Node.
 * `all.ts` imports `src/test/env.ts` (a happy-dom window, contract §14.2 T-ui-*)
 * and then the virtual module `virtual:gilde-tests`, which this script writes:
 * one import per `*.test.ts(x)` file found under `src/`. Nobody has to register
 * a test file; a new file is picked up by its name.
 *
 *   node src/test/run.mjs                      every test file
 *   node src/test/run.mjs --filter=markdown    only files whose path contains "markdown"
 *   node src/test/run.mjs --grep=revGap        only test names containing "revGap"
 *
 * Legacy test files of the old screens are skipped while their content is
 * unchanged (`quarantine.mjs`). Exit code 0 means every test that ran passed.
 */

import { createHash } from 'node:crypto';
import { mkdir, mkdtemp, readFile, readdir, rm } from 'node:fs/promises';
import { dirname, join, relative, resolve, sep } from 'node:path';
import { fileURLToPath, pathToFileURL } from 'node:url';
import { build, loadConfigFromFile } from 'vite';

import { QUARANTINE } from './quarantine.mjs';

const here = dirname(fileURLToPath(import.meta.url));
const root = resolve(here, '..', '..');
const srcDir = join(root, 'src');

const args = process.argv.slice(2);
const filters = args.filter((a) => a.startsWith('--filter=')).map((a) => a.slice('--filter='.length));
const greps = args.filter((a) => a.startsWith('--grep=')).map((a) => a.slice('--grep='.length));
for (const a of args) {
  if (!a.startsWith('--filter=') && !a.startsWith('--grep=')) {
    console.error(`run.mjs: unknown argument ${a}`);
    process.exit(2);
  }
}

/** Every `*.test.ts(x)` under src/, sorted, as repo-relative POSIX paths. */
async function findTests(dir) {
  const out = [];
  const entries = await readdir(dir, { withFileTypes: true });
  for (const entry of entries) {
    const abs = join(dir, entry.name);
    const rel = relative(root, abs).split(sep).join('/');
    if (entry.isDirectory()) {
      if (entry.name === 'node_modules' || rel === 'src/lib/gen') continue;
      out.push(...(await findTests(abs)));
    } else if (/\.test\.tsx?$/.test(entry.name)) {
      out.push(rel);
    }
  }
  return out.sort();
}

async function sha256(path) {
  return createHash('sha256').update(await readFile(join(root, path))).digest('hex');
}

const found = await findTests(srcDir);
const pinned = new Map(QUARANTINE.map((q) => [q.path, q]));
const skipped = [];
const selected = [];
for (const path of found) {
  const q = pinned.get(path);
  if (q && (await sha256(path)) === q.sha256) {
    skipped.push(q);
    continue;
  }
  if (filters.length > 0 && !filters.some((f) => path.includes(f))) continue;
  selected.push(path);
}

const VIRTUAL_ID = 'virtual:gilde-tests';
const RESOLVED_ID = '\0' + VIRTUAL_ID;
const testsModule = {
  name: 'gilde-test-discovery',
  resolveId(id) {
    return id === VIRTUAL_ID ? RESOLVED_ID : null;
  },
  load(id) {
    if (id !== RESOLVED_ID) return null;
    return selected.map((p) => `import ${JSON.stringify(join(root, p))};`).join('\n') + '\nexport {};\n';
  },
};

// The app's own `define`s (the build id of contract §4.7 / A8.1) come from
// vite.config.ts, so the tests see exactly what the app bundle carries — and an
// invalid GILDE_BUILD_ID fails the test run the way it fails the build.
const appConfig = await loadConfigFromFile({ command: 'build', mode: 'development' }, join(root, 'vite.config.ts'), root, 'silent');
const appDefine = appConfig?.config.define ?? {};
if (typeof appDefine['import.meta.env.VITE_GILDE_BUILD_ID'] !== 'string') {
  console.error('run.mjs: vite.config.ts did not define import.meta.env.VITE_GILDE_BUILD_ID');
  process.exit(2);
}

// The bundle lives under node_modules/.cache so that the one external package
// (happy-dom, loaded by env.ts) resolves from it with Node's own lookup.
const cacheDir = join(root, 'node_modules', '.cache', 'gilde-fe-tests');
await mkdir(cacheDir, { recursive: true });
const outDir = await mkdtemp(join(cacheDir, 'run-'));

try {
  await build({
    configFile: false,
    root,
    logLevel: 'warn',
    // React's act() (used by @testing-library/react) exists only in its
    // development build.
    mode: 'development',
    define: { ...appDefine, 'process.env.NODE_ENV': JSON.stringify('development') },
    plugins: [testsModule],
    ssr: { noExternal: true, external: ['happy-dom'] },
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
  const code = await module.main({ greps });
  if (skipped.length > 0) {
    const owners = [...new Set(skipped.map((q) => q.owner))].sort().join(', ');
    console.log(`${skipped.length} legacy test file(s) skipped until rewritten by ${owners} (src/test/quarantine.mjs)`);
  }
  console.log(`${selected.length} test file(s) run`);
  process.exitCode = code;
} finally {
  await rm(outDir, { recursive: true, force: true });
}

// React's async act() schedules through MessageChannel in this bundle, and the
// open ports would keep Node alive after the last test: exit explicitly.
process.exit(process.exitCode ?? 0);
