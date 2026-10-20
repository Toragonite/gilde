/**
 * Process monitor tests (T23).
 *
 * These pin the four things that are safety rules rather than styling, and that
 * a type check cannot see:
 *
 *  1. the order is computed once per scan and never again (acceptance 2);
 *  2. a selection cannot span two groups, and only `OursOrphan` has a bulk
 *     action (acceptance 4);
 *  3. a `Protected` row has **no** kill affordance in the rendered markup —
 *     removed, not disabled (acceptance 5);
 *  4. the only argv the table can render is the masked `argvDisplay`, so a
 *     secret that Rust redacted cannot reappear in the panel (acceptance 9).
 */

import { renderToStaticMarkup } from 'react-dom/server';

import { assert, assertDeepEqual, assertEqual } from '../test/harness';
import { laneTest as test, laneUiState } from '../test/lanes';
import { emptyGraph } from '../store/graphSlice';
import { useAppStore } from '../store/useAppStore';
import type { AppStore } from '../store/useAppStore';
import {
  DEFAULT_COLLAPSED,
  GROUP_ORDER,
  POLL_ACTIVE_MS,
  POLL_BACKGROUND_MS,
  allowsBulkAction,
  describeCwd,
  dryRunSets,
  emptyProcTotals,
  groupRows,
  initialProcState,
  killTargetOf,
  matchesFilter,
  maxRss,
  procPollIntervalMs,
  protectionReason,
  sortRows,
  tallyOutcomes,
} from '../store/procSlice';
import type { ProcState } from '../store/procSlice';
import type { KillOutcome, KillReport, ProcGroupKind, ProcRow, ProcTag } from '../lib/ipc';
import { ProcessMonitorTable, flattenGroups } from './ProcessMonitorTable';
import { ProcessRow, ownerLabelKey, rowStatus } from './ProcessRow';
import { KillSummary } from './KillConfirmSheet';
import { BudgetMeter, budgetReadout } from './BudgetMeter';
// M1.5-D-sidebar-ui (BACKLOG 16)
import { Sparkline } from '../components/Sparkline';

// ---------------------------------------------------------------------------
// Fixtures — shaped exactly like what `procmon_scan` returns, never demo data
// ---------------------------------------------------------------------------

function row(
  pid: number,
  group: ProcGroupKind,
  rssBytes: number,
  extra: Partial<ProcRow> = {},
): ProcRow {
  return {
    pid,
    ppid: 1,
    pgid: pid,
    name: 'claude',
    argvDisplay: 'claude -- "fix the resize path"',
    rssBytes,
    cpuPct: 1.5,
    elapsedSecs: 252,
    startTime: 1_700_000_000_000,
    cwd: null,
    userOwned: true,
    group,
    tags: [],
    owner: null,
    spawnId: null,
    ...extra,
  };
}

function report(patch: Partial<KillReport> = {}): KillReport {
  return {
    spawnId: null,
    requested: [],
    excluded: [],
    survivors: [],
    refusedReason: null,
    dryRun: false,
    escalated: [],
    fence: 'verified',
    at: 1,
    ...patch,
  };
}

function outcome(pid: number, patch: Partial<KillReport> = {}): KillOutcome {
  return { target: { target: 'pid', pid }, report: report({ requested: [pid], ...patch }) };
}

function seed(proc: Partial<ProcState>): void {
  useAppStore.setState({
    graph: emptyGraph(),
    ui: laneUiState(),
    toasts: [],
    agents: [],
    sidecar: null,
    proc: { ...initialProcState(), ...proc },
  } as Partial<AppStore>);
  Object.assign(useAppStore.getInitialState(), useAppStore.getState());
}

const NO_COLLAPSE: Partial<Record<ProcGroupKind, boolean>> = {};
const ALL: { groups: null; query: string } = { groups: null, query: '' };

// ---------------------------------------------------------------------------
// 1. ordering
// ---------------------------------------------------------------------------

test('rows sort by group, then RSS descending, then pid', () => {
  const sorted = sortRows([
    row(300, 'other', 10),
    row(100, 'oursOrphan', 1_000),
    row(200, 'oursLive', 5_000),
    row(101, 'oursOrphan', 9_000),
    row(102, 'oursOrphan', 9_000),
  ]);
  assertDeepEqual(
    sorted.map((r) => r.pid),
    [101, 102, 100, 200, 300],
    'orphans first, RSS descending, pid breaks the tie',
  );
});

test('the order is recomputed once per scan and not once per render', () => {
  seed({});
  const store = useAppStore.getState();
  const rows = [row(1, 'oursLive', 10), row(2, 'oursOrphan', 20)];

  store.applyScan({ rows, totals: emptyProcTotals(), at: 1_000 });
  const first = useAppStore.getState().proc.rows;
  assertEqual(useAppStore.getState().proc.scanCount, 1, 'one scan, one sort');

  // Anything that is not a scan must leave the array *identical*, so a repaint
  // can never move a row under the cursor.
  useAppStore.getState().setProcQuery('cl');
  useAppStore.getState().toggleProcGroupCollapsed('other');
  assert(useAppStore.getState().proc.rows === first, 'the row array is untouched');
  assertEqual(useAppStore.getState().proc.scanCount, 1, 'still one sort');

  useAppStore.getState().applyScan({ rows, totals: emptyProcTotals(), at: 2_000 });
  assertEqual(useAppStore.getState().proc.scanCount, 2, 'the next scan sorts again');
});

test('a scan older than the one already applied is dropped', () => {
  seed({});
  const store = useAppStore.getState();
  store.applyScan({ rows: [row(1, 'oursLive', 10)], totals: emptyProcTotals(), at: 5_000 });
  store.applyScan({ rows: [], totals: emptyProcTotals(), at: 4_000 });
  assertEqual(useAppStore.getState().proc.rows.length, 1, 'the stale answer did not win');
  assertEqual(useAppStore.getState().proc.at, 5_000, 'the newest scan time stands');
});

// ---------------------------------------------------------------------------
// 2. grouping, filtering, defaults
// ---------------------------------------------------------------------------

test('external residue is collapsed by default and orphans are not', () => {
  const collapsed = initialProcState().collapsed;
  assertEqual(collapsed.externalResidue, true, 'residue starts collapsed');
  assertEqual(collapsed.oursOrphan, undefined, 'orphans start expanded');
  assert(DEFAULT_COLLAPSED.includes('externalResidue'), 'the default names residue');
  assertEqual(GROUP_ORDER[0], 'oursOrphan', 'orphans lead the panel');
});

test('groups carry count, RSS sum and CPU sum of the rows they show', () => {
  const views = groupRows(
    sortRows([row(1, 'oursOrphan', 100), row(2, 'oursOrphan', 300), row(3, 'other', 700)]),
    ALL,
    NO_COLLAPSE,
  );
  assertEqual(views.length, 2, 'two groups');
  assertEqual(views[0].group, 'oursOrphan', 'orphans first');
  assertEqual(views[0].count, 2, 'count');
  assertEqual(views[0].rssBytes, 400, 'RSS sum');
  assertEqual(Math.round(views[0].cpuPct * 10), 30, 'CPU sum');
  assertEqual(maxRss(views), 700, 'the bar scale is the largest visible row');
});

test('a collapsed group keeps its header and contributes no rows or scale', () => {
  const views = groupRows(
    sortRows([row(1, 'oursOrphan', 100), row(3, 'externalResidue', 9_000)]),
    ALL,
    { externalResidue: true },
  );
  const items = flattenGroups(views);
  assertEqual(items.filter((i) => i.kind === 'group').length, 2, 'both headers render');
  assertEqual(items.filter((i) => i.kind === 'row').length, 1, 'the collapsed rows do not');
  assertEqual(maxRss(views), 100, 'a collapsed row does not set the bar scale');
});

test('the filter matches name, masked argv and pid', () => {
  const target = row(4321, 'other', 1, { name: 'node', argvDisplay: 'node mcp-server-github' });
  assert(matchesFilter(target, { groups: null, query: 'MCP' }), 'argv, case-insensitively');
  assert(matchesFilter(target, { groups: null, query: '432' }), 'pid');
  assert(!matchesFilter(target, { groups: null, query: 'codex' }), 'no false positive');
  assert(!matchesFilter(target, { groups: ['oursOrphan'], query: '' }), 'group filter applies');
});

// ---------------------------------------------------------------------------
// 3. selection and bulk rules
// ---------------------------------------------------------------------------

test('a selection never spans two groups', () => {
  seed({});
  const orphan = row(10, 'oursOrphan', 1);
  const other = row(11, 'other', 1);
  const store = () => useAppStore.getState();

  store().toggleProcSelection(orphan);
  assertDeepEqual(store().proc.selection.pids, [10], 'first pick selects');

  store().toggleProcSelection(other);
  assertDeepEqual(store().proc.selection.pids, [11], 'a foreign group replaces the selection');
  assertEqual(store().proc.selection.group, 'other', 'and takes its group');

  store().toggleProcSelection(other);
  assertEqual(store().proc.selection.group, null, 'deselecting the last row clears the group');
});

test('only OursOrphan has a bulk action', () => {
  assert(allowsBulkAction('oursOrphan'), 'orphans are provably ours');
  for (const group of GROUP_ORDER.filter((g) => g !== 'oursOrphan')) {
    assert(!allowsBulkAction(group), `${group} is confirmed row by row`);
  }
});

test('a scan drops selected pids that are gone', () => {
  seed({});
  useAppStore.getState().toggleProcSelection(row(10, 'oursOrphan', 1));
  useAppStore
    .getState()
    .applyScan({ rows: [row(11, 'oursOrphan', 1)], totals: emptyProcTotals(), at: 9 });
  assertDeepEqual(useAppStore.getState().proc.selection.pids, [], 'the dead pid left the set');
});

// ---------------------------------------------------------------------------
// 4. protection
// ---------------------------------------------------------------------------

const PROTECTED: ProcTag[] = ['protected', 'appBundleMain'];

test('a protected row renders no kill affordance at all', () => {
  seed({});
  const html = renderToStaticMarkup(
    <ProcessRow
      row={row(501, 'other', 100, { name: 'Finder', tags: PROTECTED, userOwned: true })}
      maxRssBytes={100}
      cwd={null}
      selected={false}
      onToggleSelect={() => undefined}
      onKill={() => undefined}
      onNeedCwd={() => undefined}
    />,
  );
  assert(!html.includes('<button'), 'no button — the affordance is removed, not disabled');
  assert(!html.includes('type="checkbox"'), 'and it cannot be selected into a bulk action');
  assert(html.includes('보호됨'), 'the row says so');
  assertEqual(protectionReason(row(1, 'other', 1)), 'init', 'pid ≤ 1 is protected as init');
  assertEqual(
    protectionReason(row(9, 'other', 1, { tags: PROTECTED })),
    'appBundleMain',
    'a bundle main names itself',
  );
  assertEqual(
    protectionReason(row(9, 'other', 1, { userOwned: false })),
    'foreignUid',
    'another user’s process names itself',
  );
});

test('openKillSheet refuses to carry a protected row into the sheet', () => {
  seed({});
  useAppStore
    .getState()
    .openKillSheet([row(1, 'other', 1, { tags: ['protected'] }), row(2, 'oursOrphan', 1)]);
  const plan = useAppStore.getState().proc.kill;
  assertDeepEqual(plan.rows.map((r) => r.pid), [2], 'only the unprotected row is a target');
  assertEqual(plan.bulk, false, 'one row is not a bulk action');
  useAppStore.getState().closeKillSheet();
});

test('a registry row is killed by spawn id and a bare row by pid', () => {
  assertDeepEqual(killTargetOf(row(7, 'oursLive', 1, { spawnId: 's-1' })), {
    target: 'spawn',
    spawnId: 's-1',
  }, 'a fenced kill');
  assertDeepEqual(killTargetOf(row(7, 'other', 1)), { target: 'pid', pid: 7 }, 'an unfenced kill');
});

// ---------------------------------------------------------------------------
// 5. honest reporting
// ---------------------------------------------------------------------------

test('the kill summary counts terminations, refusals and survivors separately', () => {
  const outcomes: KillOutcome[] = [
    ...[1, 2, 3, 4, 5, 6].map((pid) => outcome(pid)),
    outcome(7, { refusedReason: 'argv', fence: 'argv' }),
    outcome(8, { survivors: [8], escalated: [8] }),
  ];
  const tally = tallyOutcomes(outcomes);
  assertEqual(tally.terminated, 6, 'six terminated');
  assertDeepEqual(tally.refused, [{ reason: 'argv', count: 1 }], 'one refusal, with its reason');
  assertEqual(tally.survived, 1, 'one survivor');

  seed({});
  const html = renderToStaticMarkup(<KillSummary outcomes={outcomes} />);
  assert(
    html.includes('6개 종료, 1개 거부(argv 불일치), 1개 SIGKILL 생존'),
    `the DESIGN §8-6 sentence, verbatim: ${html}`,
  );
});

test('a dry run reports the target and exclusion sets it would use', () => {
  const sets = dryRunSets([
    outcome(11, { dryRun: true, requested: [11, 12], excluded: [{ pid: 13, reason: 'tooYoung' }] }),
    outcome(14, { dryRun: true, refusedReason: 'unmanagedBulk', requested: [] }),
  ]);
  assertDeepEqual(sets.targets, [11, 12], 'what would be signalled');
  assertDeepEqual(
    sets.excluded,
    [
      { pid: 13, reason: 'tooYoung' },
      { pid: 14, reason: 'unmanagedBulk' },
    ],
    'and what would not, with reasons',
  );
});

// ---------------------------------------------------------------------------
// 6. cwd, status and owner
// ---------------------------------------------------------------------------

test('cwd keeps the worktree segment and abbreviates the rest', () => {
  const parts = describeCwd('/Users/x/.gilde/worktrees/gilde-code-9f2a13/fix-pty-resize');
  assert(parts !== null, 'a path is described');
  assertEqual(parts?.highlight, 'fix-pty-resize', 'the worktree segment survives whole');
  assertEqual(parts?.prefix, '…/worktrees/gilde-code-9f2a13/', 'the rest is abbreviated');
  assertEqual(describeCwd('/usr/lib')?.prefix, '/usr/lib', 'a short path is kept');
  assertEqual(describeCwd(null), null, 'an unresolved cwd is nothing, not a guess');
});

test('a row status is a shared token, never a colour per group', () => {
  assertEqual(rowStatus(row(1, 'oursLive', 1)), 'working', 'live rows');
  assertEqual(rowStatus(row(1, 'oursOrphan', 1)), 'orphan', 'orphans');
  assertEqual(rowStatus(row(1, 'other', 1, { tags: ['orphan'] })), 'orphan', 'tagged orphans');
  assertEqual(rowStatus(row(1, 'externalResidue', 1)), 'idle', 'residue is neutral');
  assertEqual(ownerLabelKey(row(1, 'other', 1)), 'monitor.owner.external', 'external owner');
  assertEqual(
    ownerLabelKey(row(1, 'oursLive', 1, { spawnId: 's', owner: { sessionId: 'x' } })),
    'monitor.owner.session',
    'a session owns its row',
  );
});

// ---------------------------------------------------------------------------
// 7. polling and the budget meter
// ---------------------------------------------------------------------------

test('polling is 3 s visible, 15 s hidden and stopped while blurred', () => {
  const base = initialProcState();
  assertEqual(
    procPollIntervalMs({ ...base, panelVisible: true, windowFocused: true }),
    POLL_ACTIVE_MS,
    'visible',
  );
  assertEqual(
    procPollIntervalMs({ ...base, panelVisible: false, windowFocused: true }),
    POLL_BACKGROUND_MS,
    'hidden',
  );
  assertEqual(
    procPollIntervalMs({ ...base, panelVisible: true, windowFocused: false }),
    0,
    'blurred stops the timer',
  );
});

test('the budget meter never renders an unknown number as a healthy one', () => {
  assertEqual(budgetReadout(null).known, false, 'no budget is not 0%');
  seed({});
  const unknown = renderToStaticMarkup(<BudgetMeter />);
  assert(unknown.includes('알 수 없음'), 'it says so');

  seed({
    budget: {
      rssBytes: 2_200_000_000,
      physicalRamBytes: 16_000_000_000,
      percent: 13.75,
      warnPct: 70,
      criticalPct: 85,
      pressureLevel: 'normal',
      pressureSource: 'kern.memorystatus_vm_pressure_level',
    },
  });
  const known = renderToStaticMarkup(<BudgetMeter />);
  assert(known.includes('14%'), 'the share of physical RAM');
  assert(known.includes('압력 정상'), 'the pressure level');
  assert(known.includes('soft limit'), 'and the honest limitation');
});

// ---------------------------------------------------------------------------
// 8. security — the panel can only render what Rust masked
// ---------------------------------------------------------------------------

test('a redacted argv stays redacted in the table', () => {
  // Exactly what `procscan::mask_argv` produces for
  // `claude --api-key sk-live-abcdefghijklmnop` (DESIGN §8-7): the row that
  // reaches the front end has no unmasked argv field at all.
  const masked = 'claude --api-key «redacted»';
  seed({
    rows: sortRows([row(4242, 'oursLive', 1_000_000, { argvDisplay: masked })]),
    totals: { ...emptyProcTotals(), processes: 1, rssBytes: 1_000_000 },
    at: 1_000,
  });
  const html = renderToStaticMarkup(<ProcessMonitorTable />);
  assert(html.includes('«redacted»'), 'the mask is what the table shows');
  assert(!html.includes('sk-live'), 'and the secret is nowhere in the markup');
});

// ---------------------------------------------------------------------------
// 9. the panel as a whole
// ---------------------------------------------------------------------------

test('the panel renders groups, totals and the scan age from the store alone', () => {
  seed({
    rows: sortRows([
      row(41822, 'oursLive', 1_200_000_000, { name: 'claude' }),
      row(29331, 'oursOrphan', 300_000_000, { name: 'codex', tags: ['orphan'] }),
      row(30542, 'externalResidue', 210_000_000, { name: 'node' }),
    ]),
    totals: {
      ...emptyProcTotals(),
      processes: 812,
      rssBytes: 1_710_000_000,
      physicalRamBytes: 16_000_000_000,
      rssPercent: 10.7,
      pressureLevel: 'normal',
      byGroup: {
        oursLive: { count: 1, rssBytes: 1_200_000_000, cpuPct: 1.5 },
        oursOrphan: { count: 1, rssBytes: 300_000_000, cpuPct: 1.5 },
        externalResidue: { count: 1, rssBytes: 210_000_000, cpuPct: 1.5 },
      },
    },
    at: Date.now() - 2_000,
  });
  const html = renderToStaticMarkup(<ProcessMonitorTable />);
  assert(html.includes('고아'), 'the orphan group is there');
  assert(html.includes('41822'), 'a pid is rendered');
  assert(html.includes('프로세스 812개'), 'the footer counts the machine, not the filter');
  assert(html.includes('고아 1개 정리'), 'the bulk action names its count');
  assert(!html.includes('undefined'), 'nothing rendered the string "undefined"');
});

test('the footer says "paused" instead of an age while the window is blurred', () => {
  seed({ rows: [row(1, 'oursLive', 1)], at: 1_000, windowFocused: false });
  const html = renderToStaticMarkup(<ProcessMonitorTable />);
  assert(html.includes('일시정지'), 'a stopped poll looks stopped');
});

// ---------------------------------------------------------------------------
// M1.5 BACKLOG 16 — the RSS magnitude is visible on this ground
//
// The bar is drawn by the shared `Bar`, whose track is `--n4`; against the
// table's `--n1` that difference is invisible, so a row with a small RSS looked
// like a row with no bar at all. `monitor.css` lifts the track to `--n5` here.
// The sparkline had the same problem for the same reason — it inherited
// `currentColor` from a `--n8` meta row — and now draws in `--n9`.
// ---------------------------------------------------------------------------

test('the RSS bar and the sparkline draw in tokens above the --n5 floor', () => {
  seed({ rows: [row(41822, 'oursLive', 1_200_000_000)], at: 1_000 });
  const html = renderToStaticMarkup(<ProcessMonitorTable />);
  assert(html.includes('class="mon-rss-bar"'), 'the RSS cell still carries the bar');
  assert(html.includes('class="bar"'), 'which is the shared meter, restyled by monitor.css');

  const spark = renderToStaticMarkup(<Sparkline values={[1, 4, 2]} />);
  assert(spark.includes('stroke="var(--n9)"'), 'the line is a token, not currentColor');
  assert(!spark.includes('currentColor'), 'so it cannot inherit a dimmer context');
});
