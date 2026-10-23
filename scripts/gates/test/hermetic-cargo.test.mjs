// Tests for scripts/gates/hermetic-cargo.sh (contract §14.1, A38.1): cargo
// runs under exactly the hermetic env -i environment, the temp HOME is
// removed afterwards, the toolchain is derived from the caller, and a PATH
// that reaches a vendor CLI is refused before anything runs. A fake `cargo`
// (a Node script) records its argv and environment; nothing real runs.

import { test } from 'node:test';
import assert from 'node:assert/strict';
import { spawnSync } from 'node:child_process';
import { chmodSync, existsSync, mkdirSync, mkdtempSync, readFileSync, realpathSync, rmSync, writeFileSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { dirname, join, resolve } from 'node:path';
import { fileURLToPath } from 'node:url';

const here = dirname(fileURLToPath(import.meta.url));
const script = resolve(here, '..', 'hermetic-cargo.sh');

const LANES = {
  CARGO_TARGET_DIR: '/lane/target-b',
  CARGO_INCREMENTAL: '0',
  CARGO_BUILD_JOBS: '3',
  CARGO_PROFILE_DEV_DEBUG: 'line-tables-only',
  CARGO_PROFILE_TEST_DEBUG: 'line-tables-only',
  RUST_BACKTRACE: '1',
};

/** scripts/gen-ipc.mjs's export outputs: passed through only when set. */
const GEN_IPC = {
  TS_RS_EXPORT_DIR: '/work/ts',
  GILDE_IPC_MANIFEST_OUT: '/work/manifest.json',
};

/** A temp dir with a fake `cargo` in `<dir>/<binRel>` that records into `<dir>/record.json` and exits `code`. */
function fakeCargo(binRel, code = 7) {
  const dir = realpathSync(mkdtempSync(join(tmpdir(), 'hcargo-')));
  const bin = join(dir, binRel);
  mkdirSync(bin, { recursive: true });
  const record = join(dir, 'record.json');
  writeFileSync(
    join(bin, 'cargo'),
    `#!${process.execPath}\n` +
      `const fs = require('fs');\n` +
      `fs.writeFileSync(${JSON.stringify(record)}, JSON.stringify({ argv: process.argv.slice(2), env: process.env, homeExists: fs.existsSync(process.env.HOME || '/nonexistent') }));\n` +
      `process.exit(${code});\n`,
  );
  chmodSync(join(bin, 'cargo'), 0o755);
  const read = () => {
    const got = JSON.parse(readFileSync(record, 'utf8'));
    // macOS adds this to every process that loads CoreFoundation (the fake
    // cargo's own runtime does); the script never passes it.
    delete got.env.__CF_USER_TEXT_ENCODING;
    return got;
  };
  return { dir, bin, record, read };
}

function run(args, env) {
  return spawnSync(script, args, { env, encoding: 'utf8' });
}

test('cargo runs under exactly the hermetic environment, with its arguments and exit status', (t) => {
  const f = fakeCargo('cargo-home/bin', 7);
  t.after(() => rmSync(f.dir, { recursive: true, force: true }));
  const caller = {
    HOME: '/Users/someone',
    CARGO_HOME: join(f.dir, 'cargo-home'),
    RUSTUP_HOME: '/opt/rustup',
    PATH: '/opt/homebrew/bin:/usr/local/bin:/usr/bin:/bin',
    SHELL: '/bin/zsh',
    FOO: 'leak',
    GILDE_PATH_OVERRIDE: '/somewhere',
    TMPDIR: tmpdir(),
    ...LANES,
    ...GEN_IPC,
  };
  const r = run(['test', '--test', 'boot', '--', 'a name with spaces'], caller);
  assert.equal(r.status, 7, `cargo's exit status is the script's: ${r.stderr}`);
  const got = f.read();
  assert.deepEqual(got.argv, ['test', '--test', 'boot', '--', 'a name with spaces']);
  const home = got.env.HOME;
  assert.match(home, /^\/tmp\/gh\.[A-Za-z0-9]{6}$/, 'a fresh, short HOME under /tmp');
  assert.equal(got.homeExists, true, 'HOME exists while cargo runs');
  assert.equal(existsSync(home), false, 'and is removed afterwards');
  assert.deepEqual(got.env, {
    HOME: home,
    GILDE_HOME: `${home}/.gilde`,
    PATH: `${f.bin}:/usr/bin:/bin:/usr/sbin:/sbin`,
    RUSTUP_HOME: '/opt/rustup',
    CARGO_HOME: join(f.dir, 'cargo-home'),
    SHELL: '/nonexistent',
    LANG: 'en_US.UTF-8',
    AGENT_CLI_CREDENTIAL_STORE: 'memory',
    DISABLE_AUTOUPDATER: '1',
    GIT_CONFIG_NOSYSTEM: '1',
    ...LANES,
    ...GEN_IPC,
  });
});

test('lane and gen-ipc variables the caller did not set stay unset; a successful run exits 0', (t) => {
  const f = fakeCargo('cargo-home/bin', 0);
  t.after(() => rmSync(f.dir, { recursive: true, force: true }));
  const r = run(['--version'], { HOME: '/Users/someone', CARGO_HOME: join(f.dir, 'cargo-home'), RUSTUP_HOME: '/opt/rustup', PATH: '/usr/bin:/bin' });
  assert.equal(r.status, 0, r.stderr);
  const keys = Object.keys(f.read().env).sort();
  assert.deepEqual(keys, ['AGENT_CLI_CREDENTIAL_STORE', 'CARGO_HOME', 'DISABLE_AUTOUPDATER', 'GILDE_HOME', 'GIT_CONFIG_NOSYSTEM', 'HOME', 'LANG', 'PATH', 'RUSTUP_HOME', 'SHELL']);
});

test('the toolchain is derived from the caller: $HOME/.cargo and $HOME/.rustup, else cargo on PATH', (t) => {
  // (a) CARGO_HOME/RUSTUP_HOME unset: from HOME.
  const a = fakeCargo('.cargo/bin', 0);
  t.after(() => rmSync(a.dir, { recursive: true, force: true }));
  let r = run(['build'], { HOME: a.dir, PATH: '/usr/bin:/bin' });
  assert.equal(r.status, 0, r.stderr);
  let env = a.read().env;
  assert.equal(env.CARGO_HOME, join(a.dir, '.cargo'));
  assert.equal(env.RUSTUP_HOME, join(a.dir, '.rustup'));
  assert.equal(env.PATH, `${a.bin}:/usr/bin:/bin:/usr/sbin:/sbin`);

  // (b) no cargo in CARGO_HOME/bin: the caller's cargo on PATH, its dir resolved.
  const b = fakeCargo('elsewhere/bin', 0);
  t.after(() => rmSync(b.dir, { recursive: true, force: true }));
  const emptyCargoHome = join(b.dir, 'empty-cargo-home');
  mkdirSync(emptyCargoHome);
  r = run(['build'], { HOME: '/Users/someone', CARGO_HOME: emptyCargoHome, RUSTUP_HOME: '/opt/rustup', PATH: `${b.bin}:/usr/bin:/bin` });
  assert.equal(r.status, 0, r.stderr);
  env = b.read().env;
  assert.equal(env.PATH, `${b.bin}:/usr/bin:/bin:/usr/sbin:/sbin`);
  assert.equal(env.CARGO_HOME, emptyCargoHome, 'CARGO_HOME is still the caller\'s');
});

test('a vendor CLI on the hermetic PATH is refused before anything runs', (t) => {
  for (const vendor of ['claude', 'agent', 'tmux']) {
    const f = fakeCargo('cargo-home/bin', 0);
    t.after(() => rmSync(f.dir, { recursive: true, force: true }));
    writeFileSync(join(f.bin, vendor), '#!/bin/sh\nexit 0\n');
    chmodSync(join(f.bin, vendor), 0o755);
    const r = run(['test'], { HOME: '/Users/someone', CARGO_HOME: join(f.dir, 'cargo-home'), RUSTUP_HOME: '/opt/rustup', PATH: '/usr/bin:/bin' });
    assert.equal(r.status, 2, `${vendor}: exit 2`);
    assert.match(r.stderr, new RegExp(`vendor CLI is reachable.*${join(f.bin, vendor).replace(/[.*+?^${}()|[\]\\]/g, '\\$&')}`), `${vendor}: the refusal names it`);
    assert.equal(existsSync(f.record), false, `${vendor}: cargo never ran`);
  }
});

test('no cargo anywhere, or no HOME to derive the toolchain from, is a clear exit 2', (t) => {
  const dir = mkdtempSync(join(tmpdir(), 'hcargo-'));
  t.after(() => rmSync(dir, { recursive: true, force: true }));
  let r = run(['test'], { HOME: dir, CARGO_HOME: dir, RUSTUP_HOME: dir, PATH: '/usr/bin:/bin' });
  assert.equal(r.status, 2);
  assert.match(r.stderr, /cargo not found/);
  r = run(['test'], { PATH: '/usr/bin:/bin' });
  assert.equal(r.status, 2);
  assert.match(r.stderr, /HOME is not set/);
});
