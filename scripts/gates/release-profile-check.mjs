#!/usr/bin/env node
/**
 * release-profile-check.mjs — G0's release-profile check (contract §14.6, A8.1).
 *
 * Run it on the artifacts of a release build (`tauri build` with
 * `GILDE_BUILD_ID=<version>+<sha>.<UTC yyyymmddTHHMMSS>` in its environment):
 *
 *   node scripts/gates/release-profile-check.mjs \
 *     --webview dist \
 *     --sidecar src-tauri/sidecar/sidecar.cjs \
 *     --core "src-tauri/target/release/gilde" \
 *     --shim "src-tauri/target/release/gilde-tmux" \
 *     --capabilities src-tauri/gen/schemas/capabilities.json \
 *     [--build-id <id>] [--root <repo>] [--json]
 *
 * Checks (lib/release.mjs):
 *  - the build id: `--build-id`, else the A8.1 rule on this environment
 *    (`GILDE_BUILD_ID` non-empty, else `package.json` version); it must match
 *    the value rule, and the WebView bundle (as a string literal), `sidecar.cjs`
 *    (string literal), the core binary and the shim (raw bytes) must all
 *    contain it and no other release-form id;
 *  - `probe-log` is off: `GILDE_TMUX_LOG` is in neither binary; `probe-log` is
 *    not a Cargo default feature nor in `tauri.conf.json` `build.features`;
 *  - the debug capability is absent: the resolved capability set has no
 *    capability with `capabilities/debug.json`'s identifier, and `main.json`
 *    grants none of debug.json's permissions.
 * `--shim` is optional (reported as not checked when left out); every other
 * artifact is required.
 *
 * Exit codes: 0 every check passed; 1 a check failed; 2 the check could not run
 * (bad flags, an input file missing or unreadable).
 */

import { existsSync, readFileSync, readdirSync, statSync } from 'node:fs';
import { dirname, join, relative, resolve } from 'node:path';
import { fileURLToPath } from 'node:url';

import { BUILD_ID_ENV, resolveBuildId, isValidBuildId, BUILD_ID_PATTERN } from './lib/buildid.mjs';
import {
  capabilityIds,
  cargoDefaultHas,
  checkArtifactId,
  grantedPermissions,
  probeLogMarkers,
  tauriConfFeatures,
} from './lib/release.mjs';

const here = dirname(fileURLToPath(import.meta.url));

function fail2(message) {
  console.error(`release-profile-check: ${message}`);
  process.exit(2);
}

function parseArgs(argv) {
  const opts = { root: resolve(here, '..', '..'), webview: null, sidecar: null, core: null, shim: null, capabilities: null, buildId: null, json: false };
  const flags = { '--root': 'root', '--webview': 'webview', '--sidecar': 'sidecar', '--core': 'core', '--shim': 'shim', '--capabilities': 'capabilities', '--build-id': 'buildId' };
  for (let i = 0; i < argv.length; i++) {
    const a = argv[i];
    if (a === '--json') opts.json = true;
    else if (a === '-h' || a === '--help') {
      console.log('usage: release-profile-check.mjs --webview <dir> --sidecar <file> --core <file> --capabilities <file> [--shim <file>] [--build-id <id>] [--root <dir>] [--json]');
      process.exit(0);
    } else if (flags[a]) {
      if (i + 1 >= argv.length) fail2(`${a} needs a value`);
      opts[flags[a]] = argv[++i];
    } else fail2(`unknown argument ${a}`);
  }
  for (const k of ['webview', 'sidecar', 'core', 'capabilities']) if (!opts[k]) fail2(`--${k} is required`);
  return opts;
}

function readOrFail(path, what) {
  if (!existsSync(path)) fail2(`${what} not found: ${path}`);
  try {
    return readFileSync(path);
  } catch (e) {
    fail2(`${what} unreadable: ${path} (${e.code ?? e.message})`);
  }
}

function readJson(path, what) {
  try {
    return JSON.parse(readOrFail(path, what).toString('utf8'));
  } catch (e) {
    fail2(`${what} is not JSON: ${path}`);
  }
}

/** The WebView bundle's text files (js, mjs, html). */
function webviewFiles(path) {
  if (!existsSync(path)) fail2(`WebView bundle not found: ${path}`);
  if (statSync(path).isFile()) return [path];
  const out = [];
  const walk = (d) => {
    for (const name of readdirSync(d)) {
      const p = join(d, name);
      if (statSync(p).isDirectory()) walk(p);
      else if (/\.(m?js|html)$/.test(name)) out.push(p);
    }
  };
  walk(path);
  if (out.length === 0) fail2(`WebView bundle has no .js/.html files: ${path}`);
  return out;
}

export function run(opts) {
  const results = [];
  const check = (ok, line) => results.push({ ok, line });
  const root = resolve(opts.root);

  // -- the build id --------------------------------------------------------
  let id = opts.buildId;
  if (id !== null) {
    if (!isValidBuildId(id)) check(false, `build id ${JSON.stringify(id)} breaks the value rule ${BUILD_ID_PATTERN.source} (${BUILD_ID_ENV}, A8.1)`);
  } else {
    const pkg = readJson(join(root, 'package.json'), 'package.json');
    const r = resolveBuildId(process.env[BUILD_ID_ENV], pkg.version);
    if (r.error) check(false, r.error);
    else id = r.id;
  }
  if (id !== null && isValidBuildId(id)) {
    check(true, `build id ${id}`);
    const web = webviewFiles(resolve(opts.webview));
    const w = checkArtifactId(`WebView (${web.length} file(s) in ${relative(process.cwd(), resolve(opts.webview)) || '.'})`, 'js', web.map((f) => readOrFail(f, 'WebView file')), id);
    check(w.ok, w.detail);
    const s = checkArtifactId('sidecar.cjs', 'js', [readOrFail(resolve(opts.sidecar), 'sidecar.cjs')], id);
    check(s.ok, s.detail);
    const c = checkArtifactId('core binary', 'binary', [readOrFail(resolve(opts.core), 'core binary')], id);
    check(c.ok, c.detail);
    if (opts.shim) {
      const t = checkArtifactId('gilde-tmux shim', 'binary', [readOrFail(resolve(opts.shim), 'shim binary')], id);
      check(t.ok, t.detail);
    }
  }
  if (!opts.shim) results.push({ ok: true, line: 'gilde-tmux shim: not checked (no --shim)', note: true });

  // -- probe-log -----------------------------------------------------------
  const binaries = [['core binary', opts.core], ...(opts.shim ? [['gilde-tmux shim', opts.shim]] : [])];
  for (const [name, path] of binaries) {
    const markers = probeLogMarkers(readOrFail(resolve(path), name));
    check(markers.length === 0, markers.length ? `${name}: probe-log marker(s) present: ${markers.join(', ')}` : `${name}: no probe-log marker`);
  }
  const cargoPath = join(root, 'src-tauri', 'Cargo.toml');
  const cargo = readOrFail(cargoPath, 'src-tauri/Cargo.toml').toString('utf8');
  check(!cargoDefaultHas(cargo, 'probe-log'), cargoDefaultHas(cargo, 'probe-log') ? 'Cargo.toml: probe-log is a default feature' : 'Cargo.toml: probe-log is not a default feature');
  const conf = readJson(join(root, 'src-tauri', 'tauri.conf.json'), 'tauri.conf.json');
  const confFeatures = tauriConfFeatures(conf);
  check(!confFeatures.includes('probe-log'), confFeatures.includes('probe-log') ? 'tauri.conf.json: build.features turns probe-log on' : 'tauri.conf.json: build.features has no probe-log');

  // -- the debug capability ------------------------------------------------
  const debugCap = readJson(join(root, 'src-tauri', 'capabilities', 'debug.json'), 'capabilities/debug.json');
  const mainCap = readJson(join(root, 'src-tauri', 'capabilities', 'main.json'), 'capabilities/main.json');
  const debugId = typeof debugCap.identifier === 'string' ? debugCap.identifier : 'debug';
  const resolved = capabilityIds(readJson(resolve(opts.capabilities), 'resolved capabilities'));
  check(!resolved.includes(debugId), resolved.includes(debugId)
    ? `resolved capabilities include "${debugId}" (capabilities/debug.json) — not a release build`
    : `resolved capabilities: ${resolved.join(', ') || '(none)'} — no "${debugId}"`);
  const leaked = grantedPermissions(mainCap).filter((p) => grantedPermissions(debugCap).includes(p));
  check(leaked.length === 0, leaked.length ? `main.json grants debug-only permission(s): ${leaked.join(', ')}` : 'main.json grants no debug-only permission');

  return results;
}

function main() {
  const opts = parseArgs(process.argv.slice(2));
  const results = run(opts);
  const failed = results.filter((r) => !r.ok);
  if (opts.json) console.log(JSON.stringify({ ok: failed.length === 0, results }, null, 2));
  else {
    for (const r of results) console.log(`${r.ok ? (r.note ? 'note' : 'ok  ') : 'FAIL'} ${r.line}`);
    console.log(failed.length ? `release-profile-check: FAILED (${failed.length})` : 'release-profile-check: clean');
  }
  process.exitCode = failed.length ? 1 : 0;
}

if (process.argv[1] && resolve(process.argv[1]) === fileURLToPath(import.meta.url)) main();
