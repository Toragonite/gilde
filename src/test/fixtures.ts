/**
 * Builders for contract values in tests. Each returns a complete, valid value
 * with neutral defaults; pass the fields a test cares about.
 */

import type { AttentionItem } from '../lib/gen/AttentionItem';
import type { Block } from '../lib/gen/Block';
import type { BlockId } from '../lib/gen/BlockId';
import type { BlockKind } from '../lib/gen/BlockKind';
import type { EpisodeId } from '../lib/gen/EpisodeId';
import type { PendingInteraction } from '../lib/gen/PendingInteraction';
import type { Project } from '../lib/gen/Project';
import type { ProjectId } from '../lib/gen/ProjectId';
import type { RequestId } from '../lib/gen/RequestId';
import type { Session } from '../lib/gen/Session';
import type { SessionUid } from '../lib/gen/SessionUid';
import type { StorePatch } from '../lib/gen/StorePatch';
import type { StoreOp } from '../lib/gen/StoreOp';
import type { StoreSnapshot } from '../lib/gen/StoreSnapshot';
import type { AbsPath } from '../lib/gen/AbsPath';

/** A well-formed `SessionUid` (`s_<UUIDv7>`) from a small number. */
export function uid(n: number): SessionUid {
  const tail = n.toString(16).padStart(12, '0');
  return `s_0192f000-0000-7000-8000-${tail}` as SessionUid;
}

export function project(id: string, extra: Partial<Project> = {}): Project {
  return {
    id: id as ProjectId,
    name: id,
    root: `/Users/u/${id}` as AbsPath,
    kind: 'git',
    order: 0,
    folded: false,
    earlier: false,
    lastActivityAt: null,
    liveSessions: 0,
    isTemp: false,
    ...extra,
  };
}

export function session(n: number, extra: Partial<Session> = {}): Session {
  return {
    uid: uid(n),
    vendor: 'claude',
    accountId: null,
    vendorSessionId: null,
    projectId: null,
    locationId: null,
    cwd: null,
    control: { kind: 'inGilde', mode: 'conversation', pty: null, hostedTeam: false, transport: 'l1' },
    activity: {
      state: { kind: 'ready' },
      since: 0,
      confidence: 'high',
      evidence: null,
      episode: `e_${n}` as EpisodeId,
    },
    title: `session ${n}`,
    team: null,
    startedAt: null,
    endedAt: null,
    lastActivityAt: null,
    pending: [],
    queued: [],
    model: null,
    permissionMode: null,
    binary: null,
    claudeVersion: null,
    memoryBytes: null,
    usageToday: null,
    caps: {
      send: 'conversation',
      interrupt: true,
      forceStop: true,
      approve: 'structured',
      setModel: true,
      setPermissionMode: [],
      continueOptions: [],
      stopSession: 'claudeStop',
      terminalView: true,
      logView: false,
      changes: 'full',
      ownerFocus: false,
    },
    details: {
      vendorSessionId: null,
      accountLabel: null,
      configDir: null,
      cwd: null,
      location: null,
      model: null,
      controlMode: 'conversation',
      connection: 'l1',
      claudeVersion: null,
      compatState: null,
      binaryRealpath: null,
      versionNote: null,
      startedAt: null,
      memoryBytes: null,
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
    ...extra,
  };
}

export function pending(requestId: string, extra: Partial<PendingInteraction> = {}): PendingInteraction {
  return {
    requestId: requestId as RequestId,
    kind: 'toolApproval',
    receivedAt: 0,
    onBehalfOf: null,
    answerIn: null,
    title: 'claude wants to run a command',
    tool: null,
    detailLines: [],
    options: [],
    alwaysLabel: null,
    defaultToNo: false,
    degraded: false,
    promptRev: null,
    confidence: 'high',
    multiSelect: false,
    questions: [],
    ...extra,
  };
}

export function queueItem(n: number, band = 1, extra: Partial<AttentionItem> = {}): AttentionItem {
  return {
    key: `q_${uid(n)}`,
    band,
    subject: { kind: 'session', uid: uid(n), onBehalfOf: null, answerIn: null },
    reason: { kind: 'needsApproval' },
    since: 0,
    waitedMs: 0,
    episode: `e_${n}` as EpisodeId,
    laterUntil: null,
    primary: 'answer',
    watched: false,
    ...extra,
  };
}

export function snapshot(rev: number, extra: Partial<StoreSnapshot> = {}): StoreSnapshot {
  return {
    rev,
    projects: [],
    locations: [],
    sessions: [],
    teams: [],
    members: [],
    tasks: [],
    messages: [],
    queue: { rev: 0, items: [], needYou: 0, also: 0, pausedUntil: null },
    accounts: [],
    leftovers: [],
    compat: [],
    notices: [],
    boot: { spawnGateOpen: true, firstDiscoveryDone: true, agentsDetected: true, memoryOnly: null, agents: [], agentsScannedAt: null, recovery: null },
    sources: [],
    integrations: { orchestrator: 'absent' },
    ...extra,
  };
}

export function patch(fromRev: number, toRev: number, ops: StoreOp[]): StorePatch {
  return { fromRev, toRev, ops };
}

export function upsertSession(s: Session): StoreOp {
  return { kind: 'upsert', entity: { kind: 'session', value: s } };
}

export function block(id: string, seq: number, kind: BlockKind): Block {
  return { id: id as BlockId, seq, at: 0, kind, truncated: false };
}

export function text(id: string, seq: number, markdown: string, streaming = false): Block {
  return block(id, seq, { kind: 'assistantText', markdown, streaming, agent: null });
}
