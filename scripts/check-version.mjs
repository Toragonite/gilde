#!/usr/bin/env node
/**
 * check-version.mjs — one version, one license, everywhere they are written down.
 *
 * `package.json > version` is the source of truth. The same number has to appear
 * in seven places, because each tool reads its own file:
 *
 *   package.json                          npm (source of truth)
 *   package-lock.json                     npm (top-level "version" and packages[""])
 *   src-tauri/tauri.conf.json             Tauri: Info.plist CFBundleShortVersionString, DMG name
 *   src-tauri/Cargo.toml                  cargo / CARGO_PKG_VERSION in the binary
 *   src-tauri/Cargo.lock                  cargo (the gilde-code entry)
 *   src-tauri/sidecar/package.json        the sidecar's `mesh.hello` version (esbuild define)
 *   src-tauri/sidecar/package-lock.json   npm
 *
 * and the license (Apache-2.0) in package.json, the sidecar package.json,
 * Cargo.toml and tauri.conf.json > bundle.license, with LICENSE being the
 * Apache License 2.0 text.
 *
 *   node scripts/check-version.mjs            check; exit 1 on any drift
 *   node scripts/check-version.mjs --set X.Y.Z
 *                                            write X.Y.Z into all seven places, then check
 *
 * `tauri.conf.json > build.beforeBuildCommand` runs the check first, so a
 * release cannot be built from files that disagree.
 */

import { readFileSync, writeFileSync } from 'node:fs';
import { dirname, join } from 'node:path';
import { fileURLToPath } from 'node:url';

const root = join(dirname(fileURLToPath(import.meta.url)), '..');
const LICENSE_ID = 'Apache-2.0';
const SEMVER = /^\d+\.\d+\.\d+(?:-[0-9A-Za-z.-]+)?(?:\+[0-9A-Za-z.-]+)?$/;

const path = (rel) => join(root, rel);
const read = (rel) => readFileSync(path(rel), 'utf8');
const readJson = (rel) => JSON.parse(read(rel));

// [package] section of a Cargo.toml: from its header to the next table header.
const CARGO_PACKAGE = /(^\[package\]\n[\s\S]*?)(?=^\[)/m;
const CARGO_LOCK_ENTRY = /(\[\[package\]\]\nname = "gilde-code"\nversion = ")([^"]+)(")/;

function cargoField(text, field) {
  const pkg = CARGO_PACKAGE.exec(text);
  if (!pkg) return undefined;
  const m = new RegExp(`^${field} = "([^"]*)"`, 'm').exec(pkg[1]);
  return m ? m[1] : undefined;
}

function setVersion(version) {
  if (!SEMVER.test(version)) {
    console.error(`[check-version] "${version}" is not a semantic version (X.Y.Z[-pre][+build])`);
    process.exit(1);
  }
  // Textual edits, not JSON round trips: the files keep their formatting, and
  // check() below parses every one of them again.
  const topLevel = /^( {2}"version": ")[^"]*(")/m; // first 2-space-indented "version"
  const rootEntry = /^( {6}"version": ")[^"]*(")/m; // first 6-space one: packages[""]
  for (const rel of ['package.json', 'src-tauri/sidecar/package.json', 'src-tauri/tauri.conf.json']) {
    writeFileSync(path(rel), read(rel).replace(topLevel, `$1${version}$2`));
  }
  for (const rel of ['package-lock.json', 'src-tauri/sidecar/package-lock.json']) {
    writeFileSync(path(rel), read(rel).replace(topLevel, `$1${version}$2`).replace(rootEntry, `$1${version}$2`));
  }
  const toml = read('src-tauri/Cargo.toml');
  writeFileSync(
    path('src-tauri/Cargo.toml'),
    toml.replace(CARGO_PACKAGE, (pkg) => pkg.replace(/^version = "[^"]*"/m, `version = "${version}"`)),
  );
  const lock = read('src-tauri/Cargo.lock');
  writeFileSync(path('src-tauri/Cargo.lock'), lock.replace(CARGO_LOCK_ENTRY, `$1${version}$3`));
  console.log(`[check-version] wrote ${version} into all seven places`);
}

function check() {
  const pkg = readJson('package.json');
  const truth = pkg.version;
  const problems = [];

  const lock = readJson('package-lock.json');
  const sidecar = readJson('src-tauri/sidecar/package.json');
  const sidecarLock = readJson('src-tauri/sidecar/package-lock.json');
  const conf = readJson('src-tauri/tauri.conf.json');
  const toml = read('src-tauri/Cargo.toml');
  const cargoLock = CARGO_LOCK_ENTRY.exec(read('src-tauri/Cargo.lock'));

  const versions = [
    ['package.json', truth],
    ['package-lock.json "version"', lock.version],
    ['package-lock.json packages[""].version', lock.packages?.['']?.version],
    ['src-tauri/tauri.conf.json version', conf.version],
    ['src-tauri/Cargo.toml [package] version', cargoField(toml, 'version')],
    ['src-tauri/Cargo.lock gilde-code', cargoLock ? cargoLock[2] : undefined],
    ['src-tauri/sidecar/package.json', sidecar.version],
    ['src-tauri/sidecar/package-lock.json "version"', sidecarLock.version],
    ['src-tauri/sidecar/package-lock.json packages[""].version', sidecarLock.packages?.['']?.version],
  ];
  if (typeof truth !== 'string' || !SEMVER.test(truth)) {
    problems.push(`package.json version "${truth}" is not a semantic version`);
  }
  for (const [where, value] of versions) {
    if (value !== truth) problems.push(`${where} is ${JSON.stringify(value)}, package.json says ${JSON.stringify(truth)}`);
  }

  const licenses = [
    ['package.json license', pkg.license],
    ['src-tauri/sidecar/package.json license', sidecar.license],
    ['src-tauri/Cargo.toml license', cargoField(toml, 'license')],
    ['src-tauri/tauri.conf.json bundle.license', conf.bundle?.license],
  ];
  for (const [where, value] of licenses) {
    if (value !== LICENSE_ID) problems.push(`${where} is ${JSON.stringify(value)}, expected "${LICENSE_ID}"`);
  }
  let licenseText = '';
  try {
    licenseText = read('LICENSE');
  } catch {
    problems.push('LICENSE is missing');
  }
  if (licenseText && !/^\s*Apache License\s+Version 2\.0, January 2004/.test(licenseText)) {
    problems.push('LICENSE is not the Apache License 2.0 text');
  }

  if (problems.length > 0) {
    console.error(`[check-version] ${problems.length} problem(s):\n  - ${problems.join('\n  - ')}`);
    console.error('[check-version] fix with: node scripts/check-version.mjs --set <version>');
    process.exit(1);
  }
  console.log(`[check-version] ${truth} in all ${versions.length} places; license ${LICENSE_ID} in all ${licenses.length}`);
}

const setAt = process.argv.indexOf('--set');
if (setAt !== -1) {
  const version = process.argv[setAt + 1];
  if (!version) {
    console.error('[check-version] --set needs a version');
    process.exit(1);
  }
  setVersion(version);
}
check();
