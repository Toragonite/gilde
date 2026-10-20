#!/usr/bin/env node
/**
 * third-party-notices.mjs — generate THIRD_PARTY_NOTICES.md: every third-party
 * component a built Gilde Code.app redistributes, with its license and the
 * license / notice texts its authors ship.
 *
 * What counts as "redistributed", per ecosystem:
 *
 *   * Rust crates — `cargo metadata --filter-platform aarch64-apple-darwin
 *     --locked` (src-tauri/), walked from the `gilde-code` package over normal
 *     and build edges; dev-dependencies are left out. Build-only crates (proc
 *     macros, build scripts) are included on purpose: listing a little more than
 *     ships is safe, listing less is not.
 *   * npm, user interface — package-lock.json, every entry not marked `dev` or
 *     `devOptional` (what `npm ci --omit=dev` would install; Vite bundles the
 *     subset the UI imports into dist/, which is embedded in the binary).
 *   * npm, sidecar — src-tauri/sidecar/package-lock.json, same rule (esbuild
 *     bundles them into Contents/Resources/sidecar/sidecar.cjs).
 *   * The Node.js runtime itself is pointed to, not inlined: its 115 KB LICENSE
 *     ships verbatim at Contents/Resources/licenses/node-LICENSE
 *     (scripts/fetch-node.sh extracts it from the checksum-verified tarball).
 *
 * License texts are read from the installed sources — `~/.cargo/registry/src/…`
 * (via each package's manifest_path) and node_modules — so it needs
 * `npm ci` in both npm roots first, and cargo needs the crates in its cache
 * (any earlier `cargo build`, or `cargo fetch`). It makes no network request of
 * its own; with `--offline` cargo is told not to either.
 *
 * Output is deterministic (sorted, no timestamps, no absolute paths), so the
 * committed file can be checked:
 *
 *   node scripts/third-party-notices.mjs            regenerate THIRD_PARTY_NOTICES.md
 *   node scripts/third-party-notices.mjs --check    exit 1 if it is out of date
 *   node scripts/third-party-notices.mjs --offline  pass --offline to cargo
 */

import { execFileSync } from 'node:child_process';
import { createHash } from 'node:crypto';
import { existsSync, readFileSync, readdirSync, statSync, writeFileSync } from 'node:fs';
import { homedir } from 'node:os';
import { dirname, join } from 'node:path';
import { fileURLToPath } from 'node:url';

const repoRoot = join(dirname(fileURLToPath(import.meta.url)), '..');
const outFile = join(repoRoot, 'THIRD_PARTY_NOTICES.md');
const check = process.argv.includes('--check');
const offline = process.argv.includes('--offline');
const TARGET = 'aarch64-apple-darwin';

const die = (msg) => {
  console.error(`[third-party-notices] ${msg}`);
  process.exit(1);
};

// Files a package ships that carry its license or notice text. Top level only.
const LICENSE_FILE = /^(licen[cs]e|copying|notice|unlicense|copyright)([-._ ][^/]*)?$/i;

function licenseTexts(dir) {
  if (!existsSync(dir)) return null;
  const names = readdirSync(dir)
    .filter((n) => LICENSE_FILE.test(n) && statSync(join(dir, n)).isFile())
    // Not a license: e.g. `license.rs`, `LICENSE-generator.py`.
    .filter((n) => !/\.(rs|js|mjs|cjs|ts|py|sh|json|toml|ya?ml|html?)$/i.test(n))
    .sort();
  return names.map((n) => normalize(readFileSync(join(dir, n), 'utf8')));
}

function normalize(text) {
  return text
    .replace(/^﻿/, '')
    .replace(/\r\n?/g, '\n')
    .split('\n')
    .map((l) => l.replace(/[ \t]+$/, ''))
    .join('\n')
    .replace(/^\n+/, '')
    .replace(/\n+$/, '');
}

const byNameVersion = (a, b) =>
  a.name.localeCompare(b.name, 'en') || a.version.localeCompare(b.version, 'en', { numeric: true });

// ---------------------------------------------------------------------------
// Rust
// ---------------------------------------------------------------------------

function rustPackages() {
  const args = ['metadata', '--format-version', '1', '--filter-platform', TARGET, '--locked'];
  if (offline) args.push('--offline');
  let raw;
  try {
    raw = execFileSync('cargo', args, {
      cwd: join(repoRoot, 'src-tauri'),
      stdio: ['ignore', 'pipe', 'inherit'],
      maxBuffer: 256 * 1024 * 1024,
    });
  } catch (err) {
    die(`cargo metadata failed (${err.message.split('\n')[0]})`);
  }
  const meta = JSON.parse(raw);
  const pkgs = new Map(meta.packages.map((p) => [p.id, p]));
  const nodes = new Map(meta.resolve.nodes.map((n) => [n.id, n]));
  const root = meta.resolve.root;
  const seen = new Set();
  const stack = [root];
  while (stack.length > 0) {
    const node = nodes.get(stack.pop());
    for (const dep of node.deps) {
      if (seen.has(dep.pkg)) continue;
      if (!dep.dep_kinds.some((k) => k.kind !== 'dev')) continue;
      seen.add(dep.pkg);
      stack.push(dep.pkg);
    }
  }
  seen.delete(root);
  return [...seen].map((id) => {
    const p = pkgs.get(id);
    const dir = dirname(p.manifest_path);
    const texts = licenseTexts(dir) ?? [];
    return {
      name: p.name,
      version: p.version,
      license: p.license ?? (p.license_file ? `see ${p.license_file}` : 'UNSPECIFIED'),
      source: p.repository ?? `https://crates.io/crates/${p.name}`,
      texts,
    };
  });
}

// ---------------------------------------------------------------------------
// npm
// ---------------------------------------------------------------------------

function npmPackages(root, label) {
  const lockPath = join(root, 'package-lock.json');
  const lock = JSON.parse(readFileSync(lockPath, 'utf8'));
  const out = [];
  for (const [key, entry] of Object.entries(lock.packages ?? {})) {
    if (key === '' || entry.dev || entry.devOptional || entry.link) continue;
    const name = entry.name ?? key.slice(key.lastIndexOf('node_modules/') + 'node_modules/'.length);
    const dir = join(root, key);
    if (!existsSync(dir)) {
      // An optional dependency for another platform is legitimately absent.
      if (entry.optional) continue;
      die(`${label}: ${key} is not installed — run \`npm ci\` in ${root === repoRoot ? 'the repository root' : 'src-tauri/sidecar'} first`);
    }
    const manifest = JSON.parse(readFileSync(join(dir, 'package.json'), 'utf8'));
    let source = manifest.repository;
    if (source && typeof source === 'object') source = source.url;
    out.push({
      name,
      version: entry.version,
      license: entry.license ?? manifest.license ?? 'UNSPECIFIED',
      source: source ? String(source).replace(/^git\+/, '') : `https://www.npmjs.com/package/${name}`,
      texts: licenseTexts(dir) ?? [],
    });
  }
  return out;
}

// ---------------------------------------------------------------------------
// render
// ---------------------------------------------------------------------------

function render(sections) {
  const textIds = new Map(); // hash -> { id, text, users: [] }
  const ref = (pkg) => {
    const ids = [];
    for (const text of pkg.texts) {
      const hash = createHash('sha256').update(text).digest('hex');
      let entry = textIds.get(hash);
      if (!entry) {
        entry = { id: `T${textIds.size + 1}`, text, users: [] };
        textIds.set(hash, entry);
      }
      entry.users.push(`${pkg.name} ${pkg.version}`);
      ids.push(entry.id);
    }
    return ids.length > 0 ? ids.join(', ') : '—';
  };
  const cell = (s) => String(s).replace(/\|/g, '\\|').replace(/\n/g, ' ');

  const lines = [];
  lines.push('# Third-party notices');
  lines.push('');
  lines.push('Gilde Code is licensed under the Apache License, Version 2.0 (`LICENSE`, `NOTICE`).');
  lines.push('A built `Gilde Code.app` also contains the third-party software listed here. This');
  lines.push('file ships inside the app at `Contents/Resources/licenses/THIRD_PARTY_NOTICES.md`.');
  lines.push('');
  lines.push('Generated by `node scripts/third-party-notices.mjs` from `src-tauri/Cargo.lock` (target');
  lines.push(`\`${TARGET}\`, dev-dependencies excluded), \`package-lock.json\` and`);
  lines.push('`src-tauri/sidecar/package-lock.json` (dev-dependencies excluded). Do not edit by hand;');
  lines.push('`node scripts/third-party-notices.mjs --check` fails when it is out of date.');
  lines.push('');
  lines.push('## Node.js runtime');
  lines.push('');
  lines.push('The app bundles the official Node.js v20.19.2 `darwin-arm64` binary as');
  lines.push('`Contents/MacOS/gilde-node`. Node.js is licensed under the MIT license; its full');
  lines.push('LICENSE, including the notices of the components it links statically (V8, OpenSSL,');
  lines.push('ICU, libuv, and others), ships unmodified at');
  lines.push('`Contents/Resources/licenses/node-LICENSE`.');
  lines.push('');

  for (const { title, intro, packages } of sections) {
    lines.push(`## ${title}`);
    lines.push('');
    lines.push(intro);
    lines.push('');
    lines.push('| Package | Version | License | Source | Texts |');
    lines.push('|---|---|---|---|---|');
    for (const pkg of [...packages].sort(byNameVersion)) {
      lines.push(`| ${cell(pkg.name)} | ${cell(pkg.version)} | ${cell(pkg.license)} | ${cell(pkg.source)} | ${ref(pkg)} |`);
    }
    lines.push('');
  }

  lines.push('## License and notice texts');
  lines.push('');
  lines.push('Each text below is reproduced as the package ships it; identical texts are listed');
  lines.push('once with every package that ships them. "—" in a table means the package ships no');
  lines.push('license file; its license is the SPDX expression in its row.');
  lines.push('');
  for (const { id, text, users } of textIds.values()) {
    lines.push(`### ${id}`);
    lines.push('');
    lines.push(`Shipped by: ${users.join(', ')}`);
    lines.push('');
    const longest = Math.max(2, ...[...text.matchAll(/`+/g)].map((m) => m[0].length));
    const fence = '`'.repeat(longest + 1);
    lines.push(`${fence}text`);
    lines.push(text);
    lines.push(fence);
    lines.push('');
  }
  return lines.join('\n').replace(/\n+$/, '\n');
}

const sections = [
  {
    title: 'npm packages — user interface',
    intro: 'Production dependencies of `package.json`, bundled by Vite into the embedded web UI.',
    packages: npmPackages(repoRoot, 'user interface'),
  },
  {
    title: 'npm packages — Node sidecar',
    intro:
      'Production dependencies of `src-tauri/sidecar/package.json`, bundled by esbuild into `Contents/Resources/sidecar/sidecar.cjs`.',
    packages: npmPackages(join(repoRoot, 'src-tauri', 'sidecar'), 'sidecar'),
  },
  {
    title: 'Rust crates',
    intro: 'Crates compiled into `Contents/MacOS/gilde` (normal and build dependencies).',
    packages: rustPackages(),
  },
];

const output = render(sections);

// The file is published: it must not carry a path from the machine it was made on.
for (const leak of [repoRoot, homedir()]) {
  if (output.includes(leak)) die(`refusing to write: the output contains a local path (${leak})`);
}

if (check) {
  const current = existsSync(outFile) ? readFileSync(outFile, 'utf8') : '';
  if (current !== output) {
    die('THIRD_PARTY_NOTICES.md is out of date — run `node scripts/third-party-notices.mjs` and commit the result');
  }
  console.log('[third-party-notices] THIRD_PARTY_NOTICES.md is up to date');
} else {
  writeFileSync(outFile, output);
  const counts = sections.map((s) => `${s.packages.length} ${s.title.replace(/^.*— /, '').replace(/^Rust crates$/, 'crates')}`);
  console.log(`[third-party-notices] wrote THIRD_PARTY_NOTICES.md (${counts.join(', ')}; ${Math.round(output.length / 1024)} KB)`);
}
