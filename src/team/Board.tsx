/**
 * Team › Board (ux-ia §3.10, J5; contract §7.2.4, §7.2.5, §15.5): three
 * read-only columns built from the team's own files and transcripts —
 * Members · Tasks · Messages (tabs below 960 px of main width).
 *
 *  - Members: each member's own state; a pending card in the row takes
 *    `1`/`2`/`3` like any card (T15's `CardStack`, row variant); row keys
 *    `⏎` show pane, `M` message, `⇧⌘⌫` Stop….
 *  - Tasks: the transcript board only when the team's task tools exist
 *    (`observe.tasks = Yes`); otherwise the inferred items are the board —
 *    "Request {k}", marked inferred — and an empty list never reads as "no
 *    work" (§7.2.5, PM-55). No board at all → "This team has no shared task
 *    list." Counts carry "≈" when inferred from inboxes only.
 *  - Messages: from → to with the text read on demand (`team_message_body`,
 *    memory only, dropped when collapsed); protocol frames as one system line;
 *    "≈" on joins by order and time; "never read" for messages to a closed
 *    pane (LP-13); an empty or partial log is "Message count unknown", never
 *    "no messages" (LP-15). Older pages through `team_messages_page`.
 *  - The To: composer types into a hosted pane member (`team_send`) or
 *    prefills the lead's composer (`team.prefill.tell`, never sent). A team
 *    Gilde cannot message gets the ownership bar in its place.
 */

import { useEffect, useMemo, useRef, useState } from 'react';
import type { KeyboardEvent as ReactKeyboardEvent, ReactNode, RefObject } from 'react';

import { CardStack, ownerWord } from '../approval';
import { StateGlyph } from '../components/StateGlyph';
import type { GlyphKind } from '../components/StateGlyph';
import { VirtualList } from '../components/VirtualList';
import { isImeComposing, useHotkeys } from '../hooks/useHotkeys';
import { useResizeObserver } from '../hooks/useResizeObserver';
import { useRovingTabIndex } from '../hooks/useRovingTabIndex';
import type { MessageKey } from '../i18n';
import { useI18n } from '../i18n/useI18n';
import { commands } from '../lib/commands';
import { formatBytes, formatTimeOfDay } from '../lib/format';
import type { Member } from '../lib/gen/Member';
import type { MemberKey } from '../lib/gen/MemberKey';
import type { Party } from '../lib/gen/Party';
import type { SessionUid } from '../lib/gen/SessionUid';
import type { Task } from '../lib/gen/Task';
import type { TaskKey } from '../lib/gen/TaskKey';
import type { Team } from '../lib/gen/Team';
import type { TeamKey } from '../lib/gen/TeamKey';
import type { TeamMessage } from '../lib/gen/TeamMessage';
import type { TeamMessageBodyRes } from '../lib/gen/TeamMessageBodyRes';
import { newClientMsgId } from '../lib/ids';
import type { IpcError } from '../lib/ipcError';
import { vendorBadge } from '../lib/vendor';
import { useNow } from '../approval/useNow';
import type { WidthBand } from '../shell/layout';
import { requestTileFocus } from '../split/focus';
import { memberCapacity } from '../split/layout';
import { appStore, useAppStore } from '../store/useAppStore';
import { messagesOfTeam, sessionOf, tasksOfTeam } from '../store/worldSlice';
import type { WorldState } from '../store/worldSlice';
import { prefillLead, prefillText, setTeamMode, tellLine } from './actions';
import {
  CAP,
  STORE_MESSAGE_CAP,
  boardGroups,
  boardKind,
  canAddTask,
  canClosePane,
  cleanBody,
  deliveryNote,
  flat,
  inferredGroups,
  isPresent,
  leadMember,
  leadTarget,
  memberRoute,
  messageCount,
  messageEvent,
  noReport,
  openBlockers,
  orderedMembers,
  ownershipKey,
  panesAvailable,
  partyName,
  taskCount,
} from './model';
import type { Count, TaskGroupId } from './model';
import { bringIntoView, countText } from './Panes';

/** Below this main width the three columns become tabs (ux-ia §3.10 "900 px"). */
export const BOARD_TABS_BELOW = 960;
/** Lists longer than this are virtualized (contract §11.2 rule 5). */
export const VIRTUALIZE_ABOVE = 100;

type Tab = 'members' | 'tasks' | 'messages';
const TABS: readonly Tab[] = ['members', 'tasks', 'messages'];

// The To: composer's draft per team (memory only; kept across mode switches).
interface ToDraft {
  to: MemberKey | null;
  text: string;
}
const drafts = new Map<TeamKey, ToDraft>();

/** Tests only. */
export function resetBoardForTests(): void {
  drafts.clear();
}

/** Show a member's pane: Panes mode with its tile on screen and focused (J5). */
export function showPane(team: Team, key: MemberKey, band: WidthBand): void {
  const s = appStore.getState();
  const member = s.world.members.get(key);
  if (!member || !panesAvailable(team)) return;
  bringIntoView(s.world, team.key, key, memberCapacity(band));
  s.setTeamLayout(team.key, { focused: key, zoomed: null });
  setTeamMode(team.key, 'panes');
  const session = sessionOf(s.world, member.sessionUid);
  if (session) requestTileFocus(session.uid);
}

/** Sessions whose pending card is shown on a Board member row (present members with a card). */
export function cardSessions(world: WorldState, members: readonly Member[]): SessionUid[] {
  const out: SessionUid[] = [];
  for (const m of members) {
    const session = sessionOf(world, m.sessionUid);
    if (session && isPresent(m) && session.pending.length > 0) out.push(session.uid);
  }
  return out;
}

/** `⇧⌘⌫` on a member: Close pane… for a hosted pane, else the session's stop sheet. */
function stopMember(world: WorldState, team: Team, member: Member): void {
  const s = appStore.getState();
  const session = sessionOf(world, member.sessionUid);
  if (canClosePane(team, member)) {
    s.setTeamLayout(team.key, { focused: member.key });
    s.openSheet('closePane');
  } else if (session && session.caps.stopSession !== 'none') {
    s.setTeamLayout(team.key, { focused: member.key });
    s.openSheet('stopSession');
  }
}

export function Board({ team, band, tabs: forcedTabs }: { team: Team; band: WidthBand; tabs?: boolean }) {
  const { t } = useI18n();
  const world = useAppStore((s) => s.world);
  const rootRef = useRef<HTMLDivElement>(null);
  const [width, setWidth] = useState(0);
  useResizeObserver(rootRef, (rect) => setWidth(rect.width));
  const tabs = forcedTabs ?? (width > 0 ? width < BOARD_TABS_BELOW : band !== 'wide');
  const [tab, setTab] = useState<Tab>('members');
  const [draft, setDraftState] = useState<ToDraft>(() => drafts.get(team.key) ?? { to: null, text: '' });
  const [toToken, setToToken] = useState(0);
  const setDraft = (patch: Partial<ToDraft>) => {
    setDraftState((d) => {
      const next = { ...d, ...patch };
      drafts.set(team.key, next);
      return next;
    });
  };

  const members = useMemo(() => orderedMembers(world, team.key), [world, team.key]);
  const byKey = useMemo(() => new Map(members.map((m) => [m.key, m])), [members]);

  // Publish the sessions whose cards the Members column shows (A30.2, F-04); clear on unmount.
  const membersShown = !tabs || tab === 'members';
  const cardUids = membersShown ? cardSessions(world, members) : [];
  const cardKey = cardUids.join('|');
  useEffect(() => {
    appStore.getState().setVisibleUids(cardUids);
  }, [cardKey]); // eslint-disable-line react-hooks/exhaustive-deps
  useEffect(() => () => appStore.getState().setVisibleUids(null), []);

  const messageMember = (key: MemberKey) => {
    setDraft({ to: key });
    if (tabs) setTab('messages');
    setToToken((n) => n + 1);
  };

  const columns: Record<Tab, ReactNode> = {
    members: <MembersColumn team={team} members={members} world={world} band={band} onMessage={messageMember} />,
    tasks: <TasksColumn team={team} world={world} byMember={byKey} />,
    messages: (
      <MessagesColumn team={team} world={world} members={members} byMember={byKey} draft={draft} setDraft={setDraft} toToken={toToken} />
    ),
  };
  const labels: Record<Tab, MessageKey> = { members: 'team.board.members', tasks: 'team.board.tasks', messages: 'team.board.messages' };

  const onTabKey = (event: ReactKeyboardEvent<HTMLButtonElement>) => {
    if (event.metaKey || event.ctrlKey || event.altKey || isImeComposing(event.nativeEvent)) return;
    const at = TABS.indexOf(tab);
    let next = -1;
    if (event.key === 'ArrowRight') next = (at + 1) % TABS.length;
    else if (event.key === 'ArrowLeft') next = (at - 1 + TABS.length) % TABS.length;
    if (next < 0) return;
    event.preventDefault();
    const nextTab = TABS[next] as Tab;
    setTab(nextTab);
    rootRef.current?.querySelector<HTMLButtonElement>(`[data-board-tab="${nextTab}"]`)?.focus();
  };

  return (
    <div ref={rootRef} className="g-board" data-tabs={tabs ? 'true' : undefined}>
      {tabs && (
        <div className="g-board-tabs" role="tablist" aria-label={t('team.board.columns')}>
          {TABS.map((id) => (
            <button
              key={id}
              type="button"
              role="tab"
              className="g-board-tab"
              data-board-tab={id}
              aria-selected={tab === id}
              tabIndex={tab === id ? 0 : -1}
              onClick={() => setTab(id)}
              onKeyDown={onTabKey}
            >
              {t(labels[id])}
            </button>
          ))}
        </div>
      )}
      <div className="g-board-cols">
        {tabs ? (
          <div className="g-board-col" role="tabpanel" aria-label={t(labels[tab])}>
            {columns[tab]}
          </div>
        ) : (
          TABS.map((id) => (
            <div key={id} className="g-board-col" data-col={id}>
              {columns[id]}
            </div>
          ))
        )}
      </div>
    </div>
  );
}

function CountLabel({ count }: { count: Count }) {
  const { t, locale } = useI18n();
  return (
    <span className="g-board-count" data-count={count.kind} title={count.kind === 'approx' ? t('team.count.approxLabel', { value: count.value }) : undefined}>
      {countText(count, t, locale)}
    </span>
  );
}

/** Plain rows up to 100, a virtualized list beyond (§11.2). */
function BoardList({
  count,
  getKey,
  renderRow,
  label,
  role,
  listRef,
}: {
  count: number;
  getKey: (i: number) => string;
  renderRow: (i: number) => ReactNode;
  label: string;
  role?: 'list' | 'log';
  listRef?: RefObject<HTMLDivElement | null>;
}) {
  if (count > VIRTUALIZE_ABOVE) {
    return (
      <div className="g-board-list g-board-list-virtual" role={role ?? 'list'} aria-label={label} aria-live={role === 'log' ? 'off' : undefined}>
        <VirtualList
          count={count}
          getKey={getKey}
          estimateSize={() => 44}
          renderRow={(i) => <div role={(role ?? 'list') === 'list' ? 'listitem' : undefined}>{renderRow(i)}</div>}
          className="g-board-vlist"
        />
      </div>
    );
  }
  const itemRole = (role ?? 'list') === 'list' ? 'listitem' : undefined;
  const rows: ReactNode[] = [];
  for (let i = 0; i < count; i++) {
    rows.push(
      <div key={getKey(i)} role={itemRole}>
        {renderRow(i)}
      </div>,
    );
  }
  return (
    <div ref={listRef} className="g-board-list" role={role ?? 'list'} aria-label={label} aria-live={role === 'log' ? 'off' : undefined}>
      {rows}
    </div>
  );
}

// ---------------------------------------------------------------------------
// Members
// ---------------------------------------------------------------------------

function MembersColumn({
  team,
  members,
  world,
  band,
  onMessage,
}: {
  team: Team;
  members: readonly Member[];
  world: WorldState;
  band: WidthBand;
  onMessage: (key: MemberKey) => void;
}) {
  const { t, td, locale } = useI18n();
  const setTeamLayout = useAppStore((s) => s.setTeamLayout);
  const listRef = useRef<HTMLDivElement>(null);
  const [rowFocus, setRowFocus] = useState<number | null>(null);
  const roving = useRovingTabIndex(members.length, {
    onActiveChange: (i) => {
      const m = members[i];
      if (m) setTeamLayout(team.key, { focused: m.key });
    },
  });
  const active = members[roving.activeIndex] ?? null;
  const panes = panesAvailable(team);
  const tasks = useMemo(() => new Map(tasksOfTeam(world, team.key).map((x) => [x.key, x])), [world, team.key]);

  useHotkeys(
    [
      {
        id: 'list.open',
        combo: 'enter',
        when: (e) => (e.target as HTMLElement | null)?.hasAttribute?.('data-member-row') === true,
        run: () => {
          if (active && panes && active.sessionUid) showPane(team, active.key, band);
        },
      },
      {
        id: 'list.message',
        combo: 'm',
        when: () => !!active && memberRoute(world, team, active) !== null,
        run: () => active && onMessage(active.key),
      },
      { id: 'stop', combo: 'mod+shift+backspace', run: () => active && stopMember(world, team, active) },
    ],
    { scope: 'L', element: listRef },
  );

  const activity = (m: Member): string | null => {
    if (!isPresent(m)) return t('team.member.left');
    const task = m.currentTask ? tasks.get(m.currentTask) : null;
    if (task?.title) return flat(task.title, CAP.activity);
    if (task?.inferred && task.inferredK !== null) return t('floor.request', { k: task.inferredK });
    const evidence = sessionOf(world, m.sessionUid)?.activity.evidence;
    return evidence ? td(evidence.key, evidence.params) : null;
  };

  return (
    <section className="g-board-section" aria-label={t('team.board.members')}>
      <div className="g-board-head">
        <h2 className="g-board-title">{t('team.board.members')}</h2>
        <span className="g-board-count">{members.length}</span>
        <span className="g-board-fill" />
        <span className="g-board-hint">{t('team.member.rowHint')}</span>
      </div>
      <div
        ref={listRef}
        className="g-board-list"
        role="list"
        aria-label={t('team.board.members')}
        onFocus={(e) => {
          const row = (e.target as HTMLElement).closest('[data-member-row]');
          const i = row ? Number(row.getAttribute('data-roving-index')) : NaN;
          setRowFocus(Number.isFinite(i) ? i : null);
        }}
        onBlur={(e) => {
          if (!listRef.current?.contains(e.relatedTarget as Node | null)) setRowFocus(null);
        }}
      >
        {members.map((m, i) => {
          const session = sessionOf(world, m.sessionUid);
          const name = flat(m.name, CAP.name);
          const state: GlyphKind | null = session ? session.activity.state.kind : null;
          const word = present(m) && session ? stateWordOf(session.activity.state.kind, session.activity.confidence === 'likely', t) : t('team.member.left');
          const meta = [vendorBadge(m.vendor), session?.model ?? null, session?.memoryBytes != null ? formatBytes(session.memoryBytes, locale) : null]
            .filter((x): x is string => !!x)
            .join(' · ');
          const line = activity(m);
          const route = memberRoute(world, team, m);
          const itemProps = roving.getItemProps(i);
          return (
            <div
              key={m.key}
              role="listitem"
              className="g-brow"
              data-member-row={m.key}
              data-present={present(m) ? 'true' : 'false'}
              aria-label={[name, word, line].filter(Boolean).join(', ')}
              {...itemProps}
            >
              <div className="g-brow-1">
                <span className="g-swatch" data-colour={m.colour ?? undefined} aria-hidden="true" />
                <span className="g-brow-name">{name}</span>
                {m.role === 'lead' && <span className="g-brow-muted">· {t('team.lead')}</span>}
                <span className="g-board-fill" />
                {state && <StateGlyph state={state} likely={session?.activity.confidence === 'likely'} />}
                <span className="g-brow-state">{word}</span>
              </div>
              {meta && <div className="g-brow-2">{meta}</div>}
              {line && <div className="g-brow-3">{line}</div>}
              {session && session.pending.length > 0 && present(m) && (
                <CardStack uid={session.uid} variant="row" shortcuts={rowFocus === i} onReply={() => panes && showPane(team, m.key, band)} />
              )}
              <div className="g-brow-4">
                {panes && session && present(m) && (
                  <button type="button" className="g-sbtn" onClick={() => showPane(team, m.key, band)}>
                    {t('team.member.showPane')}
                  </button>
                )}
                {route !== null && (
                  <button type="button" className="g-sbtn" aria-keyshortcuts="M" onClick={() => onMessage(m.key)}>
                    {t('team.member.message')}
                  </button>
                )}
              </div>
            </div>
          );
        })}
      </div>
    </section>
  );
}

const present = isPresent;

function stateWordOf(kind: string, likely: boolean, t: ReturnType<typeof useI18n>['t']): string {
  const key: MessageKey = kind === 'statusUnknown' ? 'state.unknown' : kind === 'error' ? 'state.error' : (`state.${kind}` as MessageKey);
  const word = t(key);
  return likely ? t('state.likely', { state: word }) : word;
}

// ---------------------------------------------------------------------------
// Tasks
// ---------------------------------------------------------------------------

const GROUP_LABEL: Record<TaskGroupId, MessageKey> = {
  inProgress: 'team.task.inProgress',
  open: 'team.task.open',
  done: 'team.task.done',
  reported: 'floor.stn.reported',
};

function taskGlyph(task: Task): GlyphKind {
  switch (task.station) {
    case 'inProgress':
      return 'working';
    case 'needsYou':
      return 'needsApproval';
    case 'done':
    case 'reported':
      return 'newReply';
    default:
      return 'ready';
  }
}

function stationWord(task: Task, t: ReturnType<typeof useI18n>['t']): string {
  switch (task.station) {
    case 'toDo':
      return t('floor.stn.todo');
    case 'blocked':
      return t('floor.stn.blocked');
    case 'assigned':
      return t('floor.stn.assigned');
    case 'inProgress':
      return t('floor.stn.doing');
    case 'needsYou':
      return t('floor.stn.you');
    case 'done':
      return t('floor.stn.done');
    case 'reported':
      return t('floor.stn.reported');
  }
}

/** The station word adds nothing when it repeats the group heading (In progress, Open, Done, Reported). */
function stationRepeatsGroup(task: Task): boolean {
  if (task.inferred) return task.station === 'reported';
  return (
    (task.status === 'inProgress' && task.station === 'inProgress') ||
    (task.status === 'open' && task.station === 'toDo') ||
    (task.status === 'done' && task.station === 'done')
  );
}

/** A task title: from the store, else read once on demand (`team_task_title`, memory only). */
function TaskTitle({ task }: { task: Task }) {
  const { t } = useI18n();
  const [fetched, setFetched] = useState<string | null>(null);
  useEffect(() => {
    if (task.title !== null || task.inferred) return;
    let live = true;
    commands.team_task_title({ taskKey: task.key }).then(
      (res) => live && setFetched(res.title),
      () => undefined,
    );
    return () => {
      live = false;
    };
  }, [task.key, task.title, task.inferred]);
  if (task.inferred) return <>{t('floor.request', { k: task.inferredK ?? 0 })}</>;
  const title = task.title ?? fetched;
  return <>{title ? flat(title, CAP.title) : t('team.task.number', { id: task.nativeId ?? '?' })}</>;
}

function TasksColumn({ team, world, byMember }: { team: Team; world: WorldState; byMember: ReadonlyMap<MemberKey, Member> }) {
  const { t } = useI18n();
  const now = useNow();
  const tasks = useMemo(() => tasksOfTeam(world, team.key), [world, team.key]);
  const kind = boardKind(team);
  const count = taskCount(team, tasks);
  const byKey = useMemo(() => new Map<TaskKey, Task>(tasks.map((x) => [x.key, x])), [tasks]);
  const groups = kind === 'board' ? boardGroups(tasks) : kind === 'inferred' ? inferredGroups(tasks) : [];
  const addTask = canAddTask(world, team);

  // Flatten groups into rows (group headers are rows too) for one list.
  type Row = { kind: 'group'; id: TaskGroupId; n: number } | { kind: 'task'; task: Task };
  const rows: Row[] = [];
  for (const g of groups) {
    rows.push({ kind: 'group', id: g.id, n: g.tasks.length });
    for (const task of g.tasks) rows.push({ kind: 'task', task });
  }

  const renderRow = (i: number) => {
    const row = rows[i] as Row;
    if (row.kind === 'group') {
      return (
        <div className="g-btask-group">
          {t(GROUP_LABEL[row.id])} <span className="g-board-count">{row.n}</span>
        </div>
      );
    }
    const task = row.task;
    const owner = task.owner ? byMember.get(task.owner) : null;
    const ownerSession = owner ? sessionOf(world, owner.sessionUid) : null;
    const blockers = openBlockers(task, byKey);
    const extra: string[] = [];
    if (blockers.length > 0) {
      extra.push(t('team.task.blockedBy', { task: blockers.map((b) => (b.title ? flat(b.title, CAP.label) : t('team.task.number', { id: b.nativeId ?? '?' }))).join(', ') }));
    }
    if (noReport(task, ownerSession, now)) extra.push(t('floor.noReport'));
    return (
      <div className="g-btask" data-inferred={task.inferred ? 'true' : undefined} data-station={task.station}>
        <StateGlyph state={taskGlyph(task)} />
        <span className="g-btask-main">
          <span className="g-btask-title">
            <TaskTitle task={task} />
          </span>
          {task.inferred && <span className="g-btask-flag">{t('floor.inferred')}</span>}
          {!stationRepeatsGroup(task) && <span className="g-btask-station">{stationWord(task, t)}</span>}
          {extra.map((x) => (
            <span key={x} className="g-btask-extra">
              {x}
            </span>
          ))}
        </span>
        <span className="g-btask-owner">
          {owner ? (
            <>
              <span className="g-swatch" data-colour={owner.colour ?? undefined} aria-hidden="true" />
              <span className="g-btask-ownername">{flat(owner.name, CAP.name)}</span>
            </>
          ) : (
            <span className="g-btask-ownername">{t('team.task.unassigned')}</span>
          )}
        </span>
      </div>
    );
  };

  return (
    <section className="g-board-section" aria-label={t('team.board.tasks')}>
      <div className="g-board-head">
        <h2 className="g-board-title">{t('team.board.tasks')}</h2>
        {kind !== 'none' && <CountLabel count={count} />}
        <span className="g-board-fill" />
        {kind === 'board' && <span className="g-board-hint">{t('team.board.taskList')}</span>}
        {kind === 'inferred' && <span className="g-board-hint">{t('floor.inferred')}</span>}
      </div>
      {kind === 'none' ? (
        <p className="g-board-empty">{t('team.noTasks')}</p>
      ) : (
        <>
          {kind === 'inferred' && <p className="g-board-note">{t('floor.inferredLabel')}</p>}
          <BoardList count={rows.length} getKey={(i) => { const r = rows[i] as Row; return r.kind === 'group' ? `g-${r.id}` : r.task.key; }} renderRow={renderRow} label={t('team.board.tasks')} />
        </>
      )}
      {addTask && (
        <div className="g-board-foot">
          <button type="button" className="g-sbtn" onClick={() => prefillLead(team.key, `${prefillText('team.prefill.addTask')} `)}>
            {t('team.action.addTask')}
          </button>
        </div>
      )}
    </section>
  );
}

// ---------------------------------------------------------------------------
// Messages
// ---------------------------------------------------------------------------

function MessageRow({ msg, name, byMember }: { msg: TeamMessage; name: (p: Party) => string; byMember: ReadonlyMap<MemberKey, Member> }) {
  const { t, te, locale } = useI18n();
  const [open, setOpen] = useState(false);
  const [body, setBody] = useState<TeamMessageBodyRes | null>(null);
  const [error, setError] = useState<IpcError | null>(null);
  const event = messageEvent(msg, name);
  const note = deliveryNote(msg, byMember);
  const noteKey: MessageKey | null = note === 'neverRead' ? 'team.neverRead' : note === 'queued' ? 'team.msg.queued' : note === 'unknown' ? 'team.msg.deliveryUnknown' : null;

  // The text is content: fetched when opened, held only while open.
  useEffect(() => {
    if (!open) {
      setBody(null);
      setError(null);
      return;
    }
    let live = true;
    commands.team_message_body({ messageKey: msg.key }).then(
      (res) => live && setBody(res),
      (e: IpcError) => live && setError(e),
    );
    return () => {
      live = false;
    };
  }, [open, msg.key]);

  return (
    <div className="g-bmsg" data-kind={msg.kind} data-message={msg.key}>
      <div className="g-bmsg-head">
        <time className="g-bmsg-time">{formatTimeOfDay(msg.at, locale)}</time>
        {msg.derived && (
          <span className="g-bmsg-derived" title={t('team.msg.derived')}>
            ≈
          </span>
        )}
        {event ? (
          <span className="g-bmsg-event">{t(event.key, event.params)}</span>
        ) : (
          <span className="g-bmsg-route">
            <PartyName party={msg.from} name={name} byMember={byMember} />
            <span className="g-brow-muted" aria-hidden="true">
              →
            </span>
            <PartyName party={msg.to} name={name} byMember={byMember} />
          </span>
        )}
        {noteKey && <span className="g-bmsg-note">{t(noteKey)}</span>}
        <span className="g-board-fill" />
        {!event && (
          <button type="button" className="g-tbtn" aria-expanded={open} onClick={() => setOpen((o) => !o)}>
            {t(open ? 'team.msg.hideText' : 'team.msg.showText')}
          </button>
        )}
      </div>
      {open && (
        <div className="g-bmsg-body">
          {error ? (
            <p className="g-bmsg-line">
              {te(error)}
              {error.detail ? <span className="mono"> {error.detail}</span> : null}
            </p>
          ) : body === null ? (
            <p className="g-bmsg-line">{t('common.loading')}</p>
          ) : (
            <>
              {body.text ? <p className="g-bmsg-text">{cleanBody(body.text)}</p> : <p className="g-bmsg-line">{t('team.msg.unavailable')}</p>}
              {body.summary && (
                <p className="g-bmsg-line">
                  {t('team.msg.summary')} · {flat(body.summary, CAP.message)}
                </p>
              )}
            </>
          )}
        </div>
      )}
    </div>
  );
}

function PartyName({ party, name, byMember }: { party: Party; name: (p: Party) => string; byMember: ReadonlyMap<MemberKey, Member> }) {
  const colour = party.kind === 'member' ? (byMember.get(party.member)?.colour ?? null) : null;
  return (
    <span className="g-bmsg-party">
      {party.kind === 'member' && <span className="g-swatch" data-colour={colour ?? undefined} aria-hidden="true" />}
      <span className="g-bmsg-partyname">{name(party)}</span>
    </span>
  );
}

function MessagesColumn({
  team,
  world,
  members,
  byMember,
  draft,
  setDraft,
  toToken,
}: {
  team: Team;
  world: WorldState;
  members: readonly Member[];
  byMember: ReadonlyMap<MemberKey, Member>;
  draft: ToDraft;
  setDraft: (patch: Partial<ToDraft>) => void;
  toToken: number;
}) {
  const { t, te } = useI18n();
  const stored = useMemo(() => messagesOfTeam(world, team.key), [world, team.key]);
  const [older, setOlder] = useState<TeamMessage[]>([]);
  const [hasMore, setHasMore] = useState<boolean | null>(null);
  const [pageError, setPageError] = useState<IpcError | null>(null);
  const all = useMemo(() => {
    const seen = new Set(stored.map((m) => m.key));
    return [...older.filter((m) => !seen.has(m.key)), ...stored].sort((a, b) => a.at - b.at);
  }, [older, stored]);
  const count = messageCount(team, all.length);
  const name = (p: Party) => partyName(p, byMember, t);
  const canPage = hasMore === true || (hasMore === null && stored.length >= STORE_MESSAGE_CAP);

  const loadEarlier = () => {
    const first = all[0];
    if (!first) return;
    commands.team_messages_page({ teamKey: team.key, before: first.at, count: 100 }).then(
      (res) => {
        setOlder((o) => [...res.messages, ...o]);
        setHasMore(res.hasMore);
        setPageError(null);
      },
      (e: IpcError) => setPageError(e),
    );
  };

  return (
    <section className="g-board-section" aria-label={t('team.board.messages')}>
      <div className="g-board-head">
        <h2 className="g-board-title">{t('team.board.messages')}</h2>
        <CountLabel count={count} />
      </div>
      {canPage && (
        <div className="g-board-more">
          <button type="button" className="g-tbtn" onClick={loadEarlier}>
            {t('team.msg.earlier')}
          </button>
          {pageError && <span className="g-bmsg-line">{te(pageError)}</span>}
        </div>
      )}
      {all.length === 0 ? (
        count.kind === 'unknown' ? <p className="g-board-empty">{t('team.messagesUnknown')}</p> : <div className="g-board-list" />
      ) : (
        <BoardList
          role="log"
          count={all.length}
          getKey={(i) => (all[i] as TeamMessage).key}
          renderRow={(i) => <MessageRow msg={all[i] as TeamMessage} name={name} byMember={byMember} />}
          label={t('team.board.messages')}
        />
      )}
      <ToComposer team={team} world={world} members={members} draft={draft} setDraft={setDraft} focusToken={toToken} />
    </section>
  );
}

// ---------------------------------------------------------------------------
// To: composer, or the ownership bar
// ---------------------------------------------------------------------------

function ToComposer({
  team,
  world,
  members,
  draft,
  setDraft,
  focusToken,
}: {
  team: Team;
  world: WorldState;
  members: readonly Member[];
  draft: ToDraft;
  setDraft: (patch: Partial<ToDraft>) => void;
  focusToken: number;
}) {
  const { t, te } = useI18n();
  const inputRef = useRef<HTMLTextAreaElement>(null);
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<IpcError | null>(null);
  const recipients = members.filter((m) => memberRoute(world, team, m) !== null);
  const firstMember = recipients.find((m) => m.role !== 'lead') ?? recipients[0] ?? null;
  const selected = recipients.find((m) => m.key === draft.to) ?? firstMember;
  const route = selected ? memberRoute(world, team, selected) : null;

  const lastToken = useRef(focusToken);
  useEffect(() => {
    if (focusToken === lastToken.current) return;
    lastToken.current = focusToken;
    const id = requestAnimationFrame(() => inputRef.current?.focus());
    return () => cancelAnimationFrame(id);
  }, [focusToken]);

  if (recipients.length === 0 || !selected || route === null) return <TeamOwnershipBar team={team} world={world} />;

  const submit = () => {
    const text = draft.text;
    if (busy || text.trim() === '') return;
    const who = flat(selected.name, CAP.name);
    if (route === 'pane') {
      setBusy(true);
      commands.team_send({ teamKey: team.key, memberKey: selected.key, clientMsgId: newClientMsgId(), text }).then(
        () => {
          setBusy(false);
          setError(null);
          setDraft({ text: '' });
        },
        (e: IpcError) => {
          setBusy(false);
          setError(e);
        },
      );
      return;
    }
    // Through the lead: prefilled there, never sent from here.
    const line = route === 'self' ? text.trim() : tellLine(who, text);
    if (prefillLead(team.key, line)) setDraft({ text: '' });
  };

  const onKeyDown = (event: ReactKeyboardEvent<HTMLTextAreaElement>) => {
    if (event.key !== 'Enter') return;
    if (isImeComposing(event.nativeEvent)) return;
    if (event.shiftKey || event.metaKey || event.ctrlKey || event.altKey) return;
    event.preventDefault();
    if (event.repeat) return;
    submit();
  };

  return (
    <div className="g-to">
      <div className="g-to-row">
        <label className="g-to-label" htmlFor={`to-${team.key}`}>
          {t('team.msg.to')}
        </label>
        <select
          id={`to-${team.key}`}
          className="g-to-select"
          value={selected.key}
          onChange={(e) => setDraft({ to: e.target.value as MemberKey })}
        >
          {recipients.map((m) => (
            <option key={m.key} value={m.key}>
              {flat(m.name, CAP.name)}
              {m.role === 'lead' ? ` · ${t('team.lead')}` : ''}
            </option>
          ))}
        </select>
      </div>
      <div className="g-to-row">
        <textarea
          ref={inputRef}
          className="g-to-input"
          aria-label={t('team.msg.input')}
          rows={1}
          placeholder={t('team.msg.placeholder')}
          value={draft.text}
          onChange={(e) => {
            setDraft({ text: e.target.value });
            if (error) setError(null);
          }}
          onKeyDown={onKeyDown}
        />
        <button type="button" className="g-sbtn g-sbtn-primary" disabled={busy || draft.text.trim() === ''} onClick={submit}>
          {t('action.send')}
        </button>
      </div>
      <p className="g-to-note">{route === 'pane' ? t('team.msg.sentNote') : t('team.msg.viaLeadNote')}</p>
      {error && (
        <p className="g-to-note" role="status">
          {te(error)}
          {error.detail ? <span className="mono"> {error.detail}</span> : null}
        </p>
      )}
    </div>
  );
}

/**
 * Where a team Gilde cannot message from here: "This team runs in iTerm2.
 * Talk to a teammate in its pane, or ask the lead." with [Go to {owner}], and
 * [Ask the lead] when the lead runs in Gilde (ux-ia §3.10, row 100); the
 * orchestrator's teams say "Message it from its lead." (row 121, no actions).
 */
export function TeamOwnershipBar({ team, world }: { team: Team; world: WorldState }) {
  const { t, te } = useI18n();
  const [error, setError] = useState<IpcError | null>(null);
  const lead = leadMember(world, team);
  const leadSession = sessionOf(world, lead?.sessionUid ?? null);
  const watched = team.source === 'orchestrator' ? null : (leadSession?.control.kind === 'watching' && leadSession.caps.ownerFocus ? leadSession : null);
  const askLead = team.source !== 'orchestrator' && leadTarget(world, team) !== null;
  return (
    <div className="g-team-own" role="group" aria-label={t(ownershipKey(team))}>
      <p className="g-team-own-text">{t(ownershipKey(team))}</p>
      {(watched || askLead) && (
        <span className="g-team-own-actions">
          {watched && (
            <button
              type="button"
              className="g-sbtn"
              onClick={() => commands.owner_focus({ uid: watched.uid }).then(() => setError(null), (e: IpcError) => setError(e))}
            >
              {t('action.goToOwner', { owner: ownerWord(watched, t) })}
            </button>
          )}
          {askLead && (
            <button type="button" className="g-sbtn" onClick={() => prefillLead(team.key, '')}>
              {t('action.askLead')}
            </button>
          )}
        </span>
      )}
      {error && <p className="g-to-note">{te(error)}</p>}
    </div>
  );
}

