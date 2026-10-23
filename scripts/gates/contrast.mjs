#!/usr/bin/env node
/**
 * contrast.mjs — the contrast pairs of ux-ia §10, computed from the design
 * tokens (contract §14.3 "contrast pairs from ux-ia §10 computed from
 * tokens"), for the light and the dark theme.
 *
 *   node scripts/gates/contrast.mjs [--tokens src/styles/tokens.css] [--json]
 *
 * Pairs (WCAG 2 contrast ratio): text 4.5:1, focus ring and state glyph
 * colours 3:1. `--n7` (`.dim`) is never used for text that carries
 * information (ux-ia §10), so it has no text pair; `--n8` on `--n4` (the old
 * selected-row background) is the known failing pair and selected rows use
 * `--n3` instead.
 *
 * Exit codes: 0 every pair holds; 1 a pair fails; 2 the tokens could not be read.
 */

import { existsSync, readFileSync } from 'node:fs';
import { dirname, join, resolve } from 'node:path';
import { fileURLToPath } from 'node:url';

const ROOT = resolve(dirname(fileURLToPath(import.meta.url)), '..', '..');

/** [foreground, background, minimum, what] */
export const PAIRS = [
  ['--n11', '--n0', 4.5, 'primary text on the window'],
  ['--n11', '--n1', 4.5, 'primary text on panels'],
  ['--n11', '--n3', 4.5, 'primary text on a selected row'],
  ['--n9', '--n1', 4.5, 'body text on panels'],
  ['--n9', '--n2', 4.5, 'body text on raised surfaces'],
  ['--n9', '--n3', 4.5, 'body text on a selected row'],
  ['--n8', '--n1', 4.5, 'secondary text on panels'],
  ['--n8', '--n2', 4.5, 'secondary text on raised surfaces'],
  ['--accent', '--n1', 4.5, 'links on panels'],
  ['--error', '--n1', 4.5, 'error text'],
  ['--warn', '--n1', 4.5, 'warning text'],
  ['--ok', '--n1', 4.5, 'success text'],
  ['--accent', '--n1', 3, 'focus ring (non-text)'],
  ['--error', '--n2', 3, 'state glyph on raised surfaces (non-text)'],
];

/** `--name: #hex` declarations of the first block matching `selector`. */
export function tokensOf(css, selectorRe) {
  const m = selectorRe.exec(css);
  if (!m) return null;
  const start = css.indexOf('{', m.index) + 1;
  let depth = 1;
  let i = start;
  for (; i < css.length && depth > 0; i++) {
    if (css[i] === '{') depth++;
    else if (css[i] === '}') depth--;
  }
  const body = css.slice(start, i - 1);
  const out = {};
  for (const d of body.matchAll(/(--[\w-]+)\s*:\s*(#[0-9a-fA-F]{3,8})\b/g)) out[d[1]] = d[2];
  return out;
}

function rgb(hex) {
  let h = hex.slice(1);
  if (h.length === 3) h = [...h].map((c) => c + c).join('');
  return [0, 2, 4].map((i) => parseInt(h.slice(i, i + 2), 16) / 255);
}

function luminance(hex) {
  const [r, g, b] = rgb(hex).map((c) => (c <= 0.03928 ? c / 12.92 : ((c + 0.055) / 1.055) ** 2.4));
  return 0.2126 * r + 0.7152 * g + 0.0722 * b;
}

/** WCAG 2 contrast ratio of two hex colours, rounded to 2 decimals. */
export function contrast(a, b) {
  const [l1, l2] = [luminance(a), luminance(b)].sort((x, y) => y - x);
  return Math.round(((l1 + 0.05) / (l2 + 0.05)) * 100) / 100;
}

export function check(css) {
  const light = tokensOf(css, /:root\s*\{/);
  const dark = tokensOf(css, /:root\[data-theme="dark"\]\s*\{/) ?? tokensOf(css, /:root:not\(\[data-theme="light"\]\)\s*\{/);
  if (!light || !dark) return null;
  const results = [];
  for (const [theme, t] of [['light', light], ['dark', { ...light, ...dark }]]) {
    for (const [fg, bg, min, what] of PAIRS) {
      if (!t[fg] || !t[bg]) {
        results.push({ theme, fg, bg, min, what, ratio: null, ok: false });
        continue;
      }
      const ratio = contrast(t[fg], t[bg]);
      results.push({ theme, fg, bg, min, what, ratio, ok: ratio >= min });
    }
  }
  return results;
}

function main() {
  const argv = process.argv.slice(2);
  let tokens = join(ROOT, 'src', 'styles', 'tokens.css');
  const json = argv.includes('--json');
  const i = argv.indexOf('--tokens');
  if (i >= 0) tokens = resolve(argv[i + 1] ?? '');
  if (!existsSync(tokens)) {
    console.error(`contrast: tokens not found: ${tokens}`);
    process.exit(2);
  }
  const results = check(readFileSync(tokens, 'utf8'));
  if (!results) {
    console.error('contrast: the light (:root) or dark token block is missing');
    process.exit(2);
  }
  if (json) console.log(JSON.stringify(results, null, 2));
  else for (const r of results) console.log(`${r.ok ? 'ok  ' : 'FAIL'} ${r.theme.padEnd(5)} ${r.fg} on ${r.bg}: ${r.ratio ?? 'missing'} (≥ ${r.min}) ${r.what}`);
  const failed = results.filter((r) => !r.ok).length;
  if (!json) console.log(failed ? `contrast: ${failed} pair(s) fail` : 'contrast: every pair holds');
  process.exitCode = failed ? 1 : 0;
}

if (process.argv[1] && resolve(process.argv[1]) === fileURLToPath(import.meta.url)) main();
