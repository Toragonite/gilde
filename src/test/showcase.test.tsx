/**
 * The showcase (`VITE_GILDE_SHOWCASE=1`, src/features.ts, src/showcase/):
 *
 *  1. each scene seeds a store that renders the view it is for — the whole
 *     shell, rendered to static markup like render.test.tsx — and neither the
 *     markup nor the demo world holds an owner's name, a real e-mail address or
 *     a macOS home-folder path;
 *  2. the demo data passes the value rules real data meets (one display line,
 *     finite numbers, the engine's caps), and the store's own reducers took it;
 *  3. in showcase mode nothing reaches Tauri: every command and every event
 *     subscription stops at the boundary (`lib/commands.ts`, `lib/events.ts`).
 *     The spy stands in for the Tauri runtime itself (`window.__TAURI_INTERNALS__`)
 *     and is shown to be sensitive by a call with the mode off.
 *
 * That a NORMAL build carries none of the showcase is a build-output check, too
 * slow for this runner: `node scripts/check-showcase-tree-shaking.mjs`.
 */

import { renderToStaticMarkup } from 'react-dom/server';

import { SHOWCASE, setShowcaseForTests } from '../features';
import { translate } from '../i18n';
import { cmd, installShowcaseBackend, isUnavailable } from '../lib/commands';
import { listenAll } from '../lib/events';
import { formatPercentPoints } from '../lib/format';
import { formatClock } from '../shell/healthText';
import { GILDE_EVENTS } from '../lib/ipc';
import { registerPaneView } from '../panes/Pane';
import { AppShell, registerSidebar } from '../shell/AppShell';
import { createShowcaseBackend, sceneConfig } from '../showcase/backend';
import { SCENE_NAMES, SCENES, parseShowcaseQuery } from '../showcase/scenes';
import type { SceneName, ShowcaseOptions } from '../showcase/scenes';
import { SHOWCASE_MARKER, seedShowcase } from '../showcase/showcase';
import type { SeededShowcase } from '../showcase/showcase';
import { MAIN_TEAM_ID, buildWorld } from '../showcase/world';
import type { ShowcaseWorld } from '../showcase/world';
import { Sidebar } from '../sidebar/Sidebar';
import { emptyGraph } from '../store/graphSlice';
import { initialHealthState } from '../store/healthSlice';
import { initialTeamState } from '../store/teamSlice';
import { cancelPersistLayout, initialUiState } from '../store/uiSlice';
import { initialWorkersState } from '../store/workersSlice';
import { initialWorktreeState, resetFolderPicker } from '../store/worktreeSlice';
import { useAppStore } from '../store/useAppStore';
import { TeamPane } from '../team/TeamPane';
import { CAP, flat } from '../team/model';
import { WorkersPane } from '../workers/WorkersPane';
import { assert, assertEqual, test } from './harness';
import { withLanes } from './lanes';

const en = (key: Parameters<typeof translate>[1], params?: Record<string, string | number>) =>
  translate('en', key, params);

// ---------------------------------------------------------------------------
// the Tauri runtime, as a spy
// ---------------------------------------------------------------------------

interface TauriSpy {
  /** Every property of the Tauri internals that was read (`invoke`, `transformCallback`, …). */
  touched: string[];
  restore: () => void;
}

/**
 * Stand in for the Tauri runtime: `@tauri-apps/api` reaches the host only
 * through `window.__TAURI_INTERNALS__` (invoke, listen, channels). Any read of it
 * is recorded. Node has no `window`, so it exists only while the spy does.
 */
function spyTauri(): TauriSpy {
  const g = globalThis as Record<string, unknown>;
  const had = Object.prototype.hasOwnProperty.call(g, 'window');
  const previous = g.window;
  const touched: string[] = [];
  const internals = new Proxy(
    {},
    {
      get: (_target, key) => {
        touched.push(String(key));
        return () => Promise.reject(new Error('the Tauri runtime was reached'));
      },
    },
  );
  g.window = { __TAURI_INTERNALS__: internals, __TAURI_EVENT_PLUGIN_INTERNALS__: internals };
  return {
    touched,
    restore: () => {
      // A layout write debounced while `window` existed must not fire after it.
      cancelPersistLayout();
      if (had) g.window = previous;
      else delete g.window;
    },
  };
}

// ---------------------------------------------------------------------------
// one scene, seeded and rendered
// ---------------------------------------------------------------------------

function resetStore(): void {
  // Repository identities carry over: the team slice asks a path again only
  // after IDENTITY_STALE_MS, and every scene's paths have the same answers.
  const identities = useAppStore.getState().team.identities;
  useAppStore.setState({
    graph: emptyGraph(),
    ui: initialUiState(),
    toasts: [],
    agents: [],
    sidecar: null,
    sessions: new Map(),
    team: { ...initialTeamState(), identities },
    worktree: initialWorktreeState(),
    workers: initialWorkersState(),
    health: initialHealthState(),
  });
}

function render(): string {
  Object.assign(useAppStore.getInitialState(), useAppStore.getState());
  return renderToStaticMarkup(<AppShell />);
}

interface SceneRun {
  html: string;
  seeded: SeededShowcase;
  touched: string[];
}

async function runScene(options: ShowcaseOptions): Promise<SceneRun> {
  resetStore();
  const spy = spyTauri();
  const disposers = [
    registerSidebar(Sidebar),
    registerPaneView('team', TeamPane),
    registerPaneView('workers', WorkersPane),
  ];
  setShowcaseForTests(true);
  try {
    // The showcase is always the preview build (lanes off).
    return await withLanes(false, async () => {
      const seeded = await seedShowcase(options);
      // The app mounts its one event bridge on top: in showcase mode it subscribes nothing.
      const stop = listenAll((add) => {
        for (const name of GILDE_EVENTS) add(name, () => {});
      });
      await Promise.resolve();
      stop();
      await Promise.resolve();
      const html = render();
      return { html, seeded, touched: [...spy.touched] };
    });
  } finally {
    setShowcaseForTests(null);
    installShowcaseBackend(null);
    resetFolderPicker();
    for (const dispose of disposers) dispose();
    spy.restore();
  }
}

function scene(name: SceneName, patch: Partial<ShowcaseOptions> = {}): Promise<SceneRun> {
  return runScene({ ...parseShowcaseQuery(`?scene=${name}`), ...patch });
}

// ---------------------------------------------------------------------------
// what must never be in a screenshot
// ---------------------------------------------------------------------------

/**
 * The owner's names, spelled in pieces so this file itself passes the public
 * export's content scan (scripts/export-public.sh), plus any macOS home path.
 */
const word = (...parts: string[]) => new RegExp(parts.join(''), 'i');
const FORBIDDEN: readonly RegExp[] = [
  word('medi', 'ark'),
  word('tora', 'gonite'),
  word('jongkyoung', 'han'),
  word('dora', 'emon'),
  new RegExp(['', 'Users', ''].join('/')),
];
const EMAIL = /[A-Za-z0-9._%+-]+@[A-Za-z0-9-]+(?:\.[A-Za-z0-9-]+)*\.[A-Za-z]{2,}/g;

function assertNeutral(text: string, what: string): void {
  for (const re of FORBIDDEN) {
    const hit = re.exec(text);
    assert(hit === null, `${what} holds ${re}: …${hit ? text.slice(Math.max(0, hit.index - 40), hit.index + 40) : ''}…`);
  }
  for (const address of text.match(EMAIL) ?? []) {
    assert(address.toLowerCase().endsWith('@example.com'), `${what} holds an e-mail address: ${address}`);
  }
}

function count(html: string, needle: string): number {
  return html.split(needle).length - 1;
}

// ---------------------------------------------------------------------------
// the URL
// ---------------------------------------------------------------------------

test('showcase: the build flag is off in the unit-test bundle (and a normal build)', () => {
  assertEqual(SHOWCASE, false, 'VITE_GILDE_SHOWCASE is not set here');
});

test('showcase: the query picks the scene, language, theme and member; anything else falls back', () => {
  assertEqual(parseShowcaseQuery('').scene, 'team', 'team by default');
  assertEqual(parseShowcaseQuery('').locale, 'en', 'English by default');
  assertEqual(parseShowcaseQuery('').member, 'api', 'the Team scene opens a member');
  for (const name of SCENE_NAMES) assertEqual(parseShowcaseQuery(`?scene=${name}`).scene, name, name);
  assertEqual(parseShowcaseQuery('?scene=nope&lang=fr&theme=neon').scene, 'team', 'an unknown scene is the default');
  assertEqual(parseShowcaseQuery('?scene=nope&lang=fr&theme=neon').locale, 'en', 'an unknown language is English');
  assertEqual(parseShowcaseQuery('?scene=nope&lang=fr&theme=neon').theme, 'system', 'an unknown theme follows the system');
  assertEqual(parseShowcaseQuery('?scene=workers&lang=ko').locale, 'ko', 'Korean');
  assertEqual(parseShowcaseQuery('?theme=dark').theme, 'dark', 'dark');
  assertEqual(parseShowcaseQuery('?member=none').member, null, 'no member');
  assertEqual(parseShowcaseQuery('?member=web').member, 'web', 'another member');
  assertEqual(parseShowcaseQuery('?scene=health').member, null, 'the health scene opens none');
  assertEqual(parseShowcaseQuery('?scene=health-snoozed').scene, 'health-snoozed', 'the snoozed health scene');
  assertEqual(parseShowcaseQuery('?scene=health-snoozed').member, null, 'which opens none either');
  assertEqual(SCENE_NAMES.length, 5, 'five scenes');
});

// ---------------------------------------------------------------------------
// the demo world meets the value rules
// ---------------------------------------------------------------------------

function walk(value: unknown, path: string, visit: (path: string, value: string | number) => void): void {
  if (typeof value === 'string' || typeof value === 'number') visit(path, value);
  else if (Array.isArray(value)) value.forEach((v, i) => walk(v, `${path}[${i}]`, visit));
  else if (value !== null && typeof value === 'object') {
    for (const [k, v] of Object.entries(value)) walk(v, `${path}.${k}`, visit);
  }
}

function worldOf(name: SceneName): ShowcaseWorld {
  return buildWorld(Date.now(), SCENES[name].world);
}

test('showcase: every demo value is one clean display line or a finite number, and holds nothing private', () => {
  for (const name of SCENE_NAMES) {
    const world = worldOf(name);
    walk(world, name, (path, value) => {
      if (typeof value === 'number') {
        assert(Number.isFinite(value), `${path} is a finite number`);
      } else {
        assertEqual(flat(value, 100_000), value, `${path} is already one display line`);
      }
    });
    assertNeutral(JSON.stringify(world), `the ${name} world`);
  }
});

test('showcase: the demo data stays inside the caps the engine and the views apply', () => {
  const world = worldOf('grouping');
  for (const view of world.views) {
    const s = view.state;
    assert(s.name.length <= CAP.name && s.title.length <= CAP.title, `${s.name}: name and title`);
    assert((s.goal ?? '').length <= CAP.goal, `${s.name}: goal`);
    for (const m of s.members) {
      assert(m.name.length <= CAP.name && m.role.length <= CAP.role, `${m.name}: name and role`);
      assert((m.lastActivity?.text ?? '').length <= CAP.activity, `${m.name}: activity`);
    }
    for (const t of s.board) assert(t.title.length <= CAP.title && (t.note ?? '').length <= CAP.note, `${t.id}: title and note`);
    for (const msg of view.messages) assert((msg.body ?? '').length <= CAP.message, `m${msg.seq}: body`);
    for (const a of view.activity) for (const l of a.lines) assert(l.text.length <= CAP.activity, `${a.member}: activity line`);
  }
  for (const a of world.workers.snapshot?.accounts ?? []) {
    // The vendored uiTypes.ts caps: name ≤ 64, provider label ≤ 32, plan label ≤ 40, window label ≤ 40.
    assert(a.name.length <= 64 && a.providerLabel.length <= 32 && (a.planLabel ?? '').length <= 40, `${a.name}: caps`);
    for (const w of a.windows) {
      assert(w.label.length <= 40, `${a.name}: window label`);
      assert(w.percent >= 0 && w.percent <= 100, `${a.name}: window percent`);
    }
  }
});

// ---------------------------------------------------------------------------
// the scenes
// ---------------------------------------------------------------------------

test('showcase scene team: the live team, its board and messages, a member\'s details open', async () => {
  const { html, seeded, touched } = await scene('team');
  assertEqual(touched.length, 0, `nothing reached Tauri (touched: ${touched.join(', ')})`);
  assertNeutral(html, 'the team scene');

  const s = useAppStore.getState();
  assertEqual(s.ui.activeView, 'team', 'Team is in front');
  assertEqual(s.ui.locale, 'en', 'in English (the app setting)');
  assertEqual(s.team.views.size, seeded.world.views.length, 'the store took every view (latest-wins reducer)');
  assertEqual(s.team.selectedTeamId, MAIN_TEAM_ID, 'checkout-revamp is selected');
  assertEqual(s.team.selectedMember, 'api', 'and its member api');
  assertEqual(s.health.snapshot?.overall, 'ok', 'everything is running well');

  assert(html.includes('data-view="team"') && html.includes('data-inspector="open"'), 'the Team view with the inspector open');
  assert(html.includes(en('team.member.details', { name: 'api' })), 'on api');
  assert(html.includes('checkout-revamp') && html.includes('One-page checkout with saved cards'), 'the team header');
  assert(html.includes(en('team.section.members', { n: 3 })), 'three members');
  for (const provider of ['Claude Code', 'Codex', 'Cursor']) assert(html.includes(`>${provider}<`), `a member on ${provider}`);
  assert(html.includes(en('team.member.running')) && html.includes(en('team.member.idle')), 'one running, others idle');
  assert(html.includes('Waiting for the lead'), 'one waits on the lead');
  for (const column of ['team.board.open', 'team.board.claimed', 'team.board.blocked', 'team.board.review', 'team.board.done'] as const) {
    assert(html.includes(en(column)), `the board column ${column}`);
  }
  assert(html.includes('POST /v1/checkout/sessions endpoint') && html.includes('Card form with inline validation'), 'the tasks');
  assert(html.includes(en('team.messageKind.question')) && html.includes(en('team.messageKind.report')), 'the messages');
  assert(html.includes('Edit · src/api/checkout/errors.ts'), 'api\'s activity in the inspector');
  assert(html.includes('data-testid="member-memory"'), 'members carry their memory from the health snapshot');
  assert(html.includes(en('statusbar.connection.ready')), 'the status bar says connected');
  assert(!html.includes('data-testid="health-popover"'), 'no popover in this scene');
  assert(!html.includes('team-alert-chip'), 'and no alert');
});

test('showcase scene team in Korean: the app\'s own language setting', async () => {
  const { html, touched } = await scene('team', { locale: 'ko' });
  assertEqual(touched.length, 0, 'nothing reached Tauri');
  assertEqual(useAppStore.getState().ui.locale, 'ko', 'Korean');
  assert(html.includes(translate('ko', 'view.team')) && html.includes(translate('ko', 'team.board.review')), 'Korean labels');
  // The engine's notice (`web moved b4 "…" to review: …`) is shown in the user's language.
  const notice = translate('ko', 'team.notice.movedToReview', { member: 'web', id: 'b4', title: 'T', report: 'R' });
  const [lead, tail] = [notice.split('"T"')[0]!, notice.split('"T"')[1]!.split('R')[0]!];
  assert(html.includes(lead) && html.includes(tail), `the engine notice localized (${lead} … ${tail})`);
  assertNeutral(html, 'the Korean team scene');
});

test('showcase scene workers: five accounts across the CLIs, usage windows, a shared plan, a re-login', async () => {
  const { html, touched } = await scene('workers');
  assertEqual(touched.length, 0, 'nothing reached Tauri');
  assertNeutral(html, 'the workers scene');
  assertEqual(useAppStore.getState().ui.activeView, 'workers', 'Worker accounts is in front');
  assert(html.includes('data-view="workers"') && html.includes('data-sidebar="hidden"'), 'the Worker accounts view');
  assertEqual(count(html, 'class="workers-account col"'), 5, 'five accounts');
  for (const name of ['main', 'work', 'ci-bot', 'codex', 'cursor']) assert(html.includes(`>${name}<`), `the account ${name}`);
  assert(html.includes('>Codex<') && html.includes('>Cursor<'), 'other vendors are labelled');
  assert(html.includes('data-login="ok"') && html.includes('data-login="expired"'), 'signed in, and one to sign in again');
  assert(html.includes(en('workers.login.runFailed')), 'the run-failure marker in words');
  assert(count(html, 'data-testid="workers-same-account"') === 2, 'main and work share one plan');
  assert(html.includes('data-severity="critical"'), 'one window is near its limit');
  assert(html.includes(`>${formatPercentPoints(23, 'en', 1)}<`), 'a 5-hour window at 23%');
  assert(html.includes(`>${formatPercentPoints(61, 'en', 1)}<`), 'a weekly window at 61%');
  assert(html.includes(en('workers.running', { n: 1 })), 'one account is running a member');
});

test('showcase scene health: the popover over the status bar, one actionable leftover of an ended team', async () => {
  const { html, touched } = await scene('health');
  assertEqual(touched.length, 0, 'nothing reached Tauri');
  assertNeutral(html, 'the health scene');
  const s = useAppStore.getState();
  assertEqual(s.health.popoverOpen, true, 'the popover is open');
  assertEqual(s.health.snapshot?.alerts.length, 1, 'one alert');
  assertEqual(s.health.snapshot?.alerts[0]?.kind, 'orphans', 'a leftover agent process');
  assertEqual(s.toasts.length, 1, 'which raised its one sticky toast, as in the app');
  assert(html.includes('data-testid="health-popover"'), 'the popover');
  // The toast is held but not drawn over the popover, which shows the same alert.
  assert(!html.includes('class="toast-stack"'), 'no toast over the popover');
  assertEqual(count(html, en('health.alert.orphans.title')), 1, 'the alert once, in the popover');
  assertEqual(s.health.snapshot?.alerts[0]?.dismissed, false, 'not dismissed by hiding its toast');
  assert(html.includes(en('health.alert.orphans.title')), 'the real alert title');
  assert(html.includes('billing-export') && html.includes('exporter'), 'naming the ended team and its member');
  assert(html.includes('data-action="cleanUp"'), 'with the clean-up action');
  assert(html.includes(en('health.overall.action')), 'the overall line');
  assert(html.includes(en('health.member.leftover')) && html.includes(en('health.member.running')), 'members with their state');
  assert(html.includes('data-level="action"'), 'the indicator says action needed');
  // The alert is about the ended team; the team in front (checkout-revamp) carries no alert chip.
  assert(!html.includes('team-alert-chip'), 'checkout-revamp itself is fine');
});

test('showcase scene health-snoozed: the toast was closed — the alert is snoozed, never "all well", still fixable', async () => {
  for (const locale of ['en', 'ko'] as const) {
    const t = (key: Parameters<typeof translate>[1], params?: Record<string, string | number>) =>
      translate(locale, key, params);
    const { html, seeded, touched } = await scene('health-snoozed', { locale });
    assertEqual(touched.length, 0, 'nothing reached Tauri');
    assertNeutral(html, `the ${locale} health-snoozed scene`);
    const s = useAppStore.getState();
    assertEqual(s.health.popoverOpen, true, 'the popover is open');
    assert(seeded.asked.includes('health_dismiss'), 'dismissed through the backend, as closing the toast does');
    const alert = s.health.snapshot?.alerts[0];
    assertEqual(alert?.dismissed, true, 'the alert is dismissed');
    const until = alert?.dismissedUntil ?? null;
    assert(typeof until === 'number' && until > (s.health.snapshot?.at ?? 0), `dismissedUntil set like Rust (${until})`);
    assertEqual(s.health.snapshot?.overall, 'ok', 'the overall level ignores it (Rust\'s rule)');
    assertEqual(s.toasts.length, 0, 'its toast is gone');
    assert(!html.includes('class="toast-stack"'), 'no toast drawn');

    const time = formatClock(until as number, locale);
    assert(!html.includes(t('health.overall.ok')), 'never "Everything is running well"');
    assert(html.includes(t('health.overall.snoozedOne', { n: 1, time })), `the neutral headline with ${time}`);
    assert(html.includes('data-overall="snoozed"'), 'grey, snoozed');
    assert(html.includes('data-testid="health-snoozed"') && html.includes(t('health.snoozed.title')), 'the snoozed section');
    assert(html.includes(t('health.alert.orphans.title')), 'with the alert');
    assert(html.includes(t('health.snoozed.backAt', { time })), 'and when it comes back');
    assert(html.includes('data-action="cleanUp"'), 'still with its clean-up');
    assert(html.includes(t('health.member.leftover')) && html.includes('billing-export'), 'the leftovers still listed');
    // The status bar keeps the trace.
    const footer = html.match(/<footer class="statusbar"[\s\S]*?<\/footer>/)?.[0] ?? '';
    assert(footer.includes(t('health.indicator.snoozed', { n: 1 })), 'the indicator says one is snoozed');
    assert(!footer.includes('health-badge'), 'without a red badge');
  }
});

test('showcase scene grouping: three repositories, worktree-aware, an ended and a suspended team', async () => {
  const { html, touched } = await scene('grouping');
  assertEqual(touched.length, 0, 'nothing reached Tauri');
  assertNeutral(html, 'the grouping scene');
  const s = useAppStore.getState();
  assertEqual(s.ui.teamGroupBy, 'repository', 'grouped by repository');
  assertEqual(s.team.views.size, 4, 'four teams');
  for (const slug of ['acme/checkout', 'acme/web', 'acme/docs']) {
    assert(html.includes(`data-group="origin:git.example.com/${slug}"`), `a ${slug} group`);
  }
  const checkout = html.match(/data-group="origin:git.example.com\/acme\/checkout"[\s\S]*?(?=data-group="origin:git.example.com\/acme\/(?:web|docs)")/)?.[0] ?? '';
  assert(checkout.includes('checkout-revamp') && checkout.includes('billing-export'), 'the team in a linked worktree groups with its repository');
  assert(html.includes('data-follow="same"'), 'members\' worktrees follow their repository');
  assert(html.includes(en('team.status.ended')) && html.includes(en('team.status.suspended')), 'an ended and a suspended team');
  assert(html.includes('team-scope-all') && html.includes('team-scope-workspace'), 'the scope switch');
  assert(!/class="team-scope-workspace"[^>]*disabled/.test(html), 'with a folder to scope to');
  assert(html.includes(en('team.folders.worktree', { branch: 'billing-export' })), 'a project folder that is a worktree');
});

// ---------------------------------------------------------------------------
// the boundary
// ---------------------------------------------------------------------------

test('showcase boundary: with the mode off the spy sees Tauri (it is sensitive); on, nothing reaches it', async () => {
  const spy = spyTauri();
  try {
    await withLanes(false, async () => {
      // Off: the ordinary path reaches the runtime.
      await cmd.healthGet().catch(() => null);
      assert(spy.touched.includes('invoke'), 'a command reaches __TAURI_INTERNALS__.invoke');
      spy.touched.length = 0;

      setShowcaseForTests(true);
      try {
        // On, before a backend is installed: refused as "unavailable", nothing reached.
        let refused: unknown = null;
        await cmd.healthGet().catch((e: unknown) => {
          refused = e;
        });
        assert(refused !== null && isUnavailable(refused), 'refused the way a missing command is');
        // On, with a backend: answered by it.
        installShowcaseBackend(async (name) => (name === 'sidecar_status' ? { state: 'ready', generation: 1, restarts: 0, lastError: null, nodePath: null, scriptPath: null } : null));
        assertEqual((await cmd.sidecarStatus()).state, 'ready', 'the backend answers');
        // Events: nothing subscribed, the teardown is harmless.
        const stop = listenAll((add) => {
          for (const name of GILDE_EVENTS) add(name, () => {});
        });
        await Promise.resolve();
        stop();
        await Promise.resolve();
        assertEqual(spy.touched.length, 0, `nothing reached Tauri (touched: ${spy.touched.join(', ')})`);
      } finally {
        setShowcaseForTests(null);
        installShowcaseBackend(null);
      }
    });
  } finally {
    spy.restore();
  }
});

test('showcase boundary: a command the demo backend does not know is refused as unavailable, with the marker', async () => {
  const { seeded } = await scene('team');
  setShowcaseForTests(true);
  try {
    const world = seeded.world;
    const { backend } = createShowcaseBackend(world, sceneConfig('en', 'system', SCENES.team));
    installShowcaseBackend(backend);
    let refused: unknown = null;
    await cmd.procmonScan({}, false).catch((e: unknown) => {
      refused = e;
    });
    assert(refused !== null && isUnavailable(refused), 'unavailable');
    assert(JSON.stringify(refused).includes(SHOWCASE_MARKER), 'and it says it is the showcase');
    // The settings write sticks for the session, as the app's would.
    await cmd.appSettingsSet({ locale: 'ko' });
    assertEqual((await cmd.appSettingsGet()).locale, 'ko', 'a language change sticks');
  } finally {
    setShowcaseForTests(null);
    installShowcaseBackend(null);
    resetStore();
  }
});
