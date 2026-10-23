// The G5 perf harness (contract §11.1, §14.3, F-34) and the F-03 sessions of
// its fixture: parsers on synthetic samples, the wakeup-counter math, the
// fixture's hostile argv and env canary, teardown, and the CLI.

import { test } from 'node:test';
import assert from 'node:assert/strict';
import { execFileSync, spawn, spawnSync } from 'node:child_process';
import { mkdtempSync, readFileSync, rmSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { dirname, join, resolve } from 'node:path';
import { fileURLToPath } from 'node:url';

import { loadCanaries, scanForCanaries } from '../../gates/lib/canary.mjs';
import { materialize } from '../lib/fixture.mjs';
import { BUDGETS, judge, measureWakeups, parsePowermetrics, parseTop, readCounter, summarizeCpu, wakeupRate } from '../lib/measure.mjs';

const here = dirname(fileURLToPath(import.meta.url));
const fixtures = resolve(here, '..', 'fixtures');
const runner = resolve(here, '..', 'run.mjs');

test('top output: samples parsed, the first sample dropped, CPU summed over the pids', () => {
  const samples = parseTop(readFileSync(join(fixtures, 'top-sample.txt'), 'utf8'));
  assert.equal(samples.length, 3);
  assert.deepEqual(samples[0], { 4242: 7.4, 4300: 3.1, 4310: 0 });
  const s = summarizeCpu(samples, [4242, 4300, 4310]);
  assert.equal(s.samples, 2);
  assert.equal(s.totalPct, 0.75, '(0.7 + 0.8) / 2 — the 10.5 % first sample does not count');
  assert.deepEqual(summarizeCpu(samples, [4242, 9999]).missing, [9999]);
});

test('powermetrics cross-check: the core row, interrupt wakeups per 60 s', () => {
  const text = readFileSync(join(fixtures, 'powermetrics-sample.txt'), 'utf8');
  const core = parsePowermetrics(text, 4242);
  assert.equal(core.name, 'Gilde Code');
  assert.equal(core.intrPerS, 0.5);
  assert.equal(core.per60s, 30);
  assert.equal(parsePowermetrics(text, 4300).per60s, 10.2);
  assert.equal(parsePowermetrics(text, 1234), null);
});

test('budgets: §11.1 values, judged per focus state', () => {
  assert.deepEqual(BUDGETS, { cpuPct: { focused: 1, unfocused: 0.5 }, wakeupsPer60s: { focused: 40, unfocused: 12 } });
  assert.deepEqual(judge({ cpuPct: 0.75, wakeupsPer60s: 30 }, 'focused').map((r) => r.ok), [true, true]);
  assert.deepEqual(judge({ cpuPct: 0.75, wakeupsPer60s: 30 }, 'unfocused').map((r) => r.ok), [false, false]);
  assert.deepEqual(judge({ cpuPct: 1.0 }, 'focused').map((r) => r.ok), [true], 'the budget is inclusive');
});

test('wakeup counter: rate per 60 s from two readings by the core clock; bad readings are null', () => {
  assert.equal(wakeupRate({ wakeups: 100, monotonicMs: 0 }, { wakeups: 120, monotonicMs: 30_000 }), 40);
  assert.equal(wakeupRate({ wakeups: 100, monotonicMs: 10 }, { wakeups: 90, monotonicMs: 20 }), null, 'a counter going back (restart) is no reading');
  assert.equal(wakeupRate(null, { wakeups: 1, monotonicMs: 1 }), null);
  assert.equal(readCounter('/nonexistent/wakeups.json'), null);
});

test('wakeup counter: measured against the stand-in core counter (1200 per minute over 2 s)', async () => {
  const dir = mkdtempSync(join(tmpdir(), 'perf-wake-'));
  const file = join(dir, 'w.json');
  const child = spawn(process.execPath, [resolve(here, '..', 'fakes', 'wakeup-counter.mjs'), '--per-minute', '1200'], { env: { PATH: '/usr/bin:/bin', GILDE_WAKEUP_COUNT_FILE: file }, detached: true, stdio: 'ignore' });
  try {
    const r = await measureWakeups(file, 2);
    assert.ok(r.per60s >= 900 && r.per60s <= 1300, `≈1200, got ${r.per60s}`);
  } finally {
    process.kill(-child.pid, 'SIGTERM');
    rmSync(dir, { recursive: true, force: true });
  }
});

test('fixture: 10 watched stand-ins with registry rows; two carry the F-03 argv and env canary; the harness writes no canary; teardown is complete', async () => {
  const root = mkdtempSync(join(tmpdir(), 'perf-fix-'));
  const fixture = await materialize(root);
  try {
    assert.equal(fixture.sessions.length, 10);
    const { canaries } = loadCanaries();
    const hostile = fixture.sessions.filter((s) => s.hostile);
    assert.equal(hostile.length, 2);
    for (const s of hostile) {
      const args = execFileSync('/bin/ps', ['-o', 'args=', '-p', String(s.pid)], { encoding: 'utf8' });
      assert.ok(args.includes(canaries.AWS_SECRET) || args.includes(canaries.PROMPT), 'the hostile vector is the process argv (what discovery reads)');
    }
    for (const s of fixture.sessions) {
      const row = JSON.parse(readFileSync(join(fixture.configDir, 'sessions', `${s.pid}.json`), 'utf8'));
      assert.equal(row.pid, s.pid);
      assert.match(row.procStart, /^\w{3} \w{3} +\d+ \d\d:\d\d:\d\d \d{4}$/, 'procStart in the registry grammar');
    }
    assert.deepEqual(scanForCanaries([fixture.configDir, fixture.gildeHome], canaries), [], 'registry rows and transcripts carry no argv or env');
  } finally {
    const survivors = await fixture.stop();
    assert.deepEqual(survivors, [], 'every stand-in ended on SIGTERM');
    rmSync(root, { recursive: true, force: true });
  }
});

test('CLI: the dry run completes; bad usage exits 2', () => {
  const dry = spawnSync(process.execPath, [runner, '--dry-run', '--seconds', '2'], { encoding: 'utf8', timeout: 120_000 });
  assert.equal(dry.status, 0, dry.stdout + dry.stderr);
  assert.match(dry.stdout, /canary scan of the fixture: clean/);
  for (const args of [[], ['--pids', '123'], ['--pids', 'x', '--focused'], ['--dry-run', '--seconds', '0'], ['--pids', '123', '--focused', '--powermetrics', 'f']]) {
    assert.equal(spawnSync(process.execPath, [runner, ...args], { encoding: 'utf8' }).status, 2, args.join(' '));
  }
});
