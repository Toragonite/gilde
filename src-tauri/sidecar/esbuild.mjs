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
 * Three build-time constants are injected because there is no way to read them
 * at runtime from inside a bundle:
 *
 *   - `__GILDE_BUILD_ID__` — the build id of contract §4.7 / A8.1: the env var
 *     `GILDE_BUILD_ID` when it is set and non-empty, else the sidecar version
 *     (equal to the app version and `CARGO_PKG_VERSION`, kept so by
 *     `check:version`). It must match `^[0-9A-Za-z][0-9A-Za-z._+-]{0,63}$`; any
 *     other value FAILS THE BUILD with a message naming `GILDE_BUILD_ID`, and it
 *     is never trimmed, escaped or replaced. `mesh.hello` compares it with the
 *     core's and exits 78 on a mismatch. The core (`lib.rs` `BUILD_ID`) and the
 *     WebView (`src/lib/buildId.ts`) apply the same rule.
 *   - `__GILDE_SIDECAR_VERSION__` — the sidecar package version.
 *   - `__GILDE_MESH_PACKAGES__` — the exact installed versions of the five
 *     `@toragonite` packages (their `exports` maps expose only `.`, so
 *     `require('…/package.json')` does not resolve inside the bundle).
 *
 *   node esbuild.mjs                    one-shot build into sidecar.cjs
 *   node esbuild.mjs --outfile <path>   one-shot build elsewhere (the drift check)
 *   node esbuild.mjs --watch            rebuild on change (no seam check; that is a gate)
 *
 * Importing this module builds nothing: `resolveBuildId` is exported for
 * `test/stub-host.mjs` (T-buildId-rule, the sidecar half).
 */

import { readFileSync } from 'node:fs';
import { dirname, isAbsolute, join, resolve } from 'node:path';
import { fileURLToPath, pathToFileURL } from 'node:url';

/** Contract §4.7 / A8.1: what a build id may look like. */
export const BUILD_ID_PATTERN = /^[0-9A-Za-z][0-9A-Za-z._+-]{0,63}$/;

/** The env var that sets the build id. */
export const BUILD_ID_ENV = 'GILDE_BUILD_ID';

/**
 * The build id for `envValue` (`process.env.GILDE_BUILD_ID`; `undefined` when
 * unset) and the package `version`. An empty value is unset. Throws — failing
 * the build — when the chosen value breaks the rule; the message names
 * `GILDE_BUILD_ID`. Never trims or replaces anything.
 */
export function resolveBuildId(envValue, version) {
  const fromEnv = typeof envValue === 'string' && envValue !== '';
  const id = fromEnv ? envValue : version;
  if (typeof id !== 'string' || !BUILD_ID_PATTERN.test(id)) {
    const source = fromEnv
      ? `${BUILD_ID_ENV}=${JSON.stringify(id)}`
      : `${BUILD_ID_ENV} unset, sidecar package.json version ${JSON.stringify(id)}`;
    throw new Error(
      `[esbuild] invalid build id (${source}): ${BUILD_ID_ENV} must match ${BUILD_ID_PATTERN.source} — ` +
        '1 to 64 ASCII letters, digits, ".", "_", "+" or "-", starting with a letter or digit, ' +
        'no whitespace, no "/" (contract §4.7, A8.1). It is never trimmed or replaced.',
    );
  }
  return id;
}

const here = dirname(fileURLToPath(import.meta.url));

const readJson = (path) => JSON.parse(readFileSync(path, 'utf8'));

/** The five runtime packages whose exact versions the sidecar logs at start. */
const MESH_PACKAGES = [
  '@toragonite/agent-mesh',
  '@toragonite/agent-mesh-claude',
  '@toragonite/agent-mesh-codex',
  '@toragonite/agent-mesh-cursor',
  '@toragonite/agent-mesh-gemini',
];

/** The esbuild options for one build into `outfile` (absolute, or relative to this dir). */
export function buildOptions(outfile = 'sidecar.cjs', env = process.env) {
  const pkg = readJson(join(here, 'package.json'));
  const buildId = resolveBuildId(env[BUILD_ID_ENV], pkg.version);

  const packageVersions = {};
  for (const name of MESH_PACKAGES) {
    const manifest = join(here, 'node_modules', ...name.split('/'), 'package.json');
    try {
      packageVersions[name] = readJson(manifest).version;
    } catch {
      throw new Error(`[esbuild] ${name} is not installed. Run: (cd src-tauri/sidecar && npm ci)`);
    }
  }

  /** @type {import('esbuild').BuildOptions} */
  const options = {
    absWorkingDir: here,
    entryPoints: ['src/index.ts'],
    outfile: isAbsolute(outfile) ? outfile : resolve(here, outfile),
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
      __GILDE_BUILD_ID__: JSON.stringify(buildId),
      __GILDE_SIDECAR_VERSION__: JSON.stringify(pkg.version),
      __GILDE_MESH_PACKAGES__: JSON.stringify(packageVersions),
    },
  };
  return { options, buildId, version: pkg.version };
}

async function main(argv) {
  const watch = argv.includes('--watch');
  const at = argv.indexOf('--outfile');
  const outfile = at >= 0 ? argv[at + 1] : undefined;
  if (at >= 0 && (outfile === undefined || outfile.startsWith('--'))) {
    throw new Error('[esbuild] --outfile needs a path');
  }
  const { options, buildId, version } = buildOptions(outfile);
  const { build, context } = await import('esbuild');
  if (watch) {
    const ctx = await context(options);
    await ctx.watch();
    process.stderr.write(`[esbuild] watching src/ … (build id ${buildId})\n`);
  } else {
    await build(options);
    process.stderr.write(
      `[esbuild] wrote ${options.outfile} (gilde-code-sidecar ${version}, build id ${buildId})\n`,
    );
  }
}

const invokedDirectly =
  process.argv[1] !== undefined && import.meta.url === pathToFileURL(resolve(process.argv[1])).href;
if (invokedDirectly) {
  main(process.argv.slice(2)).catch((err) => {
    process.stderr.write(`${err?.message ?? err}\n`);
    process.exit(1);
  });
}
