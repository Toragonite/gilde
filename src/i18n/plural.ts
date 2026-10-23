/**
 * Plurals (ux-ia §9.1, contract §13.3).
 *
 * English strings with a count use a plural block:
 *
 *   '{n, plural, one {# session} other {# sessions}} stopped when Gilde quit.'
 *
 * `#` is the count formatted for the locale; `=0 {…}` matches an exact value
 * before the category. Categories come from `Intl.PluralRules`, so the same
 * block works for any locale. Korean needs no plural forms (`세션 {n}개`) and
 * writes the count as a plain placeholder.
 */

import type { Locale } from '../lib/gen/Locale';

const rulesCache = new Map<Locale, Intl.PluralRules>();

function rules(locale: Locale): Intl.PluralRules {
  let r = rulesCache.get(locale);
  if (!r) {
    r = new Intl.PluralRules(locale);
    rulesCache.set(locale, r);
  }
  return r;
}

/** The plural category of `n` in `locale` (`one`, `other`, …). */
export function pluralCategory(locale: Locale, n: number): Intl.LDMLPluralRule {
  return rules(locale).select(n);
}

/** One parsed plural block: exact matches (`=0`) and categories, each a sub-template. */
export interface PluralBlock {
  param: string;
  exact: Map<number, string>;
  categories: Map<string, string>;
}

/**
 * Parse the inside of `{n, plural, …}` (everything after `plural,`). Returns
 * null on a malformed block, which the interpolator then leaves visible.
 */
export function parsePluralOptions(param: string, body: string): PluralBlock | null {
  const block: PluralBlock = { param, exact: new Map(), categories: new Map() };
  let i = 0;
  const n = body.length;
  while (i < n) {
    while (i < n && /\s/.test(body[i] ?? '')) i++;
    if (i >= n) break;
    const selStart = i;
    while (i < n && !/[\s{]/.test(body[i] ?? '')) i++;
    const selector = body.slice(selStart, i);
    while (i < n && /\s/.test(body[i] ?? '')) i++;
    if (body[i] !== '{' || selector === '') return null;
    let depth = 0;
    const textStart = i + 1;
    for (; i < n; i++) {
      if (body[i] === '{') depth++;
      else if (body[i] === '}') {
        depth--;
        if (depth === 0) break;
      }
    }
    if (depth !== 0) return null;
    const text = body.slice(textStart, i);
    i++;
    if (selector.startsWith('=')) {
      const value = Number(selector.slice(1));
      if (!Number.isFinite(value)) return null;
      block.exact.set(value, text);
    } else {
      block.categories.set(selector, text);
    }
  }
  if (!block.categories.has('other')) return null;
  return block;
}

/** The sub-template a plural block picks for `n`. */
export function choosePlural(locale: Locale, block: PluralBlock, n: number): string {
  const exact = block.exact.get(n);
  if (exact !== undefined) return exact;
  return block.categories.get(pluralCategory(locale, n)) ?? block.categories.get('other') ?? '';
}
