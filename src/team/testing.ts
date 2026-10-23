/**
 * Team fixtures for the Team screen's tests: complete, valid contract values
 * with neutral defaults (a Gilde-hosted Claude team with a lead in a PTY and
 * pane teammates). Imported by tests only.
 */

import type { Member } from '../lib/gen/Member';
import type { MemberKey } from '../lib/gen/MemberKey';
import type { MessageKey } from '../lib/gen/MessageKey';
import type { PtyId } from '../lib/gen/PtyId';
import type { Session } from '../lib/gen/Session';
import type { Task } from '../lib/gen/Task';
import type { TaskKey } from '../lib/gen/TaskKey';
import type { Team } from '../lib/gen/Team';
import type { TeamCapabilities } from '../lib/gen/TeamCapabilities';
import type { TeamKey } from '../lib/gen/TeamKey';
import type { TeamMessage } from '../lib/gen/TeamMessage';
import { session, uid } from '../test/fixtures';

export const TK = 't_00000000000000a1' as TeamKey;

function hex16(n: number): string {
  return n.toString(16).padStart(16, '0');
}

/** A member key from a small number (`m_<16 hex>`). */
export function mkey(n: number): MemberKey {
  return `m_${hex16(n)}` as MemberKey;
}

export function tkey(n: number): TaskKey {
  return `k_${hex16(n)}` as TaskKey;
}

export function gkey(n: number): MessageKey {
  return `g_${hex16(n)}` as MessageKey;
}

/** A Gilde-hosted Claude team on 2.1.307: panes, via-lead acts, no task tools (inferred board), partial messages. */
export function hostedCaps(extra: Partial<TeamCapabilities> = {}): TeamCapabilities {
  return {
    observeMembers: 'yes',
    observeMemberState: 'partial',
    observeMessages: 'partial',
    observeInFlight: 'partial',
    observeTasks: 'inferred',
    observeApprovals: 'yes',
    observeQuestions: 'yes',
    actSend: ['viaMemberPane', 'viaLead'],
    actBroadcast: ['viaLead'],
    actSpawn: ['viaLead'],
    actStop: ['viaLead', 'closePane'],
    actAnswerApproval: 'inLeadUi',
    actTaskWrite: 'none',
    correlationReport: 'perTurn',
    durabilityTaskBoard: 'derived',
    durabilityMessageLog: 'derived',
    fidelityNote: null,
    ...extra,
  };
}

export function team(extra: Partial<Team> = {}): Team {
  return {
    key: TK,
    source: 'claudeNative',
    name: 'auth-split',
    rawId: 'session-25ae1157',
    projectId: null,
    lead: mkey(1),
    leadBinding: 'hosted',
    hosted: true,
    externalHost: null,
    caps: hostedCaps(),
    glyphState: { kind: 'working' },
    progress: { done: 0, total: 0, inferred: true, approx: false, inFlight: null, bottlenecks: [] },
    startedAt: null,
    endedAt: null,
    paneHosting: 'hosted',
    rosterStale: false,
    paneTiles: [],
    ...extra,
  };
}

/** The lead: member 1, session 1. */
export function lead(extra: Partial<Member> = {}): Member {
  return member(1, 'team-lead', { role: 'lead', backend: 'inProcess', pane: null, colour: null, ...extra });
}

/** A pane teammate: member n, session n, pane %n. */
export function member(n: number, name: string, extra: Partial<Member> = {}): Member {
  return {
    key: mkey(n),
    teamKey: TK,
    sessionUid: uid(n),
    name,
    agentId: `${name}@auth-split`,
    role: 'member',
    colour: 'blue',
    vendor: 'claude',
    backend: 'pane',
    joinedAt: 1_000 + n,
    presence: { kind: 'present' },
    pane: `%${n}` as Member['pane'],
    planModeRequired: false,
    currentTask: null,
    ...extra,
  };
}

/** A session In Gilde in Terminal mode (a hosted lead or pane teammate). */
export function paneSession(n: number, extra: Partial<Session> = {}, role: 'lead' | 'member' = 'member'): Session {
  const base = session(n);
  return session(n, {
    title: role === 'lead' ? 'team-lead' : `mate ${n}`,
    control: { kind: 'inGilde', mode: 'terminal', pty: `y_0192f000-0000-7000-8000-00000000000${n}` as PtyId, hostedTeam: true, transport: 'pty' },
    caps: { ...base.caps, send: 'ptyPaste', approve: 'screenKeys', terminalView: true },
    team: { teamKey: TK, memberKey: mkey(n), role, colour: role === 'lead' ? null : 'blue' },
    ...extra,
  });
}

/** A watched session (an external team's member). */
export function watchedSession(n: number, appName = 'iTerm2', extra: Partial<Session> = {}): Session {
  const base = session(n);
  return session(n, {
    control: { kind: 'watching', origin: 'itermTeam', owner: { appPid: 41, appName, kind: 'terminal', bundleId: 'com.googlecode.iterm2' } },
    caps: { ...base.caps, send: 'none', approve: 'goToOwner', ownerFocus: true, interrupt: false },
    ...extra,
  });
}

export function inferredItem(n: number, owner: MemberKey | null, k: number, station: Task['station'] = 'assigned'): Task {
  return {
    key: tkey(n),
    teamKey: TK,
    nativeId: null,
    title: null,
    inferredK: k,
    status: station === 'reported' ? 'done' : 'inProgress',
    station,
    owner,
    blockedBy: [],
    blocks: [],
    createdAt: 1_000 + n,
    startedAt: null,
    completedAt: null,
    firstSeenInProgressAt: null,
    source: 'inferred',
    inferred: true,
  };
}

export function boardTask(n: number, title: string | null, status: Task['status'], extra: Partial<Task> = {}): Task {
  return {
    key: tkey(n),
    teamKey: TK,
    nativeId: String(n),
    title,
    inferredK: null,
    status,
    station: status === 'done' ? 'done' : status === 'inProgress' ? 'inProgress' : 'toDo',
    owner: null,
    blockedBy: [],
    blocks: [],
    createdAt: 1_000 + n,
    startedAt: null,
    completedAt: null,
    firstSeenInProgressAt: null,
    source: 'transcript',
    inferred: false,
    ...extra,
  };
}

export function message(n: number, from: MemberKey | 'user' | 'system', to: MemberKey | 'user' | 'system', extra: Partial<TeamMessage> = {}): TeamMessage {
  const party = (p: MemberKey | 'user' | 'system'): TeamMessage['from'] =>
    p === 'user' ? { kind: 'user' } : p === 'system' ? { kind: 'system' } : { kind: 'member', member: p };
  return {
    key: gkey(n),
    teamKey: TK,
    from: party(from),
    to: party(to),
    at: 1_792_000_000_000 + n * 60_000,
    kind: 'plain',
    status: 'delivered',
    inReplyTo: null,
    derived: false,
    idleReason: null,
    hasSummary: false,
    ...extra,
  };
}
