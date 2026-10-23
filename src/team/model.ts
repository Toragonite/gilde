/**
 * Pure rules of the Team screen (contract §7.1.3, §7.2.3–§7.2.5, §15.5, §15.6;
 * ux-ia §3.8–§3.10, J5), kept out of the components so the tests can pin them.
 *
 * The UI talks only to the abstraction: `Team`, `Member`, `Task`,
 * `TeamMessage`, the queue and `TeamCapabilities` [M14]. A control exists
 * only when the capability allows it; a fact the adapter marks Partial or
 * Unknown is shown with "≈" or "unknown", never invented [pixel-office G1/G2]:
 *
 *  - routes: a hosted pane member is typed into (`team_send`); any other
 *    member is reached through the lead's composer, prefilled and never sent
 *    (`team.prefill.*`), and only while the lead runs in Gilde (§7.2.3);
 *  - "Ask the lead to add a task…" only when the team's task tools exist
 *    (`observe.tasks = Yes`) [F-49];
 *  - the board is the inferred items by default for native teams on 2.1.307
 *    (`observe.tasks = Inferred`, §7.2.5); an empty task list never reads as
 *    "no work"; message counts are "≈" or "unknown", never "no messages"
 *    [R6 §5 LP-15, LP-17b]; a message to a closed pane is "never read" [LP-13].
 *
 * `flat` (bidi and control stripping) is the utility contract §2.2.4 keeps
 * from the old team preview: every team string is data.
 */

import type { MessageKey } from '../i18n';
import type { ExternalHost } from '../lib/gen/ExternalHost';
import type { Member } from '../lib/gen/Member';
import type { MemberKey } from '../lib/gen/MemberKey';
import type { Party } from '../lib/gen/Party';
import type { Session } from '../lib/gen/Session';
import type { SessionUid } from '../lib/gen/SessionUid';
import type { Task } from '../lib/gen/Task';
import type { TaskKey } from '../lib/gen/TaskKey';
import type { Team } from '../lib/gen/Team';
import type { TeamKey } from '../lib/gen/TeamKey';
import type { TeamMessage } from '../lib/gen/TeamMessage';
import type { WorldState } from '../store/worldSlice';
import { membersOfTeam, sessionOf } from '../store/worldSlice';

// ---------------------------------------------------------------------------
// Display text (kept utility)
// ---------------------------------------------------------------------------

// eslint-disable-next-line no-control-regex
const CONTROL_OR_SPACE = /[\s\u0000-\u001f\u007f-\u009f\u2028\u2029]+/g;
/** Bidi embeddings, overrides and isolates: they reorder what is displayed. */
const BIDI_CONTROLS = /[\u202A-\u202E\u2066-\u2069\u200E\u200F\u061C]/g;

/** One display line of a team string: no controls, no bidi tricks, at most `max` characters. */
export function flat(value: string | null | undefined, max: number): string {
  if (typeof value !== 'string' || value === '') return '';
  const line = value.replace(BIDI_CONTROLS, '').replace(CONTROL_OR_SPACE, ' ').trim();
  if (line.length <= max) return line;
  let cut = Math.max(0, max - 1);
  const code = line.charCodeAt(cut - 1);
  if (cut > 0 && code >= 0xd800 && code <= 0xdbff) cut -= 1;
  return `${line.slice(0, cut)}…`;
}

/** Multi-line team text (a message body): bidi controls and C0/C1 controls removed, newlines and tabs kept. */
export function cleanBody(value: string | null | undefined): string {
  if (typeof value !== 'string') return '';
  // eslint-disable-next-line no-control-regex
  return value.replace(BIDI_CONTROLS, '').replace(/[\u0000-\u0008\u000b\u000c\u000e-\u001f\u007f-\u009f]/g, '');
}

/** Display caps (characters). */
export const CAP = {
  name: 40,
  title: 200,
  activity: 200,
  message: 400,
  label: 60,
} as const;

// ---------------------------------------------------------------------------
// Members
// ---------------------------------------------------------------------------

export function isPresent(m: Member): boolean {
  return m.presence.kind === 'present';
}

/** Members of a team: the lead first, then join order (pixel-office I5), then name and key. */
export function orderedMembers(world: WorldState, teamKey: TeamKey): Member[] {
  const list = membersOfTeam(world, teamKey);
  return list.sort((a, b) => {
    if (a.role !== b.role) return a.role === 'lead' ? -1 : 1;
    const ja = a.joinedAt ?? Number.MAX_SAFE_INTEGER;
    const jb = b.joinedAt ?? Number.MAX_SAFE_INTEGER;
    if (ja !== jb) return ja - jb;
    if (a.name !== b.name) return a.name < b.name ? -1 : 1;
    return a.key < b.key ? -1 : a.key > b.key ? 1 : 0;
  });
}

export function leadMember(world: WorldState, team: Team): Member | null {
  if (team.lead) {
    const m = world.members.get(team.lead);
    if (m) return m;
  }
  for (const m of world.members.values()) if (m.teamKey === team.key && m.role === 'lead') return m;
  return null;
}

/** The lead's session when Gilde can type into it — the only target of a prefill (§7.2.3). */
export function leadTarget(world: WorldState, team: Team): Session | null {
  const lead = leadMember(world, team);
  const s = sessionOf(world, lead?.sessionUid ?? null);
  if (!s || s.control.kind !== 'inGilde' || s.caps.send === 'none') return null;
  return s;
}

// ---------------------------------------------------------------------------
// Capabilities → controls (a control appears iff the capability allows it)
// ---------------------------------------------------------------------------

/** How a message to a member travels: typed into its pane, prefilled at the lead, or the lead's own composer. */
export type Route = 'pane' | 'lead' | 'self';

export function memberRoute(world: WorldState, team: Team, member: Member): Route | null {
  const send = team.caps.actSend;
  if (member.role === 'lead') return leadTarget(world, team) ? 'self' : null;
  if (!isPresent(member)) return null;
  if (send.includes('viaMemberPane') && team.hosted && member.backend === 'pane' && member.pane !== null) return 'pane';
  if (send.includes('native')) return 'pane';
  if (send.includes('viaLead') && leadTarget(world, team)) return 'lead';
  return null;
}

/** "Tell {member}…" through the lead (for members Gilde cannot type into itself). */
export function canTell(world: WorldState, team: Team, member: Member): boolean {
  return member.role !== 'lead' && isPresent(member) && team.caps.actSend.includes('viaLead') && leadTarget(world, team) !== null;
}

export function canTellAll(world: WorldState, team: Team): boolean {
  return team.caps.actBroadcast.includes('viaLead') && leadTarget(world, team) !== null;
}

/** "Ask the lead to stop {member}" — the graceful `shutdown_request` path (§7.2.3). */
export function canAskStop(world: WorldState, team: Team, member: Member): boolean {
  return member.role !== 'lead' && isPresent(member) && team.caps.actStop.includes('viaLead') && leadTarget(world, team) !== null;
}

/** "Close pane…" — hosted pane members only (§7.4.7). */
export function canClosePane(team: Team, member: Member): boolean {
  return team.caps.actStop.includes('closePane') && team.hosted && member.backend === 'pane' && member.pane !== null && isPresent(member);
}

/** "Ask the lead to add a task…" — only when the team's task tools exist [F-49]. */
export function canAddTask(world: WorldState, team: Team): boolean {
  return team.source === 'claudeNative' && team.caps.observeTasks === 'yes' && leadTarget(world, team) !== null;
}

/** Panes need pane hosting; an in-process fallback shows Board and Floor only (ux-ia §3.8). */
export function panesAvailable(team: Team): boolean {
  return team.paneHosting !== 'inProcessFallback';
}

/** The ownership bar's sentence for a team Gilde does not host (ux-ia §3.10, rows 100 and 121). */
export function ownershipKey(team: Team): MessageKey {
  if (team.source === 'orchestrator') return 'orchestrator.messageFromLead';
  return externalKey(team.externalHost);
}

export function externalKey(host: ExternalHost | null): MessageKey {
  if (host === 'iTerm2') return 'team.external';
  if (host === 'tmux') return 'team.externalTmux';
  return 'team.externalElsewhere';
}

// ---------------------------------------------------------------------------
// Counts: exact, "≈" or unknown — never a 0 that looks like data
// ---------------------------------------------------------------------------

export type Count = { kind: 'exact'; value: number } | { kind: 'approx'; value: number } | { kind: 'unknown' };

export type BoardKind = 'board' | 'inferred' | 'none';

/** Which task column the team gets (§7.2.5): the real board, the inferred items, or none. */
export function boardKind(team: Team): BoardKind {
  switch (team.caps.observeTasks) {
    case 'yes':
    case 'partial':
      return 'board';
    case 'inferred':
      return 'inferred';
    default:
      return 'none';
  }
}

/** Tasks the board shows (deleted ones never). */
export function visibleTasks(tasks: readonly Task[]): Task[] {
  return tasks.filter((t) => t.status !== 'deleted');
}

export function taskCount(team: Team, tasks: readonly Task[]): Count {
  const kind = boardKind(team);
  const n = visibleTasks(tasks).length;
  if (kind === 'none') return n > 0 ? { kind: 'approx', value: n } : { kind: 'unknown' };
  const approx = team.progress.approx || team.caps.observeTasks === 'partial';
  return approx ? { kind: 'approx', value: n } : { kind: 'exact', value: n };
}

/** Message counts: exact only when the adapter observes every message (LP-15). */
export function messageCount(team: Team, n: number): Count {
  if (team.caps.observeMessages === 'yes') return { kind: 'exact', value: n };
  return n > 0 ? { kind: 'approx', value: n } : { kind: 'unknown' };
}

// ---------------------------------------------------------------------------
// Tasks
// ---------------------------------------------------------------------------

export type TaskGroupId = 'inProgress' | 'open' | 'done' | 'reported';

export interface TaskGroup {
  id: TaskGroupId;
  tasks: Task[];
}

function byCreation(a: Task, b: Task): number {
  const ca = a.createdAt ?? Number.MAX_SAFE_INTEGER;
  const cb = b.createdAt ?? Number.MAX_SAFE_INTEGER;
  if (ca !== cb) return ca - cb;
  const na = Number(a.nativeId ?? Number.NaN);
  const nb = Number(b.nativeId ?? Number.NaN);
  if (Number.isFinite(na) && Number.isFinite(nb) && na !== nb) return na - nb;
  return a.key < b.key ? -1 : a.key > b.key ? 1 : 0;
}

/** The real board: In progress · Open (blocked ones included) · Done (ux-ia §3.10). */
export function boardGroups(tasks: readonly Task[]): TaskGroup[] {
  const shown = visibleTasks(tasks).filter((t) => !t.inferred);
  const groups: TaskGroup[] = [
    { id: 'inProgress', tasks: shown.filter((t) => t.status === 'inProgress').sort(byCreation) },
    { id: 'open', tasks: shown.filter((t) => t.status === 'open').sort(byCreation) },
    { id: 'done', tasks: shown.filter((t) => t.status === 'done').sort(byCreation) },
  ];
  return groups.filter((g) => g.tasks.length > 0);
}

/** Inferred items (§7.2.5): open requests, then reported ones, in request order. */
export function inferredGroups(tasks: readonly Task[]): TaskGroup[] {
  const items = visibleTasks(tasks).filter((t) => t.inferred);
  const k = (t: Task) => t.inferredK ?? Number.MAX_SAFE_INTEGER;
  const reported = (t: Task) => t.station === 'reported' || t.station === 'done' || t.status === 'done';
  const groups: TaskGroup[] = [
    { id: 'open', tasks: items.filter((t) => !reported(t)).sort((a, b) => k(a) - k(b)) },
    { id: 'reported', tasks: items.filter(reported).sort((a, b) => k(a) - k(b)) },
  ];
  return groups.filter((g) => g.tasks.length > 0);
}

/** Declared blockers that are still open (never inferred, ux-ia R-16). */
export function openBlockers(task: Task, byKey: ReadonlyMap<TaskKey, Task>): Task[] {
  const out: Task[] = [];
  for (const key of task.blockedBy) {
    const b = byKey.get(key);
    if (b && b.status !== 'done' && b.status !== 'deleted') out.push(b);
  }
  return out;
}

/** An inferred item still open while its owner has been Ready ≥ 10 min: "no report" (pixel-office F-13). */
export const NO_REPORT_MS = 10 * 60 * 1000;

export function noReport(task: Task, owner: Session | null, now: number): boolean {
  if (!task.inferred || task.station === 'reported' || task.station === 'done') return false;
  if (!owner || owner.activity.state.kind !== 'ready') return false;
  return now - owner.activity.since >= NO_REPORT_MS;
}

// ---------------------------------------------------------------------------
// Messages
// ---------------------------------------------------------------------------

export function partyName(party: Party, members: ReadonlyMap<MemberKey, Member>, t: (key: MessageKey) => string): string {
  switch (party.kind) {
    case 'member': {
      const m = members.get(party.member);
      return m ? flat(m.name, CAP.name) : t('team.unattributedMember');
    }
    case 'user':
      return t('team.you');
    case 'system':
      return t('team.system');
  }
}

/** A protocol frame as one system line (ux-ia §3.10); `null` for a plain message (from → to + its text on demand). */
export function messageEvent(
  msg: TeamMessage,
  name: (party: Party) => string,
): { key: MessageKey; params: Record<string, string> } | null {
  const from = name(msg.from);
  const to = name(msg.to);
  switch (msg.kind) {
    case 'plain':
      return null;
    case 'idleNotification': {
      const key: MessageKey =
        msg.idleReason === 'interrupted' ? 'team.msg.idleInterrupted' : msg.idleReason === 'failed' ? 'team.msg.idleFailed' : 'team.msg.idle';
      return { key, params: { who: from } };
    }
    case 'shutdownRequest':
      return { key: 'team.msg.shutdownRequest', params: { from, to } };
    case 'shutdownApproved':
      return { key: 'team.msg.shutdownApproved', params: { from } };
    case 'shutdownRejected':
      return { key: 'team.msg.shutdownRejected', params: { from } };
    case 'planApprovalRequest':
      return { key: 'team.msg.planReview', params: { from } };
    case 'planApprovalResponse':
      return { key: 'team.msg.planAnswered', params: { from } };
    case 'permissionRequest':
      return { key: 'team.msg.permissionRequest', params: { from } };
    case 'permissionResponse':
      return { key: 'team.msg.permissionAnswered', params: { from } };
    case 'taskAssignment':
      return { key: 'team.msg.taskAssignment', params: { from, to } };
    case 'teammateTerminated':
      return { key: 'team.msg.terminated', params: { who: msg.to.kind === 'member' ? to : from } };
    case 'unknown':
      return { key: 'team.msg.unknownKind', params: { from } };
  }
}

export type DeliveryNote = 'neverRead' | 'queued' | 'unknown' | null;

/**
 * The delivery word of a message: a message to a closed pane stays queued
 * forever, so it reads "never read" (LP-13); other queued ones "queued";
 * an ambiguous join "delivery unknown"; delivered and sent need no word.
 */
export function deliveryNote(msg: TeamMessage, members: ReadonlyMap<MemberKey, Member>): DeliveryNote {
  if (msg.status === 'queued') {
    if (msg.to.kind === 'member') {
      const m = members.get(msg.to.member);
      if (m && !isPresent(m)) return 'neverRead';
    }
    return 'queued';
  }
  if (msg.status === 'unknown') return 'unknown';
  return null;
}

/** The store keeps 500 messages per team; older ones come from `team_messages_page`. */
export const STORE_MESSAGE_CAP = 500;

// ---------------------------------------------------------------------------
// Pane tiles
// ---------------------------------------------------------------------------

export type MemberTileKind = 'session' | 'placeholder' | 'crashed';

export interface MemberTileSpec {
  key: MemberKey;
  member: Member;
  uid: SessionUid | null;
  kind: MemberTileKind;
  /** The exit code of a crashed tile ("Crashed · exit {code}"). */
  code: number | null;
}

/**
 * The tiles of Team › Panes: the lead, then every present member — its
 * session, or a "Teammate" placeholder until its pane and roster agree
 * (§7.4.5). A member that exited stays only as a crashed tile ("Crashed ·
 * exit {code}", remain-on-exit) until the user closes it; an exit 0 closes
 * the tile (§7.4.6).
 */
export function memberTiles(world: WorldState, members: readonly Member[], dismissed: ReadonlySet<string>): MemberTileSpec[] {
  const out: MemberTileSpec[] = [];
  for (const member of members) {
    const session = sessionOf(world, member.sessionUid);
    const state = session?.activity.state;
    const crashed = state?.kind === 'crashed';
    const code = state?.kind === 'crashed' ? state.exit.code : null;
    if (crashed) {
      if (!dismissed.has(member.key)) out.push({ key: member.key, member, uid: session?.uid ?? null, kind: 'crashed', code });
      continue;
    }
    if (member.role !== 'lead' && !isPresent(member)) continue;
    out.push({ key: member.key, member, uid: session?.uid ?? null, kind: session ? 'session' : 'placeholder', code: null });
  }
  return out;
}

/** The pgid-subtree memory of the team's live members, if any is known. */
export function teamMemory(world: WorldState, members: readonly Member[]): number | null {
  let total = 0;
  let known = false;
  for (const m of members) {
    const s = sessionOf(world, m.sessionUid);
    if (s?.memoryBytes !== null && s?.memoryBytes !== undefined && isPresent(m)) {
      total += s.memoryBytes;
      known = true;
    }
  }
  return known ? total : null;
}
