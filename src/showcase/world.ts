/**
 * The showcase's demo world — neutral, plausible data in the app's real shapes.
 *
 * One small engineering organisation (`acme`) on one Mac: three repositories
 * under `~/code`, four teams, five worker accounts across the agent CLIs, and the
 * agent processes the health surface sees. Every value is built from the real
 * types (`CcoTeamView`, `CcoWorkersAnswer`, `HealthSnapshot`, …), so `tsc`
 * checks the fixtures against the same contract real data meets, and every
 * time is relative to `now` (the moment the showcase starts).
 *
 * Nothing here may name a real person, account, e-mail address or machine
 * path: accounts are `main`, `work`, `ci-bot`, `codex`, `cursor`; repositories
 * are `acme/*`; paths are `~/code/*`. The showcase test scans the rendered
 * markup for anything else.
 */

import type {
  CcoBoardTask,
  CcoMemberActivity,
  CcoMemberState,
  CcoTeamListResult,
  CcoTeamMessage,
  CcoTeamState,
  CcoTeamSummary,
  CcoTeamView,
  CcoUsageWindow,
  CcoWorkerAccount,
  CcoWorkersAnswer,
  HealthAgent,
  HealthAlert,
  HealthHost,
  HealthSnapshot,
  HealthTeam,
  Project,
  RepoIdentity,
} from '../lib/ipc';
import { CCO_WORKERS_SCHEMA } from '../lib/ipc';
import { RUN_LOGIN_FAILURE_DETAIL } from '../workers/model';

/** Which teams (and which health facts) a scene shows. */
export interface WorldOptions {
  /** The four teams (Repository grouping) instead of the checkout repository's two. */
  allTeams: boolean;
  /** The ended team's agent is still running: one actionable health alert. */
  leftover: boolean;
}

export interface ShowcaseWorld {
  now: number;
  views: CcoTeamView[];
  list: CcoTeamListResult;
  identities: RepoIdentity[];
  projects: Project[];
  workers: CcoWorkersAnswer;
  health: HealthSnapshot;
}

/** The team the Team view opens on. */
export const MAIN_TEAM_ID = 'tmc7k2p9x4qa';
/** The member whose details the Team scene opens. */
export const MAIN_MEMBER = 'api';

const ENDED_TEAM_ID = 'tmb3e8w1v6nd';
const SEARCH_TEAM_ID = 'tms5r9m2h7kc';
const DOCS_TEAM_ID = 'tmd4q1z8f3ye';

const SEC = 1_000;
const MIN = 60 * SEC;
const HOUR = 60 * MIN;
const DAY = 24 * HOUR;
const GIB = 1024 * 1024 * 1024;
const RAM = 32 * GIB;

/** A git object name for the eye: 40 hex digits from a short seed. */
function sha(seed: string): string {
  return seed.repeat(Math.ceil(40 / seed.length)).slice(0, 40);
}

// ---------------------------------------------------------------------------
// repositories, folders, worktrees
// ---------------------------------------------------------------------------

const CHECKOUT = '~/code/checkout';
const CHECKOUT_BILLING = '~/code/checkout-billing';
const WEB = '~/code/web';
const DOCS = '~/code/docs';
const WORKTREES = '~/code/.team-worktrees';

interface Repo {
  slug: string;
  root: string;
  branch: string;
}

const REPOS: Record<'checkout' | 'web' | 'docs', Repo> = {
  checkout: { slug: 'acme/checkout', root: CHECKOUT, branch: 'main' },
  web: { slug: 'acme/web', root: WEB, branch: 'main' },
  docs: { slug: 'acme/docs', root: DOCS, branch: 'main' },
};

/** The worktree-aware identity of a path in one repository (what `repo_identity` answers). */
function identity(path: string, repo: Repo, worktree: { branch: string } | null): RepoIdentity {
  return {
    path,
    kind: 'repo',
    key: `origin:git.example.com/${repo.slug}`,
    commonDir: `${repo.root}/.git`,
    root: repo.root,
    toplevel: path,
    linkedWorktree: worktree !== null,
    branch: worktree ? worktree.branch : repo.branch,
    origin: `https://git.example.com/${repo.slug}.git`,
  };
}

function memberWorktree(team: string, suffix: string, member: string) {
  return {
    path: `${WORKTREES}/${team}/${member}`,
    branch: `cco/${team}-${suffix}/${member}`,
  };
}

// ---------------------------------------------------------------------------
// teams
// ---------------------------------------------------------------------------

interface MemberInput {
  name: string;
  index: number;
  role: string;
  state: CcoMemberState['state'];
  provider: string;
  worker: string;
  model: string;
  mailbox: string;
  team: string;
  suffix: string;
  baseCommit: string;
  turns: number;
  tokens: { in: number; out: number };
  costUsd: number | null;
  tasks: string[];
  rssMb: number;
  extra?: Partial<CcoMemberState>;
}

function member(m: MemberInput): CcoMemberState {
  const wt = memberWorktree(m.team, m.suffix, m.name);
  return {
    name: m.name,
    index: m.index,
    role: m.role,
    state: m.state,
    provider: m.provider,
    worker: m.worker,
    accountLabel: m.worker,
    machineLogin: false,
    sharesLeadPlan: false,
    billableOverage: false,
    runsCommands: true,
    model: m.model,
    incarnation: 1,
    mailbox: m.mailbox,
    portBase: 41000 + m.index * 10,
    worktree: { path: wt.path, branch: wt.branch, baseCommit: m.baseCommit, ready: true },
    turns: m.turns,
    tokens: m.tokens,
    costUsd: m.costUsd,
    pending: 0,
    tasks: m.tasks,
    peerWakes: 0,
    rssMb: m.rssMb,
    ...m.extra,
  };
}

function task(
  t: Pick<CcoBoardTask, 'id' | 'title' | 'state' | 'paths'> & Partial<CcoBoardTask>,
  createdAt: number,
  updatedAt: number,
): CcoBoardTask {
  return {
    allowOverlap: false,
    createdBy: 'lead',
    createdAt,
    reports: [],
    commits: [],
    turns: [],
    updatedAt,
    updatedBy: t.assignee ?? 'lead',
    ...t,
  };
}

function message(
  seq: number,
  ts: number,
  from: string,
  to: string,
  kind: string,
  body: string,
  taskId?: string,
): CcoTeamMessage {
  return {
    seq,
    id: `m${seq}`,
    ts,
    from,
    to,
    kind,
    urgent: false,
    ...(taskId === undefined ? {} : { task: taskId }),
    body,
    cut: false,
    bytes: body.length,
  };
}

function view(state: CcoTeamState, extra: Omit<CcoTeamView, 'teamId' | 'state' | 'generation' | 'truncated'>): CcoTeamView {
  return { teamId: state.id, state, generation: 1, truncated: false, ...extra };
}

/** checkout-revamp: the live team the Team view shows — lead and three members on three CLIs. */
function checkoutRevamp(now: number): CcoTeamView {
  const team = 'checkout-revamp';
  const suffix = 'c7k2';
  const base = sha('4e1c9a7b2d');
  const createdAt = now - 3 * HOUR - 12 * MIN;
  const api = member({
    name: 'api',
    index: 1,
    role: 'Backend: checkout sessions API',
    state: 'running',
    provider: 'claude',
    worker: 'work',
    model: 'opus',
    mailbox: 'tools',
    team,
    suffix,
    baseCommit: base,
    turns: 14,
    tokens: { in: 1_020_544, out: 66_310 },
    costUsd: 9.87,
    tasks: ['b3'],
    rssMb: 1_410,
    extra: {
      conversationShort: '8c2f1a',
      turn: { id: 't37', n: 14, startedAt: now - 2 * MIN - 5 * SEC, pid: 52210, lastOutputAt: now - 8 * SEC },
      lastTurn: {
        id: 't31',
        n: 13,
        outcome: 'ok',
        endedAt: now - 9 * MIN,
        head: 'Request validation for POST /v1/checkout/sessions is in; 12 tests pass.',
      },
      lastInputTokens: 61_204,
      peerWakes: 2,
      lastActivity: { ts: now - 8 * SEC, text: 'Edit · src/api/checkout/sessions.ts' },
      worktree: {
        ...memberWorktree(team, suffix, 'api'),
        baseCommit: base,
        ready: true,
        headSha: sha('9b0e5d13'),
        ahead: 4,
        filesChanged: 9,
        dirty: true,
      },
    },
  });
  const web = member({
    name: 'web',
    index: 2,
    role: 'Frontend: card form and checkout page',
    state: 'idle',
    provider: 'codex',
    worker: 'codex',
    model: '',
    mailbox: 'turns-only',
    team,
    suffix,
    baseCommit: base,
    turns: 11,
    tokens: { in: 742_118, out: 48_906 },
    costUsd: null,
    tasks: ['b4'],
    rssMb: 640,
    extra: {
      lastTurn: {
        id: 't35',
        n: 11,
        outcome: 'ok',
        endedAt: now - 6 * MIN,
        head: 'Card form validates number, expiry and CVC as you type; b4 is in review.',
      },
      lastActivity: { ts: now - 6 * MIN, text: 'Moved b4 "Card form with inline validation" to review' },
      worktree: {
        ...memberWorktree(team, suffix, 'web'),
        baseCommit: base,
        ready: true,
        headSha: sha('51ac7e02'),
        ahead: 6,
        filesChanged: 7,
        dirty: false,
      },
    },
  });
  const tests = member({
    name: 'tests',
    index: 3,
    role: 'QA: end-to-end tests',
    state: 'idle',
    provider: 'cursor',
    worker: 'cursor',
    model: '',
    mailbox: 'turns-only',
    team,
    suffix,
    baseCommit: base,
    turns: 8,
    tokens: { in: 401_655, out: 26_740 },
    costUsd: null,
    tasks: ['b5'],
    rssMb: 490,
    extra: {
      lastTurn: {
        id: 't36',
        n: 8,
        outcome: 'ok',
        endedAt: now - 3 * MIN,
        head: 'Happy-path test passes against staging; the declined-card test waits for the lead.',
      },
      lastActivity: { ts: now - 3 * MIN, text: 'Waiting for the lead: should an expired card return 402 or 422?' },
      worktree: {
        ...memberWorktree(team, suffix, 'tests'),
        baseCommit: base,
        ready: true,
        headSha: sha('e47b2c90'),
        ahead: 2,
        filesChanged: 3,
        dirty: false,
      },
    },
  });

  const board: CcoBoardTask[] = [
    task(
      {
        id: 'b1',
        title: 'Checkout sessions table and migration',
        state: 'done',
        assignee: 'api',
        paths: ['db/migrations/0042_checkout_sessions.sql', 'src/api/checkout/schema.ts'],
        commits: ['3f9a1c2'],
      },
      createdAt + 4 * MIN,
      now - 2 * HOUR - 20 * MIN,
    ),
    task(
      {
        id: 'b2',
        title: 'API contract for POST /v1/checkout/sessions',
        state: 'done',
        assignee: 'api',
        paths: ['docs/api/checkout.md'],
        commits: ['a81d0f4'],
      },
      createdAt + 5 * MIN,
      now - 1 * HOUR - 45 * MIN,
    ),
    task(
      {
        id: 'b3',
        title: 'POST /v1/checkout/sessions endpoint',
        state: 'claimed',
        assignee: 'api',
        paths: ['src/api/checkout/sessions.ts', 'src/api/checkout/sessions.test.ts', 'src/api/checkout/errors.ts'],
        note: 'Idempotency-Key required; 409 when a key is reused with a different body.',
      },
      createdAt + 6 * MIN,
      now - 14 * MIN,
    ),
    task(
      {
        id: 'b4',
        title: 'Card form with inline validation',
        state: 'review',
        assignee: 'web',
        paths: ['web/src/checkout/CardForm.tsx', 'web/src/checkout/validate.ts'],
        commits: ['c03e9b7', '77d1a2e'],
      },
      createdAt + 6 * MIN,
      now - 6 * MIN,
    ),
    task(
      {
        id: 'b5',
        title: 'E2E: happy path and declined card',
        state: 'blocked',
        assignee: 'tests',
        paths: ['e2e/checkout.spec.ts'],
        note: 'Waiting for the lead: 402 or 422 for an expired card?',
      },
      createdAt + 7 * MIN,
      now - 3 * MIN,
    ),
    task({ id: 'b6', title: 'Saved cards: list and remove', state: 'open', paths: ['web/src/checkout/SavedCards.tsx'] }, now - 40 * MIN, now - 40 * MIN),
    task({ id: 'b7', title: 'Retire the three-step checkout flow', state: 'open', paths: [] }, now - 38 * MIN, now - 38 * MIN),
  ];

  const messages: CcoTeamMessage[] = [
    message(
      42,
      now - 3 * MIN,
      'tests',
      'lead',
      'question',
      'Should an expired card return 402 or 422 from POST /v1/checkout/sessions/{id}/confirm? The contract only says 4xx, and the declined-card test needs the exact code.',
      'b5',
    ),
    message(
      41,
      now - 6 * MIN,
      'engine',
      'lead',
      'notice',
      'web moved b4 "Card form with inline validation" to review: CardForm validates number, expiry and CVC as you type and shows the decline reason under the field. 18 unit tests pass.',
      'b4',
    ),
    message(
      40,
      now - 9 * MIN,
      'api',
      'lead',
      'report',
      'Validation for POST /v1/checkout/sessions is in: amount, currency and an Idempotency-Key header are required. 12 tests pass. Starting on the confirm step.',
      'b3',
    ),
    message(
      39,
      now - 14 * MIN,
      'lead',
      'api',
      'assignment',
      'Take b3 next: the endpoint as specified in docs/api/checkout.md. Return 409 when an Idempotency-Key is reused with a different body.',
      'b3',
    ),
    message(
      38,
      now - 22 * MIN,
      'lead',
      'tests',
      'assignment',
      'Write b5 against the staging API: the happy path, a declined card and an expired card.',
      'b5',
    ),
    message(
      37,
      now - 31 * MIN,
      'lead',
      'web',
      'answer',
      'Use the existing Field component and show errors under the input, not as a toast.',
      'b4',
    ),
    message(
      36,
      now - 33 * MIN,
      'web',
      'lead',
      'question',
      'Card errors: inline under each field, or one summary above the Pay button?',
      'b4',
    ),
  ];

  const activity: CcoMemberActivity[] = [
    {
      member: 'api',
      lines: [
        { ts: now - 125 * SEC, turn: 't37', k: 'turn', text: 'Turn 14: the confirm step' },
        { ts: now - 118 * SEC, turn: 't37', k: 'tool', text: 'Read · docs/api/checkout.md' },
        { ts: now - 112 * SEC, turn: 't37', k: 'tool', text: 'Read · src/api/checkout/sessions.ts' },
        {
          ts: now - 100 * SEC,
          turn: 't37',
          k: 'text',
          text: 'The confirm step needs the card state from the processor; adding a typed error for expired cards.',
        },
        { ts: now - 80 * SEC, turn: 't37', k: 'tool', text: 'Grep · ProcessorError in src/api' },
        { ts: now - 61 * SEC, turn: 't37', k: 'tool', text: 'Edit · src/api/checkout/errors.ts' },
        { ts: now - 40 * SEC, turn: 't37', k: 'tool', text: 'Bash · npm test -- checkout/sessions' },
        { ts: now - 27 * SEC, turn: 't37', k: 'text', text: '12 passed, 1 failed: confirm returns 500 for an expired card instead of a 4xx.' },
        { ts: now - 8 * SEC, turn: 't37', k: 'tool', text: 'Edit · src/api/checkout/sessions.ts' },
      ],
    },
    {
      member: 'web',
      lines: [
        { ts: now - 9 * MIN, turn: 't35', k: 'tool', text: 'Edit · web/src/checkout/validate.ts' },
        { ts: now - 8 * MIN, turn: 't35', k: 'tool', text: 'Bash · npm run test -- CardForm' },
        { ts: now - 7 * MIN, turn: 't35', k: 'text', text: '18 tests pass.' },
        { ts: now - 6 * MIN, turn: 't35', k: 'done', text: 'Turn 11 finished' },
      ],
    },
    {
      member: 'tests',
      lines: [
        { ts: now - 5 * MIN, turn: 't36', k: 'tool', text: 'Bash · npm run e2e -- checkout.spec.ts' },
        { ts: now - 4 * MIN, turn: 't36', k: 'text', text: 'Happy path passes. The expired-card case needs the expected status code.' },
        { ts: now - 3 * MIN, turn: 't36', k: 'done', text: 'Turn 8 finished' },
      ],
    },
  ];

  const state: CcoTeamState = {
    v: 1,
    id: MAIN_TEAM_ID,
    name: team,
    title: 'One-page checkout with saved cards',
    status: 'active',
    goal: 'Replace the three-step checkout with one page: a sessions endpoint, a card form that validates as you type, and end-to-end tests for the happy path and declined cards.',
    workspace: CHECKOUT,
    repoRoot: CHECKOUT,
    leadRel: '',
    baseCommit: base,
    createdAt,
    seq: 42,
    owner: { hostPid: 51834, hostStartedAt: createdAt - 3 * MIN, session: { pid: 51790, startedAt: createdAt - 8 * MIN } },
    lead: { unread: 1, readSeq: 41, waiter: false, lastCallAt: now - 4 * MIN, wakesToday: 6 },
    user: { unread: 0, readSeq: 42 },
    members: [api, web, tests],
    board,
    budgets: { turnsToday: 33, turnsPerDay: 300, running: 1, maxRunning: 3, pingPongTripped: false },
    totals: { turns: 33, inputTokens: 2_164_317, outputTokens: 141_956, costUsd: 18.42 },
    warnings: [],
  };

  return view(state, {
    rev: 42,
    source: 'host',
    live: true,
    hostKey: 's51790',
    messages,
    activity,
    composedAt: now - 2 * SEC,
    believed: true,
  });
}

/** billing-export: ended an hour ago; its lead ran in a linked worktree of the checkout repository. */
function billingExport(now: number): CcoTeamView {
  const team = 'billing-export';
  const suffix = 'b3e8';
  const base = sha('7a3f0c5e');
  const createdAt = now - 5 * HOUR;
  const endedAt = now - 52 * MIN;
  const exporter = member({
    name: 'exporter',
    index: 1,
    role: 'Nightly CSV export of settled payments',
    state: 'ended',
    provider: 'claude',
    worker: 'work',
    model: 'sonnet',
    mailbox: 'tools',
    team,
    suffix,
    baseCommit: base,
    turns: 9,
    tokens: { in: 512_330, out: 31_118 },
    costUsd: 3.61,
    tasks: [],
    rssMb: 0,
    extra: {
      lastTurn: { id: 't18', n: 9, outcome: 'ok', endedAt: endedAt - 4 * MIN, head: 'Export job and its retry policy are merged.' },
    },
  });
  const reviewer = member({
    name: 'reviewer',
    index: 2,
    role: 'Code review and test gaps',
    state: 'ended',
    provider: 'codex',
    worker: 'codex',
    model: '',
    mailbox: 'turns-only',
    team,
    suffix,
    baseCommit: base,
    turns: 5,
    tokens: { in: 220_904, out: 12_402 },
    costUsd: null,
    tasks: [],
    rssMb: 0,
  });
  const state: CcoTeamState = {
    v: 1,
    id: ENDED_TEAM_ID,
    name: team,
    title: 'Nightly billing export',
    status: 'ended',
    goal: 'A nightly CSV export of settled payments for finance, with retries and an alert when a run fails.',
    workspace: CHECKOUT_BILLING,
    repoRoot: CHECKOUT_BILLING,
    leadRel: '',
    baseCommit: base,
    createdAt,
    endedAt,
    seq: 27,
    owner: null,
    lead: { unread: 0, readSeq: 27, waiter: false, lastCallAt: endedAt - 2 * MIN, wakesToday: 3 },
    user: { unread: 0, readSeq: 27 },
    members: [exporter, reviewer],
    board: [
      task({ id: 'b1', title: 'Export job for settled payments', state: 'done', assignee: 'exporter', paths: ['jobs/billing_export.ts'] }, createdAt, endedAt - 30 * MIN),
      task({ id: 'b2', title: 'Retry policy and failure alert', state: 'done', assignee: 'exporter', paths: ['jobs/retry.ts'] }, createdAt, endedAt - 10 * MIN),
      task({ id: 'b3', title: 'Review the export job', state: 'done', assignee: 'reviewer', paths: [] }, createdAt, endedAt - 6 * MIN),
    ],
    budgets: { turnsToday: 14, turnsPerDay: 300, running: 0, maxRunning: 2, pingPongTripped: false },
    totals: { turns: 14, inputTokens: 733_234, outputTokens: 43_520, costUsd: 3.61 },
    warnings: [],
  };
  return view(state, {
    rev: 27,
    source: 'disk',
    live: false,
    messages: [
      message(27, endedAt - 2 * MIN, 'lead', 'exporter', 'text', 'Merged. Thanks, ending the team now.'),
      message(26, endedAt - 4 * MIN, 'exporter', 'lead', 'report', 'Export job and retry policy are merged; the failure alert posts to #billing-ops.', 'b2'),
    ],
    activity: [],
    composedAt: endedAt,
    believed: false,
  });
}

/** search-ui: a live team in the web repository (shown by the grouping scene). */
function searchUi(now: number): CcoTeamView {
  const team = 'search-ui';
  const suffix = 's5r9';
  const base = sha('2d6b8f41');
  const createdAt = now - 1 * HOUR - 25 * MIN;
  const indexer = member({
    name: 'indexer',
    index: 1,
    role: 'Search index and query API',
    state: 'running',
    provider: 'claude',
    worker: 'work',
    model: 'sonnet',
    mailbox: 'tools',
    team,
    suffix,
    baseCommit: base,
    turns: 6,
    tokens: { in: 388_420, out: 22_118 },
    costUsd: 2.14,
    tasks: ['b2'],
    rssMb: 1_120,
    extra: {
      turn: { id: 't9', n: 6, startedAt: now - 4 * MIN, pid: 53102, lastOutputAt: now - 15 * SEC },
      lastActivity: { ts: now - 15 * SEC, text: 'Bash · npm test -- search/query' },
    },
  });
  const ui = member({
    name: 'ui',
    index: 2,
    role: 'Search results page',
    state: 'idle',
    provider: 'codex',
    worker: 'codex',
    model: '',
    mailbox: 'turns-only',
    team,
    suffix,
    baseCommit: base,
    turns: 4,
    tokens: { in: 160_210, out: 9_804 },
    costUsd: null,
    tasks: [],
    rssMb: 380,
    extra: { lastActivity: { ts: now - 11 * MIN, text: 'Results list renders highlights; waiting for the query API.' } },
  });
  const state: CcoTeamState = {
    v: 1,
    id: SEARCH_TEAM_ID,
    name: team,
    title: 'Instant search on the storefront',
    status: 'active',
    goal: 'Search as you type on the storefront, with highlighted matches.',
    workspace: WEB,
    repoRoot: WEB,
    leadRel: '',
    baseCommit: base,
    createdAt,
    seq: 18,
    owner: { hostPid: 53001, hostStartedAt: createdAt - 2 * MIN, session: { pid: 52990, startedAt: createdAt - 6 * MIN } },
    lead: { unread: 0, readSeq: 18, waiter: true, lastCallAt: now - 12 * MIN, wakesToday: 2 },
    user: { unread: 0, readSeq: 18 },
    members: [indexer, ui],
    board: [
      task({ id: 'b1', title: 'Results page with highlights', state: 'review', assignee: 'ui', paths: ['src/search/Results.tsx'] }, createdAt, now - 11 * MIN),
      task({ id: 'b2', title: 'Query API with prefix matching', state: 'claimed', assignee: 'indexer', paths: ['src/search/query.ts'] }, createdAt, now - 4 * MIN),
    ],
    budgets: { turnsToday: 10, turnsPerDay: 300, running: 1, maxRunning: 2, pingPongTripped: false },
    totals: { turns: 10, inputTokens: 548_630, outputTokens: 31_922, costUsd: 2.14 },
    warnings: [],
  };
  return view(state, {
    rev: 18,
    source: 'host',
    live: true,
    hostKey: 's52990',
    messages: [message(18, now - 11 * MIN, 'ui', 'lead', 'report', 'Results list renders highlighted matches; b1 is ready for review.', 'b1')],
    activity: [],
    composedAt: now - 3 * SEC,
    believed: true,
  });
}

/** docs-refresh: suspended yesterday (its lead session was closed). */
function docsRefresh(now: number): CcoTeamView {
  const team = 'docs-refresh';
  const suffix = 'd4q1';
  const base = sha('5c90e2aa');
  const createdAt = now - 1 * DAY - 2 * HOUR;
  const writer = member({
    name: 'writer',
    index: 1,
    role: 'API reference pages',
    state: 'idle',
    provider: 'claude',
    worker: 'work',
    model: 'sonnet',
    mailbox: 'tools',
    team,
    suffix,
    baseCommit: base,
    turns: 7,
    tokens: { in: 301_772, out: 40_210 },
    costUsd: 1.92,
    tasks: ['b2'],
    rssMb: 0,
  });
  const state: CcoTeamState = {
    v: 1,
    id: DOCS_TEAM_ID,
    name: team,
    title: 'Refresh the API reference',
    status: 'suspended',
    goal: 'Regenerate the API reference from the OpenAPI spec and fix the broken examples.',
    workspace: DOCS,
    repoRoot: DOCS,
    leadRel: '',
    baseCommit: base,
    createdAt,
    seq: 15,
    owner: null,
    lead: { unread: 0, readSeq: 15, waiter: false, lastCallAt: now - 20 * HOUR, wakesToday: 0 },
    user: { unread: 0, readSeq: 15 },
    members: [writer],
    board: [
      task({ id: 'b1', title: 'Generate reference pages from the spec', state: 'done', assignee: 'writer', paths: [] }, createdAt, now - 21 * HOUR),
      task({ id: 'b2', title: 'Fix the broken request examples', state: 'claimed', assignee: 'writer', paths: ['reference/examples'] }, createdAt, now - 20 * HOUR),
    ],
    budgets: { turnsToday: 0, turnsPerDay: 300, running: 0, maxRunning: 2, pingPongTripped: false },
    totals: { turns: 7, inputTokens: 301_772, outputTokens: 40_210, costUsd: 1.92 },
    warnings: [],
  };
  return view(state, {
    rev: 15,
    source: 'disk',
    live: false,
    messages: [],
    activity: [],
    composedAt: now - 20 * HOUR,
    believed: false,
  });
}

function summaryOf(v: CcoTeamView): CcoTeamSummary {
  const members = v.state.members;
  return {
    teamId: v.teamId,
    name: v.state.name,
    title: v.state.title,
    status: v.state.status,
    source: v.source,
    live: v.live,
    members: members.length,
    running: members.filter((m) => m.state === 'running').length,
    blocked: members.filter((m) => m.state === 'blocked').length,
    userUnread: v.state.user.unread,
    leadUnread: v.state.lead.unread,
    createdAt: v.state.createdAt,
    ...(v.state.endedAt === undefined ? {} : { endedAt: v.state.endedAt }),
    ...(v.hostKey === undefined ? {} : { hostKey: v.hostKey }),
    owner: v.state.owner,
    rev: v.rev,
    believed: v.believed,
  };
}

// ---------------------------------------------------------------------------
// worker accounts
// ---------------------------------------------------------------------------

const NO_STATS = { tasks: 0, inputTokens: 0, outputTokens: 0, costUsd: 0 };

function usage(
  kind: CcoUsageWindow['kind'],
  label: string,
  percent: number,
  severity: CcoUsageWindow['severity'],
  resetsAt: number,
): CcoUsageWindow {
  return { kind, label, percent, severity, resetsAt };
}

function account(a: Pick<CcoWorkerAccount, 'key' | 'name' | 'provider' | 'providerLabel' | 'login'> & Partial<CcoWorkerAccount>): CcoWorkerAccount {
  return {
    isMain: false,
    isWorker: true,
    preferred: false,
    defaultModel: null,
    sessionWorkers: a.provider === 'claude',
    permissionMode: null,
    modelRanAs: null,
    loginDetail: null,
    loginCheck: null,
    plan: null,
    planLabel: null,
    sameAccountAs: [],
    windows: [],
    windowsStaleSince: null,
    lastGoodWindowsAt: null,
    usageFetchedAt: null,
    overage: null,
    cooldownUntil: null,
    session5h: NO_STATS,
    week: NO_STATS,
    allTime: { ...NO_STATS, errors: 0 },
    lastError: null,
    running: 0,
    ...a,
  };
}

function workersAnswer(now: number): CcoWorkersAnswer {
  const generatedAt = now - 20 * SEC;
  // `main` and `work` are two config folders signed in to one claude.ai login:
  // one plan, so the engine reports the same windows for both and names each other.
  const sharedWindows = [
    usage('session', 'Session (5hr)', 23, 'normal', now + 2 * HOUR + 41 * MIN),
    usage('weekly_all', 'Weekly (all models)', 61, 'normal', now + 3 * DAY + 5 * HOUR),
  ];
  const accounts: CcoWorkerAccount[] = [
    account({
      key: 'acct-main',
      name: 'main',
      isMain: true,
      isWorker: false,
      provider: 'claude',
      providerLabel: 'Claude Code',
      login: 'ok',
      loginCheck: { state: 'ok', since: now - 2 * DAY, checkedAt: now - 4 * MIN, reason: '' },
      plan: 'max',
      planLabel: 'Claude Max',
      sameAccountAs: ['work'],
      windows: sharedWindows,
      lastGoodWindowsAt: generatedAt,
      usageFetchedAt: generatedAt,
      session5h: { tasks: 0, inputTokens: 0, outputTokens: 0, costUsd: 0 },
      week: { tasks: 0, inputTokens: 0, outputTokens: 0, costUsd: 0 },
    }),
    account({
      key: 'acct-work',
      name: 'work',
      preferred: true,
      defaultModel: 'opus',
      provider: 'claude',
      providerLabel: 'Claude Code',
      permissionMode: 'acceptEdits',
      login: 'ok',
      loginCheck: { state: 'ok', since: now - 2 * DAY, checkedAt: now - 4 * MIN, reason: '' },
      plan: 'max',
      planLabel: 'Claude Max',
      sameAccountAs: ['main'],
      windows: sharedWindows,
      lastGoodWindowsAt: generatedAt,
      usageFetchedAt: generatedAt,
      session5h: { tasks: 6, inputTokens: 1_284_300, outputTokens: 81_420, costUsd: 11.9 },
      week: { tasks: 46, inputTokens: 8_412_006, outputTokens: 512_340, costUsd: 74.31 },
      allTime: { tasks: 318, inputTokens: 52_904_118, outputTokens: 3_180_442, costUsd: 466.2, errors: 4 },
      running: 1,
    }),
    account({
      key: 'acct-ci-bot',
      name: 'ci-bot',
      defaultModel: 'sonnet',
      provider: 'claude',
      providerLabel: 'Claude Code',
      login: 'expired',
      loginDetail: RUN_LOGIN_FAILURE_DETAIL,
      loginCheck: { state: 'loggedOut', since: now - 1 * HOUR, checkedAt: now - 55 * MIN, reason: 'not logged in' },
      plan: 'pro',
      planLabel: 'Claude Pro',
      lastGoodWindowsAt: now - 26 * HOUR,
      week: { tasks: 12, inputTokens: 1_906_220, outputTokens: 102_884, costUsd: 9.4 },
      allTime: { tasks: 87, inputTokens: 12_410_552, outputTokens: 690_118, costUsd: 61.8, errors: 2 },
    }),
    account({
      key: 'acct-codex',
      name: 'codex',
      provider: 'codex',
      providerLabel: 'Codex',
      permissionMode: 'workspace-write',
      login: 'ok',
      windows: [
        usage('session', '5h limit', 94, 'critical', now + 38 * MIN),
        usage('weekly_all', 'Weekly limit', 58, 'normal', now + 4 * DAY + 2 * HOUR),
      ],
      lastGoodWindowsAt: generatedAt,
      usageFetchedAt: generatedAt,
      session5h: { tasks: 5, inputTokens: 742_118, outputTokens: 48_906, costUsd: 0 },
      week: { tasks: 23, inputTokens: 3_120_904, outputTokens: 204_118, costUsd: 0 },
      allTime: { tasks: 141, inputTokens: 19_884_301, outputTokens: 1_204_552, costUsd: 0, errors: 1 },
    }),
    account({
      key: 'acct-cursor',
      name: 'cursor',
      provider: 'cursor',
      providerLabel: 'Cursor',
      login: 'no-plan-limits',
      week: { tasks: 9, inputTokens: 1_002_440, outputTokens: 61_207, costUsd: 0 },
      allTime: { tasks: 40, inputTokens: 4_410_090, outputTokens: 262_004, costUsd: 0, errors: 0 },
    }),
  ];
  return {
    available: true,
    supported: true,
    snapshot: {
      schema: CCO_WORKERS_SCHEMA,
      generatedAt,
      accounts,
      unsupportedAccounts: [],
      servedFromCache: false,
      guards: { overageWorkerDispatch: 'block', frontierWorkerDispatch: 'allow' },
    },
    hostKey: 's51790',
    receivedAt: generatedAt,
    stale: false,
    fromCache: false,
    unavailable: null,
    nextPollAt: generatedAt + 60 * SEC,
    minPollMs: 60 * SEC,
    generation: 1,
  };
}

// ---------------------------------------------------------------------------
// health
// ---------------------------------------------------------------------------

interface AgentInput {
  pid: number;
  agent: HealthAgent['agent'];
  name: string;
  argv: string;
  rssGb: number;
  cpuPct: number;
  ageSecs: number;
  teamId: string;
  team: string;
  member: string;
  worktree: string;
  orphan: boolean;
}

function healthAgent(now: number, a: AgentInput): HealthAgent {
  const rssBytes = Math.round(a.rssGb * GIB);
  const startTime = now - a.ageSecs * SEC;
  return {
    pid: a.pid,
    pgid: a.pid,
    ppid: a.orphan ? 1 : a.pid - 40,
    agent: a.agent,
    rssBytes,
    cpuPct: a.cpuPct,
    elapsedSecs: a.ageSecs,
    startTime,
    attribution: { kind: 'teamMember', teamId: a.teamId, team: a.team, member: a.member, worktree: a.worktree },
    orphan: a.orphan,
    pids: [a.pid],
    row: {
      pid: a.pid,
      ppid: a.orphan ? 1 : a.pid - 40,
      pgid: a.pid,
      name: a.name,
      argvDisplay: a.argv,
      rssBytes,
      cpuPct: a.cpuPct,
      elapsedSecs: a.ageSecs,
      startTime,
      cwd: a.worktree,
      userOwned: true,
      group: a.orphan ? 'externalResidue' : 'agentUnmanaged',
      tags: a.orphan ? ['orphan', 'inWorktree', 'external'] : ['inWorktree', 'external'],
      owner: null,
      spawnId: null,
    },
  };
}

function healthSnapshot(now: number, options: WorldOptions): HealthSnapshot {
  const wt = (team: string, suffix: string, name: string) => memberWorktree(team, suffix, name).path;
  const live: AgentInput[] = [
    { pid: 52210, agent: 'claude', name: 'claude', argv: 'claude -p --output-format stream-json', rssGb: 1.38, cpuPct: 31.5, ageSecs: 125, teamId: MAIN_TEAM_ID, team: 'checkout-revamp', member: 'api', worktree: wt('checkout-revamp', 'c7k2', 'api'), orphan: false },
    { pid: 52480, agent: 'codex', name: 'codex', argv: 'codex exec --json', rssGb: 0.62, cpuPct: 0.4, ageSecs: 40 * 60, teamId: MAIN_TEAM_ID, team: 'checkout-revamp', member: 'web', worktree: wt('checkout-revamp', 'c7k2', 'web'), orphan: false },
    { pid: 52633, agent: 'cursor-agent', name: 'cursor-agent', argv: 'cursor-agent --print --output-format stream-json', rssGb: 0.48, cpuPct: 0.2, ageSecs: 35 * 60, teamId: MAIN_TEAM_ID, team: 'checkout-revamp', member: 'tests', worktree: wt('checkout-revamp', 'c7k2', 'tests'), orphan: false },
  ];
  if (options.allTeams) {
    live.push(
      { pid: 53102, agent: 'claude', name: 'claude', argv: 'claude -p --output-format stream-json', rssGb: 1.09, cpuPct: 24.8, ageSecs: 240, teamId: SEARCH_TEAM_ID, team: 'search-ui', member: 'indexer', worktree: wt('search-ui', 's5r9', 'indexer'), orphan: false },
      { pid: 53240, agent: 'codex', name: 'codex', argv: 'codex exec --json', rssGb: 0.37, cpuPct: 0.1, ageSecs: 20 * 60, teamId: SEARCH_TEAM_ID, team: 'search-ui', member: 'ui', worktree: wt('search-ui', 's5r9', 'ui'), orphan: false },
    );
  }
  // The ended team's two agents outlived it (its host is gone): the one actionable alert.
  const leftovers: AgentInput[] = options.leftover
    ? [
        { pid: 49310, agent: 'claude', name: 'claude', argv: 'claude -p --output-format stream-json', rssGb: 0.86, cpuPct: 0.0, ageSecs: 2 * 3600 + 14 * 60, teamId: ENDED_TEAM_ID, team: 'billing-export', member: 'exporter', worktree: wt('billing-export', 'b3e8', 'exporter'), orphan: true },
        { pid: 49388, agent: 'codex', name: 'codex', argv: 'codex exec --json', rssGb: 0.41, cpuPct: 0.0, ageSecs: 2 * 3600 + 9 * 60, teamId: ENDED_TEAM_ID, team: 'billing-export', member: 'reviewer', worktree: wt('billing-export', 'b3e8', 'reviewer'), orphan: true },
      ]
    : [];

  const agents = [...live, ...leftovers].map((a) => healthAgent(now, a));
  const byPid = new Map(agents.map((a) => [a.pid, a]));
  const rssOf = (pid: number) => byPid.get(pid)?.rssBytes ?? 0;

  const teamBlock = (teamId: string, team: string, status: string, hostAlive: boolean, members: [string, 'running' | 'idle' | 'leftover', number][]): HealthTeam => ({
    teamId,
    team,
    status,
    hostAlive,
    rssBytes: members.reduce((sum, [, , pid]) => sum + rssOf(pid), 0),
    members: members.map(([name, state, pid]) => ({ name, state, rssBytes: rssOf(pid), pids: [pid] })),
  });

  const teams: HealthTeam[] = [
    teamBlock(MAIN_TEAM_ID, 'checkout-revamp', 'active', true, [
      ['api', 'running', 52210],
      ['web', 'idle', 52480],
      ['tests', 'idle', 52633],
    ]),
  ];
  const hosts: HealthHost[] = [
    { pid: 51834, rssBytes: Math.round(0.09 * GIB), elapsedSecs: 3 * 3600 + 15 * 60, sessionPid: 51790, leadAlive: true, teamIds: [MAIN_TEAM_ID] },
  ];
  if (options.allTeams) {
    teams.push(
      teamBlock(SEARCH_TEAM_ID, 'search-ui', 'active', true, [
        ['indexer', 'running', 53102],
        ['ui', 'idle', 53240],
      ]),
    );
    hosts.push({ pid: 53001, rssBytes: Math.round(0.08 * GIB), elapsedSecs: 87 * 60, sessionPid: 52990, leadAlive: true, teamIds: [SEARCH_TEAM_ID] });
  }

  const alerts: HealthAlert[] = [];
  if (leftovers.length > 0) {
    const pids = leftovers.map((a) => a.pid);
    teams.push(
      teamBlock(ENDED_TEAM_ID, 'billing-export', 'ended', false, leftovers.map((a) => [a.member, 'leftover', a.pid] as [string, 'leftover', number])),
    );
    alerts.push({
      id: `orphans:${ENDED_TEAM_ID}`,
      kind: 'orphans',
      severity: 'action',
      titleKey: 'health.alert.orphans.title',
      bodyKey: 'health.alert.orphans.body',
      params: {
        count: pids.length,
        team: 'billing-export',
        members: leftovers.map((a) => a.member),
        rssBytes: pids.reduce((sum, pid) => sum + rssOf(pid), 0),
      },
      actions: [{ kind: 'cleanUp', pids }, { kind: 'details' }],
      since: now - 12 * MIN,
      raisedAt: now - 12 * MIN,
      teamId: ENDED_TEAM_ID,
      dismissed: false,
      dismissedUntil: null,
    });
  }

  const agentsRssBytes = agents.reduce((sum, a) => sum + a.rssBytes, 0) + hosts.reduce((sum, h) => sum + h.rssBytes, 0);
  return {
    at: now,
    overall: alerts.length > 0 ? 'action' : 'ok',
    agents,
    hosts,
    orphans: agents.filter((a) => a.orphan).map((a) => a.pid),
    teams,
    memory: { pressure: 'normal', agentsRssBytes, physicalRamBytes: RAM },
    alerts,
    teamsKnown: true,
  };
}

// ---------------------------------------------------------------------------
// the world
// ---------------------------------------------------------------------------

function projects(now: number): Project[] {
  const project = (id: string, path: string, name: string, slug: string, color: string, addedAt: number): Project => ({
    id,
    path,
    canonicalPath: path,
    repoSlug: slug,
    name,
    color,
    defaultBaseRef: 'main',
    addedAt,
    lastOpenedAt: now - 10 * MIN,
    pathExists: true,
  });
  // The colours are the backend's project palette (src-tauri/src/commands/project.rs), as data.
  return [
    project('0192f3a0-6c1e-7a54-9c2d-5f1e8b7a4c01', CHECKOUT, 'checkout', 'acme/checkout', '#59b39b', now - 9 * DAY), // lint-ui: allow
    project('0192f3a0-6c1e-7a54-9c2d-5f1e8b7a4c02', CHECKOUT_BILLING, 'checkout-billing', 'acme/checkout', '#7c8cf8', now - 6 * DAY), // lint-ui: allow
    project('0192f3a0-6c1e-7a54-9c2d-5f1e8b7a4c03', WEB, 'web', 'acme/web', '#d08b5b', now - 4 * DAY), // lint-ui: allow
  ];
}

function identities(views: readonly CcoTeamView[], folders: readonly Project[]): RepoIdentity[] {
  const repoOfWorkspace: Record<string, Repo> = {
    [CHECKOUT]: REPOS.checkout,
    [CHECKOUT_BILLING]: REPOS.checkout,
    [WEB]: REPOS.web,
    [DOCS]: REPOS.docs,
  };
  const out = new Map<string, RepoIdentity>();
  const add = (path: string, repo: Repo, worktree: { branch: string } | null) => out.set(path, identity(path, repo, worktree));
  add(CHECKOUT, REPOS.checkout, null);
  add(CHECKOUT_BILLING, REPOS.checkout, { branch: 'billing-export' });
  add(WEB, REPOS.web, null);
  add(DOCS, REPOS.docs, null);
  for (const v of views) {
    const repo = repoOfWorkspace[v.state.workspace];
    if (!repo) continue;
    for (const m of v.state.members) add(m.worktree.path, repo, { branch: m.worktree.branch });
  }
  for (const folder of folders) {
    const repo = repoOfWorkspace[folder.path];
    if (repo && !out.has(folder.path)) add(folder.path, repo, null);
  }
  return [...out.values()];
}

/** Build the whole world for one scene, every time relative to `now`. */
export function buildWorld(now: number, options: WorldOptions): ShowcaseWorld {
  const views = options.allTeams
    ? [checkoutRevamp(now), billingExport(now), searchUi(now), docsRefresh(now)]
    : [checkoutRevamp(now), billingExport(now)];
  const folders = projects(now);
  const list: CcoTeamListResult = {
    available: true,
    supported: true,
    dataRoot: '~/code/.team-data',
    teamMode: 'on',
    hosts: views
      .filter((v) => v.live && v.state.owner !== null)
      .map((v) => ({
        key: v.hostKey ?? `s${v.state.owner?.session.pid ?? 0}`,
        sessionPid: v.state.owner?.session.pid ?? 0,
        sessionStartedAt: v.state.owner?.session.startedAt ?? now,
        hostPid: v.state.owner?.hostPid ?? 0,
        workspace: v.state.workspace,
        pane: false,
        state: 'connected',
        teamId: v.teamId,
      })),
    teams: views.map(summaryOf),
    generation: 1,
  };
  return {
    now,
    views,
    list,
    identities: identities(views, folders),
    projects: folders,
    workers: workersAnswer(now),
    health: healthSnapshot(now, options),
  };
}
