/**
 * The ownership manifest of the Gilde 2 build (contract §15.1, §15.4).
 *
 * Parses the "Owns" column of the §15.2 task table into path patterns and
 * answers three questions about them:
 *   - who owns a path (`ownersOf`),
 *   - whether two patterns can match a common path (`patternsOverlap`, an exact
 *     intersection test, not a heuristic),
 *   - which Rust files sit under a parent module file owned by another task
 *     (`parentModuleRelations`, F-18).
 *
 * Parsing and matching reproduce the lead's reference implementation
 * (`owncheck.py`) exactly, so this module and that script always agree:
 *   - only rows that start `| **Txx**` count; the Owns column is the 4th cell;
 *   - every backtick token that contains "/" or "." is a pattern; `{a,b}` is
 *     expanded; anything after the first space is dropped;
 *   - `dir/**` = everything under dir; `dir/*` = direct files of dir; a token
 *     with `*`, `?` or `[` is an fnmatch glob (where `*` also crosses "/");
 *     anything else is an exact path.
 * Pure: no I/O. The CLI (`../ownership-check.mjs`) reads files and runs git.
 */

/** The task ids whose files T01 could touch as H1 edits in wave 0 (§15.1 H1). */
export const H1_TASKS = new Set([
  'T02', 'T03', 'T04', 'T05', 'T06', 'T07', 'T08', 'T09', 'T10', 'T11', 'T12', 'T13', 'T19', 'T21',
]);

/** Where tracked files that no task owns are reported (§15.4 "code roots"). */
export const CODE_ROOTS = ['src/', 'scripts/', 'src-tauri/src/'];

/** The tasks owned by wave 0/1 (§15.3); A8.2 measures `tsc` errors in their files. */
export const WAVE_01_TASKS = new Set([
  'T01', 'T02', 'T03', 'T04', 'T05', 'T06', 'T07', 'T08', 'T09', 'T10', 'T11', 'T12', 'T13', 'T19', 'T21',
]);

const SECTION_START = '### 15.2 Task table';
const SECTION_END = '### 15.3';

/** Expand the first `{a,b,…}` group recursively (no nesting, like owncheck.py). */
export function expandBraces(pattern) {
  const m = /\{([^{}]*)\}/.exec(pattern);
  if (!m) return [pattern];
  const out = [];
  for (const alt of m[1].split(',')) {
    out.push(...expandBraces(pattern.slice(0, m.index) + alt + pattern.slice(m.index + m[0].length)));
  }
  return out;
}

/**
 * The §15.2 table → `Map<task, Pattern[]>` in table order.
 * A `Pattern` is `{ task, pattern, token, deleted }`: `token` is the backtick
 * text it came from, `deleted` whether the contract marks it "(delete…)".
 * Throws when the section is missing (a renamed heading must not read as
 * "no owners").
 */
export function parseOwnership(contractText) {
  const start = contractText.indexOf(SECTION_START);
  const end = start < 0 ? -1 : contractText.indexOf(SECTION_END, start);
  if (start < 0 || end < 0) {
    throw new Error(`contract: section "${SECTION_START}" … "${SECTION_END}" not found`);
  }
  const section = contractText.slice(start, end);
  const tasks = new Map();
  for (const line of section.split('\n')) {
    const row = /^\|\s*\*\*(T\d\d)\*\*/.exec(line);
    if (!row) continue;
    const cells = line.split('|');
    if (cells.length < 5) continue;
    const owns = cells[4];
    const patterns = [];
    const tokenRe = /`([^`]+)`/g;
    let t;
    while ((t = tokenRe.exec(owns)) !== null) {
      const token = t[1].trim();
      if (!token.includes('/') && !token.includes('.')) continue;
      const after = owns.slice(t.index + t[0].length, t.index + t[0].length + 12);
      const deleted = /^\s*\(delete/.test(after);
      for (const expanded of expandBraces(token)) {
        patterns.push({ task: row[1], pattern: expanded.split(' ')[0], token, deleted });
      }
    }
    tasks.set(row[1], patterns);
  }
  if (tasks.size === 0) throw new Error('contract: the §15.2 table has no task rows');
  return tasks;
}

/** Python `fnmatch.fnmatchcase` as a RegExp (`*` and `?` also match "/"). */
export function fnmatchRegExp(pattern) {
  let re = '';
  for (let i = 0; i < pattern.length; i++) {
    const c = pattern[i];
    if (c === '*') re += '.*';
    else if (c === '?') re += '.';
    else if (c === '[') {
      const close = pattern.indexOf(']', i + 2);
      if (close < 0) {
        re += '\\[';
        continue;
      }
      let body = pattern.slice(i + 1, close);
      i = close;
      if (body.startsWith('!')) body = '^' + body.slice(1);
      re += '[' + body.replace(/\\/g, '\\\\') + ']';
    } else re += c.replace(/[.+^${}()|\\/]/g, '\\$&');
  }
  return new RegExp('^' + re + '$', 's');
}

/** Whether `path` (repo-relative, POSIX) matches `pattern` — owncheck.py's `match`. */
export function matchPattern(path, pattern) {
  if (pattern.endsWith('/**')) return path.startsWith(pattern.slice(0, -2));
  if (pattern.endsWith('/*')) {
    const dir = pattern.slice(0, -1);
    return path.startsWith(dir) && !path.slice(dir.length).includes('/');
  }
  if (/[*?[]/.test(pattern)) return fnmatchRegExp(pattern).test(path);
  return path === pattern;
}

/** Every task whose patterns match `path`, in table order. */
export function ownersOf(path, tasks) {
  const out = [];
  for (const [task, patterns] of tasks) {
    if (patterns.some((p) => matchPattern(path, p.pattern))) out.push(task);
  }
  return out;
}

// ---------------------------------------------------------------------------
// Exact pattern intersection
// ---------------------------------------------------------------------------
//
// Each pattern becomes a token list over characters: a literal character, ANY
// (one character, `?`), STAR (any string, fnmatch `*` and the `/**` tail),
// SEG (any string without "/", the `/*` tail) or a CLASS (`[…]`). Two token
// lists intersect iff some string matches both; `intersects` decides that by a
// memoized walk over position pairs (an emptiness test of the product
// automaton). No pattern in the contract nests deeper than this grammar.

const STAR = { kind: 'star' };
const SEG = { kind: 'seg' };
const ANY = { kind: 'any' };

/** A pattern as tokens with the exact semantics of `matchPattern`. */
export function tokenize(pattern) {
  if (pattern.endsWith('/**')) return [...pattern.slice(0, -2)].map(lit).concat([STAR]);
  if (pattern.endsWith('/*')) return [...pattern.slice(0, -1)].map(lit).concat([SEG]);
  if (!/[*?[]/.test(pattern)) return [...pattern].map(lit);
  const out = [];
  for (let i = 0; i < pattern.length; i++) {
    const c = pattern[i];
    if (c === '*') out.push(STAR);
    else if (c === '?') out.push(ANY);
    else if (c === '[' && pattern.indexOf(']', i + 2) >= 0) {
      const close = pattern.indexOf(']', i + 2);
      out.push({ kind: 'class', re: new RegExp('^' + fnmatchRegExp(pattern.slice(i, close + 1)).source.slice(1, -1) + '$', 's') });
      i = close;
    } else out.push(lit(c));
  }
  return out;
}

function lit(c) {
  return { kind: 'lit', c };
}

const isRepeat = (t) => t.kind === 'star' || t.kind === 'seg';

/** A character both tokens accept, or null when there is none. */
function commonChar(a, b) {
  // Candidates: either side's literal, else a neutral non-slash character.
  const candidates = [];
  if (a.kind === 'lit') candidates.push(a.c);
  if (b.kind === 'lit') candidates.push(b.c);
  candidates.push('x', '/', '_', '.', '-', '0');
  for (const c of candidates) if (accepts(a, c) && accepts(b, c)) return c;
  return null;
}

function accepts(token, c) {
  switch (token.kind) {
    case 'lit':
      return token.c === c;
    case 'star':
    case 'any':
      return true;
    case 'seg':
      return c !== '/';
    case 'class':
      return token.re.test(c);
  }
  return false;
}

/** Whether some string matches both token lists. */
export function intersects(a, b) {
  const memo = new Map();
  const visit = (i, j) => {
    const key = i * 100003 + j;
    if (memo.has(key)) return memo.get(key);
    memo.set(key, false); // cycles (both sides repeating) never lead anywhere new
    let ok = false;
    if (i === a.length && j === b.length) ok = true;
    if (!ok && i < a.length && isRepeat(a[i])) ok = visit(i + 1, j);
    if (!ok && j < b.length && isRepeat(b[j])) ok = visit(i, j + 1);
    if (!ok && i < a.length && j < b.length && commonChar(a[i], b[j]) !== null) {
      const ni = isRepeat(a[i]) ? i : i + 1;
      const nj = isRepeat(b[j]) ? j : j + 1;
      if (ni !== i || nj !== j) ok = visit(ni, nj);
    }
    memo.set(key, ok);
    return ok;
  };
  return visit(0, 0);
}

/** Whether two patterns can match a common path. */
export function patternsOverlap(p, q) {
  return intersects(tokenize(p), tokenize(q));
}

/** Every pair of overlapping patterns: `{ crossTask, intraTask }` lists of `[a, b]`. */
export function findOverlaps(tasks) {
  const all = [...tasks.values()].flat();
  const tokens = all.map((p) => tokenize(p.pattern));
  const crossTask = [];
  const intraTask = [];
  for (let i = 0; i < all.length; i++) {
    for (let j = i + 1; j < all.length; j++) {
      if (all[i].pattern === all[j].pattern && all[i].task === all[j].task) continue;
      if (!intersects(tokens[i], tokens[j])) continue;
      (all[i].task === all[j].task ? intraTask : crossTask).push([all[i], all[j]]);
    }
  }
  return { crossTask, intraTask };
}

// ---------------------------------------------------------------------------
// Tracked files
// ---------------------------------------------------------------------------

/** Tracked files against the manifest: owned by several tasks, and unowned under the code roots. */
export function classifyFiles(files, tasks, roots = CODE_ROOTS) {
  const multiOwned = [];
  const unownedInRoots = [];
  let owned = 0;
  for (const file of files) {
    const owners = ownersOf(file, tasks);
    if (owners.length > 1) multiOwned.push({ file, owners });
    if (owners.length > 0) owned++;
    else if (roots.some((r) => file.startsWith(r))) unownedInRoots.push(file);
  }
  return { total: files.length, owned, multiOwned, unownedInRoots };
}

// ---------------------------------------------------------------------------
// Parent-module relations (F-18)
// ---------------------------------------------------------------------------

const RUST_SRC = 'src-tauri/src/';

/**
 * File-level relations: an exact Rust file pattern under `src-tauri/src/`
 * (not `mod.rs`, `lib.rs`, `main.rs`, not under `bin/`) whose parent module
 * file has a different owner. The parent of `a/b.rs` is `a/mod.rs` (or `a.rs`
 * when only that one is known); a file directly in `src/` hangs off `lib.rs`.
 * Directory-level relations: every top-level module directory under
 * `src-tauri/src/` owned by a task other than `lib.rs`'s owner (its `mod` line
 * is in `lib.rs`). `files` (tracked paths) only decides `mod.rs` vs `a.rs`.
 */
export function parentModuleRelations(tasks, files = []) {
  const known = new Set(files);
  const exact = [...tasks.values()].flat().filter((p) => !/[*?[]/.test(p.pattern));
  for (const p of exact) known.add(p.pattern);

  const libOwners = ownersOf(RUST_SRC + 'lib.rs', tasks);
  const fileLevel = [];
  for (const p of exact) {
    const path = p.pattern;
    if (!path.startsWith(RUST_SRC) || !path.endsWith('.rs')) continue;
    const rel = path.slice(RUST_SRC.length);
    const base = rel.split('/').pop();
    if (rel.startsWith('bin/') || base === 'mod.rs' || rel === 'lib.rs' || rel === 'main.rs') continue;
    const dir = rel.includes('/') ? rel.slice(0, rel.lastIndexOf('/')) : '';
    let parent;
    if (dir === '') parent = RUST_SRC + 'lib.rs';
    else if (known.has(`${RUST_SRC}${dir}/mod.rs`) || !known.has(`${RUST_SRC}${dir}.rs`)) parent = `${RUST_SRC}${dir}/mod.rs`;
    else parent = `${RUST_SRC}${dir}.rs`;
    const parentOwners = ownersOf(parent, tasks);
    if (parentOwners.length === 0 || parentOwners.includes(p.task)) continue;
    fileLevel.push({ file: path, owner: p.task, parent, parentOwners });
  }

  // A top-level module directory whose root file (`<dir>/mod.rs`) belongs to
  // another task than lib.rs: its `mod` line lives in lib.rs.
  const dirLevel = [];
  const tops = new Set();
  for (const p of [...tasks.values()].flat()) {
    if (!p.pattern.startsWith(RUST_SRC)) continue;
    const rel = p.pattern.slice(RUST_SRC.length);
    if (rel.includes('/') && !rel.startsWith('bin/')) tops.add(rel.split('/')[0]);
  }
  for (const top of [...tops].sort()) {
    const root = `${RUST_SRC}${top}/mod.rs`;
    const rootOwners = ownersOf(root, tasks);
    if (rootOwners.length === 0 || rootOwners.some((o) => libOwners.includes(o))) continue;
    dirLevel.push({ dir: RUST_SRC + top + '/', owner: rootOwners.join(','), parent: RUST_SRC + 'lib.rs', parentOwners: libOwners });
  }
  return { fileLevel, dirLevel };
}

// ---------------------------------------------------------------------------
// Changed paths against one task
// ---------------------------------------------------------------------------

/**
 * Each changed path with its verdict for `task`: `own`, `h1` (owned only by H1
 * tasks, accepted with `h1: true`), `unowned` or `other`.
 */
export function checkChangedPaths(paths, task, tasks, { h1 = false } = {}) {
  const rows = [];
  for (const path of paths) {
    const owners = ownersOf(path, tasks);
    let verdict;
    if (owners.includes(task)) verdict = 'own';
    else if (h1 && owners.length > 0 && owners.every((o) => H1_TASKS.has(o))) verdict = 'h1';
    else if (owners.length === 0) verdict = 'unowned';
    else verdict = 'other';
    rows.push({ path, owners, verdict });
  }
  const outside = rows.filter((r) => r.verdict === 'unowned' || r.verdict === 'other');
  return { rows, outside };
}
