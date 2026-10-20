/**
 * System notifications for health alerts (`lib/notify.ts`, called from
 * `healthSlice.applyHealth` at the moment a health toast is created).
 *
 * The OS side is a mock backend: no Tauri, no plugin, no real notification.
 * Pinned here:
 *   1. posted when the window is unfocused + the alert is `action` + the
 *      setting is on — the toast's own title and body, EN and KO;
 *   2. not when the window is focused, not with the setting off, never for a
 *      dismissed (snoozed) or `attention` alert, never in showcase mode;
 *   3. once per (id, raisedAt): not twice for the same raise, again after an
 *      escalation (new raisedAt);
 *   4. permission asked lazily, at most once per run; denied → nothing posted
 *      and the settings sheet says where to turn it on;
 *   5. a throwing plugin never breaks the toast and leaves no unhandled rejection;
 *   6. the setting: default on, an old config without it reads as on, an
 *      explicit off is kept.
 */

import { renderToStaticMarkup } from 'react-dom/server';

import { SettingsForm } from '../dialogs/SettingsSheet';
import { setShowcaseForTests } from '../features';
import { translate } from '../i18n';
import { alertText } from '../shell/healthText';
import { initialHealthState } from '../store/healthSlice';
import { initialUiState } from '../store/uiSlice';
import { useAppStore } from '../store/useAppStore';
import { assert, assertDeepEqual, assertEqual, test } from '../test/harness';
import type { AppConfig, HealthAlert, HealthSnapshot, Locale } from './ipc';
import {
  NOTIFY_MAX_CHARS,
  clipLine,
  notifyHealthAlert,
  notifyPermission,
  resetNotifyState,
  setNotifyBackend,
} from './notify';
import type { NotifyBackend } from './notify';

const GIB = 1024 * 1024 * 1024;

// ---------------------------------------------------------------------------
// Fixtures
// ---------------------------------------------------------------------------

function alertOf(patch: Partial<HealthAlert> = {}): HealthAlert {
  return {
    id: 'orphans:tmabcdefghi1',
    kind: 'orphans',
    severity: 'action',
    titleKey: 'health.alert.orphans.title',
    bodyKey: 'health.alert.orphans.body',
    params: { count: 2, team: 'demo', members: ['alice', 'bob'], rssBytes: 2.4 * GIB },
    actions: [{ kind: 'cleanUp', pids: [501, 502] }, { kind: 'details' }],
    since: 1000,
    raisedAt: 1000,
    teamId: 'tmabcdefghi1',
    dismissed: false,
    dismissedUntil: null,
    ...patch,
  };
}

function snapshotOf(at: number, alerts: HealthAlert[]): HealthSnapshot {
  return {
    at,
    overall: alerts.some((a) => a.severity === 'action' && !a.dismissed) ? 'action' : 'ok',
    agents: [],
    hosts: [],
    orphans: [],
    teams: [],
    memory: { pressure: 'normal', agentsRssBytes: 0, physicalRamBytes: 32 * GIB },
    alerts,
    teamsKnown: true,
  } as HealthSnapshot;
}

interface MockBackend extends NotifyBackend {
  posted: { title: string; body: string }[];
  focusChecks: number;
  grantedChecks: number;
  requests: number;
}

interface MockOptions {
  focused?: boolean | 'throw';
  granted?: boolean;
  answer?: string;
  notifyThrows?: boolean;
  grantedRejects?: boolean;
}

function mockBackend(options: MockOptions = {}): MockBackend {
  const state = { granted: options.granted ?? true };
  const backend: MockBackend = {
    posted: [],
    focusChecks: 0,
    grantedChecks: 0,
    requests: 0,
    isFocused: async () => {
      backend.focusChecks += 1;
      if (options.focused === 'throw') throw new Error('window api down');
      return options.focused ?? false;
    },
    isPermissionGranted: async () => {
      backend.grantedChecks += 1;
      if (options.grantedRejects) throw new Error('plugin: is_permission_granted failed');
      return state.granted;
    },
    requestPermission: async () => {
      backend.requests += 1;
      const answer = options.answer ?? 'granted';
      if (answer === 'granted') state.granted = true;
      return answer;
    },
    notify: (message) => {
      if (options.notifyThrows) throw new Error('plugin: notify failed');
      backend.posted.push(message);
    },
  };
  return backend;
}

function seed(locale: Locale = 'ko', ui: Partial<ReturnType<typeof initialUiState>> = {}): void {
  useAppStore.setState({
    health: initialHealthState(),
    toasts: [],
    ui: { ...initialUiState(), locale, ...ui },
  });
}

/** Let the fire-and-forget notification run to the end. */
async function settle(): Promise<void> {
  for (let i = 0; i < 5; i += 1) await new Promise((resolve) => setTimeout(resolve, 0));
}

/** Run `body` with console.warn captured (the module warns on every swallowed failure). */
async function withWarnings(body: (warnings: unknown[][]) => Promise<void>): Promise<void> {
  const original = console.warn;
  const warnings: unknown[][] = [];
  console.warn = (...args: unknown[]) => {
    warnings.push(args);
  };
  try {
    await body(warnings);
  } finally {
    console.warn = original;
  }
}

/** One test with a mock backend installed and everything restored afterwards. */
async function withBackend(backend: MockBackend, body: () => Promise<void>): Promise<void> {
  setNotifyBackend(backend);
  try {
    await body();
  } finally {
    setNotifyBackend(null);
    seed();
  }
}

const s = () => useAppStore.getState();

interface NodeProcessLike {
  on(event: 'unhandledRejection', listener: (reason: unknown) => void): void;
  off(event: 'unhandledRejection', listener: (reason: unknown) => void): void;
}

// ---------------------------------------------------------------------------
// 1. posted: the toast's own text
// ---------------------------------------------------------------------------

test('notify: an action alert raised while the window is unfocused is posted once — title and body exact, EN', async () => {
  const backend = mockBackend();
  await withBackend(backend, async () => {
    seed('en');
    s().applyHealth(snapshotOf(1, [alertOf()]));
    await settle();
    assertEqual(s().toasts.length, 1, 'the toast is there as before');
    assertDeepEqual(
      backend.posted,
      [
        {
          title: 'Leftover agent processes',
          body: translate('en', 'health.alert.orphans.body', {
            n: 2,
            team: 'demo',
            members: 'alice, bob',
            gb: '2.4 GB',
          }),
        },
      ],
      'one notification with the alert title and body',
    );
    assertEqual(backend.posted[0]!.title, translate('en', 'health.alert.orphans.title'), 'the title key');
    assertEqual(backend.posted[0]!.body, alertText(alertOf(), 'en').body, 'the same body the toast shows');
    assert(!/[\u{1F300}-\u{1FAFF}\u{2600}-\u{27BF}]/u.test(backend.posted[0]!.body), 'no emoji');
  });
});

test('notify: the same, in Korean', async () => {
  const backend = mockBackend();
  await withBackend(backend, async () => {
    seed('ko');
    s().applyHealth(snapshotOf(1, [alertOf()]));
    await settle();
    assertEqual(backend.posted.length, 1, 'posted once');
    assertEqual(backend.posted[0]!.title, '남은 에이전트 프로세스', 'ko title');
    assertEqual(
      backend.posted[0]!.body,
      '팀이 멈춘 뒤에도 에이전트 프로세스 2개가 아직 실행 중입니다(팀 demo, 멤버 alice, bob). 메모리를 2.4 GB 쓰고 있습니다.',
      'ko body',
    );
  });
});

test('notify: a long text is one line of at most 200 characters', () => {
  const long = `${'가'.repeat(150)}\n${'b'.repeat(150)}`;
  const clipped = clipLine(long);
  assertEqual(Array.from(clipped).length, NOTIFY_MAX_CHARS, '200 characters');
  assert(clipped.endsWith('…'), 'an ellipsis marks the cut');
  assert(!clipped.includes('\n'), 'one line');
  assertEqual(clipLine('  short  text '), 'short text', 'a short text is kept, whitespace folded');
});

// ---------------------------------------------------------------------------
// 2. not posted
// ---------------------------------------------------------------------------

test('notify: not when the window has focus — the toast is enough', async () => {
  const backend = mockBackend({ focused: true });
  await withBackend(backend, async () => {
    seed('ko');
    s().applyHealth(snapshotOf(1, [alertOf()]));
    await settle();
    assertEqual(s().toasts.length, 1, 'toast shown');
    assertEqual(backend.focusChecks, 1, 'focus was checked');
    assertEqual(backend.posted.length, 0, 'nothing posted');
    assertEqual(backend.grantedChecks + backend.requests, 0, 'permission not even looked at');
  });
});

test('notify: when the window focus cannot be read, document.hasFocus() decides (none on Node → unfocused)', async () => {
  const backend = mockBackend({ focused: 'throw' });
  await withBackend(backend, async () => {
    await withWarnings(async (warnings) => {
      seed('ko');
      s().applyHealth(snapshotOf(1, [alertOf()]));
      await settle();
      assertEqual(backend.posted.length, 1, 'posted on the fallback');
      assert(warnings.length >= 1, 'the failed focus check is logged');
    });
  });
});

test('notify: not when the setting is off', async () => {
  const backend = mockBackend();
  await withBackend(backend, async () => {
    seed('ko', { systemNotifications: false });
    s().applyHealth(snapshotOf(1, [alertOf()]));
    await settle();
    assertEqual(s().toasts.length, 1, 'the toast still shows');
    assertEqual(backend.posted.length, 0, 'nothing posted');
    assertEqual(backend.focusChecks + backend.grantedChecks + backend.requests, 0, 'the backend is not touched');
  });
});

test('notify: never for a dismissed (snoozed) alert, nor an attention alert', async () => {
  const backend = mockBackend();
  await withBackend(backend, async () => {
    seed('ko');
    s().applyHealth(
      snapshotOf(1, [
        alertOf({ dismissed: true, dismissedUntil: 999_999 }),
        alertOf({ id: 'mem-low', severity: 'attention' }),
      ]),
    );
    await settle();
    assertEqual(s().toasts.length, 0, 'no toast');
    assertEqual(backend.posted.length, 0, 'no notification');
    // Dismissing a toasted alert does not post either.
    s().applyHealth(snapshotOf(2, [alertOf({ id: 'other', raisedAt: 5 })]));
    await settle();
    assertEqual(backend.posted.length, 1, 'the active one posts');
    s().applyHealth(snapshotOf(3, [alertOf({ id: 'other', raisedAt: 5, dismissed: true })]));
    await settle();
    assertEqual(backend.posted.length, 1, 'the dismissal posts nothing');
    // And called directly, the module refuses them too.
    assertEqual(await notifyHealthAlert(alertOf({ dismissed: true }), { locale: 'ko', enabled: true }), 'skipped', 'dismissed');
    assertEqual(
      await notifyHealthAlert(alertOf({ severity: 'attention' }), { locale: 'ko', enabled: true }),
      'skipped',
      'attention',
    );
  });
});

test('notify: never in showcase mode — the plugin is not reached', async () => {
  const backend = mockBackend();
  await withBackend(backend, async () => {
    setShowcaseForTests(true);
    try {
      assertEqual(await notifyHealthAlert(alertOf(), { locale: 'ko', enabled: true }), 'showcase', 'cut at the boundary');
    } finally {
      setShowcaseForTests(null);
    }
    assertEqual(backend.focusChecks + backend.grantedChecks + backend.requests + backend.posted.length, 0, 'untouched');
  });
});

test('notify: without a Tauri host (and no backend installed) nothing is attempted', async () => {
  setNotifyBackend(null);
  assertEqual(await notifyHealthAlert(alertOf(), { locale: 'ko', enabled: true }), 'unavailable', 'no host');
});

// ---------------------------------------------------------------------------
// 3. once per raise
// ---------------------------------------------------------------------------

test('notify: not twice for the same alert and raisedAt; again after an escalation (new raisedAt)', async () => {
  const backend = mockBackend();
  await withBackend(backend, async () => {
    seed('en');
    s().applyHealth(snapshotOf(1, [alertOf()]));
    await settle();
    s().applyHealth(snapshotOf(2, [alertOf({ params: { count: 3, team: 'demo', members: ['alice'], rssBytes: GIB } })]));
    await settle();
    s().applyHealth(snapshotOf(3, [alertOf()]));
    await settle();
    assertEqual(backend.posted.length, 1, 'the alert holding (same raisedAt) is not posted again');

    s().applyHealth(snapshotOf(4, [alertOf({ raisedAt: 9000 })]));
    await settle();
    assertEqual(backend.posted.length, 2, 'the escalation (new raisedAt) is posted once more');
    s().applyHealth(snapshotOf(5, [alertOf({ raisedAt: 9000 })]));
    await settle();
    assertEqual(backend.posted.length, 2, 'and only once');
  });
});

// ---------------------------------------------------------------------------
// 4. permission
// ---------------------------------------------------------------------------

test('notify: permission is asked lazily, once; granted → posted', async () => {
  const backend = mockBackend({ granted: false, answer: 'granted' });
  await withBackend(backend, async () => {
    seed('ko');
    assertEqual(backend.grantedChecks + backend.requests, 0, 'nothing asked before an alert');
    assertEqual(notifyPermission(), 'unknown', 'unknown before');
    s().applyHealth(snapshotOf(1, [alertOf(), alertOf({ id: 'second' })]));
    await settle();
    assertEqual(backend.requests, 1, 'two alerts raised together ask once');
    assertEqual(backend.posted.length, 2, 'both posted after the grant');
    assertEqual(notifyPermission(), 'granted', 'granted');
  });
});

test('notify: denied → asked at most once per run, nothing posted, and the settings sheet says so', async () => {
  const backend = mockBackend({ granted: false, answer: 'denied' });
  await withBackend(backend, async () => {
    seed('ko');
    s().applyHealth(snapshotOf(1, [alertOf(), alertOf({ id: 'second' })]));
    await settle();
    s().applyHealth(snapshotOf(2, [alertOf({ raisedAt: 5000 }), alertOf({ id: 'second' })]));
    await settle();
    s().applyHealth(snapshotOf(3, [alertOf({ id: 'third' })]));
    await settle();
    assertEqual(backend.requests, 1, 'requestPermission() called once in the run');
    assertEqual(backend.posted.length, 0, 'nothing posted');
    assertEqual(s().toasts.length, 1, 'the toasts are unaffected (one live alert now)');
    assertEqual(notifyPermission(), 'denied', 'denied is remembered');

    const settings = () => {
      Object.assign(useAppStore.getInitialState(), useAppStore.getState());
      return renderToStaticMarkup(
        <SettingsForm
          config={{ terminal: { fontSize: 12.5 } } as unknown as AppConfig}
          paths={null}
          diagnostics={null}
          diagnosticsUnavailable={false}
          error={null}
          onPatch={() => undefined}
          onShowDiagnostics={() => undefined}
          onRevealSignatures={() => undefined}
          onReloadSignatures={() => undefined}
        />,
      );
    };
    const html = settings();
    assert(html.includes(translate('ko', 'settings.systemNotifications')), 'the toggle is there');
    assert(html.includes(translate('ko', 'settings.systemNotifications.hint')), 'with its hint');
    assert(html.includes(translate('ko', 'settings.systemNotifications.denied')), 'and the denied line');

    resetNotifyState();
    assert(!settings().includes(translate('ko', 'settings.systemNotifications.denied')), 'no denied line otherwise');
  });
});

test('notify: when macOS allows it again later in the run, posting resumes without asking again', async () => {
  const backend = mockBackend({ granted: false, answer: 'denied' });
  await withBackend(backend, async () => {
    seed('ko');
    s().applyHealth(snapshotOf(1, [alertOf()]));
    await settle();
    assertEqual(notifyPermission(), 'denied', 'denied first');
    backend.isPermissionGranted = async () => true; // the user turned it on in System Settings
    s().applyHealth(snapshotOf(2, [alertOf({ raisedAt: 7000 })]));
    await settle();
    assertEqual(backend.requests, 1, 'not asked again');
    assertEqual(backend.posted.length, 1, 'posted');
    assertEqual(notifyPermission(), 'granted', 'granted now');
  });
});

// ---------------------------------------------------------------------------
// 5. failures never break the toast
// ---------------------------------------------------------------------------

test('notify: a throwing plugin → the toast still shows, a warning is logged, no unhandled rejection', async () => {
  const rejections: unknown[] = [];
  const onRejection = (reason: unknown) => rejections.push(reason);
  // The runner is Node; the DOM-only tsconfig does not know `process`.
  const node = (globalThis as unknown as { process: NodeProcessLike }).process;
  node.on('unhandledRejection', onRejection);
  try {
    for (const options of [
      { notifyThrows: true },
      { grantedRejects: true },
      { granted: false, answer: 'throw' as const },
    ] as MockOptions[]) {
      const backend = mockBackend(options);
      if (options.answer === 'throw') {
        backend.requestPermission = async () => {
          backend.requests += 1;
          throw new Error('plugin: request_permission failed');
        };
      }
      await withBackend(backend, async () => {
        await withWarnings(async (warnings) => {
          seed('ko');
          s().applyHealth(snapshotOf(1, [alertOf()]));
          await settle();
          assertEqual(s().toasts.length, 1, `${JSON.stringify(options)}: the toast is shown`);
          assertEqual(s().toasts[0]!.healthAlertId, 'orphans:tmabcdefghi1', 'the alert toast');
          assertEqual(backend.posted.length, 0, 'nothing posted');
          assert(warnings.some((w) => String(w[0]).includes('[gilde] notify')), 'the failure is a console.warn');
          // The health flow goes on: the alert ends, its toast goes.
          s().applyHealth(snapshotOf(2, []));
          assertEqual(s().toasts.length, 0, 'the toast goes with the alert');
        });
      });
    }
    await settle();
    assertEqual(rejections.length, 0, 'no unhandled rejection');
  } finally {
    node.off('unhandledRejection', onRejection);
  }
});

// ---------------------------------------------------------------------------
// 6. the setting
// ---------------------------------------------------------------------------

test('notify: the setting defaults to on; an old config without it reads as on; an explicit off is kept', () => {
  try {
    assertEqual(initialUiState().systemNotifications, true, 'default on');
    useAppStore.getState().hydrateFromConfig({ locale: 'ko' } as unknown as AppConfig);
    assertEqual(s().ui.systemNotifications, true, 'no notifications section → on');
    useAppStore.getState().hydrateFromConfig({ notifications: {} } as unknown as AppConfig);
    assertEqual(s().ui.systemNotifications, true, 'no field → on');
    useAppStore.getState().hydrateFromConfig({ notifications: { system: false } } as unknown as AppConfig);
    assertEqual(s().ui.systemNotifications, false, 'explicit off');
    useAppStore.getState().hydrateFromConfig({ notifications: { system: true } } as unknown as AppConfig);
    assertEqual(s().ui.systemNotifications, true, 'explicit on');
  } finally {
    seed();
  }
});
