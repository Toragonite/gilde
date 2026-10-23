// T-probe-runner-guard (contract §14.2, §14.4 OQ1): the probe runner refuses to
// start a claude process with CLAUDE_CONFIG_DIR unset, set to ~/.claude, or
// set to anything but $HOME/.claude-g or a fresh scratch dir — tested with fake
// binaries on PATH only. The fake writes a marker when it runs, so "refused"
// is proven by the marker's absence (no process ever started).

import { test, before, after } from 'node:test';
import assert from 'node:assert/strict';
import { execFileSync } from 'node:child_process';
import { chmodSync, existsSync, mkdirSync, mkdtempSync, readFileSync, rmSync, symlinkSync, writeFileSync } from 'node:fs';
import { tmpdir, userInfo } from 'node:os';
import { join } from 'node:path';

import { GuardError, allowedEnvKey, buildEnv, checkBinary, checkConfigDir, checkEnv, checkScratchRoot, cleanEnv, endGroup, guardedSpawn } from '../lib/guard.mjs';
import { preflight } from '../lib/phase.mjs';
import { PaneHost } from '../lib/panehost.mjs';

let root;
let home;
let bin;
let fake;
let marker;
let scratch;
const REAL_HOME = userInfo().homedir;

before(() => {
  root = mkdtempSync(join(tmpdir(), 'lp-guard-'));
  home = join(root, 'home');
  mkdirSync(join(home, '.claude-g'), { recursive: true });
  bin = join(root, 'bin');
  mkdirSync(bin);
  marker = join(root, 'ran');
  fake = join(bin, 'claude');
  // The fake records that it ran, and the two values the guard must control.
  writeFileSync(fake, `#!/bin/sh\necho "ran cfg=$CLAUDE_CONFIG_DIR au=$DISABLE_AUTOUPDATER pgid=$(ps -o pgid= -p $$ | tr -d ' ') pid=$$ node_options=[\${NODE_OPTIONS-unset}]" >> '${marker}'\nexit 0\n`);
  chmodSync(fake, 0o755);
  scratch = join(root, 'lp');
  mkdirSync(scratch);
});

after(() => rmSync(root, { recursive: true, force: true }));

const PATH_ONLY_FAKES = () => `${bin}:/usr/bin:/bin`;

test('harness: with the test PATH, `claude` resolves to the fake and no real CLI is reachable', () => {
  const which = execFileSync('/usr/bin/which', ['-a', 'claude'], { env: { PATH: PATH_ONLY_FAKES() }, encoding: 'utf8' }).trim().split('\n');
  assert.deepEqual(which, [fake]);
  for (const vendor of ['codex', 'cursor-agent', 'agy', 'tmux']) {
    assert.throws(() => execFileSync('/usr/bin/which', [vendor], { env: { PATH: PATH_ONLY_FAKES() }, stdio: 'ignore' }), undefined, `${vendor} must not be reachable`);
  }
});

async function attempt(configDir, { env: patch = {}, dryRun = true, fakesDir = bin } = {}) {
  const before = existsSync(marker) ? readFileSync(marker, 'utf8') : '';
  const env = { ...buildEnv({ configDir: configDir ?? 'placeholder', home, path: PATH_ONLY_FAKES(), dryRun: true }), ...patch };
  if (configDir === undefined) delete env.CLAUDE_CONFIG_DIR;
  let error = null;
  let line = null;
  try {
    const { child } = guardedSpawn(fake, [], { env, cwd: root, dryRun, fakesDir, scratchRoots: [scratch] });
    const exited = new Promise((r) => child.once('exit', r));
    child.stdin.end();
    await endGroup(child, exited, { graceMs: 5000 });
    line = (readFileSync(marker, 'utf8').slice(before.length).trim().split('\n').pop()) || null;
  } catch (e) {
    error = e;
  }
  const after = existsSync(marker) ? readFileSync(marker, 'utf8') : '';
  return { error, line, started: after.length > before.length };
}

test('refused: CLAUDE_CONFIG_DIR unset or empty', async () => {
  for (const v of [undefined, '']) {
    const r = await attempt(v);
    assert.ok(r.error instanceof GuardError, String(r.error));
    assert.equal(r.error.rule, 'unset');
    assert.equal(r.started, false, 'no process started');
  }
});

test('refused: ~/.claude in every spelling, for $HOME and for the real home (decided on the string)', async () => {
  for (const v of [join(home, '.claude'), join(home, '.claude') + '/', join(home, '.claude', 'sub'), join(REAL_HOME, '.claude'), join(REAL_HOME, '.claude') + '/', `${REAL_HOME}/x/../.claude`]) {
    const r = await attempt(v);
    assert.equal(r.error?.rule, 'defaultAccount', v);
    assert.equal(r.started, false, `${v}: no process`);
  }
});

test('refused: any other dir (the real ~/.claude-g under a relocated HOME, siblings, relative, the scratch root itself)', async () => {
  const cases = [
    [join(REAL_HOME, '.claude-g'), 'notAllowed'],
    [join(home, '.claude-work'), 'notAllowed'],
    [join(home, '.claude-g', 'nested'), 'notAllowed'],
    [join(root, 'elsewhere'), 'notAllowed'],
    ['relative/dir', 'relative'],
    [scratch, 'notAllowed'],
    [`${scratch}/../outside`, 'notAllowed'],
    ['/tmp', 'notAllowed'],
  ];
  for (const [v, rule] of cases) {
    const r = await attempt(v);
    assert.equal(r.error?.rule, rule, v);
    assert.equal(r.started, false, `${v}: no process`);
  }
});

test('refused: a scratch dir that is not fresh, is a file, or is a symlink out', async () => {
  const used = join(scratch, 'LP-09', 'used');
  mkdirSync(used, { recursive: true });
  writeFileSync(join(used, '.claude.json'), '');
  const file = join(scratch, 'LP-09', 'afile');
  writeFileSync(file, '');
  const link = join(scratch, 'LP-09', 'link');
  symlinkSync(join(root, 'elsewhere-target'), link);
  for (const [v, rule] of [[used, 'notFresh'], [file, 'notDir'], [link, 'symlink']]) {
    const r = await attempt(v);
    assert.equal(r.error?.rule, rule, v);
    assert.equal(r.started, false);
  }
});

test('refused: $HOME/.claude-g as a symlink', async () => {
  const h2 = join(root, 'home2');
  mkdirSync(h2);
  symlinkSync(join(root, 'target-g'), join(h2, '.claude-g'));
  assert.throws(() => checkConfigDir(join(h2, '.claude-g'), { home: h2, scratchRoots: [scratch] }), (e) => e.rule === 'symlink');
});

test('allowed: $HOME/.claude-g and a fresh scratch dir; the fake runs with DISABLE_AUTOUPDATER=1 in its own process group', async () => {
  for (const v of [join(home, '.claude-g'), join(scratch, 'LP-09', `empty-${Date.now()}`)]) {
    const r = await attempt(v);
    assert.equal(r.error, null, String(r.error));
    assert.ok(r.started, `${v}: the fake ran`);
    const m = /ran cfg=(\S+) au=(\S*) pgid=(\d+) pid=(\d+)/.exec(r.line);
    assert.ok(m, r.line);
    assert.equal(m[1], v);
    assert.equal(m[2], '1', 'DISABLE_AUTOUPDATER=1');
    assert.equal(m[3], m[4], 'the fake leads its own process group');
  }
});

test('refused: the environment must carry DISABLE_AUTOUPDATER=1 and no TMUX/ITERM keys', async () => {
  for (const [patch, rule] of [[{ DISABLE_AUTOUPDATER: '0' }, 'autoupdater'], [{ DISABLE_AUTOUPDATER: undefined }, 'autoupdater'], [{ TMUX: '/tmp/x,1,0' }, 'envKey'], [{ ITERM_SESSION_ID: 'w0' }, 'envKey'], [{ CLAUDECODE: '1' }, 'envKey']]) {
    const env = { ...patch };
    const r = await attempt(join(home, '.claude-g'), { env });
    assert.equal(r.error?.rule, rule, JSON.stringify(patch));
    assert.equal(r.started, false);
  }
  assert.throws(() => buildEnv({ configDir: join(home, '.claude-g'), home, extra: { DISABLE_AUTOUPDATER: '0' } }), (e) => e.rule === 'envKey', 'extras cannot touch DISABLE_AUTOUPDATER');
  assert.throws(() => buildEnv({ configDir: join(home, '.claude-g'), home, extra: { TMUX: 'x' } }), (e) => e.rule === 'envKey');
  assert.throws(() => buildEnv({ configDir: join(home, '.claude-g'), home, extra: { FAKE_CLAUDE_X: '1' } }), (e) => e.rule === 'envKey', 'fake knobs only in a dry run');
  assert.equal(buildEnv({ configDir: join(home, '.claude-g'), home, extra: { CLAUDE_CODE_ENTRYPOINT: 'gilde' } }).DISABLE_AUTOUPDATER, '1');
});

test('refused in a dry run: a binary that is not one of the fakes', async () => {
  const other = join(root, 'other-claude');
  writeFileSync(other, '#!/bin/sh\nexit 0\n');
  chmodSync(other, 0o755);
  assert.throws(() => checkBinary(other, { dryRun: true, fakesDir: bin }), (e) => e.rule === 'notFake');
  const r = await attempt(join(home, '.claude-g'), { fakesDir: join(root, 'nonexistent-fakes') });
  assert.ok(r.error);
  assert.equal(r.started, false);
});

test('scratch roots must sit inside a system temp dir and never inside a home', () => {
  assert.ok(checkScratchRoot(scratch));
  assert.throws(() => checkScratchRoot(join(REAL_HOME, 'gilde-lp')), (e) => e.rule === 'scratchRoot');
  assert.throws(() => checkScratchRoot('/usr/local/gilde-lp'), (e) => e.rule === 'scratchRoot');
  assert.throws(() => checkScratchRoot('/tmp'), (e) => e.rule === 'scratchRoot', 'a temp dir itself is too broad');
  assert.throws(() => checkScratchRoot('relative'), (e) => e.rule === 'scratchRoot');
});

test('the pane host refuses a respawn payload that names the default account, and spawns nothing', async () => {
  const sock = join(tmpdir(), `lp-guard-${process.pid}.sock`);
  const host = new PaneHost({ socketPath: sock, recorder: null, guard: { dryRun: true, fakesDir: bin, scratchRoots: [scratch] }, paneEnv: () => buildEnv({ configDir: join(home, '.claude-g'), home, path: PATH_ONLY_FAKES(), dryRun: true }) });
  const before = existsSync(marker) ? readFileSync(marker, 'utf8') : '';
  host.tmux(['-L', 'claude-swarm-1', 'new-session', '-d', '-s', 'claude-swarm', '-n', 'swarm-view', '-P', '-F', '#{pane_id}', '--', 'cat'], { HOME: home }, root);
  for (const cfg of [join(REAL_HOME, '.claude'), join(home, '.claude')]) {
    assert.throws(() => host.tmux(['-L', 'claude-swarm-1', 'respawn-pane', '-k', '-t', '%0', '--', `cd ${root} && env CLAUDE_CONFIG_DIR=${cfg} ${fake} --agent-id a@t`], { HOME: home }, root), (e) => e instanceof GuardError && e.rule === 'defaultAccount');
  }
  assert.throws(() => host.tmux(['-L', 'claude-swarm-1', 'respawn-pane', '-k', '-t', '%0', '--', `cd ${root} && /usr/bin/true`], { HOME: home }, root), (e) => e.rule === 'payload');
  const after = existsSync(marker) ? readFileSync(marker, 'utf8') : '';
  assert.equal(after, before, 'no pane process ran');
  assert.equal(host.panes.size, 0);
  const dev = host.tmux(['-L', 'claude-swarm-1', 'display-message', '-p', '#{pane_id}'], {}, root);
  assert.equal(dev.code, 1);
  assert.equal(host.deviations, 1, 'an unsupported subcommand is a deviation');
});

/** A complete, valid env built by hand (not by buildEnv), plus `extra`. */
function handEnv(extra = {}) {
  return {
    PATH: PATH_ONLY_FAKES(), HOME: home, USER: 'probe', LOGNAME: 'probe', SHELL: '/bin/zsh', TMPDIR: root, LANG: 'en_US.UTF-8',
    TERM: 'xterm-256color', COLORTERM: 'truecolor', TERM_PROGRAM: 'gilde-code', CLAUDE_CONFIG_DIR: join(home, '.claude-g'), DISABLE_AUTOUPDATER: '1',
    ...extra,
  };
}

async function spawnWith(env, { dryRun = true } = {}) {
  const before = existsSync(marker) ? readFileSync(marker, 'utf8') : '';
  let error = null;
  try {
    const { child } = guardedSpawn(fake, [], { env, cwd: root, dryRun, fakesDir: bin, scratchRoots: [scratch] });
    const exited = new Promise((r) => child.once('exit', r));
    child.stdin.end();
    await endGroup(child, exited, { graceMs: 5000 });
  } catch (e) {
    error = e;
  }
  const after = existsSync(marker) ? readFileSync(marker, 'utf8') : '';
  return { error, started: after.length > before.length };
}

test('allowlist: a hand-built env with a key outside the allowlist is refused by guardedSpawn itself', async () => {
  for (const key of ['ANTHROPIC_API_KEY', 'CLAUDE_CODE_OAUTH_TOKEN', 'NODE_OPTIONS', 'DYLD_INSERT_LIBRARIES', 'DYLD_LIBRARY_PATH', 'HTTPS_PROXY', 'ANTHROPIC_BASE_URL', 'LD_PRELOAD', 'BASH_ENV']) {
    const r = await spawnWith(handEnv({ [key]: 'x' }));
    assert.ok(r.error instanceof GuardError, `${key}: ${r.error}`);
    assert.equal(r.error.rule, 'envKey', key);
    assert.match(r.error.message, /not on the probe allowlist/, key);
    assert.equal(r.started, false, `${key}: no process started`);
  }
});

test('allowlist: FAKE_CLAUDE_* only in a dry run; the allowlisted extras pass', async () => {
  const live = await spawnWith(handEnv({ FAKE_CLAUDE_X: '1' }), { dryRun: false });
  assert.equal(live.error?.rule, 'envKey');
  assert.equal(live.started, false, 'refused before any process outside a dry run');
  const dry = await spawnWith(handEnv({ FAKE_CLAUDE_X: '1' }), { dryRun: true });
  assert.equal(dry.error, null, String(dry.error));
  assert.ok(dry.started);
  const extras = await spawnWith(handEnv({ CLAUDE_CODE_EMIT_SESSION_STATE_EVENTS: '1', CLAUDE_CODE_ENTRYPOINT: 'gilde', CLAUDE_CODE_EXPERIMENTAL_AGENT_TEAMS: '1', GILDE_PANE_HOST: '/tmp/x.sock', GILDE_SHIM_LOG: '/tmp/x.jsonl', GILDE_TMUX_LOG: '/tmp/y.jsonl' }));
  assert.equal(extras.error, null, String(extras.error));
  assert.ok(extras.started);
  assert.equal(allowedEnvKey('FAKE_CLAUDE_SELF'), false);
  assert.equal(allowedEnvKey('FAKE_CLAUDE_SELF', { dryRun: true }), true);
});

test('allowlist: a buildEnv() env still passes, in a dry run and in a live-mode check', async () => {
  const env = buildEnv({ configDir: join(home, '.claude-g'), home, path: PATH_ONLY_FAKES(), l1: true, extra: { CLAUDE_CODE_ENTRYPOINT: 'gilde' } });
  assert.doesNotThrow(() => checkEnv(env, { dryRun: false }));
  const r = await spawnWith(env, { dryRun: true });
  assert.equal(r.error, null, String(r.error));
  assert.ok(r.started);
});

test('values: strings only, no NUL; TERM_PROGRAM must be gilde-code', async () => {
  const cases = [
    [handEnv({ LANG: 42 }), /string without NUL/],
    [handEnv({ LANG: 'en\u0000US' }), /string without NUL/],
    [handEnv({ TERM_PROGRAM: 'Apple_Terminal' }), /TERM_PROGRAM/],
    [(() => { const e = handEnv(); delete e.TERM_PROGRAM; return e; })(), /TERM_PROGRAM/],
  ];
  for (const [env, re] of cases) {
    const r = await spawnWith(env);
    assert.equal(r.error?.rule, 'envKey', String(r.error));
    assert.match(r.error.message, re);
    assert.equal(r.started, false);
  }
  assert.throws(() => buildEnv({ configDir: join(home, '.claude-g'), home, extra: { CLAUDE_CODE_ENTRYPOINT: 'a\u0000b' } }), (e) => e.rule === 'envKey');
});

test('order: preflight runs the OQ1 rule before it stats anything under the config dir', async () => {
  for (const cfg of [join(REAL_HOME, '.claude'), join(home, '.claude')]) {
    const env = handEnv({ CLAUDE_CONFIG_DIR: cfg });
    await assert.rejects(preflight({ binary: fake, configDir: cfg, env, cwd: root, guard: { dryRun: true, fakesDir: bin, scratchRoots: [scratch] } }), (e) => e instanceof GuardError && e.rule === 'defaultAccount');
  }
  await assert.rejects(preflight({ binary: fake, configDir: join(home, '.claude-g'), env: handEnv({ CLAUDE_CONFIG_DIR: join(scratch, 'LP-09', 'other') }), cwd: root, guard: { dryRun: true, fakesDir: bin, scratchRoots: [scratch] } }), (e) => e.rule === 'preflight', 'the stat target and the env must agree');
  const ok = await preflight({ binary: fake, configDir: join(home, '.claude-g'), env: handEnv(), cwd: root, guard: { dryRun: true, fakesDir: bin, scratchRoots: [scratch] } });
  assert.equal(ok.claudeJsonExists, false);
});

test('the pane host refuses a pane env with a key outside the allowlist, and spawns nothing', () => {
  const sock = join(tmpdir(), `lp-guard-env-${process.pid}.sock`);
  const host = new PaneHost({ socketPath: sock, recorder: null, guard: { dryRun: true, fakesDir: bin, scratchRoots: [scratch] }, paneEnv: () => handEnv({ NODE_OPTIONS: '--require /tmp/evil.js' }) });
  host.tmux(['-L', 'claude-swarm-2', 'new-session', '-d', '-s', 'claude-swarm', '-n', 'swarm-view', '-P', '-F', '#{pane_id}', '--', 'cat'], { HOME: home }, root);
  assert.throws(() => host.tmux(['-L', 'claude-swarm-2', 'respawn-pane', '-k', '-t', '%0', '--', `cd ${root} && env CLAUDE_CONFIG_DIR=${join(home, '.claude-g')} ${fake} --agent-id a@t`], { HOME: home }, root), (e) => e instanceof GuardError && e.rule === 'envKey');
  assert.equal(host.panes.size, 0);
});

/** Spawn the fake with `env` (dry run) and return its marker line. */
async function markerLineFor(env) {
  const before = existsSync(marker) ? readFileSync(marker, 'utf8') : '';
  const { child } = guardedSpawn(fake, [], { env, cwd: root, dryRun: true, fakesDir: bin, scratchRoots: [scratch] });
  const exited = new Promise((r) => child.once('exit', r));
  child.stdin.end();
  await endGroup(child, exited, { graceMs: 5000 });
  return readFileSync(marker, 'utf8').slice(before.length).trim().split('\n').pop();
}

test('inherited keys never reach the child: an env whose prototype carries NODE_OPTIONS', async () => {
  const base = buildEnv({ configDir: join(home, '.claude-g'), home, path: PATH_ONLY_FAKES(), dryRun: true });
  const sneaky = Object.assign(Object.create({ NODE_OPTIONS: '--require /x' }), base);
  assert.equal('NODE_OPTIONS' in sneaky, true, 'inherited and enumerable for for…in');
  assert.equal(Object.keys(sneaky).includes('NODE_OPTIONS'), false, 'not an own key, so checkEnv passes');
  const line = await markerLineFor(sneaky);
  assert.match(line, /node_options=\[unset\]/, line);
  const clean = cleanEnv(sneaky);
  assert.equal(Object.getPrototypeOf(clean), null);
  const keys = [];
  for (const k in clean) keys.push(k);
  assert.deepEqual(keys.sort(), Object.keys(base).sort(), 'for…in over the spawn env sees own keys only');
});

test('inherited keys never reach the child: a polluted Object.prototype', async () => {
  const base = buildEnv({ configDir: join(home, '.claude-g'), home, path: PATH_ONLY_FAKES(), dryRun: true });
  let line;
  // eslint-disable-next-line no-extend-native
  Object.prototype.NODE_OPTIONS = '--require /x';
  try {
    line = await markerLineFor(base);
  } finally {
    delete Object.prototype.NODE_OPTIONS;
  }
  assert.equal(Object.prototype.NODE_OPTIONS, undefined, 'the pollution is removed again');
  assert.match(line, /node_options=\[unset\]/, line);
});

test('a direct own NODE_OPTIONS is still refused by the allowlist', async () => {
  const env = { ...buildEnv({ configDir: join(home, '.claude-g'), home, path: PATH_ONLY_FAKES(), dryRun: true }), NODE_OPTIONS: '--require /x' };
  assert.throws(() => guardedSpawn(fake, [], { env, cwd: root, dryRun: true, fakesDir: bin, scratchRoots: [scratch] }), (e) => e.rule === 'envKey');
});
