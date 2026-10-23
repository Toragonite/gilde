import { useCallback } from 'react';

import type { IpcError } from '../lib/gen/IpcError';
import { useAppStore } from '../store/useAppStore';
import type { Locale, MessageKey, MessageParams } from './index';
import { errorMessage, translate, translateDynamic } from './index';

export interface I18n {
  /** The message for a key (placeholders, plurals and particles: `index.ts`). */
  t: (key: MessageKey, params?: MessageParams) => string;
  /** A key that arrived from Rust as a plain string (`Notice`, `ToastEvent`). */
  td: (key: string, params?: Readonly<Record<string, unknown>> | null) => string;
  /** The sentence for an `IpcError`. */
  te: (error: Pick<IpcError, 'key' | 'params'>) => string;
  locale: Locale;
}

/**
 * The locale lives in the store, so switching it re-renders every subscriber
 * at once: there is no bundle to load and no provider to remount.
 */
export function useI18n(): I18n {
  const locale = useAppStore((s) => s.ui.locale);
  const t = useCallback((key: MessageKey, params?: MessageParams) => translate(locale, key, params), [locale]);
  const td = useCallback(
    (key: string, params?: Readonly<Record<string, unknown>> | null) => translateDynamic(locale, key, params),
    [locale],
  );
  const te = useCallback((error: Pick<IpcError, 'key' | 'params'>) => errorMessage(locale, error), [locale]);
  return { t, td, te, locale };
}

/** Just the active locale, for components that only need it for `Intl`. */
export function useLocale(): Locale {
  return useAppStore((s) => s.ui.locale);
}
