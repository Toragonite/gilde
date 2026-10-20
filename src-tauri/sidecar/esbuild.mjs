#!/usr/bin/env node
/**
 * esbuild.mjs — bundle the sidecar into a single self-contained CommonJS file.
 *
 * `esbuild src/index.ts --bundle --platform=node --target=node20 --format=cjs
 *  --outfile=sidecar.cjs --sourcemap=external` (DESIGN §5-2). All five
 * @toragonite packages depend only on agent-mesh core, so the artifact is small
 * and needs no node_modules at runtime — `tauri.conf.json > bundle.resources`
 * ships `sidecar.cjs` and nothing else. The external map is written next to it
 * for local debugging only and never bundled: it embeds the sources
 * (`sourcesContent`), and scripts/build-sidecar.mjs refuses a bundle config
 * that would ship any `.map`.
 *
 * Two build-time constants are injected because there is no way to read them at
 * runtime from inside a bundle: the package `exports` maps of the vendor
 * packages expose only `.`, so `require('@toragonite/agent-mesh/package.json')`
 * does not resolve. `mesh.hello` must report exact versions (§5-3), so they are
 * baked in here from the versions actually installed in node_modules.
 *
 *   node esbuild.mjs           one-shot build
 *   node esbuild.mjs --watch   rebuild on change (no seam check; that is a gate)
 */

import { build, context } from 'esbuild';
import { readFileSync } from 'node:fs';
import { dirname, join } from 'node:path';
import { fileURLToPath } from 'node:url';

const here = dirname(fileURLToPath(import.meta.url));
const watch = process.argv.includes('--watch');

const readJson = (path) => JSON.parse(readFileSync(path, 'utf8'));

const pkg = readJson(join(here, 'package.json'));

/** The five runtime packages whose exact versions `mesh.hello` reports. */
const MESH_PACKAGES = [
  '@toragonite/agent-mesh',
  '@toragonite/agent-mesh-claude',
  '@toragonite/agent-mesh-codex',
  '@toragonite/agent-mesh-cursor',
  '@toragonite/agent-mesh-gemini',
];

const packageVersions = {};
for (const name of MESH_PACKAGES) {
  const manifest = join(here, 'node_modules', ...name.split('/'), 'package.json');
  try {
    packageVersions[name] = readJson(manifest).version;
  } catch {
    throw new Error(
      `[esbuild] ${name} is not installed. Run: (cd src-tauri/sidecar && npm ci)`,
    );
  }
}

/** @type {import('esbuild').BuildOptions} */
const options = {
  absWorkingDir: here,
  entryPoints: ['src/index.ts'],
  outfile: 'sidecar.cjs',
  bundle: true,
  platform: 'node',
  target: 'node20',
  format: 'cjs',
  sourcemap: 'external',
  // Readable output on purpose: scripts/check-seam.mjs attributes every `spawn(`
  // in the artifact to the module it came from, using esbuild's per-module
  // `// path` banners. Minifying would erase that audit trail.
  minify: false,
  legalComments: 'none',
  logLevel: 'info',
  define: {
    __GILDE_SIDECAR_VERSION__: JSON.stringify(pkg.version),
    __GILDE_MESH_PACKAGES__: JSON.stringify(packageVersions),
  },
};

if (watch) {
  const ctx = await context(options);
  await ctx.watch();
  process.stderr.write('[esbuild] watching src/ …\n');
} else {
  await build(options);
  process.stderr.write(`[esbuild] wrote sidecar.cjs (gilde-code-sidecar ${pkg.version})\n`);
}
