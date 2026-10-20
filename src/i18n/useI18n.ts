import { useCallback } from 'react';

import { useAppStore } from '../store/useAppStore';
import type { Locale, MessageKey, MessageParams } from './index';
import { translate, translateDynamic } from './index';

export interface I18n {
  /** Interpolates `{name}` placeholders only — numbers go through `format.ts`. */
  t: (key: MessageKey, params?: MessageParams) => string;
  /** Renders a key that arrived from Rust, where the type is a plain string. */
  td: (key: string, params?: Record<string, unknown>) => string;
  locale: Locale;
  setLocale: (locale: Locale) => void;
}

/**
 * The locale lives in the store, so switching it re-renders every subscriber
 * immediately — there is no bundle to load and no provider to remount.
 */
export function useI18n(): I18n {
  const locale = useAppStore((s) => s.ui.locale);
  const setLocale = useAppStore((s) => s.setLocale);

  const t = useCallback(
    (key: MessageKey, params?: MessageParams) => translate(locale, key, params),
    [locale],
  );
  const td = useCallback(
    (key: string, params?: Record<string, unknown>) => translateDynamic(locale, key, params),
    [locale],
  );

  return { t, td, locale, setLocale };
}

/** Just the active locale, for components that only need it for `Intl`. */
export function useLocale(): Locale {
  return useAppStore((s) => s.ui.locale);
}
