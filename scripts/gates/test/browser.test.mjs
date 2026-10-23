// The G4 browser-gate runner and the token contrast gate. The real-Chrome
// self-test runs when GILDE_GATES_CHROME=1 (it launches headless Chrome with a
// temp profile and --use-mock-keychain).

import { test } from 'node:test';
import assert from 'node:assert/strict';
import { spawnSync } from 'node:child_process';
import { chmodSync, mkdirSync, mkdtempSync, rmSync, symlinkSync, writeFileSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { dirname, join, resolve } from 'node:path';
import { fileURLToPath } from 'node:url';
import { Script } from 'node:vm';

import { DENSITY_MAX, DENSITY_SCENES, FLOOR_SCENES, FLOOR_WIDTHS, SAMPLE_TIMES, SCENE_EXCEPTIONS, SNAPSHOT_SCENES, detailText, ensureOutDir, gatedScenes } from '../browser-gates.mjs';
import { parseSceneNames, showcaseScenes } from '../lib/scenes.mjs';
import { PAIRS, check, contrast, tokensOf } from '../contrast.mjs';
import { ARIA, AXE, CLIPPED, DENSITY, FLOOR, FOCUS, MODAL } from '../lib/page-checks.mjs';
import { serveStatic } from '../lib/static-server.mjs';

const here = dirname(fileURLToPath(import.meta.url));
const repo = resolve(here, '..', '..', '..');
const gates = resolve(here, '..', 'browser-gates.mjs');

test('the gate matrix covers §14.3: Floor sizes, widths, sample times; the snapshot screen list', () => {
  assert.deepEqual(FLOOR_SCENES, ['floor-1', 'floor-8', 'floor-20', 'floor-31', 'team-floor']);
  assert.deepEqual(FLOOR_WIDTHS, [1440, 1200, 900]);
  assert.deepEqual(SAMPLE_TIMES, [0, 5, 14, 15, 27, 30]);
  for (const s of ['home', 'session', 'watching', 'split', 'team', 'board', 'team-floor', 'newsession', 'history', 'accounts', 'quit']) assert.ok(SNAPSHOT_SCENES.includes(s), s);
});

test('M15 density (A26.6): ≤ 16 in Home\'s main region, plain and with a notice + warning line', () => {
  assert.equal(DENSITY_MAX, 16);
  assert.deepEqual(DENSITY_SCENES, ['home', 'home-notice']);
  assert.match(DENSITY, /querySelector\('main,\[role=main\]'\)/, 'scoped to the main landmark');
});

test('the clipping check skips visually hidden nodes (A26.4)', () => {
  assert.match(CLIPPED, /srOnly\(el\)/, 'the skip is applied');
  assert.match(CLIPPED, /r\.width <= 1 && r\.height <= 1/, 'only a 1×1 box');
  assert.match(CLIPPED, /clipPath/, 'clip-path form');
  assert.match(CLIPPED, /cs\.clip/, 'clip form');
});

test('page checks are valid expressions (compiled, never run)', () => {
  for (const [name, expr] of Object.entries({ ARIA, AXE, CLIPPED, DENSITY, FLOOR, FOCUS, MODAL })) {
    assert.doesNotThrow(() => new Script(`(${expr});`), name);
  }
});

test('static server: serves files, falls back to index.html, never escapes the root', async () => {
  const dir = mkdtempSync(join(tmpdir(), 'gates-static-'));
  try {
    mkdirSync(join(dir, 'assets'));
    writeFileSync(join(dir, 'index.html'), '<!doctype html><p>index</p>');
    writeFileSync(join(dir, 'assets', 'a.js'), 'export {}');
    const s = await serveStatic(dir);
    try {
      const get = async (p) => {
        const r = await fetch(s.url + p);
        return { status: r.status, type: r.headers.get('content-type'), body: await r.text() };
      };
      assert.equal((await get('/assets/a.js')).type, 'text/javascript; charset=utf-8');
      assert.match((await get('/some/route?scene=home')).body, /index/);
      const escape = await get('/..%2f..%2fetc%2fpasswd');
      assert.notEqual(escape.status, 200, 'an escaping path is not served as a file');
      assert.ok(!(escape.body.includes('root:')));
    } finally {
      await s.close();
    }
  } finally {
    rmSync(dir, { recursive: true, force: true });
  }
});

test('CLI: one mode at a time, known gates only, a missing build is exit 2', () => {
  assert.equal(spawnSync(process.execPath, [gates], { encoding: 'utf8' }).status, 2);
  assert.equal(spawnSync(process.execPath, [gates, '--self-test', '--build'], { encoding: 'utf8' }).status, 2);
  assert.equal(spawnSync(process.execPath, [gates, '--self-test', '--gates', 'nope'], { encoding: 'utf8' }).status, 2);
  const empty = mkdtempSync(join(tmpdir(), 'gates-empty-'));
  try {
    assert.equal(spawnSync(process.execPath, [gates, '--dist', empty], { encoding: 'utf8' }).status, 2);
  } finally {
    rmSync(empty, { recursive: true, force: true });
  }
});

test('real Chrome self-test: every gate passes on clean.html and fails on broken.html', { skip: process.env.GILDE_GATES_CHROME === '1' ? false : 'set GILDE_GATES_CHROME=1 to launch headless Chrome' }, () => {
  const r = spawnSync(process.execPath, [gates, '--self-test'], { encoding: 'utf8', timeout: 300_000 });
  assert.equal(r.status, 0, r.stdout + r.stderr);
  assert.match(r.stdout, /10 pass, 0 fail, 0 pending/);
});

test('contrast: WCAG math, token parsing for both themes, the ux-ia §10 pairs', () => {
  assert.equal(contrast('#000000', '#ffffff'), 21);
  assert.equal(contrast('#ffffff', '#ffffff'), 1);
  assert.equal(contrast('#767676', '#ffffff'), 4.54, 'the WCAG reference grey');
  assert.ok(contrast('#6b6b6b', '#e2e2e2') < 4.5, 'the known failing pair of ux-ia §10 (--n8 on --n4) still fails');
  const css = ':root {\n --n0: #ffffff; --n11: #111111; }\n:root[data-theme="dark"] { --n0: #000000; --n11: #eeeeee; }';
  assert.deepEqual(tokensOf(css, /:root\s*\{/), { '--n0': '#ffffff', '--n11': '#111111' });
  assert.ok(!PAIRS.some(([fg, bg]) => fg === '--n8' && bg === '--n4'), 'selected rows use --n3, so --n8 on --n4 is not a pair');
  assert.ok(!PAIRS.some(([fg, , min]) => fg === '--n7' && min >= 4.5), '--n7 (.dim) never carries information');
  const results = check(css);
  assert.ok(results.some((r) => r.ratio === null && !r.ok), 'a missing token fails its pair');
  const dir = mkdtempSync(join(tmpdir(), 'gates-contrast-'));
  try {
    const bad = join(dir, 't.css');
    writeFileSync(bad, 'body { color: red; }');
    assert.equal(spawnSync(process.execPath, [resolve(here, '..', 'contrast.mjs'), '--tokens', bad], { encoding: 'utf8' }).status, 2);
  } finally {
    rmSync(dir, { recursive: true, force: true });
  }
});

test('smoke: the hermetic env refuses a real vendor CLI on its PATH and accepts fakes only', () => {
  const dir = mkdtempSync(join(tmpdir(), 'smoke-env-'));
  try {
    const fake = join(dir, 'fakes');
    mkdirSync(fake);
    writeFileSync(join(fake, 'claude'), '#!/bin/sh\nexit 0\n');
    chmodSync(join(fake, 'claude'), 0o755);
    const smoke = join(repo, 'scripts', 'smoke.mjs');
    const ok = spawnSync(process.execPath, [smoke, '--fake-bin', fake, '--check-only'], { encoding: 'utf8' });
    assert.equal(ok.status, 0, ok.stdout + ok.stderr);
    assert.match(ok.stdout, /fakes reachable: claude/);
    assert.equal(spawnSync(process.execPath, [smoke], { encoding: 'utf8' }).status, 2, '--fake-bin is required');
    // A vendor name in the fake dir that links to a binary outside it is a real CLI in disguise.
    const outside = join(dir, 'outside');
    mkdirSync(outside);
    writeFileSync(join(outside, 'codex'), '#!/bin/sh\nexit 0\n');
    chmodSync(join(outside, 'codex'), 0o755);
    symlinkSync(join(outside, 'codex'), join(fake, 'codex'));
    const refused = spawnSync(process.execPath, [smoke, '--fake-bin', fake, '--check-only'], { encoding: 'utf8' });
    assert.equal(refused.status, 2);
    assert.match(refused.stderr, /a real codex is reachable/);
  } finally {
    rmSync(dir, { recursive: true, force: true });
  }
});

test('T31: the gated scenes are the showcase registry minus explicit exceptions, each with a reason', () => {
  const registry = showcaseScenes(repo);
  assert.ok(registry.includes('history-read') && registry.includes('recovered'), 'the registry is read from src/showcase/scenes.ts');
  assert.deepEqual(SNAPSHOT_SCENES, registry.filter((n) => !(n in SCENE_EXCEPTIONS)), 'every other scene is gated, in registry order');
  for (const [name, reason] of Object.entries(SCENE_EXCEPTIONS)) {
    assert.ok(registry.includes(name), `exception ${name} names a registry scene`);
    assert.ok(typeof reason === 'string' && reason.length >= 10, `exception ${name} says why`);
  }
  assert.deepEqual(gatedScenes(['a', 'b', 'new-scene'], { b: 'why' }), ['a', 'new-scene'], 'a new scene is gated without an edit');
});

test('T31: the scene registry parser reads SCENE_NAMES and refuses a missing, empty or duplicated list', () => {
  const src = "export const SCENE_NAMES = [\n  'home', 'pane-tiles',\n  'quit',\n] as const;";
  assert.deepEqual(parseSceneNames(src), ['home', 'pane-tiles', 'quit']);
  assert.throws(() => parseSceneNames('export const OTHER = [];'), /not found/);
  assert.throws(() => parseSceneNames('export const SCENE_NAMES = [] as const;'), /empty/);
  assert.throws(() => parseSceneNames("export const SCENE_NAMES = ['a', 'a'] as const;"), /duplicate/);
});

test('T31: failure details are readable text, never "[object Object]"', () => {
  assert.equal(detailText([{ a: 'label:db-agent', b: 'bubble:3', kind: 'label' }, { a: 'x', b: 'y', kind: 'seg' }]), 'label: label:db-agent × bubble:3; seg: x × y');
  assert.equal(detailText({ id: 'color-contrast', nodes: 2 }), '{"id":"color-contrast","nodes":2}');
  assert.equal(detailText('plain'), 'plain');
  assert.equal(detailText(3), '3');
  assert.equal(detailText(null), 'null');
  for (const v of [{}, [{}], { a: 1 }, [[{ x: 1 }]]]) assert.ok(!detailText(v).includes('[object Object]'), JSON.stringify(v));
});

test('T31: --out is created (with parents) when missing; no --out gives a temp dir', () => {
  const base = mkdtempSync(join(tmpdir(), 'gates-out-'));
  try {
    const nested = join(base, 'not', 'there', 'yet');
    assert.equal(ensureOutDir(nested), nested);
    writeFileSync(join(nested, 'manifest.json'), '[]');
    assert.equal(ensureOutDir(nested), nested, 'an existing dir is fine');
    const temp = ensureOutDir(null);
    assert.ok(temp.includes('gilde-gates-'));
    rmSync(temp, { recursive: true, force: true });
  } finally {
    rmSync(base, { recursive: true, force: true });
  }
});
