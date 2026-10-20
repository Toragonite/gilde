import type { Locale, LocalePreference } from '../lib/ipc';
import { en } from './en';
import { ko } from './ko';

export type { Locale, LocalePreference };

/** Every message key. `ko` is the source of truth (DESIGN §9-9). */
export type MessageKey = keyof typeof ko;

/** Values allowed as `{placeholder}` substitutions. */
export type MessageParams = Record<string, string | number>;

const TABLES: Record<Locale, Record<MessageKey, string>> = {
  ko,
  en,
};

export const LOCALES: readonly Locale[] = ['ko', 'en'];

const PLACEHOLDER = /\{(\w+)\}/g;

/**
 * Interpolate `{name}` placeholders. Nothing else: pluralization, number,
 * byte, duration and date formatting all live in `lib/format.ts` behind `Intl`,
 * because Korean and English conventions differ enough that hand formatting is
 * visibly wrong in one of them.
 *
 * An unknown key returns the key itself — a missing string shows up as a
 * greppable token instead of an empty box.
 */
export function translate(locale: Locale, key: MessageKey, params?: MessageParams): string {
  const table = TABLES[locale] ?? TABLES.ko;
  const template = table[key] ?? TABLES.ko[key];
  if (template === undefined) return key;
  if (!params) return template;
  return template.replace(PLACEHOLDER, (whole, name: string) => {
    const value = params[name];
    return value === undefined ? whole : String(value);
  });
}

/**
 * Render a key that arrived from Rust (`GildeErrorPayload`, `ToastEvent`), whose
 * type is a plain string. Falls back to `error.unknown` so a backend key the
 * front end has not learned yet never renders as a raw token.
 */
export function translateDynamic(
  locale: Locale,
  key: string,
  params?: Record<string, unknown>,
): string {
  const safe: MessageParams = {};
  if (params) {
    for (const [k, v] of Object.entries(params)) {
      if (v === null || v === undefined) continue;
      safe[k] = typeof v === 'number' ? v : String(v);
    }
  }
  if (isMessageKey(key)) return translate(locale, key, safe);
  return translate(locale, 'error.unknown');
}

export function isMessageKey(key: string): key is MessageKey {
  return Object.prototype.hasOwnProperty.call(ko, key);
}

/** A BCP 47 tag to the UI language: any `ko*` is Korean, everything else English. */
export function localeOfTag(tag: string | null | undefined): Locale {
  return typeof tag === 'string' && tag.trim().toLowerCase().startsWith('ko') ? 'ko' : 'en';
}

/** The system language (`navigator.language`) — what `system` resolves to. */
export function defaultLocale(): Locale {
  return localeOfTag(typeof navigator === 'undefined' ? '' : navigator.language);
}

/**
 * The language to show for a saved preference (F1). `system`, a missing value
 * or anything unknown follows the OS; an explicit `ko` / `en` wins.
 */
export function resolveLocale(pref: string | null | undefined, systemTag?: string): Locale {
  if (pref === 'ko' || pref === 'en') return pref;
  return systemTag === undefined ? defaultLocale() : localeOfTag(systemTag);
}

export { en, ko };
