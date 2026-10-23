/**
 * G6 probe HP-7, the UI half (contract §5.8, A33.4, A38.8): World reports a
 * macOS denial as one `notice.cantReadFolders` plus one
 * `health.folderDenied{category}` per denied folder category; the Health
 * panel shows one line per category with its KO/EN label, and nothing once
 * access is back. Prints one line `PROBE HP-7: (UI) …`. Owner: T35.
 */

import { act, cleanup, render } from '@testing-library/react';

import { deniedFolderCategories, HealthPanel } from '../accounts/HealthPanel';
import { translate } from '../i18n';
import type { Notice } from '../lib/gen/Notice';
import { appStore } from '../store/useAppStore';
import { initialUiState } from '../store/uiSlice';
import { emptyWorld } from '../store/worldSlice';
import { assertDeepEqual, assertEqual, domTest, test } from '../test/harness';

function notice(key: string, category?: string): Notice {
  return { key, params: category ? { category } : {} };
}

const DENIED: Notice[] = [
  notice('notice.cantReadFolders'),
  notice('health.folderDenied', 'config'),
  notice('health.folderDenied', 'transcripts'),
];

test('HP-7 (A38.8): the denied categories are read in World order, each once, unknown ones skipped', () => {
  assertDeepEqual(
    deniedFolderCategories([...DENIED, notice('health.folderDenied', 'config'), notice('health.folderDenied', 'bogus')]),
    ['config', 'transcripts'],
    'config then transcripts',
  );
  assertDeepEqual(deniedFolderCategories([notice('notice.memoryOnly')]), [], 'no denial, no line');
});

domTest('HP-7 (A38.8): the Health panel shows one line per denied category and clears with access', async () => {
  appStore.setState({ world: { ...emptyWorld(), notices: DENIED }, ui: { ...initialUiState(), locale: 'ko' }, toasts: [] });
  try {
    const view = render(<HealthPanel headingId="hp7-health" />);
    const lines = () => Array.from(view.container.querySelectorAll('p[role="note"]')).map((p) => p.textContent ?? '');
    const want = ['config', 'transcripts'].map((c) =>
      translate('ko', 'health.folderDenied', { category: translate('ko', `health.folderCategory.${c}` as never) }),
    );
    assertDeepEqual(lines(), want, 'one Korean line per category');
    act(() => appStore.setState({ world: { ...emptyWorld(), notices: [] } }));
    assertEqual(lines().length, 0, 'access back: no line');
    console.log(`PROBE HP-7: (UI) denied config + transcripts → ${want.length} health lines ("${want[0]}"); access back → 0 lines`);
  } finally {
    cleanup();
    appStore.setState({ world: emptyWorld(), ui: initialUiState(), toasts: [] });
  }
});
