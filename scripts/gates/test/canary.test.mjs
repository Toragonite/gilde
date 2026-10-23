// The F-03 fixture and the canary scan (T-priv-argv, T-priv-3 env variant).

import { test } from 'node:test';
import assert from 'node:assert/strict';
import { spawnSync } from 'node:child_process';
import { mkdirSync, mkdtempSync, rmSync, writeFileSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { dirname, join, resolve } from 'node:path';
import { fileURLToPath } from 'node:url';

import { loadCanaries, needles, scanForCanaries } from '../lib/canary.mjs';

const here = dirname(fileURLToPath(import.meta.url));
const cli = resolve(here, '..', 'canary-scan.mjs');

test('fixture: every placeholder resolves; each vector carries at least one canary; the env carries the env-value canary', () => {
  const { canaries, argv, env } = loadCanaries();
  const text = JSON.stringify({ argv, env });
  assert.ok(!/\$\{[A-Z_]+\}/.test(text), 'no unresolved placeholder');
  for (const v of argv) assert.ok(needles(canaries).some(({ needle }) => JSON.stringify(v.argv).includes(needle)), v.name);
  assert.equal(env.GILDE_CANARY_SECRET, canaries.ENV_VALUE);
  assert.ok(argv.some((v) => v.argv[0] === '/bin/sh' && v.argv[1] === '-c'), 'a sh -c pane command');
  assert.ok(argv.some((v) => v.argv.some((w) => w.includes('apiKeyHelper'))), 'a --settings with apiKeyHelper');
  assert.ok(argv.some((v) => v.argv.some((w) => w.startsWith('sk-ant-') || w.includes('sk-ant-'))), 'an sk-ant- token');
});

test('scan: finds canaries in UTF-8 and UTF-16LE (SQLite pages), names only; clean trees pass', () => {
  const { canaries } = loadCanaries();
  const dir = mkdtempSync(join(tmpdir(), 'canary-'));
  try {
    mkdirSync(join(dir, 'logs'));
    writeFileSync(join(dir, 'logs', 'clean.log'), 'session started pid=4242 argv=["/usr/local/bin/claude","--settings","***"]\n');
    assert.deepEqual(scanForCanaries([dir], canaries), []);
    writeFileSync(join(dir, 'state.db'), Buffer.concat([Buffer.alloc(100, 0), Buffer.from(`AWS_SECRET_ACCESS_KEY=${canaries.AWS_SECRET}`, 'utf16le'), Buffer.alloc(100, 0)]));
    writeFileSync(join(dir, 'logs', 'leak.log'), `env GILDE_CANARY_SECRET=${canaries.ENV_VALUE}\n`);
    const hits = scanForCanaries([dir], canaries);
    assert.deepEqual(hits.map((h) => [h.file.slice(dir.length), h.names]).sort(), [['/logs/leak.log', ['ENV_VALUE']], ['/state.db', ['AWS_SECRET']]]);
    const r = spawnSync(process.execPath, [cli, dir], { encoding: 'utf8' });
    assert.equal(r.status, 1);
    assert.ok(!r.stdout.includes(canaries.ENV_VALUE), 'the scan never prints a canary value');
    assert.equal(spawnSync(process.execPath, [cli, join(dir, 'logs', 'clean.log')], { encoding: 'utf8' }).status, 0);
    assert.equal(spawnSync(process.execPath, [cli, join(dir, 'logs', 'clean.log'), '--extra', 'started pid'], { encoding: 'utf8' }).status, 1, '--extra adds a canary');
    assert.equal(spawnSync(process.execPath, [cli], { encoding: 'utf8' }).status, 2);
    assert.equal(spawnSync(process.execPath, [cli, join(dir, 'nope')], { encoding: 'utf8' }).status, 2);
  } finally {
    rmSync(dir, { recursive: true, force: true });
  }
});
