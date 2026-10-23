/**
 * Settings (contract §9.6 "executable settings are never editable inline",
 * §4.2.8, A16.2 "until you resume", F-31 details off by default).
 */

import { act, cleanup, fireEvent, render } from '@testing-library/react';

import { resetHotkeysForTests } from '../hooks/useHotkeys';
import { translate } from '../i18n';
import type { Settings } from '../lib/gen/Settings';
import { PAUSED_UNTIL_RESUME } from '../shell/words';
import { appStore } from '../store/useAppStore';
import { initialUiState } from '../store/uiSlice';
import { emptyWorld } from '../store/worldSlice';
import { flush, withFakeCore } from '../test/backend';
import type { FakeCore, Handler } from '../test/backend';
import { snapshot } from '../test/fixtures';
import type { BootStatus } from '../lib/gen/BootStatus';
import { manualClock, setClockForTests } from '../lib/clock';
import { assert, assertDeepEqual, assertEqual, domTest, test } from '../test/harness';
import { envValue, pathValue } from './ExecutableDialog';
import { pauseState, RESCAN_CAP_MS, SettingsScreen } from './SettingsScreen';

const en = (key: Parameters<typeof translate>[1], params?: Parameters<typeof translate>[2]) => translate('en', key, params);
let rev = 1700;

function settings(extra: Partial<Settings> = {}): Settings {
  return {
    locale: 'en',
    theme: 'system',
    attention: { noProgressAfterSecs: 300, watchedFinishedTurns: true },
    notify: { watchedSessions: true, sound: false, showDetails: false },
    ide: { editor: 'vscode' },
    executable: { claudeBinary: '/opt/claude/bin/claude' as never, codexBinary: null, cursorBinary: null, agyBinary: null, sidecarNodePath: null, pathsOverride: null, envPassthrough: ['HTTPS_PROXY'] },
    ...extra,
  };
}

async function settle(ms = 20) {
  await act(async () => {
    await flush(10);
    await new Promise((r) => setTimeout(r, ms));
  });
}

const AGENTS: BootStatus['agents'] = [
  { vendor: 'claude', installed: true, version: '2.1.307' },
  { vendor: 'codex', installed: true, version: '0.47.0' },
  { vendor: 'cursor', installed: false, version: null },
  { vendor: 'agy', installed: false, version: null },
];

function boot(extra: Partial<BootStatus> = {}): BootStatus {
  return { spawnGateOpen: true, firstDiscoveryDone: true, agentsDetected: true, memoryOnly: null, agents: AGENTS, agentsScannedAt: 1_000, recovery: null, ...extra };
}

function harness(run: (core: FakeCore) => Promise<void>, extra: Record<string, Handler> = {}, pausedUntil: number | null = null, bootStatus: BootStatus = boot()) {
  return async () => {
    resetHotkeysForTests();
    appStore.setState({ world: emptyWorld(), ui: { ...initialUiState(), locale: 'en' } });
    // No project at all: Settings › Agents needs none (A29.1).
    act(() =>
      appStore.getState().applyStoreSnapshot(snapshot(++rev, { boot: bootStatus, queue: { rev: 1, items: [], needYou: 0, also: 0, pausedUntil } })),
    );
    let current = settings();
    try {
      await withFakeCore(
        {
          settings_get: () => current,
          settings_set: (req) => {
            const p = (req as { patch: Record<string, unknown> }).patch;
            current = { ...current, ...(p.locale ? { locale: p.locale as Settings['locale'] } : {}) };
            return current;
          },
          settings_set_executable: () => ({ applied: true }),
          ide_editors: () => ({ found: [{ editor: 'vscode', bundleId: 'b', appPath: '/Applications/x.app' }], chosen: 'vscode' }),
          agents_rescan: () => ({ scheduled: true }),
          notify_pause: () => ({ pausedUntil: PAUSED_UNTIL_RESUME }),
          notify_resume: () => ({}),
          ...extra,
        },
        run,
      );
    } finally {
      cleanup();
      resetHotkeysForTests();
      setClockForTests(null);
      appStore.setState({ world: emptyWorld(), ui: initialUiState() });
    }
  };
}

test('T18 executable values: an absolute path or valid names only', () => {
  assertDeepEqual(pathValue(' /opt/bin/claude/ '), { kind: 'path', path: '/opt/bin/claude' }, 'trimmed');
  assertEqual(pathValue('claude'), null, 'relative');
  assertEqual(pathValue('/'), null, 'root');
  assertEqual(pathValue('/a\u0000b'), null, 'NUL');
  assertDeepEqual(envValue('HTTPS_PROXY, NO_PROXY HTTPS_PROXY'), { kind: 'envNames', names: ['HTTPS_PROXY', 'NO_PROXY'] }, 'split and deduplicated');
  assertEqual(envValue('lower_case'), null, 'pattern');
  assertEqual(envValue(Array.from({ length: 33 }, (_, i) => `V${i}`).join(' ')), null, '≤ 32 names');
  assertEqual(pauseState(null), 'on', 'not paused');
  assertEqual(pauseState(PAUSED_UNTIL_RESUME), 'untilResume', 'the open-ended value');
  assertEqual(pauseState(PAUSED_UNTIL_RESUME + 1), 'untilResume', 'any value at or above it');
  assertEqual(pauseState(1_800_000_000_000), 'until', 'a time');
});

domTest(
  'T18 §9.6: executable settings are shown read-only and change only through settings_set_executable',
  harness(async (core) => {
    const view = render(<SettingsScreen />);
    await settle();
    const inputs = [...view.container.querySelectorAll('input, textarea')] as HTMLInputElement[];
    assertEqual(inputs.length, 0, 'no text field on the Settings screen itself');
    assert(view.getByText('/opt/claude/bin/claude') !== null, 'the claude path, read-only');
    assert(view.getByText('HTTPS_PROXY') !== null, 'the passed-through names');
    fireEvent.click(view.getAllByText(en('settings.exec.change'))[0] as HTMLElement);
    await settle();
    const field = document.querySelector('[role="dialog"] input') as HTMLInputElement;
    assertEqual(field.value, '/opt/claude/bin/claude', 'the dialog starts from the current value');
    fireEvent.change(field, { target: { value: 'relative/claude' } });
    fireEvent.click(document.querySelector('[role="dialog"] .btn.primary') as HTMLElement);
    await settle();
    assertEqual(core.calls('settings_set_executable').length, 0, 'a relative path never leaves the WebView');
    assert((document.body.textContent ?? '').includes(en('settings.exec.pathInvalid')), 'says why');
    fireEvent.change(field, { target: { value: '/usr/local/bin/claude' } });
    fireEvent.click(document.querySelector('[role="dialog"] .btn.primary') as HTMLElement);
    await settle();
    assertDeepEqual(core.calls('settings_set_executable')[0]?.req, { key: 'agents.claude.binary', value: { kind: 'path', path: '/usr/local/bin/claude' } }, 'through the validating command');
    for (const call of core.calls('settings_set')) {
      const keys = Object.keys((call.req as { patch: Record<string, unknown> }).patch);
      assert(!keys.some((k) => /binary|nodePath|override|envPassthrough|executable/i.test(k)), 'settings_set never carries an executable key');
    }
    assertEqual(view.getAllByText(en('settings.agents.notInstalled')).length, 2, 'cursor and agy: not installed');
    assert(view.getByText(en('settings.agents.notInstalledHint', { vendor: 'agy' })) !== null, 'with its hint');
    assertEqual(core.calls('newsession_options').length, 0, 'no project workaround (A29.1)');
  }),
);

domTest(
  'T18 A16.2: a pause that runs until the user resumes reads "until you resume"; Resume ends it',
  harness(
    async (core) => {
      const view = render(<SettingsScreen />);
      await settle();
      assert(view.getByText(en('settings.pause.state.untilResume')) !== null, 'until you resume');
      fireEvent.click(view.getByText(en('settings.pause.resume')));
      await settle();
      assertEqual(core.calls('notify_resume').length, 1, 'resume');
      assert(view.getByText(en('settings.pause.state.on')) !== null, 'on again');
    },
    {},
    PAUSED_UNTIL_RESUME,
  ),
);

domTest(
  'T18 ordinary settings: a typed patch with nulls for the rest; details default off; language applies at once',
  harness(async (core) => {
    const view = render(<SettingsScreen />);
    await settle();
    const details = view.getByRole('group', { name: en('notify.showDetails') });
    const off = [...details.querySelectorAll('button')].find((b) => b.textContent === en('settings.off')) as HTMLButtonElement;
    assertEqual(off.getAttribute('aria-pressed'), 'true', 'details off by default (F-31)');
    fireEvent.click([...details.querySelectorAll('button')].find((b) => b.textContent === en('settings.on')) as HTMLElement);
    await settle();
    assertDeepEqual(
      core.calls('settings_set')[0]?.req,
      { patch: { locale: null, theme: null, attention: null, notify: { watchedSessions: null, sound: null, showDetails: true }, ide: null } },
      'only that key',
    );
    const language = view.getByRole('group', { name: en('settings.language') });
    fireEvent.click([...language.querySelectorAll('button')].find((b) => b.textContent === '한국어') as HTMLElement);
    await settle();
    assertEqual(appStore.getState().ui.locale, 'ko', 'the UI switches language');
  }),
);

domTest(
  'T18b A29.1: agents come from boot.agents with no project; "Finding installed agents…" until detected',
  harness(
    async (core) => {
      const view = render(<SettingsScreen />);
      await settle();
      assert(view.getByText(en('newsession.findingAgents')) !== null, 'finding');
      assertEqual(view.getAllByText(en('settings.agents.unknown')).length, 4, 'no rows yet');
      act(() => appStore.getState().applyStoreSnapshot(snapshot(++rev, { boot: boot() })));
      await settle();
      assertEqual(view.queryByText(en('newsession.findingAgents')), null, 'detected');
      assert(view.getByText(en('settings.agents.installed', { version: '2.1.307' })) !== null, 'claude installed with its version');
      assertEqual(core.calls('newsession_options').length, 0, 'no newsession_options');
    },
    {},
    null,
    boot({ agentsDetected: false, agents: [], agentsScannedAt: null }),
  ),
);

domTest(
  'T18b A29.1: "Check again" shows progress until agentsScannedAt changes',
  harness(async (core) => {
    const view = render(<SettingsScreen />);
    await settle();
    fireEvent.click(view.getByText(en('settings.agents.rescan')));
    await settle();
    assertEqual(core.calls('agents_rescan').length, 1, 'rescan');
    assert(view.getByText(en('settings.agents.rescanning')) !== null, 'progress');
    assert((view.getByText(en('settings.agents.rescan')) as HTMLButtonElement).disabled, 'one rescan at a time');
    act(() => appStore.getState().applyStoreSnapshot(snapshot(++rev, { boot: boot({ agentsScannedAt: 2_000 }) })));
    await settle();
    assertEqual(view.queryByText(en('settings.agents.rescanning')), null, 'done when the scan time moves (same rows)');
  }),
);

domTest(
  'T18b A29.1: the progress stops after 15 s with no new scan; the old rows stay',
  harness(async () => {
    const clock = manualClock(50_000);
    setClockForTests(clock);
    const view = render(<SettingsScreen />);
    await settle();
    fireEvent.click(view.getByText(en('settings.agents.rescan')));
    await settle();
    assert(view.getByText(en('settings.agents.rescanning')) !== null, 'progress');
    clock.advance(RESCAN_CAP_MS - 1);
    await settle(300);
    assert(view.queryByText(en('settings.agents.rescanning')) !== null, 'still within 15 s');
    clock.advance(1);
    await settle(300);
    assertEqual(view.queryByText(en('settings.agents.rescanning')), null, 'stopped at 15 s');
    assert(view.getByText(en('settings.agents.installed', { version: '2.1.307' })) !== null, 'old rows stay');
  }),
);
