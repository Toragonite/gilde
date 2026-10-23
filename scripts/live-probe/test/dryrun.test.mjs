// The probe runner end to end against the fakes (contract §15.2 T19 "probe
// scripts dry-run against fakes"), and the CLI refusals that protect a live
// phase. Every run here uses a temp root; no real CLI can start (the guard
// accepts only the fake binary in a dry run).

import { test } from 'node:test';
import assert from 'node:assert/strict';
import { spawnSync } from 'node:child_process';
import { existsSync, mkdtempSync, readFileSync, readdirSync, rmSync } from 'node:fs';
import { tmpdir, userInfo } from 'node:os';
import { dirname, join, resolve } from 'node:path';
import { fileURLToPath } from 'node:url';

import { loadCanaries, needles } from '../../gates/lib/canary.mjs';
import { PROBES, runnable } from '../probes/index.mjs';

const here = dirname(fileURLToPath(import.meta.url));
const runner = resolve(here, '..', 'run.mjs');

function run(args, { timeout = 300_000 } = {}) {
  return spawnSync(process.execPath, [runner, ...args], { encoding: 'utf8', timeout, env: { PATH: '/usr/bin:/bin', HOME: process.env.HOME, TMPDIR: process.env.TMPDIR ?? '/tmp' } });
}

test('the LP list covers LP-01 … LP-40 and every entry says how it runs', () => {
  const ids = PROBES.map((p) => p.id);
  for (let n = 1; n <= 40; n++) assert.ok(ids.includes(`LP-${String(n).padStart(2, '0')}`), `LP-${n}`);
  assert.ok(ids.includes('LP-01b'));
  for (const p of PROBES) assert.ok(p.run || p.manual || p.notRun || p.blocked || p.conditional, `${p.id} has a status`);
  assert.equal(PROBES.find((p) => p.id === 'LP-01b').notRun.includes('OQ1'), true);
  assert.ok(PROBES.find((p) => p.id === 'LP-40').run, 'LP-40 (A6) is in the kit');
  assert.ok(runnable().length >= 19);
  const list = run(['--list']);
  assert.equal(list.status, 0);
  assert.match(list.stdout, /LP-40\s+pty\s+g\s+kit/);
});

test('dry run: LP-01, LP-09, LP-34 and LP-40 complete against the fakes; records hold no prompt text', () => {
  const root = mkdtempSync(join(tmpdir(), 'lp-dry-'));
  try {
    const r = run(['--dry-run', '--probe', 'LP-01,LP-09,LP-34,LP-40', '--dry-root', root, '--keep']);
    assert.equal(r.status, 0, r.stdout + r.stderr);
    const phase = JSON.parse(readFileSync(join(root, 'records', 'phase.json'), 'utf8'));
    assert.equal(phase.dryRun, true);
    assert.deepEqual(phase.probes.map((p) => p.id), ['LP-01', 'LP-09', 'LP-34', 'LP-40']);
    for (const p of phase.probes) assert.ok(['pass', 'fail', 'inconclusive'].includes(p.verdict), `${p.id}: ${p.verdict ?? p.error}`);
    assert.equal(phase.probes.find((p) => p.id === 'LP-09').observations.b.tokenSource, 'none', 'the scratch config dir is never signed in');
    assert.equal(phase.probes.find((p) => p.id === 'LP-40').observations.inRepoWorktree.dialogShown, false);
    assert.equal(phase.probes.find((p) => p.id === 'LP-40').observations.homeWorktree.dialogShown, true);
    // Shapes only: no prompt sent by a probe appears in any record.
    const files = [];
    const walk = (d) => {
      for (const n of readdirSync(d, { withFileTypes: true })) {
        if (n.isDirectory()) walk(join(d, n.name));
        else files.push(join(d, n.name));
      }
    };
    walk(join(root, 'records'));
    const text = files.map((f) => readFileSync(f, 'utf8')).join('\n');
    for (const phrase of ['Use the Bash tool', 'Reply with one word', 'date > lp01', 'Plan a one-line']) assert.ok(!text.includes(phrase), `records must not contain "${phrase}"`);
    const { canaries } = loadCanaries();
    for (const { needle } of needles(canaries)) assert.ok(!text.includes(needle));
    // The LP-40 worktree under the (temp) HOME was cleaned up again.
    assert.equal(existsSync(join(root, 'home', '.gilde', 'worktrees', 'lp40')), false);
  } finally {
    rmSync(root, { recursive: true, force: true });
  }
  assert.equal(existsSync(join(userInfo().homedir, '.gilde', 'worktrees', 'lp40')), false, 'the real home is never touched');
});

test('refusals before anything runs: live without a quota reading, fake knobs on a live run, --watch ~/.claude, unknown probes', () => {
  const r1 = run(['--live', '--probe', 'LP-01']);
  assert.equal(r1.status, 2);
  assert.match(r1.stderr, /quota-file/);
  const r2 = run(['--dry-run', '--claude', '/usr/bin/true']);
  assert.equal(r2.status, 2);
  const r3 = run(['--watch', join(userInfo().homedir, '.claude')]);
  assert.equal(r3.status, 2);
  assert.match(r3.stderr, /defaultAccount/);
  const r4 = run(['--dry-run', '--probe', 'LP-99']);
  assert.equal(r4.status, 2);
  const r5 = run(['--dry-run', '--live']);
  assert.equal(r5.status, 2);
});
