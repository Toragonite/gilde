/**
 * The health surface (frontend half): what the user sees of Rust's
 * `HealthSnapshot` — the status-bar indicator, the popover, the action toasts,
 * the alert actions (Copy command, Clean up) and the Team view markers.
 *
 * Rust decides every alert; these tests pin only the rendering rules:
 *   1. the indicator: dot + "N running · X GB", yellow/red with the count of
 *      non-dismissed alerts; Korean and English;
 *   2. the popover: overall line, teams → members with state and GB, hosts,
 *      alert cards; no pid in the text; "Process details" only with lanes on;
 *   3. toasts: only `action` alerts, once per (id, raisedAt), removed when the
 *      alert ends or is dismissed;
 *   4. Copy command writes exactly the command; Clean up opens the kill sheet
 *      with exactly the confirmed, unprotected leftovers and asks Rust for
 *      `onlyHealthOrphans`;
 *   5. Team view: member memory figure, leftover marker, header alert chip.
 */

import type { ReactElement } from 'react';
import { renderToStaticMarkup } from 'react-dom/server';

import fixture from '../../src-tauri/tests/fixtures/cco/team-snapshot.json';
import { translate } from '../i18n';
import { isBudgetToastKey } from '../hooks/useEventBridge';
import { cmd } from '../lib/commands';
import type {
  CcoTeamListResult,
  CcoTeamView,
  HealthAgent,
  HealthAlert,
  HealthSnapshot,
  KillOpts,
  KillTarget,
  ProcRow,
} from '../lib/ipc';
import { initialHealthState } from '../store/healthSlice';
import { dryRunSets } from '../store/procSlice';
import { KillDryRunTargets, killRunDisabled } from '../monitor/KillConfirmSheet';
import { initialTeamState } from '../store/teamSlice';
import { initialUiState } from '../store/uiSlice';
import { useAppStore } from '../store/useAppStore';
import { TeamPane } from '../team/TeamPane';
import { TeamSidebar } from '../team/TeamSidebar';
import teamCss from '../team/team.css?raw';
import { assert, assertDeepEqual, assertEqual, test } from '../test/harness';
import { laneTest, previewTest } from '../test/lanes';
import { AppShell } from './AppShell';
import { HealthPopover } from './HealthPopover';
import { ToastHost } from './ToastHost';
import {
  activeAlerts,
  alertText,
  cleanUpAgents,
  formatClock,
  formatGb,
  indicatorLevel,
  indicatorText,
  overallReading,
  snoozedAlerts,
} from './healthText';
import { HealthIndicator } from './StatusBar';

const GIB = 1024 * 1024 * 1024;
const ko = (key: Parameters<typeof translate>[1], params?: Record<string, string | number>) =>
  translate('ko', key, params);

// ---------------------------------------------------------------------------
// Fixtures
// ---------------------------------------------------------------------------

function procRow(pid: number, tags: ProcRow['tags'] = []): ProcRow {
  return {
    pid,
    ppid: 1,
    pgid: pid,
    name: 'claude',
    argvDisplay: `/usr/local/bin/claude -p task-${pid}`,
    rssBytes: GIB,
    cpuPct: 0,
    elapsedSecs: 600,
    startTime: 1_700_000_000_000 + pid,
    cwd: null,
    userOwned: true,
    group: 'agentUnmanaged',
    tags,
    owner: null,
    spawnId: null,
  } as ProcRow;
}

function agent(pid: number, patch: Partial<HealthAgent> = {}): HealthAgent {
  return {
    pid,
    pgid: pid,
    ppid: 1,
    agent: 'claude',
    rssBytes: 1.2 * GIB,
    cpuPct: 0,
    elapsedSecs: 600,
    startTime: 1_700_000_000_000 + pid,
    attribution: { kind: 'teamMember', teamId: 'tmabcdefghi1', team: 'demo', member: 'alice', worktree: '/w/alice' },
    orphan: false,
    pids: [pid],
    row: procRow(pid),
    ...patch,
  };
}

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

function snapshotOf(patch: Partial<HealthSnapshot> = {}): HealthSnapshot {
  return {
    at: 5000,
    overall: 'ok',
    agents: [agent(501), agent(502, { attribution: { kind: 'other' } }), agent(503)],
    hosts: [{ pid: 900, rssBytes: 100 * 1024 * 1024, elapsedSecs: 60, sessionPid: 800, leadAlive: true, teamIds: ['tmabcdefghi1'] }],
    orphans: [],
    teams: [
      {
        teamId: 'tmabcdefghi1',
        team: 'demo',
        status: 'active',
        hostAlive: true,
        rssBytes: 2.4 * GIB,
        members: [
          { name: 'alice', state: 'running', rssBytes: 1.2 * GIB, pids: [501, 5011] },
          { name: 'bob', state: 'idle', rssBytes: 0, pids: [] },
        ],
      },
    ],
    memory: { pressure: 'normal', agentsRssBytes: 2.4 * GIB, physicalRamBytes: 32 * GIB },
    alerts: [],
    teamsKnown: true,
    ...patch,
  };
}

function reset(): void {
  useAppStore.setState({ health: initialHealthState(), toasts: [], ui: initialUiState() });
}

function renderStatic(node: ReactElement): string {
  Object.assign(useAppStore.getInitialState(), useAppStore.getState());
  return renderToStaticMarkup(node);
}

/** Text of the markup without tags and attributes (tooltips are not "main text"). */
function textOf(html: string): string {
  return html
    .replace(/<[^>]+>/g, ' ')
    .replace(/&amp;/g, '&')
    .replace(/&#x27;/g, "'")
    .replace(/&quot;/g, '"')
    .replace(/\s+/g, ' ');
}

// ---------------------------------------------------------------------------
// 1. text and the indicator
// ---------------------------------------------------------------------------

test('health: GB is base 1024³ — <0.1 below a tenth, one decimal up to 9.9, whole GB from 10, in both locales', () => {
  assertEqual(formatGb(2.4 * GIB, 'ko'), '2.4 GB', 'ko');
  assertEqual(formatGb(6.25 * GIB, 'en'), '6.3 GB', 'en rounds to one decimal');
  assertEqual(formatGb(Number.NaN, 'ko'), '<0.1 GB', 'a broken number never prints NaN');
  assertEqual(formatGb(-5 * GIB, 'en'), '<0.1 GB', 'a negative number is nothing');
  assertEqual(formatGb(Number.POSITIVE_INFINITY, 'en'), '<0.1 GB', 'infinity is not a figure');
  // The live case: a 1.3 MB leftover process read "0.0 GB".
  assertEqual(formatGb(1.3 * 1024 * 1024, 'ko'), '<0.1 GB', 'a 1.3 MB process');
  const cases: [number, string][] = [
    [0, '<0.1 GB'],
    [1, '<0.1 GB'],
    [0.049 * GIB, '<0.1 GB'],
    [0.05 * GIB, '<0.1 GB'],
    [0.1 * GIB, '0.1 GB'],
    [9.94 * GIB, '9.9 GB'],
    [9.96 * GIB, '10 GB'],
    [10 * GIB, '10 GB'],
    [123.4 * GIB, '123 GB'],
  ];
  for (const locale of ['ko', 'en'] as const) {
    for (const [bytes, want] of cases) {
      assertEqual(formatGb(bytes, locale), want, `${locale}: ${bytes / GIB} GB`);
    }
  }
  assertEqual(formatGb(1234.4 * GIB, 'en'), '1,234 GB', 'whole GB keep the grouping separator');
});

test('health: the indicator says how many agents run and their memory, then the alert count', () => {
  const none = indicatorText(null, 'ko');
  assertEqual(none.level, 'ok', 'nothing read yet is not an alarm');
  assertEqual(none.summary, ko('health.indicator.none'), 'no agents');
  assertEqual(none.badge, null, 'no badge');

  const ok = indicatorText(snapshotOf(), 'ko');
  assertEqual(ok.summary, '에이전트 3개 실행 중 · 2.4 GB', 'ko summary');
  assertEqual(indicatorText(snapshotOf(), 'en').summary, 'Agents: 3 running · 2.4 GB', 'en summary');
  assertEqual(ok.badge, null, 'ok has no badge');

  const attention = indicatorText(
    snapshotOf({ alerts: [alertOf({ id: 'mem-low', severity: 'attention' })], overall: 'attention' }),
    'ko',
  );
  assertEqual(attention.level, 'attention', 'yellow');
  assertEqual(attention.badge, '주의 1', 'ko attention count');

  const action = snapshotOf({
    alerts: [alertOf(), alertOf({ id: 'mem-low', severity: 'attention' }), alertOf({ id: 'x', dismissed: true })],
  });
  assertEqual(indicatorLevel(action), 'action', 'red');
  assertEqual(indicatorText(action, 'ko').badge, '조치 필요 2', 'dismissed alerts are not counted');
  assertEqual(indicatorText(action, 'en').badge, 'Action needed 2', 'en action count');
  assertEqual(activeAlerts(action).length, 2, 'two active');
});

test('health: every alert kind renders plain sentences in both locales, without pid or RSS', () => {
  const kinds: HealthAlert[] = [
    alertOf(),
    alertOf({ id: 'orphans:gilde', bodyKey: 'health.alert.orphans.bodyGilde', params: { count: 1, rssBytes: GIB } }),
    alertOf({
      id: 'mem-agent:501-1',
      kind: 'memAgent',
      titleKey: 'health.alert.memAgent.title',
      bodyKey: 'health.alert.memAgent.bodyMember',
      params: { agent: 'claude', team: 'gilde-live', member: 'alpha', rssBytes: 6.2 * GIB, command: 'cco team_member alpha --stop', pid: 501 },
      actions: [{ kind: 'copyCommand', command: 'cco team_member alpha --stop' }],
    }),
    alertOf({ kind: 'memAgent', titleKey: 'health.alert.memAgent.title', bodyKey: 'health.alert.memAgent.bodySession', params: { agent: 'codex', rssBytes: 5 * GIB } }),
    alertOf({ kind: 'memAgent', titleKey: 'health.alert.memAgent.title', bodyKey: 'health.alert.memAgent.bodyOther', params: { agent: 'codex', rssBytes: 5 * GIB } }),
    alertOf({
      id: 'mem-low',
      kind: 'memLow',
      titleKey: 'health.alert.memLow.title',
      bodyKey: 'health.alert.memLow.body',
      params: { rssBytes: 9 * GIB, top: [{ label: 'alpha (gilde-live)', rssBytes: 6.2 * GIB }, { label: 'codex', rssBytes: GIB }] },
    }),
    alertOf({ kind: 'hostLost', titleKey: 'health.alert.hostLost.title', bodyKey: 'health.alert.hostLost.body', params: { team: 'x', command: 'cco team_resume --team x' } }),
    alertOf({ kind: 'turnStuck', titleKey: 'health.alert.turnStuck.title', bodyKey: 'health.alert.turnStuck.body', params: { team: 'x', member: 'alpha', minutes: 47, command: 'cco team_member alpha --interrupt' } }),
    alertOf({ kind: 'hostGone', titleKey: 'health.alert.hostGone.title', bodyKey: 'health.alert.hostGone.body', params: { team: 'x' } }),
    alertOf({ kind: 'hostNoLead', titleKey: 'health.alert.hostNoLead.title', bodyKey: 'health.alert.hostNoLead.body', params: { minutes: 12 } }),
  ];
  for (const locale of ['ko', 'en'] as const) {
    for (const a of kinds) {
      const text = alertText(a, locale);
      for (const part of [text.title, text.body]) {
        assert(part.length > 0, `${a.bodyKey} renders`);
        assert(!/\{\w+\}/.test(part), `${a.bodyKey} (${locale}) has no unfilled placeholder: ${part}`);
        assert(!/\bpid\b|\bpgid\b|\bRSS\b|501/.test(part), `${a.bodyKey} (${locale}) names no pid/RSS: ${part}`);
      }
    }
  }
  const en = alertText(kinds[2]!, 'en').body;
  assertEqual(
    en,
    'Member alpha of team gilde-live uses 6.2 GB (its tools included). Stop or restart it from your lead session: cco team_member alpha --stop — or from the Team view in VS Code.',
    'the spec sentence, verbatim',
  );
  assertEqual(
    alertText(kinds[0]!, 'en').body,
    '2 agent processes are still running after their team stopped (team demo, members alice, bob). They use 2.4 GB.',
    'orphans in English',
  );
  assert(alertText(kinds[5]!, 'ko').body.includes('alpha (gilde-live) 6.2 GB, codex 1.0 GB'), 'top 3 consumers by name');
  assert(alertText(kinds[0]!, 'ko').body.includes('멤버'), 'Korean says 멤버');
  assert(!alertText(kinds[0]!, 'ko').body.includes('구성원'), 'never 구성원');
  const unknown = alertText(alertOf({ titleKey: 'health.alert.future.title', bodyKey: 'health.alert.future.body' }), 'ko');
  assertEqual(unknown.title, ko('error.unknown'), 'a key this build does not know degrades safely');
});

// ---------------------------------------------------------------------------
// 2. status bar and popover
// ---------------------------------------------------------------------------

previewTest('health: the preview status bar carries one health indicator next to the connection', () => {
  reset();
  useAppStore.setState({
    health: { ...initialHealthState(), snapshot: snapshotOf({ alerts: [alertOf()] }) },
    team: { ...initialTeamState() },
  });
  try {
    const html = renderStatic(<AppShell />);
    const footer = html.match(/<footer class="statusbar"[\s\S]*?<\/footer>/)?.[0] ?? '';
    assertEqual((footer.match(/data-testid="health"/g) ?? []).length, 1, 'one indicator');
    assert(footer.indexOf('data-testid="health"') < footer.indexOf('data-testid="connection"'), 'next to the connection');
    assert(footer.includes('data-level="action"'), 'red');
    assert(footer.includes('에이전트 3개 실행 중 · 2.4 GB'), 'count and GB');
    assert(footer.includes('조치 필요 1'), 'alert count');
    assert(!footer.includes('메모리'), 'the preview footer still names no lane counters');
    assert(!html.includes('data-testid="health-popover"'), 'the popover is closed until clicked');
  } finally {
    reset();
  }
});

previewTest('health: the popover lists teams, members, hosts and the alerts — no pid in the text', () => {
  reset();
  useAppStore.setState({
    health: {
      ...initialHealthState(),
      popoverOpen: true,
      snapshot: snapshotOf({
        alerts: [alertOf(), alertOf({ id: 'gone', dismissed: true, titleKey: 'health.alert.hostGone.title' })],
        hosts: [
          { pid: 900, rssBytes: 1, elapsedSecs: 1, sessionPid: 800, leadAlive: true, teamIds: [] },
          { pid: 901, rssBytes: 1, elapsedSecs: 1, sessionPid: null, leadAlive: false, teamIds: [] },
        ],
      }),
    },
  });
  try {
    const html = renderStatic(<HealthPopover />);
    const text = textOf(html);
    assert(text.includes(ko('health.overall.action')), 'overall line');
    assert(text.includes('demo'), 'the team');
    assert(text.includes('alice') && text.includes(ko('health.member.running')) && text.includes('1.2 GB'), 'alice runs, 1.2 GB');
    assert(text.includes('bob') && text.includes(ko('health.member.idle')), 'bob idles');
    assert(text.includes(ko('health.hosts', { n: 2 })) && text.includes(ko('health.hosts.attention')), 'hosts line, attention');
    assert(text.includes(ko('health.alert.orphans.title')), 'the active alert card');
    const cards = html.match(/class="health-alert"[^>]*data-alert-id="[^"]*"/g) ?? [];
    assertEqual(cards.length, 1, 'one active card');
    assert(cards[0]!.includes('orphans:tmabcdefghi1'), 'the active alert is the card');
    // The dismissed one is not an active card: it is listed under "snoozed" (below).
    assert(html.includes('data-testid="health-snoozed"') && text.includes(ko('health.alert.hostGone.title')), 'the dismissed one, snoozed');
    assert(html.includes(`>${ko('health.action.cleanUp')}<`), 'Clean up button');
    assert(text.includes(ko('health.others', { n: 1, gb: '1.2 GB' })), 'agents outside a team are counted');
    assert(!/\b50[123]\b|\b90[01]\b|\b5011\b/.test(text), `no pid in the main text: ${text}`);
    assert(html.includes('title="pid 501 5011"'), 'pids only in the tooltip');
    assert(!html.includes('data-testid="health-process-details"'), 'no Process details in the preview');
  } finally {
    reset();
  }
});

laneTest('health: "Process details" appears only with the experimental lanes on', () => {
  reset();
  useAppStore.setState({ health: { ...initialHealthState(), popoverOpen: true, snapshot: snapshotOf() } });
  try {
    const html = renderStatic(<HealthPopover />);
    assert(html.includes('data-testid="health-process-details"'), 'the monitor link');
    assert(textOf(html).includes(ko('health.overall.ok')), 'everything is running well');
  } finally {
    reset();
  }
});

// ---------------------------------------------------------------------------
// 2b. a dismissal snoozes — it never reads as "all well"
// ---------------------------------------------------------------------------

/** Today at 10:12 local time: "back at 10:12" in both locales, whatever the time zone. */
function todayAt1012(): number {
  const d = new Date();
  d.setHours(10, 12, 0, 0);
  return d.getTime();
}

const en = (key: Parameters<typeof translate>[1], params?: Record<string, string | number>) =>
  translate('en', key, params);

/** The leftover alert of the billing-export report, after its toast was closed. */
function snoozedLeftover(patch: Partial<HealthAlert> = {}): HealthAlert {
  return alertOf({
    params: { count: 2, team: 'billing-export', members: ['exporter', 'reviewer'], rssBytes: 1.3 * GIB },
    dismissed: true,
    dismissedUntil: todayAt1012(),
    ...patch,
  });
}

function headlineOf(locale: 'ko' | 'en', alerts: HealthAlert[]): { html: string; line: string } {
  useAppStore.setState({
    ui: { ...initialUiState(), locale },
    health: { ...initialHealthState(), popoverOpen: true, snapshot: snapshotOf({ alerts }) },
  });
  const html = renderStatic(<HealthPopover />);
  // The first `class="health-overall"` row is the headline (the hosts row is `health-overall s12`).
  const line = html.match(/<div class="health-overall"[\s\S]*?<\/div>/)?.[0] ?? '';
  return { html, line };
}

previewTest('health: the popover headline — "all well" only with no alert at all; snoozed-only is neutral, never green', () => {
  reset();
  try {
    for (const locale of ['ko', 'en'] as const) {
      const t = (key: Parameters<typeof translate>[1], params?: Record<string, string | number>) =>
        translate(locale, key, params);

      // none: all well, green.
      let { line } = headlineOf(locale, []);
      assert(textOf(line).includes(t('health.overall.ok')), `${locale} none: all well`);
      assert(line.includes('data-status="ok"'), `${locale} none: green`);

      // active: action, red.
      ({ line } = headlineOf(locale, [alertOf()]));
      assert(textOf(line).includes(t('health.overall.action')), `${locale} active: action`);
      assert(line.includes('data-status="error"'), `${locale} active: red`);

      // only dismissed: the bug — it said "Everything is running well." with the leftovers still running.
      const { html, line: snoozedLine } = headlineOf(locale, [snoozedLeftover()]);
      const text = textOf(snoozedLine);
      assert(!textOf(html).includes(t('health.overall.ok')), `${locale} only dismissed: never "all well" (${text})`);
      assert(!snoozedLine.includes('data-status="ok"'), `${locale} only dismissed: never the green dot`);
      assert(snoozedLine.includes('data-status="idle"') && snoozedLine.includes('data-overall="snoozed"'), `${locale}: grey, snoozed`);
      assert(text.includes(t('health.overall.snoozedOne', { n: 1, time: '10:12' })), `${locale}: "${text}"`);

      // mixed: the active alert decides the line.
      ({ line } = headlineOf(locale, [snoozedLeftover({ id: 'gone' }), alertOf({ id: 'mem-low', severity: 'attention' })]));
      assert(textOf(line).includes(t('health.overall.attention')), `${locale} mixed: the active one decides`);
      assert(line.includes('data-status="stalled"'), `${locale} mixed: amber`);
    }
    // The exact words of the report's case.
    assertEqual(
      overallReading(snapshotOf({ alerts: [snoozedLeftover()] }), 'en').text,
      'Quiet for now — 1 problem snoozed until 10:12.',
      'en',
    );
    assertEqual(
      overallReading(snapshotOf({ alerts: [snoozedLeftover()] }), 'ko').text,
      '알림을 끈 문제가 1건 있습니다 — 10:12에 다시 알립니다.',
      'ko',
    );
    // Several snoozed: the earliest comeback; an older backend without the time: no time.
    const later = todayAt1012() + 5 * 60_000;
    assertEqual(
      overallReading(snapshotOf({ alerts: [snoozedLeftover({ id: 'b', dismissedUntil: later }), snoozedLeftover({ id: 'a' })] }), 'en').text,
      en('health.overall.snoozedMany', { n: 2, time: '10:12' }),
      'two: the first back',
    );
    assertEqual(
      overallReading(snapshotOf({ alerts: [snoozedLeftover({ dismissedUntil: undefined })] }), 'ko').text,
      ko('health.overall.snoozedNoTime', { n: 1 }),
      'no time known',
    );
    assertEqual(overallReading(null, 'en').kind, 'unknown', 'nothing read yet');
    assertEqual(overallReading(null, 'en').dot, 'idle', 'and not green');
  } finally {
    reset();
  }
});

previewTest('health: a snoozed alert stays in the popover — muted, below the active ones, with its back-at time and its actions', () => {
  reset();
  try {
    for (const locale of ['ko', 'en'] as const) {
      const t = (key: Parameters<typeof translate>[1], params?: Record<string, string | number>) =>
        translate(locale, key, params);
      const { html } = headlineOf(locale, [
        alertOf({ id: 'mem-low', severity: 'attention', kind: 'memLow', titleKey: 'health.alert.memLow.title', bodyKey: 'health.alert.memLow.body', params: { rssBytes: 9 * GIB, top: [] }, actions: [] }),
        snoozedLeftover({ actions: [{ kind: 'cleanUp', pids: [501, 502] }, { kind: 'copyCommand', command: 'kill 501' }, { kind: 'details' }] }),
      ]);
      const section = html.match(/<section class="health-snoozed"[\s\S]*?<\/section>/)?.[0] ?? '';
      assert(section !== '', `${locale}: the snoozed section`);
      assert(html.indexOf('class="health-alert"') < html.indexOf('class="health-snoozed"'), `${locale}: below the active ones`);
      const text = textOf(section);
      assert(text.includes(t('health.snoozed.title')), `${locale}: its heading`);
      const words = alertText(snoozedLeftover(), locale);
      assert(text.includes(words.title) && text.includes(words.body), `${locale}: title and body`);
      assert(section.includes('class="ell health-snoozed-body"'), `${locale}: the body on one line`);
      assert(text.includes(t('health.snoozed.backAt', { time: '10:12' })), `${locale}: back at 10:12 (${text})`);
      assert(section.includes(`>${t('health.action.cleanUp')}<`) && section.includes('data-action="cleanUp"'), `${locale}: Clean up`);
      assert(section.includes(`>${t('health.action.copyCommand')}<`), `${locale}: Copy command`);
      assert(!section.includes('data-action="details"'), `${locale}: no "Details" inside the popover, as for an active card`);
      assert(!section.includes('health-close'), `${locale}: no close and no unsnooze`);
      assert(!/\b50[12]\b/.test(text), `${locale}: no pid in the text`);
    }
    assertEqual(formatClock(todayAt1012(), 'ko'), '10:12', 'HH:MM today');
    const tomorrow = todayAt1012() + 24 * 3600_000;
    assert(formatClock(tomorrow, 'en') !== '10:12' && formatClock(tomorrow, 'en').includes('10:12'), 'with the date when not today');
    assertDeepEqual(snoozedAlerts(snapshotOf({ alerts: [alertOf(), snoozedLeftover({ id: 'z' })] })).map((a) => a.id), ['z'], 'only the dismissed');
  } finally {
    reset();
  }
});

test('health: the indicator keeps a muted "snoozed" trace when the only alerts are dismissed', () => {
  reset();
  try {
    const only = snapshotOf({ alerts: [snoozedLeftover()] });
    const k = indicatorText(only, 'ko');
    assertEqual(k.level, 'ok', 'no alarm level');
    assertEqual(k.badge, null, 'no red or amber badge');
    assertEqual(k.snoozed, '· 알림 끔 1', 'ko suffix');
    assertEqual(k.dot, 'idle', 'a grey dot, not green');
    assertEqual(indicatorText(only, 'en').snoozed, '· 1 snoozed', 'en suffix');

    const mixed = indicatorText(snapshotOf({ alerts: [alertOf(), snoozedLeftover({ id: 'z' })] }), 'en');
    assertEqual(mixed.badge, 'Action needed 1', 'active alerts keep today\'s badge');
    assertEqual(mixed.snoozed, null, 'and no suffix');
    assertEqual(mixed.dot, 'error', 'red');
    const none = indicatorText(snapshotOf(), 'en');
    assertDeepEqual([none.snoozed, none.dot], [null, 'ok'], 'nothing at all: green, no suffix');

    for (const locale of ['ko', 'en'] as const) {
      useAppStore.setState({ ui: { ...initialUiState(), locale }, health: { ...initialHealthState(), snapshot: only } });
      const html = renderStatic(<HealthIndicator />);
      assert(html.includes('data-testid="health-snoozed-count"'), `${locale}: the suffix is drawn`);
      assert(textOf(html).includes(translate(locale, 'health.indicator.snoozed', { n: 1 })), `${locale}: in words`);
      assert(!html.includes('health-badge'), `${locale}: no badge`);
      assert(html.includes('data-status="idle"'), `${locale}: grey dot`);
    }
  } finally {
    reset();
  }
});

// ---------------------------------------------------------------------------
// 3. toasts
// ---------------------------------------------------------------------------

test('health: while the popover is open its alerts\' toasts are hidden — not dismissed — and come back when it closes', () => {
  reset();
  try {
    const s = useAppStore.getState;
    s().applyHealth(snapshotOf({ at: 2, alerts: [alertOf()] }));
    s().pushToast({ level: 'info', key: 'health.copied', params: {}, detail: null, actionId: null, undoToken: null, ttlMs: 0 });
    const before = s().toasts;
    assertEqual(before.length, 2, 'the alert toast and an ordinary one');

    // Closed popover: both drawn.
    let html = renderStatic(<ToastHost />);
    assert(textOf(html).includes(ko('health.alert.orphans.title')), 'the alert toast is drawn');
    assert(textOf(html).includes(ko('health.copied')), 'the other toast is drawn');

    // Open: the alert toast is hidden; the other is not.
    s().setHealthPopover(true);
    html = renderStatic(<ToastHost />);
    assert(!textOf(html).includes(ko('health.alert.orphans.title')), 'hidden while the popover shows the alert');
    assert(!html.includes(`>${ko('health.action.cleanUp')}<`), 'its buttons too');
    assert(textOf(html).includes(ko('health.copied')), 'other toasts are unaffected');
    assert(s().toasts === before, 'the store\'s toast list did not change');
    assertEqual(s().health.snapshot?.alerts[0]?.dismissed, false, 'and the alert is not dismissed');
    // The whole stack goes when only a hidden toast is left.
    s().dismissToast(before[1]!.id);
    assertEqual(renderStatic(<ToastHost />), '', 'nothing drawn');
    assertEqual(s().toasts.length, 1, 'the hidden toast is still held');

    // Closed again: drawn again, the same toast.
    s().setHealthPopover(false);
    html = renderStatic(<ToastHost />);
    assert(textOf(html).includes(ko('health.alert.orphans.title')), 'drawn again after the popover closes');
    assertEqual(s().toasts[0]!.id, before[0]!.id, 'the same toast');
  } finally {
    reset();
  }
});

test('health: only action alerts toast, once per (id, raisedAt); they go when the alert ends', () => {
  reset();
  try {
    const s = useAppStore.getState;
    s().applyHealth(snapshotOf({ at: 1, alerts: [alertOf({ id: 'mem-low', severity: 'attention' })] }));
    assertEqual(s().toasts.length, 0, 'attention never toasts');

    s().applyHealth(snapshotOf({ at: 2, alerts: [alertOf()] }));
    assertEqual(s().toasts.length, 1, 'action toasts');
    const first = s().toasts[0]!;
    assertEqual(first.healthAlertId, 'orphans:tmabcdefghi1', 'the toast is the alert');
    assertEqual(first.ttlMs, 0, 'it stays until dismissed or acted on');
    assertDeepEqual(first.actions?.map((a) => a.kind), ['cleanUp', 'details'], 'with its actions');

    s().applyHealth(snapshotOf({ at: 3, alerts: [alertOf({ params: { count: 3, team: 'demo', members: [], rssBytes: 1 } })] }));
    assertEqual(s().toasts.length, 1, 'updated in place: no second toast');
    assertEqual(s().toasts[0]!.id, first.id, 'the same toast');

    s().applyHealth(snapshotOf({ at: 4, alerts: [alertOf({ raisedAt: 9000 })] }));
    assertEqual(s().toasts.length, 1, 're-raised: one toast');
    assert(s().toasts[0]!.id !== first.id, 'a fresh toast for the new raise');

    s().applyHealth(snapshotOf({ at: 5, alerts: [alertOf({ id: 'mem-low', severity: 'attention' })] }));
    assertEqual(s().toasts.length, 0, 'the alert ended: its toast goes');

    s().applyHealth(snapshotOf({ at: 6, alerts: [alertOf({ id: 'a2', raisedAt: 7 })] }));
    assertEqual(s().toasts.length, 1, 'another action alert');
    s().applyHealth(snapshotOf({ at: 7, alerts: [alertOf({ id: 'a2', raisedAt: 7, dismissed: true })] }));
    assertEqual(s().toasts.length, 0, 'dismissed elsewhere: its toast goes');

    s().applyHealth(snapshotOf({ at: 3, alerts: [alertOf({ id: 'old', raisedAt: 1 })] }));
    assertEqual(s().toasts.length, 0, 'an older snapshot is ignored');

    const warned = s().toasts.length;
    assert(warned === 0 && isBudgetToastKey('toast.budget.soft') && !isBudgetToastKey('toast.copied'), 'budget toast keys are recognised');
  } finally {
    reset();
  }
});

test('health: closing an alert toast dismisses the alert in Rust and applies the answer', async () => {
  reset();
  const original = cmd.healthDismiss;
  const asked: string[] = [];
  try {
    const s = useAppStore.getState;
    s().applyHealth(snapshotOf({ at: 2, alerts: [alertOf()] }));
    assertEqual(s().toasts.length, 1, 'toasted');
    cmd.healthDismiss = async (id: string) => {
      asked.push(id);
      return snapshotOf({ at: 2, alerts: [alertOf({ dismissed: true })] });
    };
    await s().dismissHealthAlert('orphans:tmabcdefghi1');
    assertDeepEqual(asked, ['orphans:tmabcdefghi1'], 'health_dismiss with the alert id');
    assertEqual(s().toasts.length, 0, 'the toast is gone');
    assertEqual(s().health.snapshot?.alerts[0]?.dismissed, true, 'the answer is applied');
    assertEqual(indicatorLevel(s().health.snapshot), 'ok', 'a dismissed alert no longer colours the indicator');
  } finally {
    cmd.healthDismiss = original;
    reset();
  }
});

test('health: an action toast renders the alert sentence and its buttons', () => {
  reset();
  try {
    useAppStore.getState().applyHealth(
      snapshotOf({
        at: 2,
        alerts: [
          alertOf({
            id: 'host-lost:tm1',
            kind: 'hostLost',
            titleKey: 'health.alert.hostLost.title',
            bodyKey: 'health.alert.hostLost.body',
            params: { team: 'x', command: 'cco team_resume --team x' },
            actions: [{ kind: 'copyCommand', command: 'cco team_resume --team x' }],
          }),
        ],
      }),
    );
    const html = renderStatic(<ToastHost />);
    const text = textOf(html);
    assert(text.includes(ko('health.alert.hostLost.title')), 'the title');
    assert(text.includes('cco team_resume --team x'), 'the command in the body');
    assert(html.includes(`>${ko('health.action.copyCommand')}<`), 'the Copy command button');
  } finally {
    reset();
  }
});

// ---------------------------------------------------------------------------
// 4. actions
// ---------------------------------------------------------------------------

test('health: Copy command writes exactly the command', async () => {
  reset();
  const nav = globalThis.navigator as unknown as { clipboard?: unknown };
  const before = nav.clipboard;
  const written: string[] = [];
  nav.clipboard = { writeText: async (text: string) => void written.push(text) };
  try {
    const a = alertOf({
      kind: 'turnStuck',
      params: { command: 'cco team_member alpha --interrupt' },
      actions: [{ kind: 'copyCommand', command: 'cco team_member alpha --interrupt' }],
    });
    await useAppStore.getState().runHealthAction(a, a.actions[0]!);
    assertDeepEqual(written, ['cco team_member alpha --interrupt'], 'the exact command');
    const toasts = useAppStore.getState().toasts;
    const toast = toasts[toasts.length - 1];
    assertEqual(toast?.key, 'health.copied', 'a short confirmation');
  } finally {
    nav.clipboard = before;
    reset();
  }
});

test('health: Clean up opens the kill sheet with exactly the leftovers and asks Rust for onlyHealthOrphans', async () => {
  reset();
  const original = cmd.procmonKill;
  const calls: { targets: KillTarget[]; opts: KillOpts }[] = [];
  cmd.procmonKill = async (targets, opts) => {
    calls.push({ targets, opts });
    return targets.map((target) => ({
      target,
      report: {
        spawnId: null,
        requested: [],
        excluded: [],
        survivors: [],
        refusedReason: null,
        dryRun: opts.dryRun,
        escalated: [],
        fence: 'unfenced',
        at: 0,
      },
    })) as never;
  };
  try {
    const snapshot = snapshotOf({
      agents: [
        agent(501, { orphan: true }),
        agent(502, { orphan: true }),
        agent(503),
        agent(504, { orphan: true, row: procRow(504, ['protected']) }),
      ],
      orphans: [501, 502, 504],
      alerts: [alertOf({ actions: [{ kind: 'cleanUp', pids: [501, 502, 503, 504] }] })],
    });
    assertDeepEqual(
      cleanUpAgents(snapshot, [501, 502, 503, 504]).map((a) => a.pid),
      [501, 502],
      'only confirmed, unprotected leftovers',
    );
    const s = useAppStore.getState;
    s().applyHealth(snapshot);
    const a = s().health.snapshot!.alerts[0]!;
    await s().runHealthAction(a, a.actions[0]!);
    const kill = s().proc.kill;
    assert(kill.open, 'the confirm sheet is open');
    assertDeepEqual(kill.rows.map((r) => r.pid), [501, 502], 'exactly the orphan pids');
    assertEqual(kill.onlyHealthOrphans, true, 'restricted to leftovers');
    await new Promise((r) => setTimeout(r, 0));
    assertEqual(calls.length, 1, 'a bulk sheet opens on its dry run');
    assertDeepEqual(calls[0]!.targets, [{ target: 'pid', pid: 501 }, { target: 'pid', pid: 502 }], 'the targets');
    assertEqual(calls[0]!.opts.onlyHealthOrphans, true, 'onlyHealthOrphans on the dry run');
    assertEqual(calls[0]!.opts.dryRun, true, 'dry run first');

    // The sheet is mounted once, globally — reachable in the preview. It is a
    // portal into document.body, which the server renderer cannot produce, so
    // the open sheet shows up as that one refusal while the closed one renders.
    let portalRefused = false;
    try {
      renderStatic(<AppShell />);
    } catch (e) {
      portalRefused = /document|portal/i.test(String(e));
    }
    assert(portalRefused, 'the open kill sheet is rendered by the preview shell');

    s().setKillDryRun(false);
    await s().runKill();
    assertEqual(calls.length, 2, 'the real run');
    assertEqual(calls[1]!.opts.onlyHealthOrphans, true, 'onlyHealthOrphans on the real run');
    assertEqual(calls[1]!.opts.dryRun, false, 'not a dry run');

    s().closeKillSheet();
    s().openKillSheet([procRow(700)]);
    assertEqual(s().proc.kill.onlyHealthOrphans, false, 'a monitor kill is not restricted');
  } finally {
    cmd.procmonKill = original;
    useAppStore.getState().closeKillSheet();
    reset();
  }
});

test('health: a single-row Clean up of a Gilde-session leftover goes by pid, shows its named dry run first and cannot run before it', async () => {
  reset();
  const original = cmd.procmonKill;
  const calls: { targets: KillTarget[]; opts: KillOpts }[] = [];
  let release: () => void = () => {};
  const gate = new Promise<void>((r) => (release = r));
  cmd.procmonKill = async (targets, opts) => {
    calls.push({ targets, opts });
    if (opts.dryRun) await gate;
    return targets.map((target) => ({
      target,
      report: {
        spawnId: null,
        requested: [601, 602],
        excluded: [],
        survivors: [],
        refusedReason: null,
        dryRun: opts.dryRun,
        escalated: [],
        fence: 'unfenced',
        at: 0,
        names: [
          { pid: 601, name: 'claude' },
          { pid: 602, name: 'cargo' },
        ],
      },
    })) as never;
  };
  try {
    // A Gilde session's leftover carries a spawnId: addressing it by that
    // would be refused by Rust (`notLeftover`) and could never be cleaned.
    const row = { ...procRow(601), spawnId: 'sp-old', group: 'oursOrphan' } as ProcRow;
    const snapshot = snapshotOf({
      agents: [agent(601, { orphan: true, attribution: { kind: 'gildeSession', sessionId: 'sess-1' }, row })],
      orphans: [601],
      alerts: [alertOf({ id: 'orphans:gilde', bodyKey: 'health.alert.orphans.bodyGilde', actions: [{ kind: 'cleanUp', pids: [601] }] })],
    });
    const s = useAppStore.getState;
    s().applyHealth(snapshot);
    const a = s().health.snapshot!.alerts[0]!;
    await s().runHealthAction(a, a.actions[0]!);
    assert(s().proc.kill.open, 'the confirm sheet is open');
    assertEqual(s().proc.kill.bulk, false, 'one row');
    assertEqual(s().proc.kill.dryRun, true, 'a single health row opens on its dry run too');
    assertEqual(calls.length, 1, 'the dry run was asked at once');
    assertDeepEqual(calls[0]!.targets, [{ target: 'pid', pid: 601 }], 'by pid, never by spawnId');
    assertEqual(calls[0]!.opts.onlyHealthOrphans, true, 'restricted to leftovers');

    // Before the preview arrived nothing can run — not the button, not the action.
    assertEqual(killRunDisabled(s().proc.kill), true, 'Run is off until the dry run is shown');
    s().setKillDryRun(false);
    await s().runKill();
    assertEqual(calls.filter((c) => !c.opts.dryRun).length, 0, 'no real run without the preview');

    // The preview arrives: every pid it would signal, named.
    s().setKillDryRun(true);
    release();
    await new Promise((r) => setTimeout(r, 0));
    await new Promise((r) => setTimeout(r, 0));
    const preview = s().proc.kill.preview;
    assert(preview !== null, 'the preview is held');
    const sets = dryRunSets(preview!);
    assertDeepEqual(sets.targets, [601, 602], 'its own descendants included');
    assertDeepEqual(sets.names, { 601: 'claude', 602: 'cargo' }, 'each target named');
    const html = renderStatic(<KillDryRunTargets sets={sets} />);
    assert(html.includes('601') && html.includes('claude') && html.includes('602') && html.includes('cargo'), html);
    assertEqual(killRunDisabled(s().proc.kill), false, 'now it can be confirmed');

    s().setKillDryRun(false);
    await s().runKill();
    const real = calls.filter((c) => !c.opts.dryRun);
    assertEqual(real.length, 1, 'the real run');
    assertDeepEqual(real[0]!.targets, [{ target: 'pid', pid: 601 }], 'by pid');
    assertEqual(real[0]!.opts.onlyHealthOrphans, true, 'onlyHealthOrphans on the real run');
  } finally {
    release();
    cmd.procmonKill = original;
    useAppStore.getState().closeKillSheet();
    reset();
  }
});

// ---------------------------------------------------------------------------
// 5. Team view markers
// ---------------------------------------------------------------------------

function teamView(): CcoTeamView {
  const raw = (fixture as unknown as { view: CcoTeamView }).view;
  return { ...JSON.parse(JSON.stringify(raw)), generation: 1, believed: true };
}

function listOf(view: CcoTeamView): CcoTeamListResult {
  return {
    available: true,
    supported: true,
    dataRoot: '/h/.claude-code-orchestrator',
    teamMode: 'on',
    hosts: [],
    teams: [
      {
        teamId: view.teamId,
        name: view.state.name,
        title: view.state.title,
        status: view.state.status,
        source: view.source,
        live: view.live,
        members: view.state.members.length,
        running: 0,
        blocked: 0,
        userUnread: 0,
        leadUnread: 0,
        createdAt: view.state.createdAt,
        owner: view.state.owner,
        rev: view.rev,
        believed: view.believed,
      },
    ],
    generation: 1,
  };
}

test('health: Team view member rows show memory and the leftover marker; the header an alert chip', () => {
  reset();
  const view = teamView();
  useAppStore.setState({
    team: { ...initialTeamState(), watching: true, list: listOf(view), views: new Map([[view.teamId, view]]), selectedTeamId: view.teamId },
    startTeamWatch: () => () => undefined,
    health: {
      ...initialHealthState(),
      snapshot: snapshotOf({
        teams: [
          {
            teamId: view.teamId,
            team: 'demo',
            status: 'active',
            hostAlive: true,
            rssBytes: 3 * GIB,
            members: [
              { name: 'alice', state: 'running', rssBytes: 1.2 * GIB, pids: [501] },
              { name: 'bob', state: 'leftover', rssBytes: 1.8 * GIB, pids: [777] },
            ],
          },
        ],
        alerts: [alertOf({ teamId: view.teamId })],
      }),
    },
  });
  try {
    const html = renderStatic(
      <TeamPane
        descriptor={{ view: 'team' }}
        tab={{ id: 't1', title: 'team', descriptor: { view: 'team' }, closable: true }}
        paneId="pane-1"
        active
      />,
    );
    const rows = html.match(/<button[^>]*class="team-member"[\s\S]*?<\/button>/g) ?? [];
    const alice = rows.find((r) => r.includes('>alice<')) ?? '';
    const bob = rows.find((r) => r.includes('>bob<')) ?? '';
    assert(alice.includes('1.2 GB') && !alice.includes('member-leftover'), 'alice: memory, no marker');
    assert(bob.includes('1.8 GB') && bob.includes('data-testid="member-leftover"'), 'bob: memory and the leftover marker');
    assert(bob.includes(ko('health.member.leftover')), 'Korean marker text');
    assert(!textOf(bob).includes('777'), 'the pid is only in a tooltip');
    assert(html.includes('data-testid="team-alert-chip"'), 'the header chip');
    assert(html.includes('조치 필요 1'), 'which counts the team alert');

    // No alert for this team → no chip; a member without a live process → no figure.
    useAppStore.setState({
      health: {
        ...initialHealthState(),
        snapshot: snapshotOf({ teams: [{ teamId: view.teamId, team: 'demo', status: 'active', hostAlive: true, rssBytes: 0, members: [{ name: 'alice', state: 'idle', rssBytes: 0, pids: [] }] }], alerts: [alertOf({ teamId: 'tmother000001' , params: { team: 'other' } })] }),
      },
    });
    const quiet = renderStatic(
      <TeamPane
        descriptor={{ view: 'team' }}
        tab={{ id: 't1', title: 'team', descriptor: { view: 'team' }, closable: true }}
        paneId="pane-1"
        active
      />,
    );
    assert(!quiet.includes('data-testid="team-alert-chip"'), 'no chip for another team');
    assert(!quiet.includes('data-testid="member-memory"'), 'no figure without a live process');
  } finally {
    useAppStore.setState({ team: { ...initialTeamState() } });
    reset();
  }
});

test('health: a sidebar member row keeps the name whole at 220 px — branch equal to the name omitted, details on their own line', () => {
  reset();
  const view = teamView();
  const alpha = view.state.members[0]!;
  alpha.name = 'alpha';
  alpha.state = 'ended';
  alpha.worktree = { ...alpha.worktree!, path: '/x/wt-alpha', branch: 'cco/demo-hi1/alpha' };
  const bob = view.state.members[1]!;
  bob.worktree = { ...bob.worktree!, branch: 'feature/login-form' };
  useAppStore.setState({
    team: { ...initialTeamState(), watching: true, list: listOf(view), views: new Map([[view.teamId, view]]), selectedTeamId: view.teamId },
    startTeamWatch: () => () => undefined,
    health: {
      ...initialHealthState(),
      snapshot: snapshotOf({
        teams: [
          {
            teamId: view.teamId,
            team: 'demo',
            status: 'active',
            hostAlive: true,
            rssBytes: 1.3 * 1024 * 1024,
            members: [{ name: 'alpha', state: 'leftover', rssBytes: 1.3 * 1024 * 1024, pids: [4242] }],
          },
        ],
      }),
    },
  });
  try {
    const html = renderStatic(
      <div style={{ width: 220 }}>
        <TeamSidebar />
      </div>,
    );
    const rows = html.match(/<button[^>]*class="tree-row team-sb-member"[\s\S]*?<\/button>/g) ?? [];
    const row = rows.find((r) => r.includes('>alpha<')) ?? '';
    assert(row !== '', 'the alpha row renders');

    // The name: whole, in its own element with no truncation class, first in the identity line.
    const name = /<span class="([^"]*)" data-testid="sb-member-name"[^>]*>([^<]*)<\/span>/.exec(row);
    assert(name !== null, 'the name element');
    assertEqual(name![2], 'alpha', 'the full name text, not ellipsized');
    assert(!/\bell\b/.test(name![1]) && !/\bgrow\b/.test(name![1]), 'the name carries no truncation class');
    assert(name![1].includes('team-sb-member-name'), 'the name keeps its priority rule');

    // The branch equals the name → omitted; the branch and path stay in the worktree icon's tooltip.
    assert(!row.includes('team-sb-branch'), 'a branch equal to the name is not shown');
    assertEqual(textOf(row).split(' ').filter((w) => w === 'alpha').length, 1, 'the name appears once');
    const wt = /<span class="team-wt[^"]*"[^>]*>/.exec(row)?.[0] ?? '';
    assert(wt.includes('role="img"') && wt.includes(`aria-label="${ko('team.worktree.marker')}"`), 'the worktree marker is a labelled icon');
    assert(wt.includes('cco/demo-hi1/alpha') && wt.includes('/x/wt-alpha'), 'its tooltip carries the full branch and the path');
    assert(!textOf(row).includes(ko('team.worktree.marker')), 'no worktree word in the row text');

    // The identity line holds only dot, name and icon; leftover, GB and state are the details group after it.
    const id = /<span class="team-sb-member-id">([\s\S]*?)<span class="s11 team-sb-member-meta">/.exec(row)?.[1] ?? '';
    assert(id.includes('sb-member-name') && id.includes('team-wt'), 'identity: name and worktree icon');
    assert(!id.includes('member-leftover') && !id.includes('member-memory') && !id.includes(ko('team.member.ended')), 'no details beside the name');
    const meta = row.slice(row.indexOf('team-sb-member-meta'));
    assert(meta.includes('data-testid="member-leftover"') && meta.includes(ko('health.member.leftover')), 'the leftover marker is a detail');
    assert(meta.includes('&lt;0.1 GB') && !row.includes('0.0 GB'), 'a 1.3 MB process reads <0.1 GB');
    assert(meta.includes(ko('team.member.ended')), 'the state is a detail');

    // A branch that differs from the name is still shown, short, among the details.
    const bobRow = rows.find((r) => r.includes('>bob<')) ?? '';
    assert(/team-sb-branch"[^>]*>feature\/login-form<\/span>/.test(bobRow), 'a different branch is shown');
    assert(bobRow.indexOf('team-sb-branch') > bobRow.indexOf('team-sb-member-meta'), 'among the details, not beside the name');

    // The layout rules that make it so: the row wraps (the details go to a second line as one
    // group) rather than squeezing; the name is never a fixed-width, ellipsized-first item.
    const css = teamCss as string;
    // Every rule whose selector list ends in `sel` (a selector may be shared with a sibling).
    const rule = (sel: string) => {
      const bodies: string[] = [];
      for (let at = css.indexOf(`${sel} {`); at >= 0; at = css.indexOf(`${sel} {`, at + 1)) {
        bodies.push(css.slice(at, css.indexOf('}', at)));
      }
      assert(bodies.length > 0, `team.css has ${sel}`);
      return bodies.join('\n');
    };
    const rowRule = rule('.tree-row.team-sb-member');
    assert(/flex-wrap:\s*wrap/.test(rowRule) && /height:\s*auto/.test(rowRule), 'the row wraps and grows');
    const idRule = rule('.team-sb-member-id');
    assert(/max-width:\s*100%/.test(idRule) && !/max-width:\s*\d/.test(rule('.team-sb-member-name')), 'the name may take the whole row (about 20 characters at 220 px), never a fixed cap');
    assert(/margin-left:\s*auto/.test(rule('.team-sb-member-meta')), 'the details are one group after the identity');
    assert(/min-width:\s*0/.test(rule('.team-sb-branch')), 'the branch is what shrinks among the details');
  } finally {
    useAppStore.setState({ team: { ...initialTeamState() } });
    reset();
  }
});
