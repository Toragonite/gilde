/**
 * Worker accounts view tests (Gilde plan #3c).
 *
 * The snapshot is not hand-written here: it is THE fixture
 * (`src-tauri/sidecar/test/fixtures/cco-workers-snapshot.json`), the
 * `WorkersSnapshot` the sidecar test's fake dispatch host serves and proves the
 * sidecar passes through unchanged. Docs and screenshots use the same file —
 * never a live snapshot (host-protocol RULES, vendored at CCO 0b7b1b8). Its times are
 * shifted to "now" so relative times ("resets in", "left") are deterministic.
 *
 * What is pinned:
 *
 *  1. the view renders the engine's values, each state distinctly: login
 *     ok / expired (the run-failure marker as a sentence) / error / refreshing,
 *     a cooldown in the future, overage on, another vendor's account, a shared
 *     claude.ai login, carried-forward (stale) windows; percents as given;
 *     "as of" the engine's time; resets as local time with "resets in";
 *  2. it is READ-ONLY: no input, and one button — the refresh;
 *  3. no snapshot is not an empty one: an older host, an unknown schema
 *     (`cco.workers/2`) and no host each say so in a sentence, and show no
 *     account at all — nothing of such an answer is parsed;
 *  4. stale, never blank: the last snapshot stays, marked with its time;
 *  5. hostile strings are flattened, capped and escaped;
 *  6. the slice's pure rules: an answer without a snapshot keeps the previous
 *     one (stale), and the poll delay stays within 5 s … the 60 s floor;
 *  7. CCO 0b7b1b8: "cached by the host" next to "as of" exactly when the host's
 *     snapshot says `servedFromCache: true` (not for Gilde's own floor), and the
 *     run-failure marker is the vendored constant, imported — never a copied literal.
 */

import { renderToStaticMarkup } from 'react-dom/server';

import fixture from '../../src-tauri/sidecar/test/fixtures/cco-workers-snapshot.json';
import { translate } from '../i18n';
import type { CcoWorkerAccount, CcoWorkersAnswer, CcoWorkersSnapshot } from '../lib/ipc';
import { formatDuration, formatPercentPoints } from '../lib/format';
import { assert, assertEqual, test } from '../test/harness';
import { initialUiState } from '../store/uiSlice';
import { useAppStore } from '../store/useAppStore';
import { initialWorkersState, mergeWorkersAnswer, nextWorkersPollDelay } from '../store/workersSlice';
import type { WorkersViewState } from '../store/workersSlice';
import { RUN_LOGIN_FAILURE_DETAIL, formatClock, isCoolingDown, loginDetailKey, loginPill, windowTone } from './model';
import modelSource from './model.ts?raw';
import { WorkersPane } from './WorkersPane';
import { RUN_LOGIN_FAILURE_DETAIL as VENDORED_RUN_LOGIN_FAILURE_DETAIL } from '../../src-tauri/sidecar/src/vendored/cco/uiTypes';

// ---------------------------------------------------------------------------
// Fixtures
// ---------------------------------------------------------------------------

const FIXTURE = fixture as unknown as CcoWorkersSnapshot;

/** Every time field moved by `delta` ms (null stays null). */
function shifted(snapshot: CcoWorkersSnapshot, delta: number): CcoWorkersSnapshot {
  const t = (v: number | null) => (v === null ? null : v + delta);
  return {
    ...snapshot,
    generatedAt: snapshot.generatedAt + delta,
    accounts: snapshot.accounts.map((a) => ({
      ...a,
      windows: a.windows.map((w) => ({ ...w, resetsAt: t(w.resetsAt) })),
      windowsStaleSince: t(a.windowsStaleSince),
      lastGoodWindowsAt: t(a.lastGoodWindowsAt),
      usageFetchedAt: t(a.usageFetchedAt),
      cooldownUntil: t(a.cooldownUntil),
    })),
  };
}

/** The fixture as if the engine composed it right now. */
function liveSnapshot(): CcoWorkersSnapshot {
  return shifted(JSON.parse(JSON.stringify(FIXTURE)), Date.now() - FIXTURE.generatedAt);
}

function answerOf(patch: Partial<CcoWorkersAnswer> = {}): CcoWorkersAnswer {
  const snapshot = 'snapshot' in patch ? (patch.snapshot ?? null) : liveSnapshot();
  return {
    available: true,
    supported: true,
    snapshot,
    hostKey: 's424242',
    receivedAt: snapshot?.generatedAt ?? null,
    stale: false,
    fromCache: false,
    unavailable: null,
    nextPollAt: Date.now() + 60_000,
    minPollMs: 60_000,
    generation: 1,
    ...patch,
  };
}

function seed(workers: Partial<WorkersViewState>): void {
  useAppStore.setState({
    ui: initialUiState(),
    workers: { ...initialWorkersState(), ...workers },
    // On the server the watch effect never runs; the action must still be harmless.
    startWorkersWatch: () => () => undefined,
  });
}

function render(): string {
  Object.assign(useAppStore.getInitialState(), useAppStore.getState());
  return renderToStaticMarkup(
    <WorkersPane
      descriptor={{ view: 'workers' }}
      tab={{ id: 't1', title: 'workers', descriptor: { view: 'workers' }, closable: true }}
      paneId="pane-1"
      active
    />,
  );
}

const ko = (key: Parameters<typeof translate>[1], params?: Record<string, string | number>) => translate('ko', key, params);

function textOf(html: string): string {
  return html
    .replace(/<[^>]+>/g, ' ')
    .replace(/&amp;/g, '&')
    .replace(/&lt;/g, '<')
    .replace(/&gt;/g, '>')
    .replace(/&quot;/g, '"')
    .replace(/&#x27;/g, "'")
    .replace(/\s+/g, ' ');
}

/** The markup of one account row, by its name. */
function rowOf(html: string, name: string): string {
  const rows = html.split('class="workers-account col"').slice(1);
  return rows.find((r) => r.includes(`<span class="strong ell">${name}</span>`)) ?? '';
}

const accountNamed = (s: CcoWorkersSnapshot, name: string): CcoWorkerAccount => s.accounts.find((a) => a.name === name)!;

// ---------------------------------------------------------------------------
// 1 + 2. The engine's values, each state distinctly; read-only
// ---------------------------------------------------------------------------

test('the workers view renders the fixture snapshot: every account state distinctly', () => {
  const answer = answerOf();
  const snap = answer.snapshot!;
  seed({ answer });
  const html = render();
  const text = textOf(html);
  const now = Date.now();

  assert(text.includes(ko('workers.preview')), 'the read-only line is on the view');
  assert(text.includes(ko('workers.asOf', { time: formatClock(snap.generatedAt, 'ko', now) })), '"as of" the engine time');
  assert(text.includes(ko('workers.section.accounts', { n: 6 })), 'six accounts');
  for (const name of ['main', 'w1', 'w2', 'w3', 'w4', 'cx1']) assert(rowOf(html, name) !== '', `${name} has a row`);

  // login states
  assert(textOf(rowOf(html, 'main')).includes(ko('workers.login.ok')), 'main: signed in');
  const w2 = textOf(rowOf(html, 'w2'));
  assert(w2.includes(ko('workers.login.expired')) && w2.includes(ko('workers.login.runFailed')), 'w2: logged out, the run-failure marker as a sentence');
  assert(!w2.includes('the last run failed on login'), 'the marker itself is not shown in ko');
  const w3 = textOf(rowOf(html, 'w3'));
  assert(w3.includes(ko('workers.login.error')) && w3.includes('usage probe timed out after 5s'), 'w3: check failed with the engine detail');
  assert(textOf(rowOf(html, 'w4')).includes(ko('workers.login.refreshing')), 'w4: not read yet');
  assert(rowOf(html, 'w2').includes('data-login="expired"') && rowOf(html, 'w3').includes('data-login="error"') && rowOf(html, 'w4').includes('data-login="refreshing"'), 'each login state is marked distinctly');

  // cooldown in the future, overage on
  const w1Row = rowOf(html, 'w1');
  const w1 = accountNamed(snap, 'w1');
  assert(w1Row.includes('data-cooldown="true"'), 'w1 is cooling down');
  assert(textOf(w1Row).includes(ko('workers.cooldown', { at: formatClock(w1.cooldownUntil!, 'ko', now), in: formatDuration(w1.cooldownUntil! - now, 'ko') })), 'the cooldown end as local time and what is left');
  assert(textOf(w1Row).includes(ko('workers.overage')) && textOf(w1Row).includes('$17.50 of $50.00'), 'overage on, with the engine spend label');
  assert(!rowOf(html, 'main').includes('data-cooldown') && !textOf(rowOf(html, 'main')).includes(ko('workers.overage')), 'main: neither');
  assert(textOf(w1Row).includes('rate limited (429) on ~/proj dispatch'), 'the last error, as the engine wrote it (home redacted by the engine)');

  // other vendor, shared login, stale windows
  const cx1 = textOf(rowOf(html, 'cx1'));
  assert(cx1.includes('Codex'), 'another vendor shows its CLI name');
  assert(!textOf(rowOf(html, 'w1')).includes('Claude Code'), 'a Claude Code account carries no vendor badge');
  assert(cx1.includes(ko('workers.windowsStale', { time: formatClock(accountNamed(snap, 'cx1').windowsStaleSince!, 'ko', now) })), 'carried-forward windows say when they were read');
  assert(rowOf(html, 'cx1').includes('data-stale="true"'), 'and are marked');
  assert(textOf(rowOf(html, 'main')).includes(ko('workers.sameAccount', { names: 'w2' })), 'main shares a claude.ai login with w2');
  assert(w2.includes(ko('workers.sameAccount', { names: 'main' })), 'and w2 with main');

  // windows: percents as given, severity from the engine, resets as local time
  const main = textOf(rowOf(html, 'main'));
  assert(main.includes(formatPercentPoints(42, 'ko', 1)) && main.includes(formatPercentPoints(97.5, 'ko', 1)), 'percents as given (97.5 stays 97.5)');
  const session = accountNamed(snap, 'main').windows[0]!;
  assert(main.includes(ko('workers.resetsIn', { in: formatDuration(session.resetsAt! - now, 'ko'), at: formatClock(session.resetsAt!, 'ko', now) })), 'resets in … · local time');
  assert(rowOf(html, 'main').includes('data-severity="critical"') && rowOf(html, 'main').includes('data-tone="error"'), 'critical is toned by the engine severity');
  assert(cx1.includes(ko('workers.resetUnknown')), 'a window without a reset time says so');

  // guards and unsupported entries
  assert(text.includes(ko('workers.guards', { overage: ko('workers.guard.block'), frontier: ko('workers.guard.allow') })), 'the billing guards as the host reads them');
  assert(text.includes('old-agent') && text.includes('unknown provider "copilot"'), 'an unsupported registry entry is named');
  assert(!text.includes(ko('workers.stale', { time: formatClock(snap.generatedAt, 'ko', now) })), 'a fresh answer is not marked stale');
});

test('the workers view is read-only: no input, one button (refresh)', () => {
  seed({ answer: answerOf() });
  const html = render();
  assert(!/<input\b/.test(html) && !/<textarea\b/.test(html) && !/<select\b/.test(html), 'no form control');
  const buttons = html.match(/<button\b[^>]*>/g) ?? [];
  assertEqual(buttons.length, 1, 'the refresh only');
  assert(buttons[0]!.includes(`aria-label="${ko('workers.refresh')}"`), 'and it is the refresh');
  const text = textOf(html).toLowerCase();
  for (const word of ['re-login', 'remove', 'login again', '로그인하기', '삭제']) {
    assert(!text.includes(word), `no "${word}" action`);
  }
});

// ---------------------------------------------------------------------------
// 3. No snapshot is not an empty one
// ---------------------------------------------------------------------------

test('an older host, an unknown schema, a host error and no host each say so — and show no account', () => {
  const at = Date.now();
  const cases: Array<[string, string | null, string]> = [
    ['host-too-old', null, ko('workers.reason.hostTooOld')],
    ['schema-unknown', 'cco.workers/2', ko('workers.reason.schemaUnknown')],
    ['host-error', 'worker registry unreadable', ko('workers.reason.hostError')],
    ['no-host', null, ko('workers.reason.noHost')],
  ];
  for (const [reason, detail, sentence] of cases) {
    seed({ answer: answerOf({ snapshot: null, unavailable: { reason, detail, at } }) });
    const html = render();
    const text = textOf(html);
    assert(text.includes(sentence), `${reason}: its sentence`);
    if (detail) assert(text.includes(detail), `${reason}: its detail as text`);
    assert(!html.includes('workers-account'), `${reason}: no account rows`);
    assert(!text.includes(ko('workers.empty')), `${reason}: never "no accounts registered"`);
  }
  assertEqual(translate('en', 'workers.reason.schemaUnknown'), 'The dispatch host is newer or older than this Gilde. Update one of them.', 'the en sentence says newer/older');
});

test('a cco.workers/2 snapshot that reached the view is not read either (the sidecar refuses it)', () => {
  // The sidecar turns an unknown schema into `schema-unknown` and keeps nothing of it
  // (test/cco-team.mjs K12). This pins the view side: an answer carrying the reason and
  // no snapshot names the schema and renders no account — not the one in the host text.
  seed({ answer: answerOf({ snapshot: null, unavailable: { reason: 'schema-unknown', detail: 'cco.workers/2', at: Date.now() } }) });
  const text = textOf(render());
  assert(text.includes('cco.workers/2') && text.includes(ko('workers.reason.schemaUnknown')), 'host newer/older, naming the schema');
  assert(!text.includes('from-a-newer-schema'), 'nothing of that answer');
});

test('empty states: build without the command, sidecar down, unsupported platform', () => {
  seed({ commandMissing: true });
  assert(textOf(render()).includes(ko('workers.commandMissing')), 'an older backend says so');
  seed({ answer: answerOf({ available: false, snapshot: null }) });
  assert(textOf(render()).includes(ko('workers.sidecarDown')), 'sidecar down');
  seed({ answer: answerOf({ supported: false, snapshot: null }) });
  assert(textOf(render()).includes(ko('workers.unsupported')), 'unsupported platform');
});

// ---------------------------------------------------------------------------
// 4. Stale, never blank
// ---------------------------------------------------------------------------

test('a stale snapshot stays on the view, marked with its time and why', () => {
  const answer = answerOf({ stale: true, unavailable: { reason: 'host-gone', detail: null, at: Date.now() } });
  seed({ answer });
  const html = render();
  const text = textOf(html);
  assert(text.includes(ko('workers.stale', { time: formatClock(answer.receivedAt!, 'ko', Date.now()) })), 'stale, with the receive time');
  assert(text.includes(ko('workers.reason.hostGone')), 'and why');
  assert(html.includes('data-stale="true"') && rowOf(html, 'w1') !== '', 'the accounts are still shown');

  const prev = answerOf();
  const merged = mergeWorkersAnswer(prev, answerOf({ available: false, snapshot: null, hostKey: null, receivedAt: null }));
  assert(merged.snapshot === prev.snapshot && merged.stale === true && merged.receivedAt === prev.receivedAt && merged.available === false, 'a sidecar restart keeps the last snapshot, stale, with its time');
  const fresh = answerOf();
  assert(mergeWorkersAnswer(prev, fresh) === fresh, 'a new snapshot replaces it');
  assert(mergeWorkersAnswer(null, answerOf({ snapshot: null })).snapshot === null, 'nothing held, nothing invented');
});

// ---------------------------------------------------------------------------
// 5. Hostile strings
// ---------------------------------------------------------------------------

test('hostile strings from the engine are flattened, capped and escaped', () => {
  const snap = liveSnapshot();
  const nasty = '<script>alert(1)</script>\n\u202Eevil\u0007 ' + 'x'.repeat(5_000);
  snap.accounts[1] = { ...snap.accounts[1]!, name: nasty, lastError: nasty, loginDetail: nasty, login: 'error', planLabel: nasty, providerLabel: nasty, provider: 'codex', windows: [{ kind: 'other', label: nasty, percent: 50, severity: 'normal', resetsAt: null }] };
  seed({ answer: answerOf({ snapshot: snap, unavailable: { reason: 'host-error', detail: nasty, at: Date.now() } }) });
  const html = render();
  assert(!html.includes('<script>'), 'markup is escaped, never rendered');
  assert(!html.includes('\u202E') && !html.includes('\u0007'), 'bidi overrides and controls are removed');
  assert(!html.includes('x'.repeat(201)), 'every engine string is capped for layout');
});

// ---------------------------------------------------------------------------
// 6. Pure rules
// ---------------------------------------------------------------------------

test('model and slice rules: login pills, severity tones, cooldown shown only in the future, poll delay bounds', () => {
  const a = liveSnapshot().accounts[0]!;
  assertEqual(loginPill({ ...a, login: 'ok' }).key, 'workers.login.ok', 'ok');
  assertEqual(loginPill({ ...a, login: 'expired' }).key, 'workers.login.expired', 'expired');
  assertEqual(loginPill({ ...a, login: 'no-plan-limits' }).key, 'workers.login.noPlanLimits', 'no plan limits');
  assertEqual(loginPill({ ...a, login: 'error' }).key, 'workers.login.error', 'error');
  assertEqual(loginPill({ ...a, login: 'refreshing' }).key, 'workers.login.refreshing', 'refreshing');
  assertEqual(loginPill({ ...a, login: 'quantum' as never }).key, null, 'an unknown state is shown raw');
  assertEqual(loginDetailKey('the last run failed on login'), 'workers.login.runFailed', 'the run-failure marker');
  assertEqual(loginDetailKey('something else'), null, 'any other detail is shown as text');
  assertEqual(windowTone('critical'), 'error', 'critical');
  assertEqual(windowTone('warning'), 'warn', 'warning');
  assertEqual(windowTone('normal'), undefined, 'normal is neutral');
  const now = Date.now();
  assert(isCoolingDown({ ...a, cooldownUntil: now + 1_000 }, now), 'a future end is shown');
  assert(!isCoolingDown({ ...a, cooldownUntil: now - 1_000 }, now) && !isCoolingDown({ ...a, cooldownUntil: null }, now), 'a past end or none is not');

  assertEqual(nextWorkersPollDelay(null, now), 60_000, 'no answer: the floor');
  assertEqual(nextWorkersPollDelay(answerOf({ nextPollAt: now + 60_000 }), now), 60_000, 'a fresh answer: the floor');
  assertEqual(nextWorkersPollDelay(answerOf({ nextPollAt: now }), now), 5_000, 'never sooner than 5 s');
  assertEqual(nextWorkersPollDelay(answerOf({ nextPollAt: now + 10 * 60_000 }), now), 60_000, 'never later than the floor');
  assertEqual(nextWorkersPollDelay(answerOf({ nextPollAt: now + 20_000 }), now), 20_000, 'otherwise when the sidecar says');
});

// ---------------------------------------------------------------------------
// 7. CCO 0b7b1b8: servedFromCache, the exported run-failure marker
// ---------------------------------------------------------------------------

test('"cached by the host" sits next to "as of" only when the host answered from its cache', () => {
  const cached = liveSnapshot();
  cached.servedFromCache = true;
  seed({ answer: answerOf({ snapshot: cached }) });
  const html = render();
  const asOf = html.indexOf('data-testid="workers-as-of"');
  const hostCached = html.indexOf('data-testid="workers-host-cached"');
  assert(asOf !== -1 && hostCached > asOf, 'the note follows "as of" in the header');
  assert(textOf(html).includes(ko('workers.hostCached')), 'in the reader\'s language');
  assert(html.includes(`title="${ko('workers.hostCachedHint')}"`), 'with the why as its title');
  assert(/workers-host-cached"[^>]*>/.test(html) && html.includes('class="s12 muted" data-testid="workers-host-cached"'), 'subtle: small and muted, not a banner');

  const fresh = liveSnapshot();
  fresh.servedFromCache = false;
  seed({ answer: answerOf({ snapshot: fresh }) });
  assert(!render().includes('workers-host-cached'), 'false: no note');

  const older = liveSnapshot();
  delete older.servedFromCache;
  seed({ answer: answerOf({ snapshot: older, fromCache: true }) });
  assert(!render().includes('workers-host-cached'), 'absent (an older host), even when Gilde\'s own floor served it: no note');
  assertEqual(FIXTURE.servedFromCache, false, 'the fixture carries the field as the host sends it');
});

test('the run-failure marker is the vendored uiTypes constant, imported — not a copied literal', () => {
  assertEqual(RUN_LOGIN_FAILURE_DETAIL, VENDORED_RUN_LOGIN_FAILURE_DETAIL, 'one constant');
  assertEqual(accountNamed(liveSnapshot(), 'w2').loginDetail, RUN_LOGIN_FAILURE_DETAIL, 'the fixture\'s w2 carries it');
  assertEqual(loginDetailKey(VENDORED_RUN_LOGIN_FAILURE_DETAIL), 'workers.login.runFailed', 'recognized and localized');
  assert(modelSource.includes("from '../../src-tauri/sidecar/src/vendored/cco/uiTypes'"), 'model.ts imports it from the vendored copy');
  assert(!modelSource.includes(`'${VENDORED_RUN_LOGIN_FAILURE_DETAIL}'`) && !modelSource.includes(`"${VENDORED_RUN_LOGIN_FAILURE_DETAIL}"`), 'model.ts holds no copy of the literal');
  assertEqual(translate('en', 'workers.hostCached'), 'cached by the host', 'the en note');
});
