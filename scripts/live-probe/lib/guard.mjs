/**
 * The OQ1 guard of the live-probe kit (contract §14.4 rules 1, 5, 8;
 * T-probe-runner-guard).
 *
 * Every Claude process the kit starts goes through `guardedSpawn`, the only
 * spawn in the kit. Before anything runs it checks:
 *  - CLAUDE_CONFIG_DIR (`checkConfigDir`): set; never the default account
 *    (`~/.claude`, under the real home or `$HOME`); either exactly
 *    `$HOME/.claude-g` (account g, not a symlink) or a fresh scratch dir
 *    strictly inside a scratch root (default `/tmp/gilde-lp`) that does not
 *    exist yet or is empty (never signed in, LP-09(b)). Anything else is
 *    refused.
 *  - the environment: `buildEnv` rebuilds it from an allowlist with
 *    `DISABLE_AUTOUPDATER=1` set last (rule 8, A1), and `guardedSpawn`
 *    enforces the same allowlist itself (`checkEnv`) whoever built the env —
 *    no key outside it, no TMUX*, ITERM_*, LC_TERMINAL*, string values
 *    without NUL, TERM_PROGRAM=gilde-code.
 *  - the binary (`checkBinary`): in a dry run it must be one of the kit's
 *    fakes, so a dry run can never start a real CLI.
 * A refusal throws `GuardError` before any process exists. Processes run in
 * their own process group and are ended by closing stdin, `/exit` or SIGTERM
 * to the group — never SIGKILL (rule 5, contract C5).
 */

import { spawn } from 'node:child_process';
import { existsSync, lstatSync, readdirSync, realpathSync } from 'node:fs';
import { tmpdir, userInfo } from 'node:os';
import { basename, dirname, isAbsolute, join, resolve } from 'node:path';

export class GuardError extends Error {
  constructor(rule, message) {
    super(`refused (${rule}): ${message}`);
    this.name = 'GuardError';
    this.rule = rule;
  }
}

/** The contract's scratch root for probe projects and the LP-09(b) config dir. */
export const DEFAULT_SCRATCH_ROOT = '/tmp/gilde-lp';

/** Extra env keys a probe may add (the base set is fixed in `buildEnv`). */
export const EXTRA_ENV_ALLOWED = new Set([
  'CLAUDE_CODE_ENTRYPOINT', // LP-31
  'CLAUDE_CODE_EXPERIMENTAL_AGENT_TEAMS', // hosted teams
  'GILDE_PANE_HOST', // the kit's pane host socket
  'GILDE_SHIM_LOG', // the kit's logging shim
  'GILDE_TMUX_LOG', // the built probe-log shim (LP-10)
]);

/** Keys never passed to a probe process (§7.4.1 Variant E, R6 §1.1). */
const ENV_DENY = /^(TMUX|TMUX_PANE|ITERM_.*|LC_TERMINAL.*|TERM_SESSION_ID|CLAUDECODE)$/;

/** The keys `buildEnv` sets itself (R6 §1.1); with EXTRA_ENV_ALLOWED the whole probe allowlist. */
export const BASE_ENV_KEYS = new Set([
  'PATH', 'HOME', 'USER', 'LOGNAME', 'SHELL', 'TMPDIR', 'LANG', 'TERM', 'COLORTERM', 'TERM_PROGRAM',
  'CLAUDE_CONFIG_DIR', 'DISABLE_AUTOUPDATER', 'CLAUDE_CODE_EMIT_SESSION_STATE_EVENTS',
]);

/** Whether `key` may reach a probe process (FAKE_CLAUDE_* only in a dry run). */
export function allowedEnvKey(key, { dryRun = false } = {}) {
  return BASE_ENV_KEYS.has(key) || EXTRA_ENV_ALLOWED.has(key) || (dryRun && key.startsWith('FAKE_CLAUDE_'));
}

/**
 * The environment rule every spawn enforces itself, whoever built the env
 * (contract §14.4, R6 §1.1 "env rebuilt from an allowlist"):
 * DISABLE_AUTOUPDATER=1; no deny-listed key; every key on the allowlist
 * (`allowedEnvKey`); every value a string without NUL; TERM_PROGRAM=gilde-code.
 */
export function checkEnv(env, { dryRun = false } = {}) {
  if (!env || typeof env !== 'object' || Array.isArray(env)) throw new GuardError('env', 'no environment given');
  if (env.DISABLE_AUTOUPDATER !== '1') throw new GuardError('autoupdater', 'DISABLE_AUTOUPDATER=1 is required for every probe process (A1)');
  for (const k of Object.keys(env)) if (ENV_DENY.test(k)) throw new GuardError('envKey', `env key ${k} must not reach a probe process`);
  for (const k of Object.keys(env)) if (!allowedEnvKey(k, { dryRun })) throw new GuardError('envKey', `env key ${k} is not on the probe allowlist`);
  for (const [k, v] of Object.entries(env)) {
    if (typeof v !== 'string' || v.includes('\0')) throw new GuardError('envKey', `env value of ${k} must be a string without NUL`);
  }
  if (env.TERM_PROGRAM !== 'gilde-code') throw new GuardError('envKey', 'TERM_PROGRAM must be gilde-code');
  return env;
}

/** The real home from the user database (not `$HOME`, which a run may relocate). */
export function realHome() {
  return userInfo().homedir;
}

function within(path, dir) {
  const d = dir.endsWith('/') ? dir.slice(0, -1) : dir;
  return path === d || path.startsWith(d + '/');
}

/** `realpath` of `p`, or of its longest existing ancestor with the rest appended. */
export function realOrResolved(p) {
  const rest = [];
  let cur = resolve(p);
  for (;;) {
    try {
      return join(realpathSync(cur), ...rest);
    } catch {
      const parent = dirname(cur);
      if (parent === cur) return resolve(p);
      rest.unshift(basename(cur));
      cur = parent;
    }
  }
}

/**
 * Whether any component of `path` strictly below `top` is a symlink, dangling
 * or not (`lstat`, never following the link).
 */
export function symlinkBelow(path, top) {
  for (let p = path; p !== top && within(p, top); p = dirname(p)) {
    const st = lstatSync(p, { throwIfNoEntry: false });
    if (st && st.isSymbolicLink()) return true;
    if (dirname(p) === p) break;
  }
  return false;
}

/** System temp dirs a scratch root must live under (never the home). */
function systemTmpDirs() {
  const out = new Set(['/tmp', '/private/tmp', '/private/var/folders', '/var/folders']);
  try {
    out.add(realpathSync(tmpdir()));
  } catch {
    out.add(resolve(tmpdir()));
  }
  return [...out];
}

/**
 * A scratch root is acceptable when it resolves strictly inside a system temp
 * dir and not inside any home. Returns its real path or throws.
 */
export function checkScratchRoot(root, homes = [realHome()]) {
  if (typeof root !== 'string' || !isAbsolute(root)) throw new GuardError('scratchRoot', `scratch root must be an absolute path, got ${JSON.stringify(root)}`);
  const real = realOrResolved(root);
  const tmps = systemTmpDirs().map(realOrResolved);
  if (!tmps.some((t) => within(real, t) && real !== t)) throw new GuardError('scratchRoot', `scratch root ${root} is not inside a system temp dir`);
  for (const h of homes) if (within(real, realOrResolved(h))) throw new GuardError('scratchRoot', `scratch root ${root} is inside the home dir`);
  return real;
}

/**
 * The OQ1 rule for CLAUDE_CONFIG_DIR. `home` is the HOME the process will get;
 * `scratchRoots` are the accepted scratch roots. Returns `{ kind, dir }` with
 * kind `accountG` or `scratch`, or throws `GuardError`.
 *
 * Every refusal of the default account and of an unlisted dir is decided on
 * the path string alone, so the guard never touches `~/.claude` (not even a
 * stat). Only a candidate that passed those checks is resolved, to refuse
 * symlinks that would lead elsewhere.
 */
export function checkConfigDir(value, { home, scratchRoots = [DEFAULT_SCRATCH_ROOT] } = {}) {
  if (value === undefined || value === null || value === '') {
    throw new GuardError('unset', 'CLAUDE_CONFIG_DIR is unset — every probe names its config dir explicitly (OQ1); unset would mean the default account');
  }
  if (typeof value !== 'string' || /[\0\n\r]/.test(value)) throw new GuardError('invalid', 'CLAUDE_CONFIG_DIR is not a plain path');
  if (!isAbsolute(value)) throw new GuardError('relative', `CLAUDE_CONFIG_DIR must be absolute, got ${JSON.stringify(value)}`);
  if (typeof home !== 'string' || !isAbsolute(home)) throw new GuardError('home', 'HOME for the probe process must be an absolute path');
  const norm = resolve(value);
  const homes = [...new Set([resolve(home), realHome()])];
  for (const h of homes) {
    if (within(norm, join(h, '.claude'))) {
      throw new GuardError('defaultAccount', `CLAUDE_CONFIG_DIR ${value} is the default account (~/.claude) — never used by probes (OQ1)`);
    }
  }
  const accountG = join(resolve(home), '.claude-g');
  if (norm === accountG) {
    if (lstatSync(accountG, { throwIfNoEntry: false })?.isSymbolicLink()) throw new GuardError('symlink', `${accountG} is a symlink`);
    if (realOrResolved(accountG) !== join(realOrResolved(home), '.claude-g')) throw new GuardError('symlink', `${accountG} resolves elsewhere`);
    return { kind: 'accountG', dir: norm };
  }
  for (const root of scratchRoots) {
    const rootReal = checkScratchRoot(root, homes);
    const rootNorm = resolve(root);
    if (!(within(norm, rootNorm) && norm !== rootNorm) && !(within(norm, rootReal) && norm !== rootReal)) continue;
    const top = within(norm, rootNorm) ? rootNorm : rootReal;
    if (symlinkBelow(norm, top)) throw new GuardError('symlink', `scratch config dir ${value} goes through a symlink`);
    const real = realOrResolved(norm);
    if (!within(real, rootReal) || real === rootReal) throw new GuardError('symlink', `scratch config dir ${value} resolves outside the scratch root`);
    const st = lstatSync(norm, { throwIfNoEntry: false });
    if (st) {
      if (!st.isDirectory()) throw new GuardError('notDir', `scratch config dir ${value} is not a directory`);
      if (readdirSync(norm).length > 0) throw new GuardError('notFresh', `scratch config dir ${value} is not empty (a fresh, never-signed-in dir is required)`);
    }
    return { kind: 'scratch', dir: norm };
  }
  throw new GuardError('notAllowed', `CLAUDE_CONFIG_DIR ${value} is neither $HOME/.claude-g nor a fresh dir under ${scratchRoots.join(', ')} (OQ1)`);
}

/**
 * The probe process environment, rebuilt from an allowlist (R6 §1.1).
 * `extra` may only use EXTRA_ENV_ALLOWED keys (plus FAKE_CLAUDE_* in a dry
 * run, read by the fakes). DISABLE_AUTOUPDATER=1 is set last.
 */
export function buildEnv({ configDir, home, path, l1 = false, extra = {}, dryRun = false }) {
  const user = userInfo().username;
  const env = {
    PATH: path ?? '/usr/bin:/bin:/usr/sbin:/sbin:/opt/homebrew/bin',
    HOME: home,
    USER: user,
    LOGNAME: user,
    SHELL: '/bin/zsh',
    TMPDIR: process.env.TMPDIR || '/tmp',
    LANG: 'en_US.UTF-8',
    TERM: 'xterm-256color',
    COLORTERM: 'truecolor',
    TERM_PROGRAM: 'gilde-code',
    CLAUDE_CONFIG_DIR: configDir,
  };
  if (l1) env.CLAUDE_CODE_EMIT_SESSION_STATE_EVENTS = '1';
  for (const [k, v] of Object.entries(extra)) {
    const fake = dryRun && k.startsWith('FAKE_CLAUDE_');
    if (!EXTRA_ENV_ALLOWED.has(k) && !fake) throw new GuardError('envKey', `env key ${k} is not on the probe allowlist`);
    if (typeof v !== 'string' || v.includes('\0')) throw new GuardError('envKey', `env value for ${k} must be a string without NUL`);
    env[k] = v;
  }
  env.DISABLE_AUTOUPDATER = '1';
  return env;
}

/**
 * The env actually handed to a child: a null-prototype copy of the checked
 * env's own keys. `checkEnv` reads own keys only, while child_process copies
 * with `for…in` (inherited enumerable keys too), so the caller's object — or a
 * polluted Object.prototype — must never reach `spawn`.
 */
export function cleanEnv(env) {
  return Object.assign(Object.create(null), Object.fromEntries(Object.entries(env)));
}

/** The binary must be a kit fake in a dry run. Returns its real path. */
export function checkBinary(binary, { dryRun, fakesDir }) {
  if (typeof binary !== 'string' || !isAbsolute(binary)) throw new GuardError('binary', `the claude binary must be an absolute path, got ${JSON.stringify(binary)}`);
  if (!existsSync(binary)) throw new GuardError('binary', `the claude binary does not exist: ${binary}`);
  const real = realpathSync(binary);
  if (dryRun) {
    if (!fakesDir || !within(real, realpathSync(fakesDir))) throw new GuardError('notFake', `dry run: ${binary} is not one of the kit's fakes (${fakesDir})`);
  }
  return real;
}

/**
 * A pseudo-terminal for a command: macOS script(1) fed by a small perl pump.
 * script needs a real pipe on stdin (Node's stdio pipes are sockets, which it
 * rejects); the pump creates one, copies our stdin into it, and exits with
 * the program's own exit code as soon as script exits (a `cat |` bridge
 * would keep the group alive until our stdin closed). perl and script ship
 * with macOS.
 */
export const PTY_PUMP = 'pipe(my $r, my $w) or die "pipe: $!"; my $pid = fork(); die "fork: $!" unless defined $pid; if ($pid == 0) { close $w; open(STDIN, "<&", $r) or die; exec @ARGV; exit 127 } close $r; $SIG{CHLD} = sub { if (waitpid($pid, 1) == $pid) { exit($? & 127 ? 128 + ($? & 127) : $? >> 8) } }; binmode STDIN; binmode $w; my $old = select($w); $| = 1; select($old); while (1) { my $n = sysread(STDIN, my $buf, 65536); if (!defined $n) { next if $!{EINTR}; last } last if $n == 0; syswrite($w, $buf) } close $w; waitpid($pid, 0); exit($? & 127 ? 128 + ($? & 127) : $? >> 8);';

/** argv for running `/bin/sh -c <inner>` in a `rows`×`cols` PTY. */
export function ptyArgv(inner, { rows = 40, cols = 120 } = {}) {
  const sized = `stty rows ${Number(rows) | 0} cols ${Number(cols) | 0} 2>/dev/null; ${inner}`;
  return ['/usr/bin/perl', ['-e', PTY_PUMP, '/usr/bin/script', '-q', '/dev/null', '/bin/sh', '-c', sized]];
}

/** Single-quote a word for /bin/sh. */
export function shQuote(word) {
  return `'${String(word).replace(/'/g, `'\\''`)}'`;
}

/**
 * The only spawn of the kit. `pty` wraps the binary in a pseudo-terminal of
 * `rows`×`cols` (`ptyArgv`). Every check runs before the process exists.
 */
export function guardedSpawn(binary, args, { env, cwd, dryRun = false, fakesDir = null, scratchRoots = [DEFAULT_SCRATCH_ROOT], pty = null } = {}) {
  if (!env || typeof env !== 'object') throw new GuardError('env', 'no environment given');
  checkConfigDir(env.CLAUDE_CONFIG_DIR, { home: env.HOME, scratchRoots });
  checkEnv(env, { dryRun });
  const clean = cleanEnv(env);
  const real = checkBinary(binary, { dryRun, fakesDir });
  if (typeof cwd !== 'string' || !isAbsolute(cwd) || !existsSync(cwd)) throw new GuardError('cwd', `cwd must be an existing absolute dir, got ${JSON.stringify(cwd)}`);
  if (pty) {
    const [cmd, cmdArgs] = ptyArgv(`exec ${[real, ...args].map(shQuote).join(' ')}`, pty);
    return { child: spawn(cmd, cmdArgs, { env: clean, cwd, detached: true, stdio: ['pipe', 'pipe', 'pipe'] }), binary: real };
  }
  return { child: spawn(real, args, { env: clean, cwd, detached: true, stdio: ['pipe', 'pipe', 'pipe'] }), binary: real };
}

/**
 * End a process group the kit started: wait for a voluntary exit, then SIGTERM
 * to the group, then wait again. Never SIGKILL: a survivor is reported.
 * `exited()` resolves when the child has exited.
 */
export async function endGroup(child, exited, { graceMs = 3000, termWaitMs = 10000 } = {}) {
  const wait = (ms) => Promise.race([exited.then(() => true), new Promise((r) => setTimeout(() => r(false), ms))]);
  if (child.exitCode !== null || child.signalCode !== null) return { ended: 'exited', survivor: false };
  if (await wait(graceMs)) return { ended: 'exited', survivor: false };
  try {
    process.kill(-child.pid, 'SIGTERM');
  } catch (e) {
    if (e.code !== 'ESRCH') throw e;
  }
  if (await wait(termWaitMs)) return { ended: 'sigterm', survivor: false };
  return { ended: 'none', survivor: true };
}
