/**
 * Recovery / settings / accounts / toast tests (T26).
 *
 * What a type check cannot see, and what these pin:
 *
 *  1. an undo toast counts a real 10 s window down, and pressing undo holds the
 *     answer on screen instead of vanishing with the window;
 *  2. the Recovered Sessions sheet defaults every row to *Terminate*, offers
 *     "Keep all" as an explicit button, and has no way out that is not a
 *     decision — no close button, no dismiss;
 *  3. a `KillOutcome` is rendered honestly: survivors are not a success, and a
 *     kept row is not a refusal;
 *  4. the settings patches are exactly the `config.json` subtrees DESIGN §10-3
 *     names, with an empty override clearing the key rather than storing "";
 *  5. the preview build shows Appearance and Diagnostics only, and there is no
 *     account UI left anywhere (the Settings › Accounts import panel and the
 *     disabled "M2" pickers were removed — Worker accounts is the one place).
 */

import { renderToStaticMarkup } from 'react-dom/server';

import { assert, assertDeepEqual, assertEqual, test } from '../test/harness';
import { AppShell } from '../shell/AppShell';
// The side effect under test: the three overlays this task mounts into the
// shell. Importing it here is what proves the wiring `App.tsx` relies on.
import './register';
// The launch and dispatch forms cannot be server-rendered — `Sheet` is a
// portal — so the absence of an account picker is pinned on their source.
import newSessionSource from '../terminal/NewSessionDialog.tsx?raw';
import composerSource from '../dispatch/DispatchComposer.tsx?raw';
import { setExperimentalLanesForTests } from '../features';
import { initialUiState } from '../store/uiSlice';
import { useAppStore } from '../store/useAppStore';
import type { AppStore } from '../store/useAppStore';
import { emptyGraph } from '../store/graphSlice';
import { translate } from '../i18n';
import type { MessageKey } from '../i18n';
import type { Toast } from '../store/toastSlice';
import type {
  AppConfig,
  Diagnostics,
  KillOutcome,
  RecoveredProc,
} from '../lib/ipc';
import {
  ToastHost,
  ToastRow,
  UNDO_ANSWER_MS,
  toastDue,
  toastExpired,
  toastRemainingMs,
  toastSecondsLeft,
  toastStatus,
  undoStateOf,
  undoStatusKey,
} from '../shell/ToastHost';
import {
  RECOVERY_ACTIONS,
  RecoveredSessionsCard,
  decisionsFrom,
  initialChoices,
  keepAllDecisions,
  outcomeFor,
  outcomeSummary,
  outstandingRows,
} from './RecoveredSessionsSheet';
import {
  BINARY_KINDS,
  DiagnosticsView,
  SettingsForm,
  binaryPatch,
  joinArgs,
  launchOverridePatch,
  parseBounded,
  splitArgs,
} from './SettingsSheet';

// ---------------------------------------------------------------------------
// Fixtures — shaped exactly like what Rust sends
// ---------------------------------------------------------------------------

function seedStore(patch: Partial<AppStore> = {}): void {
  useAppStore.setState({
    graph: emptyGraph(),
    ui: initialUiState(),
    toasts: [],
    agents: [],
    sidecar: null,
    recoveryPending: 0,
    ...patch,
  });
  Object.assign(useAppStore.getInitialState(), useAppStore.getState());
}

function toast(patch: Partial<Toast> = {}): Toast {
  return {
    id: 1,
    level: 'warn',
    key: 'toast.recovery.headlessTerminated',
    params: { name: 'codex', minutes: 95 },
    detail: null,
    actionId: null,
    undoToken: 'undo-1',
    at: 1_000_000,
    ttlMs: 10_000,
    ...patch,
  };
}

function recovered(patch: Partial<RecoveredProc> = {}): RecoveredProc {
  return {
    spawnId: 'spawn-1',
    kind: 'pty',
    vendor: 'claude',
    pid: 41822,
    argvDisplay: 'claude --api-key «redacted» -- "fix the ring buffer"',
    cwd: '/Users/x/.gilde/worktrees/gilde-code/pty-manager',
    worktreeId: 'wt-1',
    rssBytes: 1_200_000_000,
    ageSecs: 3600,
    keepAlive: false,
    suggested: 'terminate',
    ...patch,
  };
}

function outcome(spawnId: string, patch: Partial<KillOutcome['report']> = {}): KillOutcome {
  return {
    target: { target: 'spawn', spawnId },
    report: {
      spawnId,
      requested: [41822, 41823],
      excluded: [],
      survivors: [],
      refusedReason: null,
      dryRun: false,
      escalated: [],
      fence: 'verified',
      at: 1_000_000,
      ...patch,
    },
  };
}

function config(): AppConfig {
  return {
    schemaVersion: 1,
    locale: 'ko',
    theme: 'system',
    projects: [],
    agents: {
      pathOverride: null,
      envPassthrough: [],
      binaries: { claude: '/usr/local/bin/claude' },
      launchOverrides: { claude: { promptMode: 'argv', args: ['--verbose'] } },
    },
    sidecar: { enabled: true, nodePath: null, pingIntervalSeconds: 10 },
    dispatch: { defaultTimeoutMinutes: 45, permissionMode: 'acceptEdits', maxConcurrentRuns: 4 },
    worktree: {
      root: '~/.gilde/worktrees',
      archiveRoot: '~/.gilde/archive',
      branchPrefix: 'gilde/',
      aliasProviders: [],
      lockWhileLive: true,
    },
    supervision: {
      killGraceMs: 5000,
      maxHeadlessLifetimeMinutes: 90,
      idleTimeoutMinutes: 0,
      autoKillOverHardCeiling: false,
      softRssMiB: 2048,
      hardRssMiB: 6144,
      globalWarnPct: 70,
      globalCriticalPct: 85,
      scanIntervalSecondsActive: 3,
      scanIntervalSecondsBackground: 15,
      minAgeSecondsForBulk: 120,
      rawPtyLog: false,
    },
    status: { idleAfterMs: 1200, stallAfterMs: 120000 },
    terminal: {
      fontSize: 12.5,
      fontFamily: 'SF Mono, Menlo, monospace',
      scrollbackBytes: 2_097_152,
      webgl: true,
      shell: null,
    },
    layout: { sidebarWidth: 300, inspectorOpen: true, inspectorWidth: 360, panes: {}, tabs: [] },
    accounts: { registryMode: 'import-once' },
  };
}

test('the three overlays mount into the shell and stay silent when idle', () => {
  seedStore();
  const html = renderToStaticMarkup(<AppShell />);
  assert(html.includes('class="shell"'), 'the shell still renders with the overlays registered');
  assert(!html.includes('toast-stack'), 'an empty toast stack renders nothing');
  assert(
    !html.includes(translate('ko', 'recovery.title')),
    'the recovery sheet stays closed while there is nothing to decide',
  );
  // The rail's settings button carries the same word, so the sheet is checked
  // by its own root class rather than by its title.
  assert(!html.includes('class="set"'), 'and the settings form stays closed until ⌘, opens it');
});

// ---------------------------------------------------------------------------
// 1. Toast host — the 10 s undo window (acceptance 2)
// ---------------------------------------------------------------------------

test('an undo toast counts its window down and expires exactly once', () => {
  const t = toast();
  assertEqual(toastRemainingMs(t, t.at), 10_000, 'the window opens at 10 s');
  assertEqual(toastSecondsLeft(t, t.at + 1_400), 9, 'seconds round up, so 8.6 s reads as 9');
  assert(!toastExpired(t, t.at + 9_999), 'still live one millisecond before the end');
  assert(toastExpired(t, t.at + 10_000), 'expired at the end of the window');

  const sticky = toast({ ttlMs: 0, level: 'error', undoToken: null });
  assertEqual(toastRemainingMs(sticky, sticky.at + 10 ** 6), null, 'an error toast stays');
  assert(!toastExpired(sticky, sticky.at + 10 ** 6), 'and never expires on its own');
});

test('pressing undo holds the answer on screen past the window', () => {
  const t = toast();
  const answeredAt = t.at + 9_000;
  assert(!toastDue(t, { state: 'sending', answeredAt: 0 }, t.at + 20_000), 'never while in flight');
  assert(
    !toastDue(t, { state: 'undone', answeredAt }, answeredAt + UNDO_ANSWER_MS - 1),
    'the answer is readable after the window would have closed',
  );
  assert(
    toastDue(t, { state: 'undone', answeredAt }, answeredAt + UNDO_ANSWER_MS),
    'and then it goes',
  );
  assert(toastDue(t, undefined, t.at + 10_000), 'an untouched toast still follows its ttl');
});

test('undo_cancel(false) is a spent window, not a failure', () => {
  assertEqual(undoStateOf(true), 'undone', 'true cancelled the scheduled kill');
  assertEqual(undoStateOf(false), 'spent', 'false means the window had already closed');
  assertEqual(undoStatusKey('undone'), 'toast.undo.done', 'the confirmation is a message key');
  assertEqual(undoStatusKey('spent'), 'toast.undo.spent', 'so is the spent answer');
  assertEqual(undoStatusKey('armed'), null, 'an untouched toast shows the button instead');
  assertEqual(toastStatus('error'), 'error', 'levels map onto the four semantic tokens');
  assertEqual(toastStatus('warn'), 'stalled', 'warn reuses the amber status');
  assertEqual(toastStatus('success'), 'done', 'success reuses the ok status');
  assertEqual(toastStatus('info'), 'idle', 'info is neutral');
});

test('a toast renders its Rust key in Korean with an undo link and a countdown', () => {
  seedStore();
  const t = toast();
  const html = renderToStaticMarkup(
    <ToastRow
      toast={t}
      now={t.at + 2_000}
      undo="armed"
      onUndo={() => undefined}
      onDismiss={() => undefined}
    />,
  );
  assert(html.includes('codex'), 'the params are interpolated');
  assert(html.includes(translate('ko', 'toast.undo')), 'the undo affordance is there');
  assert(html.includes('8초'), 'the countdown shows the seconds left');
  assert(!html.includes('undefined'), 'nothing rendered the string "undefined"');
});

test('the toast host renders nothing when there is nothing to say', () => {
  seedStore({ toasts: [] });
  assertEqual(renderToStaticMarkup(<ToastHost />), '', 'an empty stack is an empty DOM');
});

test('the toast host stacks what the store holds', () => {
  seedStore({
    toasts: [toast(), toast({ id: 2, level: 'error', key: 'error.pty', ttlMs: 0, undoToken: null })],
  });
  const html = renderToStaticMarkup(<ToastHost />);
  assert(html.includes('class="toast-stack"'), 'the stack is mounted');
  assert(html.includes(translate('ko', 'error.pty')), 'an error toast renders its key');
  assert(html.includes(translate('ko', 'toast.undo')), 'and the undo toast keeps its affordance');
});

// ---------------------------------------------------------------------------
// 2. Recovered sessions — decisions (acceptance 1)
// ---------------------------------------------------------------------------

test('every row is pre-selected to Terminate and Keep all is a separate button', () => {
  const rows = [recovered(), recovered({ spawnId: 'spawn-2', vendor: 'codex' })];
  const choices = initialChoices(rows);
  assertDeepEqual(
    decisionsFrom(rows, choices),
    [
      { spawnId: 'spawn-1', action: 'terminate' },
      { spawnId: 'spawn-2', action: 'terminate' },
    ],
    'the suggested action from Rust is the default, and it is terminate',
  );
  assertDeepEqual(
    keepAllDecisions(rows),
    [
      { spawnId: 'spawn-1', action: 'keep' },
      { spawnId: 'spawn-2', action: 'keep' },
    ],
    'keeping everything is one explicit decision for every row',
  );
  assertDeepEqual(
    decisionsFrom(rows, { ...choices, 'spawn-2': 'adopt' }),
    [
      { spawnId: 'spawn-1', action: 'terminate' },
      { spawnId: 'spawn-2', action: 'adopt' },
    ],
    'a per-row choice overrides the suggestion',
  );
  assertEqual(RECOVERY_ACTIONS.length, 3, 'exactly three actions exist');
});

test('an answered row does not reopen the sheet on the next poll', () => {
  const rows = [recovered(), recovered({ spawnId: 'spawn-2' })];
  assertEqual(outstandingRows(rows, []).length, 2, 'both rows start outstanding');
  assertDeepEqual(
    outstandingRows(rows, ['spawn-1']).map((row) => row.spawnId),
    ['spawn-2'],
    'a row the user already answered is not offered again',
  );
  assertEqual(outstandingRows(rows, ['spawn-1', 'spawn-2']).length, 0, 'and the sheet closes');
});

test('a KillOutcome is summarised honestly, per decision', () => {
  const t = (key: Parameters<typeof translate>[1], params?: Record<string, string | number>) =>
    translate('ko', key, params);

  const clean = outcomeSummary(outcome('spawn-1'), 'terminate', t, 'ko');
  assertEqual(clean?.tone, 'ok', 'no survivors is the only success');
  assert(clean?.facts.includes('verified'), 'the identity fence is always shown');

  const survived = outcomeSummary(
    outcome('spawn-1', {
      survivors: [41823],
      escalated: [41823],
      excluded: [{ pid: 999, reason: 'argvMismatch' }],
    }),
    'terminate',
    t,
    'ko',
  );
  assertEqual(survived?.tone, 'error', 'a survivor is never rendered as a success');
  assert(
    survived?.facts.some((fact) => fact.includes('argvMismatch')),
    'the exclusion reason is reported verbatim',
  );

  const kept = outcomeSummary(
    outcome('spawn-1', { refusedReason: 'kept by the user', fence: 'unfenced' }),
    'keep',
    t,
    'ko',
  );
  assertEqual(kept?.text, translate('ko', 'recovery.outcome.kept'), 'keep is not a refusal');
  assertEqual(kept?.tone, undefined, 'and it is not an error either');

  const gone = outcomeSummary(
    outcome('spawn-1', { refusedReason: 'notFound' }),
    'adopt',
    t,
    'ko',
  );
  assertEqual(gone?.text, translate('ko', 'recovery.outcome.gone'), 'a vanished row says so');

  const refused = outcomeSummary(
    outcome('spawn-1', { refusedReason: 'argvMismatch' }),
    'terminate',
    t,
    'ko',
  );
  assertEqual(refused?.tone, 'warn', 'a refused kill is a warning, not a success');
  assertEqual(outcomeSummary(undefined, 'terminate', t, 'ko'), null, 'no outcome, no line');
  assertEqual(
    outcomeFor([outcome('spawn-2')], 'spawn-1'),
    undefined,
    'outcomes are matched by spawn id',
  );
});

test('the sheet states the facts and offers no exit that is not a decision', () => {
  seedStore();
  const rows = [recovered()];
  const html = renderToStaticMarkup(
    <RecoveredSessionsCard
      rows={rows}
      choices={initialChoices(rows)}
      outcomes={null}
      busy={false}
      error={null}
      onChoose={() => undefined}
    />,
  );

  assert(html.includes('«redacted»'), 'the argv is the masked one Rust produced');
  assert(html.includes('claude'), 'the vendor badge is there');
  assert(html.includes('1.2 GB') || html.includes('1.2GB'), 'RSS is shown');
  assert(html.includes('pty-manager'), 'the cwd is shown');
  assert(html.includes(translate('ko', 'recovery.action.terminate')), 'Terminate is offered');
  assert(html.includes(translate('ko', 'recovery.action.keep')), 'Keep is offered');
  assert(html.includes(translate('ko', 'recovery.action.adopt')), 'Adopt is offered');
  assert(html.includes('aria-pressed="true"'), 'one action is pre-selected');
  assert(!/aria-label="[^"]*닫기/.test(html), 'the card carries no dismiss affordance');
  assert(!html.includes('toast-close'), 'and no close glyph of any kind');
});

test('after resolving, each row shows what actually happened', () => {
  seedStore();
  const rows = [recovered()];
  const html = renderToStaticMarkup(
    <RecoveredSessionsCard
      rows={rows}
      choices={{ 'spawn-1': 'terminate' }}
      outcomes={[outcome('spawn-1', { survivors: [41823] })]}
      busy={false}
      error={null}
      onChoose={() => undefined}
    />,
  );
  assert(html.includes('data-tone="error"'), 'a survivor keeps the row red');
  assert(html.includes('verified'), 'the fence is quoted verbatim');
  assert(
    !html.includes(translate('ko', 'recovery.action.adopt')) || html.includes('rec-outcome'),
    'the action segment is replaced by the outcome once resolved',
  );
});

// ---------------------------------------------------------------------------
// 3. Settings patches (acceptance 3)
// ---------------------------------------------------------------------------

test('a binary override patches exactly one config subtree and clears with null', () => {
  assertDeepEqual(
    binaryPatch('codex', '  /opt/homebrew/bin/codex '),
    { agents: { binaries: { codex: '/opt/homebrew/bin/codex' } } },
    'the path is trimmed and nested under agents.binaries',
  );
  assertDeepEqual(
    binaryPatch('codex', '   '),
    { agents: { binaries: { codex: null } } },
    'an emptied field clears the key back to detection, it does not store ""',
  );
  assertDeepEqual(
    launchOverridePatch('claude', { promptMode: 'typeAfterReady' }),
    { agents: { launchOverrides: { claude: { promptMode: 'typeAfterReady' } } } },
    'launch overrides are per agent',
  );
  assertDeepEqual(splitArgs('  --verbose   --mcp-debug '), ['--verbose', '--mcp-debug'], 'split');
  assertEqual(joinArgs(['--verbose', '--mcp-debug']), '--verbose --mcp-debug', 'and join back');
  assertEqual(joinArgs(undefined), '', 'no override reads as an empty line');
});

test('a number field refuses anything outside its range', () => {
  assertEqual(parseBounded('4096', 128, 65536), 4096, 'an in-range number commits');
  assertEqual(parseBounded('0', 128, 65536), null, 'below the minimum does not');
  assertEqual(parseBounded('99999999', 128, 65536), null, 'nor above the maximum');
  assertEqual(parseBounded('abc', 0, 10), null, 'nor a non-number');
  assertEqual(parseBounded('0', 0, 1440), 0, 'zero is valid where zero means "off"');
});

test('with the experimental lanes on, the settings form exposes every field T26 requires', () => {
  seedStore();
  setExperimentalLanesForTests(true);
  let html: string;
  try {
    html = renderToStaticMarkup(
    <SettingsForm
      config={config()}
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
  } finally {
    setExperimentalLanesForTests(null);
  }

  for (const key of [
    'settings.locale',
    'settings.theme',
    'settings.fontSize',
    'settings.rawPtyLog',
    'settings.softRss',
    'settings.hardRss',
    'settings.warnPct',
    'settings.criticalPct',
    'settings.autoKill',
    'settings.idleTimeout',
    'settings.scanActive',
    'settings.scanBackground',
    'settings.openSignatures',
    'settings.reloadSignatures',
    'settings.showDiagnostics',
  ] as const) {
    assert(html.includes(translate('ko', key)), `${key} is on the sheet`);
  }
  for (const kind of BINARY_KINDS) {
    assert(html.includes(translate('ko', `settings.binary.${kind}` as MessageKey)), `${kind} has a field`);
  }
  assert(html.includes('/usr/local/bin/claude'), 'the configured binary is pre-filled');
  assert(html.includes('--verbose'), 'the launch override args are pre-filled');
  assert(
    html.includes(translate('ko', 'settings.autoKill.hint')),
    'the default-off supervision knobs say they are off',
  );
  assert(!html.includes('undefined'), 'nothing rendered the string "undefined"');
});

test('diagnostics render the fields DESIGN §10-6 promises', () => {
  seedStore();
  const diagnostics: Diagnostics = {
    resolvedPath: ['/usr/local/bin', '/usr/bin'],
    pathSource: 'loginShell',
    pathFingerprint: 'ab12cd34',
    agents: [],
    sidecar: {
      nodePath: '/Applications/Gilde Code.app/Contents/MacOS/gilde-node',
      nodeSource: 'vendored',
      scriptPath: '/Applications/Gilde Code.app/Contents/Resources/sidecar.cjs',
      state: 'ready',
      generation: 2,
      restarts: 1,
      lastError: null,
    },
    db: { path: '/Users/x/.gilde/state.db', schemaVersion: 3, integrityOk: true },
    memoryPressureSource: 'kern.memorystatus_vm_pressure_level',
    appInstanceId: 'inst-1',
    bootId: 'boot-1',
    appDataDir: '/Users/x/.gilde',
    worktreeRoot: '/Users/x/.gilde/worktrees',
    dispatchRoot: '/Users/x/.gilde/dispatch',
  };
  const html = renderToStaticMarkup(<DiagnosticsView diagnostics={diagnostics} />);
  assert(html.includes('loginShell'), 'the PATH source is quoted verbatim');
  assert(html.includes('kern.memorystatus_vm_pressure_level'), 'the pressure probe is named');
  assert(html.includes('state.db'), 'the database path is there');
  assert(html.includes('inst-1'), 'so are the instance and boot ids');
});

// ---------------------------------------------------------------------------
// 4. The preview build: Appearance and Diagnostics only, no account UI
// ---------------------------------------------------------------------------

function previewForm(): string {
  return renderToStaticMarkup(
    <SettingsForm
      config={config()}
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
}

test('the preview settings show Appearance and Diagnostics, and hide every lane section', () => {
  seedStore();
  setExperimentalLanesForTests(false);
  let html: string;
  try {
    html = previewForm();
  } finally {
    setExperimentalLanesForTests(null);
  }
  for (const key of ['settings.section.appearance', 'settings.section.diagnostics', 'settings.locale', 'settings.theme', 'settings.showDiagnostics'] as const) {
    assert(html.includes(translate('ko', key)), `${key} is shown in the preview`);
  }
  for (const key of [
    'settings.section.terminal',
    'settings.section.memory',
    'settings.section.scan',
    'settings.section.binaries',
    'settings.section.launch',
    'settings.rawPtyLog',
    'settings.fontSize',
    'settings.reloadSignatures',
    'settings.openSignatures',
  ] as const) {
    assert(!html.includes(translate('ko', key)), `${key} is hidden in the preview`);
  }
  assert(html.includes(translate('ko', 'settings.locale.system')), 'the language can follow the system');
  assert(html.includes('한국어') && html.includes('English'), 'each language is offered by its own name');
});

test('no account UI is left: no Accounts section, no disabled "M2" picker', () => {
  seedStore();
  setExperimentalLanesForTests(true);
  let html: string;
  try {
    html = previewForm();
  } finally {
    setExperimentalLanesForTests(null);
  }
  assert(!html.includes('registry.json'), 'the one-time registry import panel is gone');
  assert(!/AccountPicker|accountsList/.test(newSessionSource), 'the launch form has no account picker');
  assert(!/accountM2|accountAmbient/.test(composerSource), 'nor does the dispatch composer');
});
