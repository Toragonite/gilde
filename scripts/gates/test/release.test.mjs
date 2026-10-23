// Tests for scripts/gates/release-profile-check.mjs (contract §14.6 G0, A8.1)
// against small fixture artifacts built here: matching, mismatching and
// missing build ids, probe-log markers, and the debug capability.

import { test } from 'node:test';
import assert from 'node:assert/strict';
import { spawnSync } from 'node:child_process';
import { mkdirSync, mkdtempSync, readFileSync, rmSync, writeFileSync, copyFileSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { dirname, join, resolve } from 'node:path';
import { fileURLToPath } from 'node:url';

import { BUILD_ID_PATTERN, resolveBuildId } from '../lib/buildid.mjs';
import { cargoDefaultHas, capabilityIds, otherReleaseIds, hasQuotedLiteral } from '../lib/release.mjs';

const here = dirname(fileURLToPath(import.meta.url));
const repo = resolve(here, '..', '..', '..');
const cli = join(repo, 'scripts', 'gates', 'release-profile-check.mjs');

const VERSION = '0.1.0';
const RELEASE = '0.1.0+abc1234.20261022T101010';
const OTHER = '0.1.0+def5678.20261022T111111';

/** A fake repo root with the config files the check reads (copied from this repo). */
function makeRoot(dir, { cargoDefault = null, confFeatures = null, mainExtra = [] } = {}) {
  const root = join(dir, 'root');
  mkdirSync(join(root, 'src-tauri', 'capabilities'), { recursive: true });
  writeFileSync(join(root, 'package.json'), JSON.stringify({ name: 'gilde-code', version: VERSION }));
  let cargo = readFileSync(join(repo, 'src-tauri', 'Cargo.toml'), 'utf8');
  if (cargoDefault) cargo = cargo.replace(/^default = \[[^\]]*\]/m, `default = ${cargoDefault}`);
  writeFileSync(join(root, 'src-tauri', 'Cargo.toml'), cargo);
  const conf = JSON.parse(readFileSync(join(repo, 'src-tauri', 'tauri.conf.json'), 'utf8'));
  if (confFeatures) conf.build.features = confFeatures;
  writeFileSync(join(root, 'src-tauri', 'tauri.conf.json'), JSON.stringify(conf));
  copyFileSync(join(repo, 'src-tauri', 'capabilities', 'debug.json'), join(root, 'src-tauri', 'capabilities', 'debug.json'));
  const main = JSON.parse(readFileSync(join(repo, 'src-tauri', 'capabilities', 'main.json'), 'utf8'));
  main.permissions = [...main.permissions, ...mainExtra];
  writeFileSync(join(root, 'src-tauri', 'capabilities', 'main.json'), JSON.stringify(main));
  return root;
}

/** Fixture artifacts: each id is written the way its producer embeds it. */
function makeArtifacts(dir, { web, sidecar, core, shim, shimProbeLog = false, resolved = ['main'] }) {
  const a = join(dir, 'artifacts');
  mkdirSync(join(a, 'dist', 'assets'), { recursive: true });
  writeFileSync(join(a, 'dist', 'index.html'), '<!doctype html><script type="module" src="/assets/index-x1.js"></script>');
  writeFileSync(join(a, 'dist', 'assets', 'index-x1.js'), web === null ? 'const a=1;export{a};' : `const a=1;var B=${JSON.stringify(web)};export{a,B};`);
  writeFileSync(join(a, 'sidecar.cjs'), sidecar === null ? '"use strict";module.exports={};' : `"use strict";const BUILD_ID=${JSON.stringify(sidecar)};module.exports={BUILD_ID};`);
  // Rust packs string literals without delimiters: put other text right against the id.
  const bin = (id, extra = '') => Buffer.concat([
    Buffer.from([0xcf, 0xfa, 0xed, 0xfe, 0, 1, 2, 3]),
    Buffer.from('src/lib.rs' + (id ?? '') + 'gilde-tmuxcalled `Option::unwrap()`' + extra, 'latin1'),
    Buffer.alloc(64, 0x90),
  ]);
  writeFileSync(join(a, 'gilde'), bin(core));
  writeFileSync(join(a, 'gilde-tmux'), bin(shim, shimProbeLog ? 'GILDE_TMUX_LOG' : ''));
  const caps = Object.fromEntries(resolved.map((id) => [id, { identifier: id, windows: ['main'], permissions: [] }]));
  writeFileSync(join(a, 'capabilities.json'), JSON.stringify(caps));
  return a;
}

function run(root, a, { env = {}, extra = [], shim = true } = {}) {
  const childEnv = { ...process.env, ...env };
  if (!('GILDE_BUILD_ID' in env)) delete childEnv.GILDE_BUILD_ID;
  const args = [cli, '--root', root, '--webview', join(a, 'dist'), '--sidecar', join(a, 'sidecar.cjs'), '--core', join(a, 'gilde'), '--capabilities', join(a, 'capabilities.json'), ...(shim ? ['--shim', join(a, 'gilde-tmux')] : []), ...extra];
  return spawnSync(process.execPath, args, { encoding: 'utf8', env: childEnv });
}

function withDir(body) {
  const dir = mkdtempSync(join(tmpdir(), 'release-check-'));
  try {
    return body(dir);
  } finally {
    rmSync(dir, { recursive: true, force: true });
  }
}

test('matching release id in all four artifacts passes', () => withDir((dir) => {
  const root = makeRoot(dir);
  const a = makeArtifacts(dir, { web: RELEASE, sidecar: RELEASE, core: RELEASE, shim: RELEASE });
  const r = run(root, a, { env: { GILDE_BUILD_ID: RELEASE } });
  assert.equal(r.status, 0, r.stdout + r.stderr);
  assert.match(r.stdout, /release-profile-check: clean/);
}));

test('GILDE_BUILD_ID unset (or empty) → the version is the id, and it must be in every artifact', () => withDir((dir) => {
  const root = makeRoot(dir);
  const a = makeArtifacts(dir, { web: VERSION, sidecar: VERSION, core: VERSION, shim: VERSION });
  assert.equal(run(root, a).status, 0);
  const empty = run(root, a, { env: { GILDE_BUILD_ID: '' } });
  assert.equal(empty.status, 0, empty.stdout);
  assert.match(empty.stdout, /build id 0\.1\.0/);
}));

test('mismatch: the committed sidecar carries the version while the rest carry the release id', () => withDir((dir) => {
  const root = makeRoot(dir);
  const a = makeArtifacts(dir, { web: RELEASE, sidecar: VERSION, core: RELEASE, shim: RELEASE });
  const r = run(root, a, { env: { GILDE_BUILD_ID: RELEASE } });
  assert.equal(r.status, 1);
  assert.match(r.stdout, /FAIL sidecar\.cjs: build id 0\.1\.0\+abc1234\.20261022T101010 not found/);
}));

test('mismatch: one artifact built with another release id is named', () => withDir((dir) => {
  const root = makeRoot(dir);
  const a = makeArtifacts(dir, { web: OTHER, sidecar: RELEASE, core: RELEASE, shim: RELEASE });
  const r = run(root, a, { env: { GILDE_BUILD_ID: RELEASE } });
  assert.equal(r.status, 1);
  assert.match(r.stdout, /FAIL WebView .*carries 0\.1\.0\+def5678\.20261022T111111 instead of 0\.1\.0\+abc1234/);
}));

test('an artifact with both the right id and a stale release id fails', () => withDir((dir) => {
  const root = makeRoot(dir);
  const a = makeArtifacts(dir, { web: RELEASE, sidecar: RELEASE, core: RELEASE + 'x' + OTHER, shim: RELEASE });
  const r = run(root, a, { env: { GILDE_BUILD_ID: RELEASE } });
  assert.equal(r.status, 1);
  assert.match(r.stdout, /FAIL core binary: carries 0\.1\.0\+abc1234\.20261022T101010 and also 0\.1\.0\+def5678/);
}));

test('missing id: a core binary without any build id fails; the shim too', () => withDir((dir) => {
  const root = makeRoot(dir);
  const a = makeArtifacts(dir, { web: RELEASE, sidecar: RELEASE, core: null, shim: null });
  const r = run(root, a, { env: { GILDE_BUILD_ID: RELEASE } });
  assert.equal(r.status, 1);
  assert.match(r.stdout, /FAIL core binary: build id .* not found/);
  assert.match(r.stdout, /FAIL gilde-tmux shim: build id .* not found/);
}));

test('missing id in the WebView: a bare number is not a string literal', () => withDir((dir) => {
  const root = makeRoot(dir);
  const a = makeArtifacts(dir, { web: null, sidecar: VERSION, core: VERSION, shim: VERSION });
  writeFileSync(join(a, 'dist', 'assets', 'index-x1.js'), 'var v=10.1.0;var s="v0.1.0-beta";');
  const r = run(root, a);
  assert.equal(r.status, 1);
  assert.match(r.stdout, /FAIL WebView .*build id 0\.1\.0 not found/);
}));

test('a bad GILDE_BUILD_ID fails the check and names GILDE_BUILD_ID (never trimmed)', () => withDir((dir) => {
  const root = makeRoot(dir);
  const a = makeArtifacts(dir, { web: VERSION, sidecar: VERSION, core: VERSION, shim: VERSION });
  for (const bad of ['0.1.0\n', ' 0.1.0', 'a/b', '.hidden', '-flag', 'x'.repeat(65), 'é1']) {
    const r = run(root, a, { env: { GILDE_BUILD_ID: bad } });
    assert.equal(r.status, 1, `GILDE_BUILD_ID=${JSON.stringify(bad)}`);
    assert.match(r.stdout, /FAIL invalid build id \(GILDE_BUILD_ID=/);
  }
  const flag = run(root, a, { extra: ['--build-id', 'bad id'] });
  assert.equal(flag.status, 1);
  assert.match(flag.stdout, /breaks the value rule/);
}));

test('probe-log: a marker in the shim, a Cargo default or a tauri.conf feature fails', () => withDir((dir) => {
  const a = makeArtifacts(dir, { web: VERSION, sidecar: VERSION, core: VERSION, shim: VERSION, shimProbeLog: true });
  const r = run(makeRoot(dir), a);
  assert.equal(r.status, 1);
  assert.match(r.stdout, /FAIL gilde-tmux shim: probe-log marker\(s\) present: GILDE_TMUX_LOG/);

  const clean = makeArtifacts(join(dir, 'c'), { web: VERSION, sidecar: VERSION, core: VERSION, shim: VERSION });
  const d1 = join(dir, 'd1');
  mkdirSync(d1);
  const r2 = run(makeRoot(d1, { cargoDefault: '["test-fixtures", "probe-log"]' }), clean);
  assert.equal(r2.status, 1);
  assert.match(r2.stdout, /FAIL Cargo\.toml: probe-log is a default feature/);
  const d2 = join(dir, 'd2');
  mkdirSync(d2);
  const r3 = run(makeRoot(d2, { confFeatures: ['probe-log'] }), clean);
  assert.equal(r3.status, 1);
  assert.match(r3.stdout, /FAIL tauri\.conf\.json: build\.features turns probe-log on/);
}));

test('debug capability: in the resolved set, or a debug permission granted by main.json, fails', () => withDir((dir) => {
  const root = makeRoot(dir);
  const a = makeArtifacts(dir, { web: VERSION, sidecar: VERSION, core: VERSION, shim: VERSION, resolved: ['main', 'debug'] });
  const r = run(root, a);
  assert.equal(r.status, 1);
  assert.match(r.stdout, /FAIL resolved capabilities include "debug"/);

  const d1 = join(dir, 'd1');
  mkdirSync(d1);
  const leakRoot = makeRoot(d1, { mainExtra: ['allow-app-diagnostics'] });
  const ok = makeArtifacts(d1, { web: VERSION, sidecar: VERSION, core: VERSION, shim: VERSION });
  const r2 = run(leakRoot, ok);
  assert.equal(r2.status, 1);
  assert.match(r2.stdout, /FAIL main\.json grants debug-only permission\(s\): allow-app-diagnostics/);
}));

test('the shim is optional; missing inputs exit 2', () => withDir((dir) => {
  const root = makeRoot(dir);
  const a = makeArtifacts(dir, { web: VERSION, sidecar: VERSION, core: VERSION, shim: VERSION });
  const r = run(root, a, { shim: false });
  assert.equal(r.status, 0, r.stdout);
  assert.match(r.stdout, /note gilde-tmux shim: not checked/);
  const missing = spawnSync(process.execPath, [cli, '--root', root, '--webview', join(a, 'dist'), '--sidecar', join(a, 'nope.cjs'), '--core', join(a, 'gilde'), '--capabilities', join(a, 'capabilities.json')], { encoding: 'utf8' });
  assert.equal(missing.status, 2);
  const noFlag = spawnSync(process.execPath, [cli, '--root', root], { encoding: 'utf8' });
  assert.equal(noFlag.status, 2);
}));

test('helpers: literals, release-id scan, Cargo defaults, capability file shapes', () => {
  assert.equal(hasQuotedLiteral('x="0.1.0"', '0.1.0'), true);
  assert.equal(hasQuotedLiteral('x=0.1.0', '0.1.0'), false);
  assert.deepEqual(otherReleaseIds(`a${RELEASE}b${OTHER}c`, RELEASE), [OTHER]);
  assert.deepEqual(otherReleaseIds(`a1${RELEASE}`, RELEASE), [], 'a longer match that contains the id is the id');
  assert.equal(cargoDefaultHas('[features]\ndefault = ["a", "probe-log"]\n[deps]\n', 'probe-log'), true);
  assert.equal(cargoDefaultHas('[features]\ndefault = ["a"]\nprobe-log = []\n', 'probe-log'), false);
  assert.deepEqual(capabilityIds({ main: { identifier: 'main' } }), ['main']);
  assert.deepEqual(capabilityIds([{ identifier: 'main' }, { identifier: 'debug' }]), ['main', 'debug']);
});

test('the Node build-id rule matches the WebView rule (src/lib/buildId.ts) and A8.1', () => {
  const ts = readFileSync(join(repo, 'src', 'lib', 'buildId.ts'), 'utf8');
  const m = /export const BUILD_ID_PATTERN = \/(.+)\/;/.exec(ts);
  assert.ok(m, 'BUILD_ID_PATTERN in src/lib/buildId.ts');
  assert.equal(BUILD_ID_PATTERN.source, m[1]);
  assert.deepEqual(resolveBuildId(undefined, '0.1.0'), { id: '0.1.0', source: 'version' });
  assert.deepEqual(resolveBuildId('', '0.1.0'), { id: '0.1.0', source: 'version' });
  assert.deepEqual(resolveBuildId(RELEASE, '0.1.0'), { id: RELEASE, source: 'env' });
  assert.match(resolveBuildId('0.1.0\n', '0.1.0').error, /GILDE_BUILD_ID="0\.1\.0\\n"/);
  assert.match(resolveBuildId(undefined, 'bad version').error, /GILDE_BUILD_ID unset/);
});
