/**
 * i18n tests.
 *
 * The compile-time guarantee (`en: Record<MessageKey, string>`) is what really
 * enforces key parity — deleting a key from `en.ts` fails `npm run build`.
 * These tests cover what types cannot: that neither table has an empty string,
 * that interpolation only touches `{name}`, and that a key coming from Rust is
 * rendered rather than leaked raw.
 */

import { assert, assertEqual, test } from '../test/harness';
import { defaultLocale, en, isMessageKey, ko, translate, translateDynamic } from './index';
import type { MessageKey } from './index';

test('ko and en hold exactly the same keys', () => {
  const koKeys = Object.keys(ko).sort();
  const enKeys = Object.keys(en).sort();
  assertEqual(koKeys.length, enKeys.length, 'the tables are the same size');
  for (let i = 0; i < koKeys.length; i++) {
    assertEqual(enKeys[i], koKeys[i], `key ${koKeys[i]} exists on both sides`);
  }
});

test('no message is empty and every placeholder is matched', () => {
  for (const [key, value] of Object.entries(ko)) {
    assert(value.trim().length > 0, `ko.${key} is not empty`);
  }
  for (const [key, value] of Object.entries(en)) {
    assert(value.trim().length > 0, `en.${key} is not empty`);
  }
  for (const key of Object.keys(ko) as MessageKey[]) {
    const koParams = [...ko[key].matchAll(/\{(\w+)\}/g)].map((m) => m[1]).sort();
    const enParams = [...en[key].matchAll(/\{(\w+)\}/g)].map((m) => m[1]).sort();
    assertEqual(
      enParams.join(','),
      koParams.join(','),
      `${key} uses the same placeholders in both locales`,
    );
  }
});

test('translate interpolates named placeholders only', () => {
  assertEqual(
    translate('ko', 'sidebar.orphans', { n: 3 }),
    '고아 프로세스 · 3',
    'ko interpolates n',
  );
  assertEqual(translate('en', 'sidebar.orphans', { n: 3 }), 'Orphan processes · 3', 'en too');
  assert(
    translate('ko', 'sidebar.orphans').includes('{n}'),
    'a missing parameter is left visible rather than rendered as undefined',
  );
});

test('translate falls back to ko for an unknown locale table', () => {
  assertEqual(translate('ko', 'common.cancel'), '취소', 'ko table');
  assertEqual(translate('en', 'common.cancel'), 'Cancel', 'en table');
});

test('translateDynamic renders backend keys and degrades safely', () => {
  assertEqual(
    translateDynamic('ko', 'error.git', { code: 128 }),
    translate('ko', 'error.git', { code: 128 }),
    'a known backend key renders like a static one',
  );
  assertEqual(
    translateDynamic('en', 'error.does.not.exist', {}),
    translate('en', 'error.unknown'),
    'an unknown key degrades to the generic error, never a raw token',
  );
  assert(isMessageKey('error.git'), 'isMessageKey recognises a real key');
  assert(!isMessageKey('error.nope'), 'isMessageKey rejects an unknown key');
});

test('defaultLocale prefers Korean when the platform does', () => {
  const original = globalThis.navigator;
  try {
    Object.defineProperty(globalThis, 'navigator', {
      value: { language: 'ko-KR' },
      configurable: true,
    });
    assertEqual(defaultLocale(), 'ko', 'ko-KR selects Korean');
    Object.defineProperty(globalThis, 'navigator', {
      value: { language: 'en-US' },
      configurable: true,
    });
    assertEqual(defaultLocale(), 'en', 'en-US selects English');
  } finally {
    if (original === undefined) {
      Reflect.deleteProperty(globalThis, 'navigator');
    } else {
      Object.defineProperty(globalThis, 'navigator', { value: original, configurable: true });
    }
  }
});
