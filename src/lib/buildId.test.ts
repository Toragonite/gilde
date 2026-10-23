/**
 * T-buildId-rule, the WebView half (contract §4.7, A8.1): the helper that
 * vite.config.ts uses, and vite.config.ts itself failing on a bad
 * GILDE_BUILD_ID.
 */

import { spawnSync } from 'node:child_process';
import { existsSync } from 'node:fs';
import { join, resolve } from 'node:path';

import pkg from '../../package.json';
import { assert, assertEqual, test } from '../test/harness';
import { BUILD_ID_PATTERN, isValidBuildId, resolveBuildId } from './buildId';

function rejects(env: string, why: string): void {
  let message = '';
  try {
    resolveBuildId(env, '0.1.0');
  } catch (e) {
    message = e instanceof Error ? e.message : String(e);
  }
  assert(message !== '', `${why}: ${JSON.stringify(env)} is rejected`);
  assert(message.includes('GILDE_BUILD_ID'), `${why}: the message names GILDE_BUILD_ID`);
}

test('T-buildId-rule: GILDE_BUILD_ID when set and non-empty, else the version', () => {
  assertEqual(resolveBuildId(undefined, pkg.version), pkg.version, 'unset → the package version');
  assertEqual(resolveBuildId('', pkg.version), pkg.version, 'empty is unset');
  assertEqual(resolveBuildId('2.0.0+b425b5e.20261022T073000', '0.1.0'), '2.0.0+b425b5e.20261022T073000', 'a release-style id');
  assertEqual(resolveBuildId('2.0.0', '0.1.0'), '2.0.0', 'a plain version');
  assert(isValidBuildId(pkg.version), 'the package version itself is valid');
  const max = 'a'.repeat(64);
  assertEqual(resolveBuildId(max, '0.1.0'), max, 'exactly 64 bytes');
});

test('T-buildId-rule: every bad class fails, never trimmed or replaced', () => {
  rejects('2.0.0\n', 'a trailing newline (command substitution)');
  rejects(' 2.0.0', 'leading whitespace');
  rejects('2.0.0 ', 'trailing whitespace');
  rejects('2.0 .0', 'inner whitespace');
  rejects('2.0.0\t', 'a tab');
  rejects('a/b', 'a slash');
  rejects('../x', 'a leading dot and a slash');
  rejects('.hidden', 'a leading dot');
  rejects('-rf', 'a leading dash');
  rejects('+1', 'a leading plus');
  rejects('a'.repeat(65), 'more than 64 bytes');
  rejects('빌드', 'non-ASCII');
  rejects('a:b', 'a colon');
  let threw = false;
  try {
    resolveBuildId(undefined, 'bad version');
  } catch (e) {
    threw = true;
    assert(String(e).includes('GILDE_BUILD_ID'), 'an invalid version names GILDE_BUILD_ID too');
  }
  assert(threw, 'the version obeys the same rule');
  assertEqual(BUILD_ID_PATTERN.source, '^[0-9A-Za-z][0-9A-Za-z._+-]{0,63}$', 'the contract pattern');
});

function repoRoot(): string {
  let dir = process.cwd();
  for (let i = 0; i < 6; i++) {
    if (existsSync(join(dir, 'vite.config.ts'))) return dir;
    dir = resolve(dir, '..');
  }
  throw new Error('vite.config.ts not found above the working directory');
}

/** Load vite.config.ts in a child Node with `GILDE_BUILD_ID` set (or unset), as `vite build` does. */
function loadConfig(buildId: string | undefined): { ok: boolean; out: string } {
  const root = repoRoot();
  const script = [
    'const { loadConfigFromFile } = await import("vite");',
    'try {',
    `  const r = await loadConfigFromFile({ command: "build", mode: "production" }, ${JSON.stringify(join(root, 'vite.config.ts'))}, ${JSON.stringify(root)}, "silent");`,
    '  console.log("ID=" + r.config.define["import.meta.env.VITE_GILDE_BUILD_ID"]);',
    '} catch (e) { console.log("FAIL " + e.message); process.exitCode = 1; }',
  ].join('\n');
  const env: Record<string, string | undefined> = { ...process.env };
  if (buildId === undefined) delete env.GILDE_BUILD_ID;
  else env.GILDE_BUILD_ID = buildId;
  const run = spawnSync(process.execPath, ['--input-type=module', '-e', script], { cwd: root, env, encoding: 'utf8' });
  return { ok: run.status === 0, out: `${run.stdout}${run.stderr}` };
}

test('T-buildId-rule: vite.config.ts always defines the id and fails the build on a bad one', () => {
  const unset = loadConfig(undefined);
  assert(unset.ok, `unset loads\n${unset.out}`);
  assert(unset.out.includes(`ID=${JSON.stringify(pkg.version)}`), 'unset → the version is defined');
  const release = loadConfig('2.0.0+b425b5e.20261022T073000');
  assert(release.out.includes('ID="2.0.0+b425b5e.20261022T073000"'), 'a release id is defined as is');
  const empty = loadConfig('');
  assert(empty.out.includes(`ID=${JSON.stringify(pkg.version)}`), 'empty → the version');
  const bad = loadConfig('2.0.0\n');
  assert(!bad.ok, 'a trailing newline fails the config');
  assert(bad.out.includes('GILDE_BUILD_ID'), `the failure names GILDE_BUILD_ID\n${bad.out}`);
});
