#!/usr/bin/env node
// check-vendor-writes.mjs — vendor directories are never written (Gilde 2
// contract §9.10, A11.5; owner T11).
//
// Two rules over production Rust code (comments, string contents and
// `#[cfg(test)]` items stripped; `src/**/tests.rs` are test modules):
//
//  1. **No write at all** in `src-tauri/src/discovery/`, `src-tauri/src/follow/`
//     and `src-tauri/src/world/teams/` — `fs::write/rename/copy/remove_*/
//     create_dir*/set_permissions`, `File::create`, writing `OpenOptions`,
//     `DirBuilder`, symlinks, libc write/unlink/rename/mkdir/clonefile/…,
//     `libc::open` with a write flag, read-write SQLite opens.
//     **One exemption (A11.5):** in `discovery/sqlite.rs` only, the region from
//     `pub fn vendorcopy(` through the marker comment
//     `// -- end of the one write site` (which includes its private
//     `clone_file`). The helper checks at run time that every path lies under
//     `<app_data>/tmp/vendorcopy/<uuid>/`. Anything after the marker, the same
//     helper in another file, or the helper without its marker stays rejected.
//  2. **Anywhere in `src-tauri/src/`:** a write whose path argument is built
//     from a vendor root — `.claude*`, `.codex`, `.cursor`, `.gemini`,
//     `claude-code-orchestrator`, `CLAUDE_CONFIG_DIR`, `CODEX_HOME`, a
//     `config_dir`/`cfg_dir`/`vendor_root` value — directly or through a `let`
//     binding of such a value in the same file.
//
//   node scripts/check-vendor-writes.mjs [--root <repo>]
//   node scripts/check-vendor-writes.mjs --self-test   (seeded violations and
//                                                      the A11.5 fixtures)
//
// Exit 0 = clean, 1 = violations, 2 = usage or I/O error.

import fs from 'node:fs';
import os from 'node:os';
import path from 'node:path';
import { fileURLToPath } from 'node:url';

import { lineOf, rustFiles, stripTests, views } from './check-launch-sites.mjs';

/** Directories where no write is allowed at all (rule 1). */
const NO_WRITE_DIRS = ['discovery/', 'follow/', 'world/teams/'];
/** The one exempt helper (A11.5). */
const EXEMPT_FILE = 'discovery/sqlite.rs';
const EXEMPT_START = 'pub fn vendorcopy(';
const EXEMPT_END = '// -- end of the one write site';

const VENDOR_TOKENS = /\.claude|\.codex\b|\.cursor\b|\.gemini\b|claude-code-orchestrator|CLAUDE_CONFIG_DIR|CODEX_HOME|\bconfig_dir\b|\bcfg_dir\b|\bvendor_root/;

/** A write operation; `args: true` means the path is the call's arguments. */
const WRITE_OPS = [
  { re: /\b(?:std::|tokio::)?fs::(?:write|rename|copy|hard_link|soft_link|remove_file|remove_dir|remove_dir_all|create_dir|create_dir_all|set_permissions)\s*\(/g, what: 'fs write' },
  { re: /\bFile::(?:create|create_new|options)\s*\(/g, what: 'File::create' },
  { re: /\bOpenOptions::new\s*\(\s*\)/g, what: 'OpenOptions', chain: true },
  { re: /\bDirBuilder::new\s*\(\s*\)/g, what: 'DirBuilder', chain: true, always: true },
  { re: /(?<![A-Za-z0-9_])symlink\s*\(/g, what: 'symlink' },
  { re: /\blibc::(?:unlink|unlinkat|rename|renameat|renamex_np|mkdir|mkdirat|rmdir|clonefile|clonefileat|fclonefileat|copyfile|fcopyfile|link|linkat|symlink|symlinkat|chmod|fchmod|chown|fchown|truncate|ftruncate|write|pwrite|writev|utimes|futimes)\s*\(/g, what: 'libc write' },
  { re: /\blibc::open(?:at)?\s*\(/g, what: 'libc::open', flags: true },
  { re: /\bConnection::open\s*\(/g, what: 'read-write SQLite open' },
  { re: /\bSQLITE_OPEN_(?:READ_WRITE|CREATE)\b/g, what: 'read-write SQLite flag', bareOnly: true },
  { re: /\.set_len\s*\(/g, what: 'set_len' },
];

function matchParen(text, open) {
  let depth = 0;
  for (let i = open; i < text.length; i++) {
    if (text[i] === '(') depth++;
    else if (text[i] === ')') {
      depth--;
      if (depth === 0) return i;
    }
  }
  return text.length - 1;
}

/** End of the statement starting at `i` (the next `;` or `{`/`}` at depth 0). */
function statementEnd(text, i) {
  let depth = 0;
  for (let j = i; j < text.length; j++) {
    const c = text[j];
    if (c === '(' || c === '[') depth++;
    else if (c === ')' || c === ']') depth--;
    else if (depth <= 0 && (c === ';' || c === '{' || c === '}')) return j;
  }
  return text.length;
}

/** `let` bindings whose initializer names a vendor root (per file). */
function taintedNames(code) {
  const names = new Set();
  const re = /\blet\s+(?:mut\s+)?([A-Za-z_][A-Za-z0-9_]*)\s*(?::[^=;]+)?=/g;
  let m;
  while ((m = re.exec(code))) {
    const end = statementEnd(code, re.lastIndex);
    if (VENDOR_TOKENS.test(code.slice(re.lastIndex, end))) names.add(m[1]);
  }
  return names;
}

function exemptRange(rel, raw) {
  if (rel !== EXEMPT_FILE) return null;
  const s = raw.indexOf(EXEMPT_START);
  if (s < 0) return null;
  const e = raw.indexOf(EXEMPT_END, s);
  if (e < 0) return null;
  return [s, e + EXEMPT_END.length];
}

/** The text a write op touches: its arguments, or for a builder chain the
 *  whole statement (flags and the `.open(…)` path). */
function opSpan(v, m, op) {
  if (op.bareOnly) return [m.index, m.index + m[0].length];
  if (op.chain) return [m.index, statementEnd(v.bare, m.index + m[0].length)];
  const open = v.bare.indexOf('(', m.index + m[0].length - 1);
  return [open, matchParen(v.bare, open)];
}

export function scan(root) {
  const src = path.join(root, 'src-tauri', 'src');
  const violations = [];
  for (const file of rustFiles(src)) {
    const rel = path.relative(src, file).split(path.sep).join('/');
    const raw = fs.readFileSync(file, 'utf8');
    const v = stripTests(views(raw));
    const tainted = taintedNames(v.code);
    const noWrite = NO_WRITE_DIRS.some((d) => rel.startsWith(d));
    const exempt = exemptRange(rel, raw);
    for (const op of WRITE_OPS) {
      op.re.lastIndex = 0;
      let m;
      while ((m = op.re.exec(v.bare))) {
        const [a, b] = opSpan(v, m, op);
        const span = v.code.slice(a, b + 1);
        if (op.chain && !op.always && !/\.(?:write|append|create|create_new|truncate)\s*\(\s*true\s*\)/.test(span)) continue;
        if (op.flags && !/\b(?:O_WRONLY|O_RDWR|O_CREAT|O_TRUNC|O_APPEND)\b/.test(span)) continue;
        const line = lineOf(raw, m.index);
        const inExempt = exempt && m.index >= exempt[0] && m.index <= exempt[1];
        if (noWrite && !inExempt) {
          violations.push({ rel, line, what: op.what, rule: `no write in ${NO_WRITE_DIRS.find((d) => rel.startsWith(d))}` });
          continue;
        }
        const viaName = [...tainted].find((n) => new RegExp(`\\b${n}\\b`).test(span));
        if (VENDOR_TOKENS.test(span) || viaName) {
          violations.push({ rel, line, what: op.what, rule: viaName ? `path built from a vendor root (via \`${viaName}\`)` : 'path built from a vendor root' });
        }
      }
    }
  }
  return violations;
}

function report(violations) {
  for (const x of violations) {
    console.error(`check-vendor-writes: FORBIDDEN ${x.what} — ${x.rule}: src-tauri/src/${x.rel}:${x.line}`);
  }
  if (violations.length) {
    console.error('check-vendor-writes: vendor directories are never written (§9.10); the one write site of discovery is discovery/sqlite.rs `vendorcopy` (A11.5).');
    return 1;
  }
  console.log('check-vendor-writes: clean');
  return 0;
}

// ---------------------------------------------------------------------------
// self-test (the A11.5 positive and negative fixtures, plus seeded violations)
// ---------------------------------------------------------------------------

const HELPER = [
  'pub fn vendorcopy_dir(app_data: &Path) -> PathBuf { app_data.join("tmp").join("vendorcopy") }',
  '',
  '/// The one write/delete site.',
  'pub fn vendorcopy(app_data: &Path, op: CopyOp<\'_>) -> Result<Option<PathBuf>, SqliteError> {',
  '    std::fs::DirBuilder::new().mode(0o700).create(&dir).map_err(|_| SqliteError::CopyFailed)?;',
  '    let _ = std::fs::remove_dir_all(&dir);',
  '    Ok(None)',
  '}',
  '',
  'fn clone_file(root: &Root, src: &Path, dst: &Path) -> Result<(), SqliteError> {',
  '    if unsafe { libc::clonefile(s.as_ptr(), d.as_ptr(), 1) } == 0 { return Ok(()); }',
  '    std::fs::write(dst, &*bytes).map_err(|_| SqliteError::CopyFailed)',
  '}',
  '// -- end of the one write site ------------------------------------------------',
  '',
].join('\n');

function seed(files) {
  const root = fs.mkdtempSync(path.join(os.tmpdir(), 'vendor-writes-'));
  for (const [rel, body] of Object.entries(files)) {
    const p = path.join(root, 'src-tauri', 'src', rel);
    fs.mkdirSync(path.dirname(p), { recursive: true });
    fs.writeFileSync(p, body);
  }
  return root;
}

function selfTest() {
  const cases = [
    { name: 'A11.5 positive: the vendorcopy helper (with clone_file) is the one exempt write site', files: { 'discovery/sqlite.rs': HELPER }, ok: true },
    {
      name: 'A11.5 negative: a write after the end marker is rejected',
      files: { 'discovery/sqlite.rs': HELPER + 'fn later(p: &Path) { std::fs::write(p, b"x").ok(); }\n' },
      ok: false,
    },
    { name: 'A11.5 negative: the same helper in another discovery file is not exempt', files: { 'discovery/codexdb.rs': HELPER }, ok: false },
    {
      name: 'A11.5 negative: the helper without its marker is not exempt',
      files: { 'discovery/sqlite.rs': HELPER.replace('// -- end of the one write site', '// end') },
      ok: false,
    },
    {
      name: 'A11.5 negative: a second write helper before vendorcopy is rejected',
      files: { 'discovery/sqlite.rs': 'fn sneak(p: &Path) { let _ = std::fs::remove_file(p); }\n' + HELPER },
      ok: false,
    },
    { name: 'a writing OpenOptions in follow/ is rejected', files: { 'follow/tail.rs': 'fn f(p: &Path) { let f = std::fs::OpenOptions::new()\n    .append(true)\n    .open(p); }\n' }, ok: false },
    { name: 'a read-only OpenOptions in follow/ passes', files: { 'follow/tail.rs': 'fn f(p: &Path) { let f = std::fs::OpenOptions::new().read(true).open(p); }\n' }, ok: true },
    { name: 'a rename in world/teams/ is rejected', files: { 'world/teams/claude.rs': 'fn f(a: &Path, b: &Path) { std::fs::rename(a, b).ok(); }\n' }, ok: false },
    { name: 'libc::open with O_CREAT in discovery is rejected', files: { 'discovery/x.rs': 'unsafe fn f(c: *const i8) { libc::open(c, libc::O_WRONLY | libc::O_CREAT); }\n' }, ok: false },
    { name: 'libc::open read-only in discovery passes', files: { 'discovery/x.rs': 'unsafe fn f(c: *const i8) { libc::open(c, libc::O_RDONLY | libc::O_NOFOLLOW); }\n' }, ok: true },
    { name: 'a read-write SQLite open in discovery is rejected', files: { 'discovery/codex.rs': 'fn f(p: &Path) { let c = rusqlite::Connection::open(p); }\n' }, ok: false },
    { name: 'a write into ~/.claude anywhere is rejected', files: { 'accounts/mod.rs': 'fn f(home: &Path) { std::fs::write(home.join(".claude").join("settings.json"), b"{}").ok(); }\n' }, ok: false },
    {
      name: 'a write through a binding of a vendor root is rejected',
      files: { 'accounts/mod.rs': 'fn f(home: &Path) {\n    let dir = home.join(".codex");\n    std::fs::create_dir_all(&dir).ok();\n}\n' },
      ok: false,
    },
    { name: 'a write into an account config_dir is rejected', files: { 'accounts/x.rs': 'fn f(a: &Account) { let _ = std::fs::remove_file(a.config_dir.join("x")); }\n' }, ok: false },
    {
      name: "Gilde's own writes, comments, strings and tests pass",
      files: {
        'config/mod.rs': 'fn f(paths: &AppPaths) { std::fs::write(paths.config_file(), b"{}").ok(); }\n',
        'discovery/claude/mod.rs': [
          '// std::fs::write(home.join(".claude"), x) never happens',
          'const NOTE: &str = "fs::remove_file(";',
          'fn f() {}',
          '#[cfg(test)]',
          'mod tests {',
          '    fn fixture(d: &Path) { std::fs::write(d.join(".claude/sessions/1.json"), b"{}").unwrap(); }',
          '}',
          '',
        ].join('\n'),
      },
      ok: true,
    },
  ];
  let failed = 0;
  for (const c of cases) {
    const root = seed(c.files);
    const v = scan(root);
    fs.rmSync(root, { recursive: true, force: true });
    const pass = (v.length === 0) === c.ok;
    if (!pass) {
      failed++;
      console.log(`FAIL ${c.name}${v.length ? `: ${JSON.stringify(v)}` : ''}`);
    } else {
      console.log(`ok   ${c.name}`);
    }
  }
  console.log(failed ? `check-vendor-writes self-test: ${failed} case(s) failed` : 'check-vendor-writes self-test: all cases pass');
  return failed ? 1 : 0;
}

function main(argv) {
  if (argv.includes('--self-test')) return selfTest();
  let root = path.resolve(path.dirname(fileURLToPath(import.meta.url)), '..');
  const i = argv.indexOf('--root');
  if (i >= 0) {
    if (!argv[i + 1]) {
      console.error('usage: check-vendor-writes.mjs [--root <repo>] | --self-test');
      return 2;
    }
    root = path.resolve(argv[i + 1]);
  }
  if (!fs.existsSync(path.join(root, 'src-tauri', 'src'))) {
    console.error(`check-vendor-writes: no src-tauri/src under ${root}`);
    return 2;
  }
  return report(scan(root));
}

if (process.argv[1] && path.resolve(process.argv[1]) === fileURLToPath(import.meta.url)) {
  process.exit(main(process.argv.slice(2)));
}
