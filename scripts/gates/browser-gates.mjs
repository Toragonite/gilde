#!/usr/bin/env node
/**
 * browser-gates.mjs — the G4 real-browser and accessibility gates (contract
 * §14.3) on the showcase build, in headless Chrome over CDP (temp profile,
 * `--use-mock-keychain`; lib/cdp.mjs).
 *
 *   node scripts/gates/browser-gates.mjs --dist <showcase build dir> [--gates …] [--out <dir>] [--baseline <dir>] [--strict]
 *   node scripts/gates/browser-gates.mjs --build [...]          build the showcase first (VITE_GILDE_SHOWCASE=1 vite build)
 *   node scripts/gates/browser-gates.mjs --self-test            run every gate on fixtures/browser/{clean,broken}.html
 *
 * Gates (`--gates floor,density,a11y,clipping,keyboard,snapshots`, default all):
 *  - floor: scenes floor-1/8/20/31 and team-floor (store fixtures with 1, 8,
 *    20, 31 agents, native-inferred and orchestrator teams) at 1440/1200/900
 *    × light/dark × t ∈ {0, 5, 14, 15, 27, 30} s (`__gildeShowcase.advanceTo`):
 *    `__flOverlaps()` must be `[]`;
 *  - density: Focus Home at 1440×900, ≤ 16 controls above the fold in the
 *    main region, plain and with one notice + one warning line (M15, A26.6);
 *  - a11y: axe-core (pinned dev dependency) on every screen, no serious or
 *    critical violations; every input named; no duplicate landmarks; the
 *    Session scene's approval card is a group with aria-keyshortcuts and at
 *    most one assertive live region exists; no uncaught page exception;
 *  - clipping: no text cut without an ellipsis at 900 px (KO and EN);
 *  - keyboard: the New session sheet traps Tab and closes on Escape; ⌘N opens
 *    it from Home; on every scene, a modal sheet keeps Tab inside and
 *    otherwise Tab reaches a control;
 *  - snapshots: one PNG per screen × KO/EN × light/dark into `--out` (created
 *    when missing), with a sha-256 manifest; `--baseline` lists the changed
 *    ones for review.
 * The per-screen gates (a11y, clipping, keyboard, snapshots) run on every scene
 * of the showcase registry (`src/showcase/scenes.ts`, lib/scenes.mjs) except
 * the ones in `SCENE_EXCEPTIONS`, each with its reason (T31).
 * A check whose feature this build does not have yet (the Floor before T17,
 * the screens before wave 2) is `pending`; `--strict` (the G4 run of wave 3)
 * turns pending into a failure.
 *
 * Exit codes: 0 no failure; 1 a gate failed (or pending under --strict, or a
 * self-test expectation missed); 2 the gates could not run.
 */

import { spawnSync } from 'node:child_process';
import { createHash } from 'node:crypto';
import { existsSync, mkdirSync, mkdtempSync, readFileSync, readdirSync, rmSync, writeFileSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { dirname, join, resolve } from 'node:path';
import { fileURLToPath } from 'node:url';

import { Page, launchChrome } from './lib/cdp.mjs';
import { ARIA, AXE, CLIPPED, DENSITY, FLOOR, FOCUS, MODAL, READY } from './lib/page-checks.mjs';
import { showcaseScenes } from './lib/scenes.mjs';
import { serveStatic } from './lib/static-server.mjs';

const HERE = dirname(fileURLToPath(import.meta.url));
const ROOT = resolve(HERE, '..', '..');
const ALL_GATES = ['floor', 'density', 'a11y', 'clipping', 'keyboard', 'snapshots'];
/**
 * Scenes the per-screen gates skip — each with its reason. Every other scene of
 * the showcase registry is gated, so a new scene is covered without an edit here.
 */
export const SCENE_EXCEPTIONS = Object.freeze({
  floor: 'the same fixture as floor-20 (floorScene(20)); gated once, as floor-20',
});
/** The gated scenes: the registry minus the exceptions, in registry order. */
export function gatedScenes(names = showcaseScenes(ROOT), exceptions = SCENE_EXCEPTIONS) {
  return names.filter((n) => !Object.prototype.hasOwnProperty.call(exceptions, n));
}
export const SNAPSHOT_SCENES = gatedScenes();
export const FLOOR_SCENES = ['floor-1', 'floor-8', 'floor-20', 'floor-31', 'team-floor'];
export const FLOOR_WIDTHS = [1440, 1200, 900];
export const SAMPLE_TIMES = [0, 5, 14, 15, 27, 30];
export const DENSITY_MAX = 16;
/** Focus Home's density is measured in its main area, plain and with a notice + warning line (M15, A26.6). */
export const DENSITY_SCENES = ['home', 'home-notice'];

/** A check's detail as readable text: Floor overlaps read "kind: a × b", other objects as JSON, never "[object Object]". */
export function detailText(value) {
  if (value === null || value === undefined) return String(value);
  if (typeof value === 'string') return value;
  if (typeof value !== 'object') return String(value);
  if (Array.isArray(value)) return value.map(detailText).join('; ');
  if (typeof value.a === 'string' && typeof value.b === 'string' && typeof value.kind === 'string') return `${value.kind}: ${value.a} × ${value.b}`;
  try {
    return JSON.stringify(value);
  } catch {
    return Object.prototype.toString.call(value);
  }
}

/** `--out`: created (with its parents) when missing; a temp dir when not given. */
export function ensureOutDir(out) {
  if (out === null || out === undefined) return mkdtempSync(join(tmpdir(), 'gilde-gates-'));
  mkdirSync(out, { recursive: true });
  return out;
}

function fail2(msg) {
  console.error(`browser-gates: ${msg}`);
  process.exit(2);
}

export function parseArgs(argv) {
  const o = { dist: null, build: false, selfTest: false, gates: ALL_GATES, out: null, baseline: null, strict: false, chrome: null };
  for (let i = 0; i < argv.length; i++) {
    const a = argv[i];
    const val = () => (i + 1 < argv.length ? argv[++i] : fail2(`${a} needs a value`));
    switch (a) {
      case '--dist': o.dist = resolve(val()); break;
      case '--build': o.build = true; break;
      case '--self-test': o.selfTest = true; break;
      case '--gates': o.gates = val().split(',').map((s) => s.trim()); break;
      case '--out': o.out = resolve(val()); break;
      case '--baseline': o.baseline = resolve(val()); break;
      case '--strict': o.strict = true; break;
      case '--chrome': o.chrome = resolve(val()); break;
      default: fail2(`unknown argument ${a}`);
    }
  }
  if ([o.dist !== null, o.build, o.selfTest].filter(Boolean).length !== 1) fail2('pick one of --dist, --build, --self-test');
  for (const g of o.gates) if (!ALL_GATES.includes(g)) fail2(`unknown gate ${g}`);
  return o;
}

/** A result row: status pass | fail | pending. */
const row = (gate, target, status, detail) => ({ gate, target, status, detail });

async function load(page, url, width, height, theme) {
  await page.viewport(width, height, theme);
  await page.goto(url);
  return page.waitFor(READY, 30_000);
}

const axeSource = () => readFileSync(join(ROOT, 'node_modules', 'axe-core', 'axe.min.js'), 'utf8');

async function a11yChecks(page, target, { approval = false } = {}) {
  const out = [];
  await page.evaluate(axeSource());
  const violations = (await page.evaluate(AXE, 120_000)).filter((v) => v.impact === 'serious' || v.impact === 'critical');
  out.push(row('a11y', target, violations.length ? 'fail' : 'pass', violations.length ? violations.map((v) => `${v.id}(${v.impact}, ${v.nodes})`).join(' ') : 'no serious/critical axe violations'));
  const aria = await page.evaluate(ARIA);
  out.push(row('a11y', target, aria.unnamedInputs ? 'fail' : 'pass', `unnamed inputs: ${aria.unnamedInputs}`));
  out.push(row('a11y', target, aria.duplicateLandmarks ? 'fail' : 'pass', `duplicate landmarks: ${aria.duplicateLandmarks}`));
  if (approval) {
    out.push(row('a11y', target, aria.approvalGroup ? 'pass' : 'pending', aria.approvalGroup ? 'approval card is a group with aria-keyshortcuts' : 'no approval group in this build'));
    out.push(row('a11y', target, aria.assertiveRegions <= 1 ? 'pass' : 'fail', `assertive live regions: ${aria.assertiveRegions}`));
  }
  out.push(row('a11y', target, page.exceptions.length ? 'fail' : 'pass', page.exceptions.length ? `uncaught: ${page.exceptions.slice(0, 3).map(detailText).join(' | ')}` : 'no uncaught page exception'));
  return out;
}

async function floorChecks(page, target) {
  const out = [];
  for (const t of SAMPLE_TIMES) {
    await page.evaluate(`window.__gildeShowcase ? window.__gildeShowcase.advanceTo(${t}) : 0`);
    await page.evaluate(`window.__flSetTime ? window.__flSetTime(${t}) : 0`);
    await new Promise((r) => setTimeout(r, 50));
    const overlaps = await page.evaluate(FLOOR);
    if (overlaps === null) {
      out.push(row('floor', `${target} t=${t}`, 'pending', '__flOverlaps() not in this build (the Floor, T17)'));
      break;
    }
    out.push(row('floor', `${target} t=${t}`, overlaps.length ? 'fail' : 'pass', overlaps.length ? detailText(overlaps.slice(0, 5)) : '[]'));
  }
  return out;
}

async function keyboardChecks(page, target, base) {
  const out = [];
  // The New session sheet (opened by the scene): focus inside, Tab stays inside, Escape closes.
  await load(page, `${base}&scene=newsession`, 1440, 900, 'light');
  let f = await page.evaluate(FOCUS);
  if (!f.dialog) {
    out.push(row('keyboard', `${target} sheet`, 'pending', 'no dialog in this build'));
  } else {
    for (let i = 0; i < f.focusables + 2; i++) await page.key('Tab');
    const inside = (await page.evaluate(FOCUS)).focusInside;
    out.push(row('keyboard', `${target} sheet`, inside ? 'pass' : 'fail', inside ? 'Tab stays inside the sheet' : 'focus left the sheet'));
    await page.key('Escape');
    await new Promise((r) => setTimeout(r, 200));
    f = await page.evaluate(FOCUS);
    out.push(row('keyboard', `${target} sheet`, f.dialog ? 'fail' : 'pass', f.dialog ? 'Escape did not close the sheet' : 'Escape closes the sheet'));
  }
  // ⌘N from Home opens the New session sheet.
  await load(page, `${base}&scene=home`, 1440, 900, 'light');
  await page.key('n', { code: 'KeyN', modifiers: 4 });
  const opened = await page.waitFor(`(${FOCUS}).dialog`, 3000);
  out.push(row('keyboard', `${target} ⌘N`, opened ? 'pass' : 'pending', opened ? '⌘N opens New session' : 'no sheet opened (screens of wave 2)'));
  return out;
}

/** Every scene: a modal sheet keeps Tab inside; otherwise Tab reaches a control (the page is keyboard-reachable). */
async function sceneKeyboardCheck(page, target) {
  const m = await page.evaluate(MODAL);
  if (m.modal) {
    for (let i = 0; i < m.focusables + 2; i++) await page.key('Tab');
    const inside = (await page.evaluate(MODAL)).inside;
    return row('keyboard', target, inside ? 'pass' : 'fail', inside ? 'Tab stays inside the open sheet' : 'focus left the open sheet');
  }
  let reached = null;
  for (let i = 0; i < 3 && !reached; i++) {
    await page.key('Tab');
    reached = (await page.evaluate(MODAL)).active;
  }
  return row('keyboard', target, reached ? 'pass' : 'fail', reached ? `Tab reaches a control (${reached})` : 'Tab reached no control');
}

async function runShowcase(o, base, page) {
  const results = [];
  const shots = [];
  const outDir = ensureOutDir(o.out);
  if (o.gates.includes('density')) {
    // Home as it usually is, and at its busiest: one notice and one warning line (A26.6).
    for (const scene of DENSITY_SCENES) {
      for (const lang of ['en', 'ko']) {
        if (!(await load(page, `${base}&scene=${scene}&lang=${lang}`, 1440, 900, 'light'))) {
          results.push(row('density', `${scene} ${lang}`, 'fail', 'the showcase never became ready'));
          continue;
        }
        const n = await page.evaluate(DENSITY);
        results.push(row('density', `${scene} ${lang} 1440×900`, n <= DENSITY_MAX ? 'pass' : 'fail', `${n} controls above the fold in main (≤ ${DENSITY_MAX})`));
      }
    }
  }
  for (const scene of SNAPSHOT_SCENES) {
    for (const lang of ['en', 'ko']) {
      for (const theme of ['light', 'dark']) {
        const target = `${scene} ${lang} ${theme}`;
        if (!(await load(page, `${base}&scene=${scene}&lang=${lang}&theme=${theme}`, 1440, 900, theme))) {
          results.push(row('a11y', target, 'fail', 'the showcase never became ready'));
          continue;
        }
        if (o.gates.includes('a11y')) results.push(...(await a11yChecks(page, target, { approval: scene === 'session' })));
        if (o.gates.includes('keyboard') && lang === 'en' && theme === 'light') results.push(await sceneKeyboardCheck(page, `${scene} keys`));
        if (o.gates.includes('snapshots')) {
          const png = await page.screenshot();
          const name = `${scene}-${lang}-${theme}.png`;
          writeFileSync(join(outDir, name), png);
          shots.push({ name, sha256: createHash('sha256').update(png).digest('hex') });
        }
        if (o.gates.includes('clipping') && theme === 'light') {
          await page.viewport(900, 900, theme);
          await new Promise((r) => setTimeout(r, 100));
          const clipped = await page.evaluate(CLIPPED);
          results.push(row('clipping', `${scene} ${lang} 900`, clipped.length ? 'fail' : 'pass', clipped.length ? detailText(clipped.slice(0, 5)) : 'no clipped text'));
        }
      }
    }
  }
  if (o.gates.includes('floor')) {
    for (const scene of FLOOR_SCENES) {
      for (const width of FLOOR_WIDTHS) {
        for (const theme of ['light', 'dark']) {
          const target = `${scene} ${width} ${theme}`;
          if (!(await load(page, `${base}&scene=${scene}&theme=${theme}`, width, 900, theme))) {
            results.push(row('floor', target, 'fail', 'the showcase never became ready'));
            continue;
          }
          results.push(...(await floorChecks(page, target)));
        }
      }
    }
  }
  if (o.gates.includes('keyboard')) results.push(...(await keyboardChecks(page, 'showcase', base)));
  if (o.gates.includes('snapshots')) {
    writeFileSync(join(outDir, 'manifest.json'), JSON.stringify(shots, null, 2));
    let changed = [];
    if (o.baseline && existsSync(join(o.baseline, 'manifest.json'))) {
      const old = new Map(JSON.parse(readFileSync(join(o.baseline, 'manifest.json'), 'utf8')).map((s) => [s.name, s.sha256]));
      changed = shots.filter((s) => old.get(s.name) !== s.sha256).map((s) => s.name);
    }
    results.push(row('snapshots', `${shots.length} screens`, 'pass', `written to ${outDir}${o.baseline ? `; ${changed.length} differ from the baseline (review): ${changed.slice(0, 10).join(', ')}` : ''}`));
  }
  return results;
}

/** Self-test: every gate passes on clean.html and fails on broken.html. */
async function runSelfTest(base, page) {
  const results = [];
  const expect = (gate, target, wanted, rows) => {
    const got = rows.some((r) => r.status === 'fail') ? 'fail' : rows.some((r) => r.status === 'pending') ? 'pending' : 'pass';
    results.push(row(`self-test ${gate}`, target, got === wanted ? 'pass' : 'fail', `expected ${wanted}, got ${got}: ${rows.map((r) => r.detail).join(' / ')}`));
  };
  for (const [file, wanted] of [['clean.html', 'pass'], ['broken.html', 'fail']]) {
    const url = `${base}/${file}?scene=session`;
    if (!(await load(page, url, 1440, 900, 'light'))) {
      results.push(row('self-test', file, 'fail', 'fixture never became ready'));
      continue;
    }
    const n = await page.evaluate(DENSITY);
    expect('density', file, wanted, [row('density', file, n <= DENSITY_MAX ? 'pass' : 'fail', `${n} controls`)]);
    expect('a11y', file, wanted, await a11yChecks(page, file, { approval: true }));
    expect('floor', file, wanted, await floorChecks(page, file));
    await page.viewport(900, 900, 'light');
    const clipped = await page.evaluate(CLIPPED);
    expect('clipping', file, wanted, [row('clipping', file, clipped.length ? 'fail' : 'pass', clipped.join(' ') || 'none')]);
    await load(page, url, 1440, 900, 'light');
    const f = await page.evaluate(FOCUS);
    for (let i = 0; i < f.focusables + 2; i++) await page.key('Tab');
    const inside = (await page.evaluate(FOCUS)).focusInside;
    expect('keyboard', file, wanted, [row('keyboard', file, f.dialog && inside ? 'pass' : 'fail', `dialog ${f.dialog}, focus inside after Tab ×${f.focusables + 2}: ${inside}`)]);
  }
  return results;
}

function buildShowcase() {
  const out = mkdtempSync(join(tmpdir(), 'gilde-showcase-build-'));
  const env = { ...process.env, VITE_GILDE_SHOWCASE: '1' };
  const r = spawnSync('npx', ['vite', 'build', '--outDir', out, '--emptyOutDir', '--logLevel', 'error'], { cwd: ROOT, env, stdio: ['ignore', 'inherit', 'inherit'] });
  if (r.status !== 0) {
    rmSync(out, { recursive: true, force: true });
    fail2(`the showcase build failed (exit ${r.status}); the WebView build works again at the wave-2 exit (A8.2)`);
  }
  return out;
}

async function main() {
  const o = parseArgs(process.argv.slice(2));
  const dist = o.selfTest ? join(HERE, 'fixtures', 'browser') : o.build ? buildShowcase() : o.dist;
  if (!existsSync(join(dist, o.selfTest ? 'clean.html' : 'index.html'))) fail2(`no ${o.selfTest ? 'clean.html' : 'index.html'} in ${dist}`);
  if (!existsSync(join(ROOT, 'node_modules', 'axe-core', 'axe.min.js'))) fail2('axe-core is not installed (npm ci)');
  const server = await serveStatic(dist);
  let chrome;
  try {
    chrome = await launchChrome({ chrome: o.chrome });
  } catch (e) {
    await server.close();
    fail2(e.message);
  }
  let results = [];
  try {
    const page = await Page.open(chrome.cdp);
    results = o.selfTest ? await runSelfTest(server.url, page) : await runShowcase(o, `${server.url}/?live=0`, page);
    await page.close();
  } finally {
    await chrome.close();
    await server.close();
    if (o.build) rmSync(dist, { recursive: true, force: true });
  }
  const failed = results.filter((r) => r.status === 'fail' || (o.strict && r.status === 'pending'));
  for (const r of results) console.log(`${r.status === 'pass' ? 'ok     ' : r.status === 'pending' ? 'pending' : 'FAIL   '} ${r.gate.padEnd(18)} ${r.target.padEnd(28)} ${detailText(r.detail)}`);
  if (!o.selfTest) console.log(`browser-gates: ${SNAPSHOT_SCENES.length} scenes gated (registry ${SNAPSHOT_SCENES.length + Object.keys(SCENE_EXCEPTIONS).length}; exceptions: ${Object.entries(SCENE_EXCEPTIONS).map(([k, v]) => `${k} — ${v}`).join('; ') || 'none'})`);
  const count = (s) => results.filter((r) => r.status === s).length;
  console.log(`browser-gates: ${count('pass')} pass, ${count('fail')} fail, ${count('pending')} pending${o.strict ? ' (strict)' : ''}`);
  process.exitCode = failed.length ? 1 : 0;
}

if (process.argv[1] && resolve(process.argv[1]) === fileURLToPath(import.meta.url)) {
  main().catch((e) => {
    console.error(`browser-gates: ${e?.stack ?? e}`);
    process.exit(2);
  });
}
