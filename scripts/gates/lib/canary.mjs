/**
 * The F-03 canary fixture (`../fixtures/hostile-argv.json`): hostile argv
 * vectors and an env-value canary for T-priv-argv and the T-priv-3 env
 * variant (contract §9.7, §14.2), plus the scan that proves a canary never
 * reached an output.
 *
 * `loadCanaries()` returns the canary values, the argv vectors and the env
 * with every `${NAME}` placeholder replaced. `scanForCanaries(paths, values)`
 * searches files and directories byte-wise (SQLite pages, logs, JSON exports
 * alike), also in the UTF-16LE form a binary store might use, and reports
 * which canary names were found in which files — never the bytes around them.
 */

import { readFileSync, readdirSync, statSync } from 'node:fs';
import { dirname, join } from 'node:path';
import { fileURLToPath } from 'node:url';

export const FIXTURE = join(dirname(fileURLToPath(import.meta.url)), '..', 'fixtures', 'hostile-argv.json');

function substitute(value, canaries) {
  if (typeof value === 'string') return value.replace(/\$\{([A-Z_]+)\}/g, (m, name) => (name in canaries ? canaries[name] : m));
  if (Array.isArray(value)) return value.map((v) => substitute(v, canaries));
  if (value && typeof value === 'object') return Object.fromEntries(Object.entries(value).map(([k, v]) => [k, substitute(v, canaries)]));
  return value;
}

export function loadCanaries(path = FIXTURE) {
  const raw = JSON.parse(readFileSync(path, 'utf8'));
  const canaries = raw.canaries;
  return {
    canaries,
    argv: raw.argv.map((a) => ({ name: a.name, argv: substitute(a.argv, canaries) })),
    env: substitute(raw.env, canaries),
  };
}

/** The distinctive part of each canary to search for (the shared marker would match innocent text). */
export function needles(canaries) {
  return Object.entries(canaries).map(([name, value]) => {
    const m = /gildecanary[\w-]*|AKIAGILDECANARY\w*/i.exec(value);
    return { name, needle: m ? m[0] : value };
  });
}

/** Every file under `paths` (files or dirs), skipping nothing; symlinks are not followed. */
function walk(paths) {
  const out = [];
  const visit = (p) => {
    let st;
    try {
      st = statSync(p, { throwIfNoEntry: false });
    } catch {
      return;
    }
    if (!st) return;
    if (st.isDirectory()) for (const n of readdirSync(p)) visit(join(p, n));
    else if (st.isFile()) out.push(p);
  };
  for (const p of paths) visit(p);
  return out;
}

/** `[{ file, names }]` for every file that holds a canary (UTF-8 or UTF-16LE). */
export function scanForCanaries(paths, canaries) {
  const list = needles(canaries).map(({ name, needle }) => ({ name, utf8: Buffer.from(needle, 'utf8'), utf16: Buffer.from(needle, 'utf16le') }));
  const hits = [];
  for (const file of walk(paths)) {
    let buf;
    try {
      buf = readFileSync(file);
    } catch {
      continue;
    }
    const names = list.filter((c) => buf.includes(c.utf8) || buf.includes(c.utf16)).map((c) => c.name);
    if (names.length) hits.push({ file, names });
  }
  return hits;
}
