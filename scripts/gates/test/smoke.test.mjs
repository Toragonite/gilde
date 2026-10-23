// Tests for scripts/smoke.mjs (contract §14.1, A38.4): the PATH the smoke run
// checks is the PATH cargo gets (nothing of the caller's added), the fake bin
// holds only fixture binaries and reaches the test through
// GILDE_SMOKE_FAKE_BIN, and a run that executes 0 tests or reports no step
// fails. Nothing runs here: only files are stat'ed.

import { test } from 'node:test';
import assert from 'node:assert/strict';
import { chmodSync, mkdirSync, mkdtempSync, realpathSync, rmSync, symlinkSync, writeFileSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { join } from 'node:path';

import { checks, countTestsRun, hermeticEnv, leftoversOf, nonFixturesIn, parsePs, vendorsOnPath } from '../../smoke.mjs';

function writeExec(path, body = '#!/bin/sh\nexit 0\n') {
  writeFileSync(path, body);
  chmodSync(path, 0o755);
}

/** A toolchain with a `cargo`, a fake bin with `claude` → `fake_claude`, and a base dir. */
function tree(t) {
  const dir = realpathSync(mkdtempSync(join(tmpdir(), 'smoke-test-')));
  t.after(() => rmSync(dir, { recursive: true, force: true }));
  const cargoHome = join(dir, 'cargo-home');
  mkdirSync(join(cargoHome, 'bin'), { recursive: true });
  writeExec(join(cargoHome, 'bin', 'cargo'));
  const fake = join(dir, 'fake-bin');
  const fixtures = join(dir, 'target', 'debug');
  mkdirSync(fake);
  mkdirSync(fixtures, { recursive: true });
  writeExec(join(fixtures, 'fake_claude'));
  symlinkSync(join(fixtures, 'fake_claude'), join(fake, 'claude'));
  const base = join(dir, 'base');
  mkdirSync(base);
  const caller = {
    HOME: '/Users/someone',
    CARGO_HOME: cargoHome,
    RUSTUP_HOME: '/opt/rustup',
    PATH: '/opt/homebrew/bin:/usr/local/bin:/usr/bin:/bin',
    SHELL: '/bin/zsh',
    FOO: 'leak',
    CARGO_TARGET_DIR: '/lane/target-b',
    CARGO_BUILD_JOBS: '3',
  };
  return { dir, cargoHome, fake, fixtures, base, caller };
}

test('cargo gets exactly the PATH that was checked; the fake bin is pinned, never on PATH', (t) => {
  const x = tree(t);
  const { env, cargo, reachable } = hermeticEnv(x.fake, x.base, x.caller);
  const checked = `${join(x.cargoHome, 'bin')}:/usr/bin:/bin:/usr/sbin:/sbin`;
  assert.equal(env.PATH, checked);
  assert.deepEqual(vendorsOnPath(env.PATH), [], 'nothing vendor-named is on it');
  assert.equal(cargo, join(x.cargoHome, 'bin', 'cargo'), 'cargo is called by its absolute path');
  assert.equal(env.GILDE_SMOKE_FAKE_BIN, x.fake);
  assert.ok(!env.PATH.split(':').includes(x.fake), 'the fake bin is not on PATH');
  assert.equal(env.SHELL, '/nonexistent');
  assert.equal(env.GIT_CONFIG_NOSYSTEM, '1');
  assert.equal(env.HOME, join(x.base, 'home'));
  assert.equal(env.CARGO_TARGET_DIR, '/lane/target-b', 'lane variables pass');
  assert.equal(env.FOO, undefined, 'nothing else of the caller does');
  assert.equal(reachable.claude, true);
});

test('a vendor CLI on the checked PATH refuses the run', (t) => {
  for (const name of ['codex', 'agent']) {
    const x = tree(t);
    writeExec(join(x.cargoHome, 'bin', name));
    assert.throws(() => hermeticEnv(x.fake, x.base, x.caller), /vendor CLI is reachable on the smoke PATH/, name);
  }
});

test('a fake bin entry that resolves outside it to anything but a fixture binary refuses the run', (t) => {
  // A script written inside the fake bin is a fake by construction.
  let x = tree(t);
  writeExec(join(x.fake, 'agy'));
  assert.deepEqual(nonFixturesIn(x.fake), []);
  // A link to something outside that is not a fake_* binary is a real CLI in disguise.
  x = tree(t);
  writeExec(join(x.dir, 'real-codex'));
  symlinkSync(join(x.dir, 'real-codex'), join(x.fake, 'codex'));
  assert.equal(nonFixturesIn(x.fake).length, 1);
  assert.throws(() => hermeticEnv(x.fake, x.base, x.caller), /a real codex is reachable through/);
  // The same for a link into a directory that only looks like a build dir.
  x = tree(t);
  writeExec(join(x.fixtures, 'cursor-agent'));
  symlinkSync(join(x.fixtures, 'cursor-agent'), join(x.fake, 'cursor-agent'));
  assert.throws(() => hermeticEnv(x.fake, x.base, x.caller), /a real cursor-agent is reachable/);
});

test('a run that executes 0 tests, or reports no step, fails', () => {
  const zero = checks([], 0, [], countTestsRun('running 0 tests\n\ntest result: ok. 0 passed; 0 failed; 0 ignored; 0 measured; 9 filtered out\n'));
  assert.equal(zero.find((c) => c.name === 'harness').verdict, 'PASS', 'cargo itself exits 0 on an empty filter');
  assert.equal(zero.find((c) => c.name === 'tests').verdict, 'FAIL');
  assert.equal(zero.find((c) => c.name === 'steps').verdict, 'FAIL');

  const events = [
    { step: 'boot', ok: true },
    { step: 'pty', ok: true, pid: 41 },
    { step: 'registry', ok: true, rows: [{ pid: 41 }] },
    { step: 'shutdown', ok: true, survivors: 0 },
  ];
  const good = checks(events, 0, [], countTestsRun('running 1 test\ntest full_path_smoke ... ok\n\ntest result: ok. 1 passed; 0 failed; 0 ignored\n'));
  assert.deepEqual(good.filter((c) => c.verdict === 'FAIL'), []);

  const bad = checks([...events, { step: 'detect', ok: false }], 0, [], 1);
  assert.equal(bad.find((c) => c.name === 'steps').verdict, 'FAIL');
  assert.equal(countTestsRun('test result: FAILED. 0 passed; 2 failed;\ntest result: ok. 3 passed; 0 failed;'), 5);
});

test('leftovers are this run\'s survivors only: orphans and processes below it, never another program\'s', () => {
  const SELF = 900;
  const before = parsePs('  77     1 /x/target/debug/fake_agent hang\n');
  const after = parsePs(
    [
      '  77     1 /x/target/debug/fake_agent hang', // already there before the run
      ' 100    50 /bin/zsh -c eval cd /private/tmp/claude-501/x', // another session's shell
      ' 101     1 /x/target/debug/fake_agent hang', // an orphaned survivor
      ' 200   900 /Users/u/.cargo/bin/cargo test --test boot',
      ' 201   200 /x/binaries/gilde-node /x/sidecar/sidecar.cjs', // still below this run
      ' 300     1 /usr/bin/ssh-agent -l', // an orphan that matches nothing
      '',
    ].join('\n'),
  );
  assert.equal(after.get(101).args, '/x/target/debug/fake_agent hang');
  const found = leftoversOf(before, after, SELF).map(([pid]) => pid).sort();
  assert.deepEqual(found, [101, 201]);
});
