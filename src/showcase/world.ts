/**
 * The showcase world: a deterministic `StoreSnapshot` plus the content the
 * screens fetch on demand (conversation blocks, message bodies, task titles,
 * History rows), built from neutral sample data (ux-merge-decisions
 * "Additional requirements": projects like checkout-service, infra-live,
 * mobile-app, docs-site; teams like auth-split).
 *
 * `buildWorld(options, now)` is pure: the same options and `now` give the same
 * world, so screenshots and the browser gates (contract §14.3) are repeatable.
 * The sizes the Floor geometry gate needs (1, 8, 20, 31 agents, with
 * native-inferred and orchestrator teams) are `WorldOptions.agents`.
 *
 * Every value follows the contract's rules where the UI can see them: the
 * attention queue is computed from the sessions with Q1–Q8 (§8.1), so its
 * counts always equal what the sidebar shows; hosted-team approvals carry
 * `onBehalfOf`/`answerIn` (Q3); ids have the §3.2 shapes.
 */

import type { AbsPath } from '../lib/gen/AbsPath';
import type { Account } from '../lib/gen/Account';
import type { ActivityState } from '../lib/gen/ActivityState';
import type { AttentionItem } from '../lib/gen/AttentionItem';
import type { AttentionQueue } from '../lib/gen/AttentionQueue';
import type { AttentionReason } from '../lib/gen/AttentionReason';
import type { Block } from '../lib/gen/Block';
import type { BlockKind } from '../lib/gen/BlockKind';
import type { ClaudeCompat } from '../lib/gen/ClaudeCompat';
import type { Control } from '../lib/gen/Control';
import type { HistoryRow } from '../lib/gen/HistoryRow';
import type { UndoToken } from '../lib/gen/UndoToken';
import type { IdentityColour } from '../lib/gen/IdentityColour';
import type { Location } from '../lib/gen/Location';
import type { Member } from '../lib/gen/Member';
import type { PaneTile } from '../lib/gen/PaneTile';
import type { MemberKey } from '../lib/gen/MemberKey';
import type { MessageKey } from '../lib/gen/MessageKey';
import type { MessageKind } from '../lib/gen/MessageKind';
import type { PendingInteraction } from '../lib/gen/PendingInteraction';
import type { PrimaryAction } from '../lib/gen/PrimaryAction';
import type { ProcessRecord } from '../lib/gen/ProcessRecord';
import type { Project } from '../lib/gen/Project';
import type { Session } from '../lib/gen/Session';
import type { SessionCaps } from '../lib/gen/SessionCaps';
import type { SessionUid } from '../lib/gen/SessionUid';
import type { SourceStatus } from '../lib/gen/SourceStatus';
import type { StoreSnapshot } from '../lib/gen/StoreSnapshot';
import type { Task } from '../lib/gen/Task';
import type { TaskKey } from '../lib/gen/TaskKey';
import type { Team } from '../lib/gen/Team';
import type { TeamCapabilities } from '../lib/gen/TeamCapabilities';
import type { TeamKey } from '../lib/gen/TeamKey';
import type { TeamMessage } from '../lib/gen/TeamMessage';
import type { Vendor } from '../lib/gen/Vendor';
import {
  accountId,
  blockId,
  episodeId,
  locationId,
  memberKey,
  messageKey,
  paneId,
  processKey,
  projectId,
  ptyId,
  requestId,
  sessionUid,
  taskKey,
  teamKey,
} from './ids';

// ---------------------------------------------------------------------------
// Options and result
// ---------------------------------------------------------------------------

/** The three team shapes the screens must render (§7.2, §7.3.1, §7.2.5). */
export type TeamKind = 'native' | 'nativeInferred' | 'orchestrator';

export interface WorldOptions {
  /** Agents on the Floor: solo sessions plus every team member (leads included). 0 = empty world. */
  agents: number;
  /** Teams to form out of the agents, in this order. */
  teams: readonly TeamKind[];
  /** Approval and question cards are waiting. */
  pending: boolean;
  /** Leftover processes held for 2 scans (band 6). */
  leftovers: number;
  /** Ended sessions in History. */
  history: number;
  /** The default claude account is at 95 % of its 5-hour window (Home's warning line, A26.6). */
  quotaHot?: boolean;
  /** The boot sweep's undo window is open: the Recovered dialog (A29.3, T18b). */
  recovery?: boolean;
  /**
   * The hosted team also shows panes without a member session (A30.1): one
   * starting, one whose spawn failed, one running unknown pane.
   */
  paneTiles?: boolean;
}

/** The Recovered-dialog fixture's group token and window (A29.3). */
export const RECOVERY_UNDO = 'u_showcase-recovery' as UndoToken;
export const RECOVERY_WINDOW_MS = 10_000;

export interface ShowcaseWorld {
  now: number;
  snapshot: StoreSnapshot;
  /** Conversation blocks per session (what `convo_subscribe` resets to). */
  convo: ReadonlyMap<SessionUid, readonly Block[]>;
  /** On-demand texts (never in the store, §3.4.7–§3.4.8). */
  messageBodies: ReadonlyMap<MessageKey, string>;
  taskTitles: ReadonlyMap<TaskKey, string>;
  history: readonly HistoryRow[];
  /** Sessions a scene may open, by role. */
  roles: WorldRoles;
}

export interface WorldRoles {
  /** A Conversation session with an approval card (the Session scene). */
  approval: SessionUid | null;
  /** A session another app owns (the Watching ownership bar). */
  watching: SessionUid | null;
  /** A Terminal-mode session. */
  terminal: SessionUid | null;
  /** Up to 6 live sessions for Split. */
  split: SessionUid[];
  /** The first team, if any. */
  team: TeamKey | null;
}

// ---------------------------------------------------------------------------
// Sample data
// ---------------------------------------------------------------------------

export const HOME_DIR = '/Users/demo';

export const PROJECT_NAMES = ['checkout-service', 'infra-live', 'mobile-app', 'docs-site'] as const;

const TITLES = [
  'Fix the retry loop in the payment client',
  'Add rate limiting to the public API',
  'Migrate the deploy pipeline to the new runner',
  'Investigate flaky checkout integration tests',
  'Update the onboarding screens for tablets',
  'Write the release notes for 4.2',
  'Refactor the session cache eviction',
  'Rotate the staging certificates',
  'Profile the cold start of the mobile build',
  'Document the webhook signing scheme',
  'Split the auth module into separate token and session services with a shared key store',
  '결제 재시도 로직 정리',
  'Clean up unused feature flags',
  'Add tracing to the order worker',
  '배포 스크립트 점검',
  'Speed up the docs search index',
] as const;

const MEMBER_NAMES = [
  'token-agent', 'session-agent', 'db-agent', 'docs-agent', 'api-agent', 'test-agent',
  'perf-agent', 'infra-agent', 'ui-agent', 'review-agent', 'build-agent', 'data-agent',
] as const;

const TASK_TITLES = [
  'Extract the token signer',
  'Move session storage behind an interface',
  'Add key rotation tests',
  'Update the auth docs',
  'Wire the new service into the gateway',
] as const;

const COLOURS: readonly IdentityColour[] = ['blue', 'green', 'yellow', 'purple', 'orange', 'pink', 'cyan', 'red'];
const TEAM_NAMES: Readonly<Record<TeamKind, string>> = { native: 'auth-split', nativeInferred: 'cache-rework', orchestrator: 'docs-refresh' };

const MIN = 60_000;

// ---------------------------------------------------------------------------
// Building blocks
// ---------------------------------------------------------------------------

function abs(path: string): AbsPath {
  return path as AbsPath;
}

function projectRoot(name: string): string {
  return `${HOME_DIR}/code/${name}`;
}

const WORKTREE = `${HOME_DIR}/.gilde/worktrees/checkout-service/retry-fix`;

export function buildProjects(now: number, live: ReadonlyMap<string, number>): Project[] {
  return PROJECT_NAMES.map((name, order) => ({
    id: projectId(name),
    name,
    root: abs(projectRoot(name)),
    kind: name === 'docs-site' ? 'folder' : 'git',
    order,
    folded: false,
    earlier: (live.get(name) ?? 0) === 0 && name === 'docs-site',
    lastActivityAt: now - (order + 1) * 7 * MIN,
    liveSessions: live.get(name) ?? 0,
    isTemp: false,
  }));
}

export function buildLocations(): Location[] {
  const out: Location[] = PROJECT_NAMES.map((name) => ({
    id: locationId(projectRoot(name)),
    projectId: projectId(name),
    path: abs(projectRoot(name)),
    kind: name === 'docs-site' ? 'folder' : 'main',
    branch: name === 'docs-site' ? null : 'main',
    worktreeName: null,
    gildeCreated: false,
    exists: true,
  }));
  out.push({
    id: locationId(WORKTREE),
    projectId: projectId('checkout-service'),
    path: abs(WORKTREE),
    kind: 'worktree',
    branch: 'gilde/retry-fix',
    worktreeName: 'retry-fix',
    gildeCreated: true,
    exists: true,
  });
  return out;
}

const CAPS_CONVERSATION: SessionCaps = {
  send: 'conversation', interrupt: true, forceStop: true, approve: 'structured', setModel: true,
  setPermissionMode: ['default', 'acceptEdits', 'plan'], continueOptions: [], stopSession: 'claudeStop',
  terminalView: true, logView: false, changes: 'full', ownerFocus: false,
};
const CAPS_TERMINAL: SessionCaps = {
  ...CAPS_CONVERSATION, send: 'ptyPaste', approve: 'screenKeys', setModel: false, setPermissionMode: [], stopSession: 'fenced',
};
const CAPS_TURNS: SessionCaps = {
  ...CAPS_CONVERSATION, send: 'turn', approve: 'none', setPermissionMode: [], stopSession: 'fenced', terminalView: false, logView: true,
};
const CAPS_WATCHING: SessionCaps = {
  send: 'none', interrupt: false, forceStop: false, approve: 'goToOwner', setModel: false, setPermissionMode: [],
  continueOptions: ['fork'], stopSession: 'termAfterIdentity', terminalView: false, logView: false, changes: 'readOnly', ownerFocus: true,
};
const CAPS_ENDED: SessionCaps = {
  send: 'none', interrupt: false, forceStop: false, approve: 'none', setModel: false, setPermissionMode: [],
  continueOptions: ['resume', 'fork'], stopSession: 'none', terminalView: false, logView: false, changes: 'readOnly', ownerFocus: false,
};

export interface SessionSpec {
  n: number;
  vendor: Vendor;
  title: string;
  project: string;
  worktree?: boolean;
  control: Control;
  state: ActivityState;
  sinceMin: number;
  startedMin: number;
  pending?: PendingInteraction[];
  team?: Session['team'];
}

export function capsFor(control: Control): SessionCaps {
  if (control.kind === 'watching') return CAPS_WATCHING;
  if (control.kind === 'notRunning') return CAPS_ENDED;
  if (control.mode === 'terminal') return CAPS_TERMINAL;
  if (control.mode === 'turnBased') return CAPS_TURNS;
  return CAPS_CONVERSATION;
}

const MODELS: Readonly<Record<Vendor, string>> = { claude: 'sonnet', codex: 'gpt-codex', cursor: 'auto', agy: 'default' };

export function buildSession(spec: SessionSpec, now: number): Session {
  const uid = sessionUid(spec.n);
  const cwd = spec.worktree ? WORKTREE : projectRoot(spec.project);
  const live = spec.control.kind !== 'notRunning';
  const startedAt = now - spec.startedMin * MIN;
  const claude = spec.vendor === 'claude';
  const transport = spec.control.kind === 'inGilde' ? spec.control.transport : null;
  return {
    uid,
    vendor: spec.vendor,
    accountId: accountId(spec.vendor, 'default'),
    vendorSessionId: `0192f0aa-0000-4000-8000-${spec.n.toString(16).padStart(12, '0')}`,
    projectId: projectId(spec.project),
    locationId: locationId(cwd),
    cwd: abs(cwd),
    control: spec.control,
    activity: {
      state: spec.state,
      since: now - spec.sinceMin * MIN,
      confidence: spec.control.kind === 'watching' ? 'likely' : 'high',
      evidence: null,
      episode: episodeId(uid, 1),
    },
    title: spec.title,
    team: spec.team ?? null,
    startedAt,
    endedAt: live ? null : now - spec.sinceMin * MIN,
    lastActivityAt: now - spec.sinceMin * MIN,
    pending: spec.pending ?? [],
    queued: [],
    model: MODELS[spec.vendor],
    permissionMode: claude ? 'default' : null,
    binary: null,
    claudeVersion: claude ? '2.1.307' : null,
    memoryBytes: live ? 200 * 1024 * 1024 : null,
    usageToday: null,
    caps: capsFor(spec.control),
    details: {
      vendorSessionId: null,
      accountLabel: 'Default',
      configDir: claude ? '~/.claude' : null,
      cwd: cwd.replace(HOME_DIR, '~'),
      location: spec.worktree ? 'wt/retry-fix' : 'main',
      model: MODELS[spec.vendor],
      controlMode: spec.control.kind === 'inGilde' ? spec.control.mode : null,
      connection: transport,
      claudeVersion: claude ? '2.1.307' : null,
      compatState: claude ? 'verified' : null,
      binaryRealpath: null,
      versionNote: null,
      startedAt,
      memoryBytes: live ? 200 * 1024 * 1024 : null,
      transcriptPath: null,
      teamRawId: null,
      orchestratorTaskTitle: null,
      usageToday: null,
      approvalChannelUnconfirmed: null,
      followLastRowAt: null,
      teammateVersionDiffers: false,
      teamFormatUnverified: false,
    },
    mergedInto: null,
  };
}

/** An approval card for a shell command (contract §3.4.5). */
export function approvalCard(uid: SessionUid, n: number, receivedAt: number, extra: Partial<PendingInteraction> = {}): PendingInteraction {
  return {
    requestId: requestId(uid, n),
    kind: 'toolApproval',
    receivedAt,
    onBehalfOf: null,
    answerIn: null,
    title: 'Run the test suite',
    tool: { tool: 'Bash', verb: 'run', object: 'npm test -- --watch=false' },
    detailLines: [{ key: 'card.detailText', params: { text: 'npm test -- --watch=false' } }],
    // As the producer sends them (convo/approvals.rs, A25.4): `alwaysLabel` is
    // the rule content (a prefix rule), never a sentence; button 2 carries it.
    options: [
      { key: '1', label: 'Allow once', role: 'allowOnce' },
      { key: '2', label: 'npm test:*', role: 'allowSession' },
      { key: '3', label: 'Deny', role: 'deny' },
    ],
    alwaysLabel: 'npm test:*',
    defaultToNo: false,
    degraded: false,
    promptRev: null,
    confidence: 'high',
    multiSelect: false,
    questions: [],
    ...extra,
  };
}

/** A question card (AskUserQuestion, §6.1.4). */
export function questionCard(uid: SessionUid, n: number, receivedAt: number): PendingInteraction {
  return {
    requestId: requestId(uid, n),
    kind: 'question',
    receivedAt,
    onBehalfOf: null,
    answerIn: null,
    title: 'Which database should the cache use?',
    tool: { tool: 'AskUserQuestion', verb: 'ask', object: null },
    detailLines: [],
    options: [],
    alwaysLabel: null,
    defaultToNo: false,
    degraded: false,
    promptRev: null,
    confidence: 'high',
    multiSelect: false,
    questions: [{ text: 'Which database should the cache use?', options: ['Redis', 'Postgres', 'In-memory only'], multiSelect: false }],
  };
}

// ---------------------------------------------------------------------------
// Attention queue (§8.1 Q1–Q8, computed the way the core does)
// ---------------------------------------------------------------------------

/** The band of a session's state (§3.5.7), or null when it does not queue. */
export function bandOf(session: Session): number | null {
  switch (session.activity.state.kind) {
    case 'needsApproval':
      return 1;
    case 'needsAnswer':
      return 2;
    case 'error':
    case 'crashed':
      return 3;
    case 'noProgress':
      return 4;
    case 'newReply':
      return 5;
    default:
      return null;
  }
}

function reasonOf(session: Session): AttentionReason {
  const s = session.activity.state;
  switch (s.kind) {
    case 'needsApproval':
      return { kind: 'needsApproval' };
    case 'needsAnswer':
      return { kind: 'needsAnswer' };
    case 'error':
      return { kind: 'error', sub: s.sub };
    case 'crashed':
      return { kind: 'crashed' };
    case 'noProgress':
      return { kind: 'noProgress', forSecs: s.forSecs };
    default:
      return { kind: 'newReply' };
  }
}

function primaryOf(session: Session): PrimaryAction {
  const s = session.activity.state;
  const watching = session.control.kind === 'watching';
  switch (s.kind) {
    case 'needsApproval':
    case 'needsAnswer':
      return watching ? 'goToOwner' : 'answer';
    case 'error':
      if (s.sub.kind === 'usageLimit') return 'continueOnOtherAccount';
      if (s.sub.kind === 'signInNeeded') return 'signIn';
      return 'retry';
    case 'crashed':
      return 'continueInGilde';
    case 'noProgress':
      return 'open';
    default:
      return 'review';
  }
}

/**
 * Q1–Q8: one item per session in its band, ranked; one aggregated leftover
 * item. `later` maps uids to their Later deadline (Q5, memory only).
 */
export function computeQueue(
  sessions: readonly Session[],
  leftovers: readonly ProcessRecord[],
  rev: number,
  now: number,
  later: ReadonlyMap<SessionUid, number> = new Map(),
): AttentionQueue {
  const items: AttentionItem[] = [];
  for (const s of sessions) {
    if (s.mergedInto !== null) continue;
    const band = bandOf(s);
    if (band === null) continue;
    const card = s.pending[0] ?? null;
    const until = later.get(s.uid) ?? null;
    items.push({
      key: `q_${s.uid}`,
      band,
      subject: { kind: 'session', uid: s.uid, onBehalfOf: card?.onBehalfOf ?? null, answerIn: card?.answerIn ?? null },
      reason: reasonOf(s),
      since: s.activity.since,
      waitedMs: Math.max(0, now - s.activity.since),
      episode: s.activity.episode,
      laterUntil: until !== null && until > now ? until : null,
      primary: primaryOf(s),
      watched: s.control.kind === 'watching',
    });
  }
  const held = leftovers.filter((p) => p.leftover !== null && p.leftover.heldScans >= 2);
  if (held.length > 0) {
    items.push({
      key: 'q_leftovers',
      band: 6,
      subject: { kind: 'leftovers', count: held.length, bytes: held.reduce((n, p) => n + p.rssBytes, 0) },
      reason: { kind: 'leftovers' },
      since: Math.min(...held.map((p) => p.leftover?.since ?? now)),
      waitedMs: 0,
      episode: 'e_leftovers' as AttentionItem['episode'],
      laterUntil: null,
      primary: 'cleanUp',
      watched: false,
    });
  }
  items.sort((a, b) => {
    if (a.band !== b.band) return a.band - b.band;
    const newestFirst = a.band === 3 || a.band === 5;
    if (a.since !== b.since) return newestFirst ? b.since - a.since : a.since - b.since;
    return a.key < b.key ? -1 : a.key > b.key ? 1 : 0;
  });
  const visible = items.filter((i) => i.laterUntil === null);
  return {
    rev,
    items,
    needYou: visible.filter((i) => i.band <= 2).length,
    also: visible.filter((i) => i.band >= 3).length,
    pausedUntil: null,
  };
}

// ---------------------------------------------------------------------------
// Teams
// ---------------------------------------------------------------------------

function teamCaps(kind: TeamKind): TeamCapabilities {
  if (kind === 'orchestrator') {
    return {
      observeMembers: 'yes', observeMemberState: 'yes', observeMessages: 'no', observeInFlight: 'no', observeTasks: 'yes',
      observeApprovals: 'no', observeQuestions: 'no', actSend: [], actBroadcast: [], actSpawn: [], actStop: [],
      actAnswerApproval: 'none', actTaskWrite: 'none', correlationReport: 'perRequest', durabilityTaskBoard: 'durable',
      durabilityMessageLog: 'transient', fidelityNote: null,
    };
  }
  const hosted = kind === 'native';
  return {
    observeMembers: 'yes', observeMemberState: hosted ? 'yes' : 'partial', observeMessages: 'yes', observeInFlight: 'partial',
    observeTasks: kind === 'nativeInferred' ? 'inferred' : 'yes', observeApprovals: 'yes', observeQuestions: 'partial',
    actSend: hosted ? ['viaLead', 'viaMemberPane'] : ['viaLead'], actBroadcast: ['viaLead'], actSpawn: ['viaLead'],
    actStop: hosted ? ['viaLead', 'closePane'] : ['viaLead'], actAnswerApproval: 'inLeadUi', actTaskWrite: 'none',
    correlationReport: 'perTurn', durabilityTaskBoard: kind === 'nativeInferred' ? 'derived' : 'durable',
    durabilityMessageLog: 'durable', fidelityNote: null,
  };
}

interface TeamBuild {
  team: Team;
  members: Member[];
  sessions: Session[];
  tasks: Task[];
  messages: TeamMessage[];
  bodies: [MessageKey, string][];
  titles: [TaskKey, string][];
}

/** A team of `size` agents (lead + members), session numbers from `firstN`. */
function buildTeam(kind: TeamKind, size: number, firstN: number, now: number, pending: boolean, withPanes = false): TeamBuild {
  const name = TEAM_NAMES[kind];
  const key = teamKey(name);
  const project = kind === 'orchestrator' ? 'docs-site' : kind === 'native' ? 'checkout-service' : 'infra-live';
  const members: Member[] = [];
  const sessions: Session[] = [];
  const leadName = kind === 'orchestrator' ? 'coordinator' : 'team-lead';
  const leadKey = memberKey(name, leadName);
  const leadUid = sessionUid(firstN);

  for (let i = 0; i < size; i++) {
    const lead = i === 0;
    const base = MEMBER_NAMES[(i - 1 + MEMBER_NAMES.length) % MEMBER_NAMES.length] ?? 'agent';
    const mname = lead ? leadName : i > MEMBER_NAMES.length ? `${base}-${i}` : base;
    const mkey = lead ? leadKey : memberKey(name, mname);
    const n = firstN + i;
    const uid = sessionUid(n);
    const colour = lead ? null : (COLOURS[(i - 1) % COLOURS.length] ?? null);
    let control: Control;
    if (kind === 'native') control = { kind: 'inGilde', mode: 'terminal', pty: ptyId(uid), hostedTeam: true, transport: 'pty' };
    else if (kind === 'nativeInferred') control = { kind: 'watching', origin: 'itermTeam', owner: { appPid: 4100, appName: 'iTerm2', kind: 'terminal', bundleId: 'com.googlecode.iterm2' } };
    else control = { kind: 'watching', origin: 'orchestrator', owner: null };
    const vendor: Vendor = kind === 'orchestrator' ? (i % 2 === 0 ? 'claude' : 'codex') : 'claude';
    let state: ActivityState = i % 3 === 2 ? { kind: 'ready' } : { kind: 'working' };
    let cards: PendingInteraction[] = [];
    if (pending && kind === 'native' && i === 2) {
      // Q3: the lead prompts for its teammate; the member's item answers in the lead.
      state = { kind: 'needsApproval' };
      cards = [approvalCard(uid, 1, now - 3 * MIN, {
        onBehalfOf: mkey,
        answerIn: leadUid,
        title: 'Edit src/auth/token.ts',
        tool: { tool: 'Edit', verb: 'edit', object: 'src/auth/token.ts' },
        detailLines: [{ key: 'card.detailText', params: { text: 'src/auth/token.ts' } }],
        // Keys 1 and 3 — numbers never shift when button 2 is hidden (§3.4.5).
        options: [
          { key: '1', label: 'Allow once', role: 'allowOnce' },
          { key: '3', label: 'Deny', role: 'deny' },
        ],
        alwaysLabel: null,
      })];
    }
    if (kind === 'nativeInferred' && i === 3) state = { kind: 'noProgress', forSecs: 720 };
    sessions.push(buildSession({
      n, vendor, project, control, state,
      title: lead ? `Coordinate ${name}` : (TITLES[(n + 3) % TITLES.length] ?? 'Task'),
      sinceMin: 2 + (i % 5), startedMin: 40 + i, pending: cards,
      team: { teamKey: key, memberKey: mkey, role: lead ? 'lead' : 'member', colour },
    }, now));
    members.push({
      key: mkey,
      teamKey: key,
      sessionUid: uid,
      name: mname,
      agentId: `${mname}@${name}`,
      role: lead ? 'lead' : 'member',
      colour,
      vendor,
      backend: kind === 'native' ? (lead ? 'inProcess' : 'pane') : kind === 'orchestrator' ? 'orchestrator' : 'inProcess',
      joinedAt: now - (40 - i) * MIN,
      presence: { kind: 'present' },
      pane: kind === 'native' && !lead ? paneId(i) : null,
      planModeRequired: false,
      currentTask: null,
    });
  }

  const tasks: Task[] = [];
  const titles: [TaskKey, string][] = [];
  const inferred = kind === 'nativeInferred';
  const nTasks = Math.max(2, size + 1);
  for (let t = 0; t < nTasks; t++) {
    const tkey = taskKey(name, t + 1);
    const owner = members[1 + (t % Math.max(1, members.length - 1))]?.key ?? null;
    const status = t < 2 ? 'done' : t % 3 === 0 ? 'open' : 'inProgress';
    tasks.push({
      key: tkey,
      teamKey: key,
      nativeId: inferred ? null : String(t + 1),
      title: inferred ? null : (TASK_TITLES[t % TASK_TITLES.length] ?? null),
      inferredK: inferred ? t + 1 : null,
      status,
      station: status === 'done' ? (inferred ? 'reported' : 'done') : status === 'open' ? (inferred ? 'assigned' : 'toDo') : 'inProgress',
      owner,
      blockedBy: [],
      blocks: [],
      createdAt: now - (60 - t) * MIN,
      startedAt: status === 'open' ? null : now - (50 - t) * MIN,
      completedAt: status === 'done' ? now - (20 - t) * MIN : null,
      firstSeenInProgressAt: null,
      source: inferred ? 'inferred' : kind === 'orchestrator' ? 'orchestrator' : 'file',
      inferred,
    });
    if (!inferred) titles.push([tkey, TASK_TITLES[t % TASK_TITLES.length] ?? 'Task']);
  }
  // A declared dependency (never on inferred items, §3.4.8): task 4 waits for task 3.
  const t3 = tasks[2];
  const t4 = tasks[3];
  if (!inferred && t3 && t4) {
    t3.blocks = [t4.key];
    t4.blockedBy = [t3.key];
    if (t4.status === 'open') t4.station = 'blocked';
  }
  for (const m of members) {
    const own = tasks.find((t) => t.owner === m.key && t.status === 'inProgress');
    if (own) m.currentTask = own.key;
  }

  const messages: TeamMessage[] = [];
  const bodies: [MessageKey, string][] = [];
  if (kind !== 'orchestrator') {
    const kinds: MessageKind[] = ['taskAssignment', 'plain', 'idleNotification', 'plain'];
    members.slice(1).forEach((m, i) => {
      const k = kinds[i % kinds.length] ?? 'plain';
      const toLead = i % 2 === 1;
      const mk = messageKey(name, i + 1);
      messages.push({
        key: mk,
        teamKey: key,
        from: toLead ? { kind: 'member', member: m.key } : { kind: 'member', member: leadKey },
        to: toLead ? { kind: 'member', member: leadKey } : { kind: 'member', member: m.key },
        at: now - (30 - i) * MIN,
        kind: k,
        status: 'delivered',
        inReplyTo: null,
        derived: kind === 'nativeInferred',
        idleReason: k === 'idleNotification' ? 'available' : null,
        hasSummary: k === 'plain',
      });
      bodies.push([mk, toLead ? 'Done with the first pass. Two tests still fail on the refresh path.' : 'Please take the next task on the board when you are free.']);
    });
  }

  const done = tasks.filter((t) => t.status === 'done').length;
  const team: Team = {
    key,
    source: kind === 'orchestrator' ? 'orchestrator' : 'claudeNative',
    name,
    rawId: `session-${key.slice(2, 10)}`,
    projectId: projectId(project),
    lead: leadKey,
    leadBinding: kind === 'native' ? 'hosted' : kind === 'orchestrator' ? 'none' : 'exact',
    hosted: kind === 'native',
    externalHost: kind === 'nativeInferred' ? 'iTerm2' : null,
    caps: teamCaps(kind),
    glyphState: sessions.some((s) => s.activity.state.kind === 'needsApproval') ? { kind: 'needsApproval' } : { kind: 'working' },
    progress: { done, total: tasks.length, inferred, approx: inferred, inFlight: kind === 'native' ? null : 1, bottlenecks: [] },
    startedAt: now - 45 * MIN,
    endedAt: null,
    paneHosting: kind === 'native' ? 'hosted' : kind === 'nativeInferred' ? 'outsideGilde' : 'notApplicable',
    rosterStale: false,
    paneTiles: kind === 'native' && withPanes ? paneTileFixtures(size, firstN, now) : [],
  };
  return { team, members, sessions, tasks, messages, bodies, titles };
}

/** A30.1 pane tiles of a hosted team: starting, spawn failed, a running unknown pane (its terminal read-only). */
function paneTileFixtures(size: number, firstN: number, now: number): PaneTile[] {
  return [
    { paneId: paneId(size + 1), state: { kind: 'starting', since: now, spawnFailed: false }, title: null, pty: null, createdAt: now - 4_000 },
    { paneId: paneId(size + 2), state: { kind: 'starting', since: now - 6_000, spawnFailed: true }, title: null, pty: null, createdAt: now - 3_000 },
    {
      paneId: paneId(size + 3),
      state: { kind: 'unknown', running: true, exitCode: null },
      title: 'dev-server',
      pty: ptyId(sessionUid(firstN + 90)),
      createdAt: now - 2_000,
    },
  ];
}

// ---------------------------------------------------------------------------
// Solo sessions, accounts, leftovers, content
// ---------------------------------------------------------------------------

type SoloShape = { vendor: Vendor; control: 'conversation' | 'terminal' | 'turns' | 'vsCode' | 'terminalApp'; state: ActivityState };

/**
 * The first solo sessions of every world, each with a job: 0 waits for an
 * approval (the Session scene), 1 is the session that asks at 5 s, 2 is owned
 * by another app (Watching), 3 is a Terminal-mode PTY, 4 waits for an answer,
 * 5 gets a reply at 27 s, 6 ends at 30 s; the rest spread the other states.
 * Larger worlds cycle the table.
 */
const SOLO: readonly SoloShape[] = [
  { vendor: 'claude', control: 'conversation', state: { kind: 'needsApproval' } },
  { vendor: 'claude', control: 'conversation', state: { kind: 'working' } },
  { vendor: 'claude', control: 'vsCode', state: { kind: 'working' } },
  { vendor: 'cursor', control: 'terminal', state: { kind: 'working' } },
  { vendor: 'claude', control: 'conversation', state: { kind: 'needsAnswer' } },
  { vendor: 'codex', control: 'turns', state: { kind: 'ready' } },
  { vendor: 'claude', control: 'conversation', state: { kind: 'working' } },
  { vendor: 'claude', control: 'conversation', state: { kind: 'error', sub: { kind: 'usageLimit', accountId: null, resetsAt: null } } },
  { vendor: 'agy', control: 'turns', state: { kind: 'newReply' } },
  { vendor: 'claude', control: 'terminal', state: { kind: 'ready' } },
  { vendor: 'codex', control: 'turns', state: { kind: 'starting' } },
  { vendor: 'claude', control: 'terminalApp', state: { kind: 'noProgress', forSecs: 900 } },
];

/** Solo session `i` of a world. */
function soloSpec(i: number, n: number, pending: boolean): SessionSpec {
  const shape = SOLO[i % SOLO.length] ?? { vendor: 'claude', control: 'conversation', state: { kind: 'ready' } };
  const vendor = shape.vendor;
  const project = PROJECT_NAMES[i % 3] ?? 'checkout-service';
  const title = TITLES[i % TITLES.length] ?? 'Task';
  const uid = sessionUid(n);
  let control: Control;
  switch (shape.control) {
    case 'turns':
      control = { kind: 'inGilde', mode: 'turnBased', pty: null, hostedTeam: false, transport: 'turns' };
      break;
    case 'terminal':
      control = { kind: 'inGilde', mode: 'terminal', pty: ptyId(uid), hostedTeam: false, transport: 'pty' };
      break;
    case 'vsCode':
      control = { kind: 'watching', origin: 'vsCode', owner: { appPid: 3200, appName: 'Visual Studio Code', kind: 'vsCode', bundleId: 'com.microsoft.VSCode' } };
      break;
    case 'terminalApp':
      control = { kind: 'watching', origin: 'terminal', owner: { appPid: 3300, appName: 'Terminal', kind: 'terminal', bundleId: 'com.apple.Terminal' } };
      break;
    default:
      control = { kind: 'inGilde', mode: 'conversation', pty: null, hostedTeam: false, transport: 'l1' };
  }
  let state: ActivityState = shape.state;
  let cards: PendingInteraction[] = [];
  if (state.kind === 'needsApproval') {
    if (pending) cards = [approvalCard(uid, 1, 0)];
    else state = { kind: 'working' };
  } else if (state.kind === 'needsAnswer') {
    if (pending) cards = [questionCard(uid, 1, 0)];
    else state = { kind: 'ready' };
  } else if (state.kind === 'error' && state.sub.kind === 'usageLimit') {
    state = { kind: 'error', sub: { kind: 'usageLimit', accountId: accountId(vendor, 'default'), resetsAt: null } };
  }
  return { n, vendor, title, project, worktree: i === 0, control, state, sinceMin: 1 + (i % 9), startedMin: 15 + i * 3, pending: cards };
}

function accounts(now: number, quotaHot = false): Account[] {
  const quota = (five: number, week: number) => ({
    windows: [
      { kind: 'fiveHour' as const, label: null, usedPct: five, resetsAt: now + 95 * MIN },
      { kind: 'week' as const, label: null, usedPct: week, resetsAt: now + 3 * 24 * 60 * MIN },
    ],
    fetchedAt: now - 2 * MIN,
    stale: false,
    unavailable: null,
  });
  return [
    { id: accountId('claude', 'default'), vendor: 'claude', configDir: abs(`${HOME_DIR}/.claude`), label: 'Default', isDefault: true, source: 'ambient', usableForNew: true, signIn: 'signedIn', quota: quota(quotaHot ? 95 : 42, 63), liveSessions: 0 },
    { id: accountId('claude', 'work'), vendor: 'claude', configDir: abs(`${HOME_DIR}/.claude-work`), label: 'Work', isDefault: false, source: 'user', usableForNew: true, signIn: 'signedIn', quota: quota(8, 21), liveSessions: 0 },
    { id: accountId('codex', 'default'), vendor: 'codex', configDir: abs(`${HOME_DIR}/.codex`), label: 'Default', isDefault: true, source: 'ambient', usableForNew: true, signIn: 'signedIn', quota: null, liveSessions: 0 },
    { id: accountId('cursor', 'default'), vendor: 'cursor', configDir: null, label: 'Default', isDefault: true, source: 'ambient', usableForNew: true, signIn: 'signedIn', quota: null, liveSessions: 0 },
    { id: accountId('agy', 'default'), vendor: 'agy', configDir: null, label: 'Default', isDefault: true, source: 'ambient', usableForNew: true, signIn: 'unknown', quota: null, liveSessions: 0 },
  ];
}

function leftoverRecords(count: number, now: number): ProcessRecord[] {
  const start = Math.floor(now / 1000) - 7200;
  return Array.from({ length: count }, (_, i): ProcessRecord => ({
    key: processKey(52000 + i, start - i),
    pid: 52000 + i,
    startSec: start - i,
    vendor: 'claude',
    role: i === 0 ? 'teammate' : 'toolServer',
    sessionUid: null,
    gildeOwned: false,
    rssBytes: (i === 0 ? 300 : 100) * 1024 * 1024,
    leftover: { reason: i === 0 ? 'teammateOfGoneLead' : 'toolServerOfEndedSession', since: now - 25 * MIN, heldScans: 2 },
  }));
}

function sources(): SourceStatus[] {
  const row = (vendor: Vendor, source: SourceStatus['source']): SourceStatus => ({
    vendor, source, state: { kind: 'ok' }, lastReconcileAt: null, hiddenStale: 0, indexed: 0, truncated: false,
  });
  return [
    row('claude', 'registry'), row('claude', 'processes'), row('claude', 'teams'), row('claude', 'transcripts'),
    row('codex', 'rollouts'), row('cursor', 'chats'), row('agy', 'summaries'),
  ];
}

export const COMPAT: ClaudeCompat = {
  version: '2.1.307',
  conversation: 'verified',
  paneHosting: 'verified',
  source: 'shipped',
  staticCanary: { kind: 'pass' },
  handshakeOk: true,
  belowMinimum: false,
  note: null,
  features: { handshakeShape: true, allowSession: true, denyAndStop: true, nowPriority: true, interruptReceipt: true, sessionStateEvents: true },
};

/** The conversation of one session: a short, plausible exchange in blocks (§3.7.1). */
export function conversationOf(session: Session, now: number): Block[] {
  const uid = session.uid;
  let seq = 0;
  const block = (kind: BlockKind, minAgo: number): Block => {
    seq += 1;
    return { id: blockId(uid, seq), seq, at: now - minAgo * MIN, kind, truncated: false };
  };
  const tool = (n: number) => `toolu_${uid.slice(-8)}_${n}`;
  const out: Block[] = [
    block({ kind: 'userText', text: session.title ?? 'Continue', origin: 'human', delivery: 'delivered', clientMsgId: null }, 12),
    block({ kind: 'thinking', seconds: 4, text: null }, 11),
    block({ kind: 'toolCall', toolUseId: tool(1), summary: { tool: 'Read', verb: 'read', object: 'src/client/retry.ts' }, inputPreview: 'src/client/retry.ts', status: 'ok', resultPreview: '128 lines', fileChange: null, incomplete: false }, 11),
    block({
      kind: 'assistantText',
      markdown:
        'The retry loop never resets its backoff after a success, so one slow response keeps every later call waiting.\n\n' +
        'Plan:\n\n1. Reset the attempt counter on success.\n2. Cap the delay at 8 seconds.\n3. Add a test for the reset.\n\n' +
        '```ts\nif (ok) attempt = 0;\n```',
      streaming: false,
      agent: null,
    }, 10),
    block({ kind: 'toolCall', toolUseId: tool(2), summary: { tool: 'Edit', verb: 'edit', object: 'src/client/retry.ts' }, inputPreview: 'src/client/retry.ts', status: 'ok', resultPreview: null, fileChange: { pathRel: 'src/client/retry.ts', added: 6, removed: 2, change: 'modified' }, incomplete: false }, 8),
  ];
  const card = session.pending[0];
  if (card) {
    out.push(block({ kind: 'interaction', requestId: card.requestId, interactionKind: card.kind, outcome: 'pending' }, 3));
  } else if (session.activity.state.kind === 'working') {
    out.push(block({ kind: 'toolCall', toolUseId: tool(3), summary: { tool: 'Bash', verb: 'run', object: 'npm test -- retry' }, inputPreview: 'npm test -- retry', status: 'running', resultPreview: null, fileChange: null, incomplete: false }, 1));
  } else {
    out.push(block({ kind: 'assistantText', markdown: 'Done. The backoff now resets after a success and the new test passes.', streaming: false, agent: null }, 2));
  }
  return out;
}

/**
 * A History row's conversation, as the core's history fallback of
 * `convo_subscribe` delivers it for a session World does not hold (A29.2).
 */
export function historyConversationOf(row: HistoryRow, now: number): Block[] {
  const at = row.startedAt ?? now;
  const block = (seq: number, kind: BlockKind, offsetMin: number): Block => ({ id: blockId(row.uid, seq), seq, at: at + offsetMin * MIN, kind, truncated: false });
  return [
    block(1, { kind: 'userText', text: row.title ?? 'Continue', origin: 'human', delivery: 'delivered', clientMsgId: null }, 0),
    block(2, { kind: 'assistantText', markdown: 'I looked at the failing path first and kept the change small.', streaming: false, agent: null }, 1),
    block(3, { kind: 'assistantText', markdown: 'Done. The change is merged and the tests pass.', streaming: false, agent: null }, 38),
  ];
}

function historyRows(count: number, now: number): HistoryRow[] {
  const vendors: readonly Vendor[] = ['claude', 'codex', 'claude', 'cursor', 'agy'];
  return Array.from({ length: count }, (_, i): HistoryRow => {
    const vendor = vendors[i % vendors.length] ?? 'claude';
    const titleOnly = vendor === 'cursor' || vendor === 'agy';
    const startedAt = now - (i + 1) * 3 * 60 * MIN;
    return {
      uid: sessionUid(5000 + i),
      vendor,
      projectId: projectId(PROJECT_NAMES[i % PROJECT_NAMES.length] ?? 'checkout-service'),
      accountId: accountId(vendor, 'default'),
      origin: i % 3 === 0 ? 'terminal' : i % 3 === 1 ? 'vsCode' : 'headless',
      startedAt,
      endedAt: startedAt + 40 * MIN,
      running: false,
      titleOnly,
      label: titleOnly ? 'vendorDate' : 'title',
      title: titleOnly ? null : (TITLES[(i + 5) % TITLES.length] ?? null),
      continueOptions: titleOnly ? [] : ['resume', 'fork'],
      continuedIn: null,
    };
  });
}

// ---------------------------------------------------------------------------
// The world
// ---------------------------------------------------------------------------

/** Agents per team: half of all agents shared among the teams, 2–12 each. */
export function teamSizes(agents: number, teams: readonly TeamKind[]): number[] {
  if (agents < 3 || teams.length === 0) return [];
  const each = Math.max(2, Math.min(12, Math.round((agents * 0.5) / teams.length)));
  const sizes: number[] = [];
  let left = agents;
  for (let i = 0; i < teams.length && left >= 2; i++) {
    const size = Math.min(each, left);
    sizes.push(size);
    left -= size;
  }
  return sizes;
}

/** Live sessions per project; a team counts once [ux-ia §2.5 Counts]. */
export function liveCounts(sessions: readonly Session[]): Map<string, number> {
  const live = new Map<string, number>();
  const counted = new Set<string>();
  for (const s of sessions) {
    if (s.control.kind === 'notRunning' || s.mergedInto !== null) continue;
    const project = PROJECT_NAMES.find((p) => projectId(p) === s.projectId);
    if (!project) continue;
    if (s.team) {
      const key = `${project}\0${s.team.teamKey}`;
      if (counted.has(key)) continue;
      counted.add(key);
    }
    live.set(project, (live.get(project) ?? 0) + 1);
  }
  return live;
}

export function buildWorld(options: WorldOptions, now: number): ShowcaseWorld {
  const sessions: Session[] = [];
  const teams: Team[] = [];
  const members: Member[] = [];
  const tasks: Task[] = [];
  const messages: TeamMessage[] = [];
  const bodies = new Map<MessageKey, string>();
  const titles = new Map<TaskKey, string>();

  const sizes = teamSizes(options.agents, options.teams);
  sizes.forEach((size, i) => {
    const kind = options.teams[i] ?? 'native';
    const t = buildTeam(kind, size, 100 * (i + 1), now, options.pending, options.paneTiles === true);
    teams.push(t.team);
    members.push(...t.members);
    sessions.push(...t.sessions);
    tasks.push(...t.tasks);
    messages.push(...t.messages);
    for (const [k, v] of t.bodies) bodies.set(k, v);
    for (const [k, v] of t.titles) titles.set(k, v);
  });
  const solo = Math.max(0, options.agents - sizes.reduce((a, b) => a + b, 0));
  for (let i = 0; i < solo; i++) {
    const s = buildSession(soloSpec(i, i + 1, options.pending), now);
    // Cards arrived a few minutes ago, the earlier sessions first (Q4 ranks by wait).
    s.pending = s.pending.map((c, k) => ({ ...c, receivedAt: now - (4 + i + k) * MIN }));
    sessions.push(s);
  }

  const accountList = accounts(now, options.quotaHot ?? false);
  for (const a of accountList) a.liveSessions = sessions.filter((s) => s.accountId === a.id && s.control.kind !== 'notRunning').length;
  const leftovers = leftoverRecords(options.leftovers, now);
  const rev = 1;
  const snapshot: StoreSnapshot = {
    rev,
    projects: buildProjects(now, liveCounts(sessions)),
    locations: buildLocations(),
    sessions,
    teams,
    members,
    tasks,
    messages,
    queue: computeQueue(sessions, leftovers, rev, now),
    accounts: accountList,
    leftovers,
    compat: [COMPAT],
    notices: [],
    boot: {
      spawnGateOpen: true,
      firstDiscoveryDone: true,
      agentsDetected: true,
      memoryOnly: null,
      agents: [
        { vendor: 'claude', installed: true, version: '2.1.307' },
        { vendor: 'codex', installed: true, version: '0.47.0' },
        { vendor: 'cursor', installed: false, version: null },
        { vendor: 'agy', installed: false, version: null },
      ],
      agentsScannedAt: now - 2 * MIN,
      recovery: options.recovery ? { stopping: 3, undoToken: RECOVERY_UNDO, until: now + RECOVERY_WINDOW_MS } : null,
    },
    sources: sources(),
    integrations: { orchestrator: teams.some((t) => t.source === 'orchestrator') ? 'present' : 'absent' },
  };

  const convo = new Map<SessionUid, readonly Block[]>();
  for (const s of sessions) convo.set(s.uid, conversationOf(s, now));

  const liveSolo = sessions.filter((s) => !s.team && s.control.kind !== 'notRunning');
  const roles: WorldRoles = {
    approval: sessions.find((s) => s.pending.some((c) => c.kind === 'toolApproval') && s.control.kind === 'inGilde' && s.control.mode === 'conversation')?.uid ?? null,
    watching: sessions.find((s) => s.control.kind === 'watching' && !s.team)?.uid ?? null,
    terminal: sessions.find((s) => s.control.kind === 'inGilde' && s.control.mode === 'terminal' && !s.team)?.uid ?? null,
    split: liveSolo.slice(0, 6).map((s) => s.uid),
    team: teams[0]?.key ?? null,
  };

  return { now, snapshot, convo, messageBodies: bodies, taskTitles: titles, history: historyRows(options.history, now), roles };
}

/** Members of a team in roster order (lead first). */
export function teamMembers(world: ShowcaseWorld, key: TeamKey): Member[] {
  return world.snapshot.members.filter((m) => m.teamKey === key);
}

/** The member key of a session, if it is a team member. */
export function memberOf(world: ShowcaseWorld, uid: SessionUid): MemberKey | null {
  return world.snapshot.members.find((m) => m.sessionUid === uid)?.key ?? null;
}
