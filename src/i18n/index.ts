/**
 * Messages (ux-ia §9, contract §9.12).
 *
 * The tables are split by screen area (`areas/*.ts`, contract §15.1 H3); `ko.ts`
 * and `en.ts` merge them. Every area exports `ko` and `en` with the same keys
 * (`en` is typed `Record<keyof typeof ko, string>`), so a key on one side only
 * is a compile error, and `i18n.test.ts` checks the rest: no key in two areas,
 * the same placeholders on both sides, the Korean register, English plurals.
 *
 * Message syntax, all of it:
 *  - `{name}`: a parameter. Numbers are formatted for the locale; strings and
 *    booleans are inserted as they are. A missing parameter stays visible as
 *    `{name}` rather than becoming "undefined".
 *  - `{name}(이)가`, `{owner}(으)로`, … (Korean): the particle after a parameter
 *    is chosen for the substituted word (`josa.ts`).
 *  - `{n, plural, one {# session} other {# sessions}}` (English): the form for
 *    the count; `#` is the formatted count (`plural.ts`).
 * Times, durations, bytes and dates are formatted by the caller through
 * `lib/format.ts` and passed in as strings.
 */

import type { IpcError } from '../lib/gen/IpcError';
import type { Locale } from '../lib/gen/Locale';
import { en } from './en';
import { josaAt, withJosa } from './josa';
import { choosePlural, parsePluralOptions } from './plural';
import { ko } from './ko';

export type { Locale };

/** Every message key. */
export type MessageKey = keyof typeof ko;

/** Values allowed as parameters (the same set `IpcError.params` and `Notice.params` carry). */
export type MessageParams = Readonly<Record<string, string | number | boolean>>;

const TABLES: Readonly<Record<Locale, Readonly<Record<MessageKey, string>>>> = { ko, en };

export const LOCALES: readonly Locale[] = ['ko', 'en'];

const numberFormats = new Map<Locale, Intl.NumberFormat>();

function formatNumber(locale: Locale, value: number): string {
  let f = numberFormats.get(locale);
  if (!f) {
    f = new Intl.NumberFormat(locale, { maximumFractionDigits: 2 });
    numberFormats.set(locale, f);
  }
  return f.format(value);
}

/** Index of the `}` that closes the `{` at `open`, or -1. */
function closingBrace(template: string, open: number): number {
  let depth = 0;
  for (let i = open; i < template.length; i++) {
    const c = template[i];
    if (c === '{') depth++;
    else if (c === '}') {
      depth--;
      if (depth === 0) return i;
    }
  }
  return -1;
}

const SIMPLE = /^\s*(\w+)\s*$/;
const PLURAL = /^\s*(\w+)\s*,\s*plural\s*,([\s\S]*)$/;

/**
 * Render one template. Parameter values are inserted once and never scanned
 * again, so a value that contains `{x}` or `(이)가` is shown as it is.
 */
export function formatMessage(
  locale: Locale,
  template: string,
  params: MessageParams = {},
  hash?: string,
): string {
  let out = '';
  let i = 0;
  while (i < template.length) {
    const c = template[i];
    if (c === '#' && hash !== undefined) {
      out += hash;
      i++;
      continue;
    }
    if (c !== '{') {
      out += c;
      i++;
      continue;
    }
    const close = closingBrace(template, i);
    if (close < 0) {
      out += template.slice(i);
      break;
    }
    const inner = template.slice(i + 1, close);
    const whole = template.slice(i, close + 1);
    i = close + 1;

    let value: string | null = null;
    const simple = SIMPLE.exec(inner);
    const plural = simple ? null : PLURAL.exec(inner);
    if (simple) {
      const name = simple[1] as string;
      if (Object.prototype.hasOwnProperty.call(params, name)) {
        const v = params[name];
        value = typeof v === 'number' ? formatNumber(locale, v) : String(v);
      }
    } else if (plural) {
      const name = plural[1] as string;
      const v = Object.prototype.hasOwnProperty.call(params, name) ? params[name] : undefined;
      const block = parsePluralOptions(name, plural[2] ?? '');
      if (typeof v === 'number' && block) {
        value = formatMessage(locale, choosePlural(locale, block, v), params, formatNumber(locale, v));
      }
    }

    if (value === null) {
      out += whole;
      continue;
    }
    const spelling = josaAt(template, i);
    if (spelling) {
      out += withJosa(value, spelling);
      i += spelling.length;
    } else {
      out += value;
    }
  }
  return out;
}

/** The message for `key` in `locale`, with parameters. An unknown key returns the key. */
export function translate(locale: Locale, key: MessageKey, params?: MessageParams): string {
  const table = TABLES[locale] ?? TABLES.ko;
  const template = table[key] ?? TABLES.ko[key];
  if (template === undefined) return key;
  return formatMessage(locale, template, params);
}

export function isMessageKey(key: string): key is MessageKey {
  return Object.prototype.hasOwnProperty.call(ko, key);
}

/** Keep only the parameter types messages accept (Rust sends no others, but never trust a payload). */
export function safeParams(params: Readonly<Record<string, unknown>> | null | undefined): MessageParams {
  const out: Record<string, string | number | boolean> = {};
  if (!params) return out;
  for (const [k, v] of Object.entries(params)) {
    if (typeof v === 'string' || typeof v === 'number' || typeof v === 'boolean') out[k] = v;
  }
  return out;
}

/**
 * Render a key that arrived from Rust (`IpcError.key`, `Notice.key`,
 * `ToastEvent.key`), whose type is a plain string. A key the WebView does not
 * know renders as the generic error sentence, never as a raw token.
 */
export function translateDynamic(
  locale: Locale,
  key: string,
  params?: Readonly<Record<string, unknown>> | null,
): string {
  const safe = safeParams(params);
  if (isMessageKey(key)) return translate(locale, key, safe);
  return translate(locale, 'error.internal', safe);
}

/** The sentence for an `IpcError` (its `detail` is shown separately, in mono, never interpolated). */
export function errorMessage(locale: Locale, error: Pick<IpcError, 'key' | 'params'>): string {
  return translateDynamic(locale, error.key, error.params);
}

/** A BCP 47 tag to the UI language: any `ko*` is Korean, everything else English. */
export function localeOfTag(tag: string | null | undefined): Locale {
  return typeof tag === 'string' && tag.trim().toLowerCase().startsWith('ko') ? 'ko' : 'en';
}

/** The system language (`navigator.language`). */
export function defaultLocale(): Locale {
  return localeOfTag(typeof navigator === 'undefined' ? '' : navigator.language);
}

/** The language to show for a saved preference: `ko`/`en` win, anything else follows the system. */
export function resolveLocale(pref: string | null | undefined, systemTag?: string): Locale {
  if (pref === 'ko' || pref === 'en') return pref;
  return systemTag === undefined ? defaultLocale() : localeOfTag(systemTag);
}

export { en, ko };
