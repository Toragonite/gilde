#!/usr/bin/env node
// check-launch-sites.mjs — the launch-site allowlist of Gilde 2 (contract §9.3,
// enforcement 2; owner T11).
//
// A process may be created (`Command::new`, `portable_pty` spawns, `posix_spawn`,
// `fork`) only in the five launch sites of §9.3:
//
//   src-tauri/src/supervise/spawn.rs   (the grouped-spawn builders)
//   src-tauri/src/pty/manager.rs       (PTYs, PaneHost's internal API)
//   src-tauri/src/agents/path.rs       (the login-shell PATH probe)
//   src-tauri/src/worktree/git.rs      (git, the worktree post-create program)
//   src-tauri/src/ide/opener.rs        (the editor CLI / `open -b`)
//
// Only production code counts: comments, string literals and `#[cfg(test)]`
// items are stripped first, and `src/**/tests.rs` files are test modules.
//
// PENDING lists spawn sites in other owners' files that predate this check.
// Each entry pins the exact number of sites (a new one fails) and names the
// owner and the ruling that moves it into a launch site (A17.3: T09b makes
// `--strict` pass); they are
// reported as warnings, and `--strict` turns them into failures.
//
//   node scripts/check-launch-sites.mjs [--root <repo>] [--strict]
//   node scripts/check-launch-sites.mjs --self-test     (seeded violations)
//
// Exit 0 = clean, 1 = violations, 2 = usage or I/O error.

import fs from 'node:fs';
import os from 'node:os';
import path from 'node:path';
import { fileURLToPath } from 'node:url';

const ALLOWED = [
  'supervise/spawn.rs',
  'pty/manager.rs',
  'agents/path.rs',
  'worktree/git.rs',
  'ide/opener.rs',
];

/** file (relative to src-tauri/src) → { count, owner, why } */
const PENDING = {
  'agents/mod.rs': {
    count: 1,
    owner: 'T09',
    why: 'the `--version` probe spawns directly; T09b moves it behind supervise (A17.3)',
  },
  'supervise/procscan.rs': {
    count: 1,
    owner: 'T09',
    why: 'the lazy `ps`/`lsof` fallback spawns directly; T09b moves it behind supervise (A17.3)',
  },
  'commands/repo_identity.rs': {
    count: 1,
    owner: 'T03',
    why: 'the bounded git probe spawns directly; T09b moves it to worktree::git::exec (A17.3)',
  },
};

const SPAWN_PATTERNS = [
  { re: /\bCommand::new\s*\(/g, what: 'Command::new' },
  { re: /\bCommandBuilder::new\s*\(/g, what: 'portable_pty CommandBuilder' },
  { re: /\bnative_pty_system\s*\(/g, what: 'portable_pty native_pty_system' },
  { re: /\.spawn_command\s*\(/g, what: 'portable_pty spawn_command' },
  { re: /\bposix_spawnp?\s*\(/g, what: 'posix_spawn' },
  { re: /\b(?:libc|nix::unistd|unistd)::fork\s*\(/g, what: 'fork' },
];

// ---------------------------------------------------------------------------
// Rust source views
// ---------------------------------------------------------------------------

/**
 * Two views of `src` with identical offsets: `code` without comments (string
 * literals kept) and `bare` without comments **and** string/char contents.
 */
export function views(src) {
  const code = src.split('');
  const bare = src.split('');
  const blank = (arr, i) => {
    if (arr[i] !== '\n') arr[i] = ' ';
  };
  let i = 0;
  const n = src.length;
  while (i < n) {
    const c = src[i];
    const d = src[i + 1];
    if (c === '/' && d === '/') {
      while (i < n && src[i] !== '\n') {
        blank(code, i);
        blank(bare, i);
        i++;
      }
      continue;
    }
    if (c === '/' && d === '*') {
      let depth = 0;
      while (i < n) {
        if (src[i] === '/' && src[i + 1] === '*') {
          depth++;
          blank(code, i); blank(bare, i); blank(code, i + 1); blank(bare, i + 1);
          i += 2;
          continue;
        }
        if (src[i] === '*' && src[i + 1] === '/') {
          depth--;
          blank(code, i); blank(bare, i); blank(code, i + 1); blank(bare, i + 1);
          i += 2;
          if (depth === 0) break;
          continue;
        }
        blank(code, i); blank(bare, i);
        i++;
      }
      continue;
    }
    // raw strings r"…", r#"…"#, br"…"
    const raw = /^b?r(#*)"/.exec(src.slice(i, i + 12));
    if (raw && (i === 0 || !/[A-Za-z0-9_]/.test(src[i - 1]))) {
      const hashes = raw[1];
      const close = '"' + hashes;
      let j = i + raw[0].length;
      const end = src.indexOf(close, j);
      const stop = end < 0 ? n : end + close.length;
      for (let k = j; k < (end < 0 ? n : end); k++) blank(bare, k);
      i = stop;
      continue;
    }
    if (c === '"' || (c === 'b' && d === '"' && (i === 0 || !/[A-Za-z0-9_]/.test(src[i - 1])))) {
      let j = c === 'b' ? i + 2 : i + 1;
      while (j < n && src[j] !== '"') {
        if (src[j] === '\\') {
          blank(bare, j);
          j++;
        }
        blank(bare, j);
        j++;
      }
      i = j + 1;
      continue;
    }
    if (c === '\'') {
      // a char literal ('x', '\n', '\'', '"') — not a lifetime ('a)
      if (d === '\\') {
        let j = i + 2;
        while (j < n && src[j] !== '\'') j++;
        for (let k = i + 1; k < j; k++) blank(bare, k);
        i = j + 1;
        continue;
      }
      if (src[i + 2] === '\'') {
        blank(bare, i + 1);
        i += 3;
        continue;
      }
    }
    i++;
  }
  return { code: code.join(''), bare: bare.join('') };
}

function matchBrace(text, open) {
  let depth = 0;
  for (let i = open; i < text.length; i++) {
    if (text[i] === '{') depth++;
    else if (text[i] === '}') {
      depth--;
      if (depth === 0) return i;
    }
  }
  return text.length - 1;
}

/** Blank every `#[cfg(test)]` / `#[cfg(all(test, …))]` item in both views. */
export function stripTests(v) {
  const code = v.code.split('');
  const bare = v.bare.split('');
  const re = /#\s*\[\s*cfg\s*\(\s*(?:test\s*\)|all\s*\(\s*test\b)/g;
  let m;
  while ((m = re.exec(v.bare))) {
    const start = m.index;
    let j = start;
    while (j < v.bare.length && v.bare[j] !== '{' && v.bare[j] !== ';') j++;
    const end = v.bare[j] === '{' ? matchBrace(v.bare, j) : j;
    for (let k = start; k <= end && k < code.length; k++) {
      if (code[k] !== '\n') code[k] = ' ';
      if (bare[k] !== '\n') bare[k] = ' ';
    }
    re.lastIndex = end + 1;
  }
  return { code: code.join(''), bare: bare.join('') };
}

export function lineOf(text, offset) {
  let line = 1;
  for (let i = 0; i < offset && i < text.length; i++) if (text[i] === '\n') line++;
  return line;
}

export function rustFiles(dir) {
  const out = [];
  const walk = (d) => {
    let entries;
    try {
      entries = fs.readdirSync(d, { withFileTypes: true });
    } catch {
      return;
    }
    for (const e of entries) {
      const p = path.join(d, e.name);
      if (e.isDirectory()) {
        if (e.name === 'tests') continue;
        walk(p);
      } else if (e.isFile() && e.name.endsWith('.rs') && e.name !== 'tests.rs') {
        out.push(p);
      }
    }
  };
  walk(dir);
  return out.sort();
}

// ---------------------------------------------------------------------------
// the check
// ---------------------------------------------------------------------------

/** Scan `<root>/src-tauri/src`; returns `{ violations, pending }`. */
export function scan(root, pending = PENDING) {
  const src = path.join(root, 'src-tauri', 'src');
  const violations = [];
  const pendingHits = [];
  for (const file of rustFiles(src)) {
    const rel = path.relative(src, file).split(path.sep).join('/');
    const text = fs.readFileSync(file, 'utf8');
    const v = stripTests(views(text));
    const hits = [];
    for (const { re, what } of SPAWN_PATTERNS) {
      re.lastIndex = 0;
      let m;
      while ((m = re.exec(v.bare))) hits.push({ line: lineOf(text, m.index), what });
    }
    if (hits.length === 0 || ALLOWED.includes(rel)) continue;
    const p = pending[rel];
    if (p && hits.length <= p.count) {
      pendingHits.push({ rel, hits, ...p });
      continue;
    }
    for (const h of hits) violations.push({ rel, ...h, pinned: p ? p.count : 0 });
  }
  return { violations, pending: pendingHits };
}

function report(result, strict) {
  for (const p of result.pending) {
    const where = p.hits.map((h) => `${p.rel}:${h.line}`).join(', ');
    console.error(`check-launch-sites: ${strict ? 'FAIL' : 'pending'} (${p.owner}) ${where} — ${p.why}`);
  }
  for (const v of result.violations) {
    const extra = v.pinned ? ` (more than the ${v.pinned} pinned site(s))` : '';
    console.error(`check-launch-sites: FORBIDDEN ${v.what} outside the §9.3 launch sites${extra}: src-tauri/src/${v.rel}:${v.line}`);
  }
  const failed = result.violations.length > 0 || (strict && result.pending.length > 0);
  if (failed) {
    console.error(`check-launch-sites: launches are allowed only in ${ALLOWED.map((a) => 'src-tauri/src/' + a).join(', ')}`);
    return 1;
  }
  console.log(`check-launch-sites: clean (${result.pending.length} pending site file(s) pinned)`);
  return 0;
}

// ---------------------------------------------------------------------------
// self-test: seeded violations must fail, clean trees must pass
// ---------------------------------------------------------------------------

function seed(files) {
  const root = fs.mkdtempSync(path.join(os.tmpdir(), 'launch-sites-'));
  for (const [rel, body] of Object.entries(files)) {
    const p = path.join(root, 'src-tauri', 'src', rel);
    fs.mkdirSync(path.dirname(p), { recursive: true });
    fs.writeFileSync(p, body);
  }
  return root;
}

function selfTest() {
  const cases = [
    {
      name: 'a launch in an allowed site passes',
      files: { 'worktree/git.rs': 'fn f() { let c = tokio::process::Command::new(git_binary()); }\n' },
      ok: true,
    },
    {
      name: 'a seeded Command::new elsewhere fails',
      files: { 'world/x.rs': 'fn f() { std::process::Command::new("/bin/ls").spawn(); }\n' },
      ok: false,
    },
    {
      name: 'a seeded portable_pty spawn elsewhere fails',
      files: { 'panehost/panes.rs': 'fn f() { let pty = native_pty_system(); let c = CommandBuilder::new("x"); }\n' },
      ok: false,
    },
    {
      name: 'posix_spawn and fork are launches too',
      files: { 'notify/x.rs': 'unsafe fn f() { libc::fork(); }\n' },
      ok: false,
    },
    {
      name: 'comments, strings and test modules do not count',
      files: {
        'world/y.rs': [
          '// Command::new("x") in prose',
          'const S: &str = "Command::new(";',
          '/* native_pty_system() */',
          'fn f() {}',
          '#[cfg(test)]',
          'mod tests {',
          '    fn t() { std::process::Command::new("git"); }',
          '}',
          '',
        ].join('\n'),
        'supervise/health/tests.rs': 'fn t() { std::process::Command::new("x"); }\n',
      },
      ok: true,
    },
    {
      name: 'a pending file within its pinned count passes',
      files: { 'agents/mod.rs': 'fn f() { Command::new(binary); }\n' },
      ok: true,
    },
    {
      name: 'a pending file with one more site fails',
      files: { 'agents/mod.rs': 'fn f() { Command::new(a); Command::new(b); }\n' },
      ok: false,
    },
    {
      name: 'a pending file fails under --strict',
      files: { 'agents/mod.rs': 'fn f() { Command::new(binary); }\n' },
      ok: false,
      strict: true,
    },
  ];
  let failed = 0;
  for (const c of cases) {
    const root = seed(c.files);
    const r = scan(root);
    const ok = r.violations.length === 0 && !(c.strict && r.pending.length > 0);
    fs.rmSync(root, { recursive: true, force: true });
    const pass = ok === c.ok;
    if (!pass) failed++;
    console.log(`${pass ? 'ok  ' : 'FAIL'} ${c.name}`);
  }
  console.log(failed ? `check-launch-sites self-test: ${failed} case(s) failed` : 'check-launch-sites self-test: all cases pass');
  return failed ? 1 : 0;
}

// ---------------------------------------------------------------------------

function main(argv) {
  if (argv.includes('--self-test')) return selfTest();
  let root = path.resolve(path.dirname(fileURLToPath(import.meta.url)), '..');
  const i = argv.indexOf('--root');
  if (i >= 0) {
    if (!argv[i + 1]) {
      console.error('usage: check-launch-sites.mjs [--root <repo>] [--strict] | --self-test');
      return 2;
    }
    root = path.resolve(argv[i + 1]);
  }
  if (!fs.existsSync(path.join(root, 'src-tauri', 'src'))) {
    console.error(`check-launch-sites: no src-tauri/src under ${root}`);
    return 2;
  }
  return report(scan(root), argv.includes('--strict'));
}

if (process.argv[1] && path.resolve(process.argv[1]) === fileURLToPath(import.meta.url)) {
  process.exit(main(process.argv.slice(2)));
}
