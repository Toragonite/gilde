/**
 * TeamPane — the READ-ONLY preview of a Claude Code Orchestrator team (team
 * contract v2 §8.2, S1-2): the roster with the engine's state pills, the board
 * grouped by state, the last messages, and a member's activity.
 *
 * What is NOT here, on purpose: a composer, a member action (interrupt, stop,
 * pause, restart), a take-over, a board edit. The only controls are local —
 * which team and which member to look at, a re-read, and adding a project
 * folder to watch. Every pane carries the preview line, and steering happens in
 * the lead session or VS Code.
 *
 * Plain language (F7): engine codes (member reasons, warning kinds, mailbox
 * modes, message kinds, turn outcomes) are shown as sentences from `model.ts`;
 * a code this build does not know falls back to a generic sentence with the
 * raw code in a tooltip. Member rows are at most two lines — the details of a
 * selected member are in the inspector (`TeamMemberInspector`). Process ids
 * appear only in tooltips.
 *
 * All data comes from the store (`store/teamSlice.ts`), which holds what the
 * sidecar observed and Rust relayed; every string of it goes through `flat()`.
 */

import { useEffect, useLayoutEffect, useMemo, useRef } from 'react';
import type { ReactNode } from 'react';

import { Badge } from '../components/Badge';
import { EmptyState } from '../components/EmptyState';
import { FolderPlusIcon, RefreshIcon } from '../components/icons';
import { StatusDot, StatusPill, statusTone } from '../components/StatusDot';
import { useI18n } from '../i18n/useI18n';
import type { I18n } from '../i18n/useI18n';
import { formatCostUsd, formatNumber, formatTimeOfDay } from '../lib/format';
import type { Locale } from '../lib/ipc';
import type { CcoBoardTask, CcoMemberState, CcoTeamListResult, CcoTeamMessage, CcoTeamView } from '../lib/ipc';
import type { PaneViewProps } from '../panes/Pane';
import { useAppStore, useShallow } from '../store/useAppStore';
import { leadWorktreeText, orderedTeams, teamsInScope } from './grouping';
import type { IdentityMap } from './grouping';
import { useRepoIdentities, useTeamScope } from './useTeamScope';
import { ENGINE_SENDER, isEngineMessage, localizeNotice, noticeDisplay, senderDisplay, warningDisplay } from './notices';
import { formatGb, healthMemberOf, teamAlerts } from '../shell/healthText';
import {
  BOARD_COLUMNS,
  BOARD_COLUMN_KEYS,
  CAP,
  boardColumns,
  flat,
  memberPill,
  messageKindKey,
  providerBadge,
  reasonLabel,
  statusHint,
  teamPill,
  warningLabel,
} from './model';
import './team.css';

/** Messages listed in the timeline (the sidecar keeps 50). */
const TIMELINE_SHOWN = 30;
/** Warnings listed under the header. */
const WARNINGS_SHOWN = 8;

export function TeamPane(_props: PaneViewProps) {
  const { t, td } = useI18n();
  const startTeamWatch = useAppStore((s) => s.startTeamWatch);
  const refreshTeams = useAppStore((s) => s.refreshTeams);
  const { list, views, selectedTeamId, loading, error } = useAppStore(
    useShallow((s) => ({
      list: s.team.list,
      views: s.team.views,
      selectedTeamId: s.team.selectedTeamId,
      loading: s.team.loading,
      error: s.team.error,
    })),
  );

  // Watch while shown: one read now, then the list every few seconds.
  useEffect(() => startTeamWatch(), [startTeamWatch]);

  // The scope needs the project folders even while the sidebar is hidden.
  const rosterLoaded = useAppStore((s) => s.worktree.loaded);
  const loadRoster = useAppStore((s) => s.loadRoster);
  useEffect(() => {
    if (!rosterLoaded) void loadRoster();
  }, [rosterLoaded, loadRoster]);

  // The sidebar's scope, reflected here: the header says which, the chips list
  // only the teams in it (plus the selected one, so it is never orphaned).
  const ids = useRepoIdentities();
  const { effective, folder } = useTeamScope();
  const chips = useMemo(() => {
    const shown = teamsInScope(orderedTeams(views), effective, folder, ids);
    const selected = selectedTeamId === null ? undefined : views.get(selectedTeamId);
    return selected && !shown.includes(selected) ? [selected, ...shown] : shown;
  }, [views, effective, folder, ids, selectedTeamId]);

  const view = selectedTeamId === null ? undefined : views.get(selectedTeamId);

  // Live check G-a: the pane opens at the TOP (the team header). The scroll
  // container is one element shared by every team and by the loading → view
  // transition, so without this a team opened at whatever offset the previous
  // one (or the previous render) was left at — usually down in the messages.
  const bodyRef = useRef<HTMLDivElement | null>(null);
  const shownTeamId = view?.teamId ?? null;
  useLayoutEffect(() => {
    resetTeamScroll(bodyRef.current);
  }, [shownTeamId]);

  return (
    <div className="team">
      <div className="hdr team-hdr">
        <span className="strong">{t('view.team')}</span>
        <span
          className="s12 muted ell team-scope-label"
          data-scope={effective}
          title={effective === 'workspace' && folder ? flat(folder.path, CAP.tip) : undefined}
        >
          {effective === 'workspace' && folder
            ? t('team.scope.of', { name: flat(folder.name, CAP.name) })
            : t('team.scope.all')}
        </span>
        <TeamChips views={chips} selectedTeamId={selectedTeamId} reasons={listReasons(list, t)} />
        <span className="grow" />
        <button
          type="button"
          className="btn team-refresh"
          onClick={() => void refreshTeams()}
          aria-label={t('team.refresh')}
          title={t('team.refresh')}
        >
          <RefreshIcon size="sm" />
        </button>
      </div>
      <div className="team-preview s12 muted">{t('team.preview')}</div>
      <div className="team-body scroll-y" ref={bodyRef} data-testid="team-body">
        {error ? (
          <EmptyState
            message={td(error.key, error.params)}
            detail={error.detail}
            actionLabel={t('common.retry')}
            onAction={() => void refreshTeams()}
          />
        ) : list === null ? (
          loading ? <div className="team-loading s12 muted">{t('common.loading')}</div> : null
        ) : !list.supported ? (
          <EmptyState message={t('team.unsupported')} />
        ) : !list.available ? (
          <EmptyState
            message={t('team.sidecarDown')}
            actionLabel={t('team.refresh')}
            onAction={() => void refreshTeams()}
          />
        ) : view === undefined ? (
          <NoTeam list={list} />
        ) : (
          <TeamView view={view} listReason={listReasons(list, t).get(view.teamId)} ids={ids} />
        )}
      </div>
    </div>
  );
}

/**
 * Put the team body back at its top. Exported for the tests (no DOM there):
 * anything with a writable `scrollTop` will do; `null` (not mounted) is a no-op.
 */
export function resetTeamScroll(element: { scrollTop: number } | null): void {
  if (element !== null && element.scrollTop !== 0) element.scrollTop = 0;
}

/**
 * Why a connected dispatch host lists each team as it does (CCO `TeamListEntry.reason`,
 * relayed in the list summaries): team id → one flattened, capped line. Host text, shown
 * as text — in the user's language when it is a known engine sentence (`host gone`),
 * else as written; a blank one is no reason.
 */
export function listReasons(list: CcoTeamListResult | null, t: I18n['t']): Map<string, string> {
  const out = new Map<string, string>();
  for (const summary of list?.teams ?? []) {
    const line = noticeDisplay(summary.reason, t, CAP.listReason);
    if (line !== '') out.set(summary.teamId, line);
  }
  return out;
}

/** Which team to look at — only when there is more than one. A local choice, nothing is sent. */
function TeamChips({
  views,
  selectedTeamId,
  reasons,
}: {
  /** The teams in scope, in order. */
  views: CcoTeamView[];
  selectedTeamId: string | null;
  reasons: Map<string, string>;
}) {
  const { t } = useI18n();
  const selectTeam = useAppStore((s) => s.selectTeam);
  if (views.length < 2) return null;
  const ordered = views;
  return (
    <div className="team-chips row gap-4" role="group" aria-label={t('team.list.label')}>
      {ordered.map((v) => (
        <button
          key={v.teamId}
          type="button"
          className="chip"
          data-selected={v.teamId === selectedTeamId ? 'true' : undefined}
          aria-pressed={v.teamId === selectedTeamId}
          onClick={() => selectTeam(v.teamId)}
          title={reasons.has(v.teamId) ? t('team.listReason', { reason: reasons.get(v.teamId) ?? '' }) : undefined}
          data-reason={reasons.has(v.teamId) ? 'true' : undefined}
        >
          {flat(v.state.name, CAP.name)}
          {reasons.has(v.teamId) ? <span className="team-chip-reason muted"> · {flat(reasons.get(v.teamId), CAP.chipReason)}</span> : null}
        </button>
      ))}
    </div>
  );
}

/**
 * Which of the three "no team" situations this is (F3): the orchestrator is not
 * set up on this Mac (no readable `registry.json`), Teammate mode is off, or it
 * is on and no team exists yet.
 */
export type NoTeamCase = 'notSetUp' | 'modeOff' | 'empty';

export function noTeamCase(list: CcoTeamListResult): NoTeamCase {
  if (list.teamMode === 'on') return 'empty';
  if (list.teamMode === 'off') return 'modeOff';
  return 'notSetUp';
}

/** "Add project folder": the project_add flow (folder picker), then a re-read. */
export function AddProjectFolderButton({ variant = 'btn' }: { variant?: 'btn' | 'icon' }) {
  const { t } = useI18n();
  const addProject = useAppStore((s) => s.addProject);
  const refreshTeams = useAppStore((s) => s.refreshTeams);
  const onClick = () => {
    void addProject().then((project) => {
      if (project) void refreshTeams();
    });
  };
  if (variant === 'icon') {
    return (
      <button
        type="button"
        className="btn icon ghost"
        title={t('team.addFolder')}
        aria-label={t('team.addFolder')}
        onClick={onClick}
      >
        <FolderPlusIcon size="sm" />
      </button>
    );
  }
  return (
    <button type="button" className="btn" onClick={onClick}>
      <FolderPlusIcon size="sm" />
      {t('team.addFolder')}
    </button>
  );
}

/** The project_add refusal (e.g. not a git repository), one sentence, dismissible. */
export function ProjectFolderError() {
  const { t, td } = useI18n();
  const error = useAppStore((s) => s.worktree.error);
  const clear = useAppStore((s) => s.clearWorktreeError);
  if (!error) return null;
  return (
    <div className="team-note s12 team-warn row gap-6" role="alert">
      <span className="grow">{td(error.key, error.params)}</span>
      <button type="button" className="btn ghost" onClick={clear}>
        {t('common.close')}
      </button>
    </div>
  );
}

/** No team in sight: say which of the three cases it is, what to do, plus what the hosts said. */
function NoTeam({ list }: { list: CcoTeamListResult }) {
  const { t } = useI18n();
  const notes = list.hosts.filter((h) => h.state === 'no-teams' || h.state === 'failed');
  const which = noTeamCase(list);
  let body: ReactNode;
  if (which === 'notSetUp') {
    body = (
      <>
        <div className="team-empty-title">{t('team.notSetUp')}</div>
        <div className="s12 muted">{t('team.notSetUp.how')}</div>
        {list.dataRoot ? (
          <div className="s12 muted mono ell" title={list.dataRoot}>
            {t('team.notSetUp.where', { path: flat(list.dataRoot, CAP.path) })}
          </div>
        ) : null}
      </>
    );
  } else if (which === 'modeOff') {
    body = (
      <>
        <div className="team-empty-title">{t('team.modeOff')}</div>
        <div className="s12 muted">{t('team.modeOff.how')}</div>
      </>
    );
  } else {
    body = <div className="team-empty-title">{t('team.empty')}</div>;
  }
  return (
    <div className="empty team-empty" data-case={which}>
      {body}
      <div className="row gap-8">
        <AddProjectFolderButton />
      </div>
      <ProjectFolderError />
      {notes.map((host) => (
        <div key={host.key} className="team-note s12 muted" title={t('team.host.pid', { pid: host.hostPid })}>
          {host.state === 'no-teams' ? t('team.host.noTeams') : t('team.host.failed')}
        </div>
      ))}
    </div>
  );
}

function TeamView({
  view,
  listReason,
  ids,
}: {
  view: CcoTeamView;
  listReason: string | undefined;
  ids: IdentityMap;
}) {
  const { t, locale } = useI18n();
  const { state } = view;
  const leadInWorktree = leadWorktreeText(view, ids, t);
  const pill = teamPill(state, view.believed);
  const hint = statusHint(view);
  const columns = boardColumns(state.board);
  const nf = (n: number) => formatNumber(n, locale);

  return (
    <div className="team-view col">
      <section className="team-head col gap-4">
        <div className="row gap-8 team-title">
          <span className="strong">{flat(state.name, CAP.name)}</span>
          {state.title ? <span className="muted ell">{flat(state.title, CAP.title)}</span> : null}
          <StatusPill status={pill.status} label={pill.key ? t(pill.key) : pill.raw} />
          <TeamAlertChip teamId={view.teamId} teamName={state.name} />
          <Badge>{view.live ? t('team.source.live') : t('team.source.disk')}</Badge>
          {view.paneSessionPid !== undefined ? (
            <Badge title={t('team.host.pid', { pid: view.paneSessionPid })}>{t('team.inGilde')}</Badge>
          ) : null}
          {leadInWorktree ? (
            <span className="s12 muted ell team-lead-worktree" title={flat(state.workspace, CAP.tip)}>
              {leadInWorktree}
            </span>
          ) : null}
        </div>
        {hint ? <div className="team-hint s12">{t(hint)}</div> : null}
        {listReason ? (
          <div className="team-list-reason s12 muted" data-testid="team-list-reason">
            {t('team.listReason', { reason: listReason })}
          </div>
        ) : null}
        {state.status === 'damaged' && state.damagedReason ? (
          <pre className="mono s11 muted team-pre">{flat(state.damagedReason, CAP.reason)}</pre>
        ) : null}
        {state.goal ? (
          <div className="team-goal s12">
            <span className="muted">{t('team.section.goal')} · </span>
            {flat(state.goal, CAP.goal)}
          </div>
        ) : null}
        <div className="mono s12 muted row gap-8 team-figures">
          <span>
            {t('team.totals', {
              turns: nf(state.totals.turns),
              in: nf(state.totals.inputTokens),
              out: nf(state.totals.outputTokens),
            })}
          </span>
          {state.totals.costUsd !== null && state.totals.costUsd !== undefined ? (
            <span>{t('team.cost', { cost: formatCostUsd(state.totals.costUsd, locale) })}</span>
          ) : null}
          <span>
            {t('team.budgets', {
              today: nf(state.budgets.turnsToday),
              perDay: nf(state.budgets.turnsPerDay),
              running: nf(state.budgets.running),
              max: nf(state.budgets.maxRunning),
            })}
          </span>
          <span>{t('team.unread', { lead: nf(state.lead.unread), user: nf(state.user.unread) })}</span>
        </div>
        {state.budgets.pingPongTripped ? <div className="team-warn s12">{t('team.pingPong')}</div> : null}
        {state.warnings.length > 0 ? (
          <ul className="team-warnings s12" aria-label={t('team.section.warnings')}>
            {state.warnings.slice(0, WARNINGS_SHOWN).map((w, i) => {
              const label = warningLabel(w.kind);
              const detail = warningDisplay(w.kind, w.text, t, CAP.warning);
              return (
                <li
                  key={`${label.code}-${i}`}
                  className="team-warn"
                  title={label.known ? undefined : t('team.code', { code: label.code })}
                >
                  <span className="strong">{t(label.key)}</span>
                  {detail !== '' ? <> · {detail}</> : null}
                </li>
              );
            })}
          </ul>
        ) : null}
        {view.truncated ? <div className="team-note s12 muted">{t('team.truncated')}</div> : null}
      </section>

      <section className="team-section">
        <div className="sec">{t('team.section.members', { n: state.members.length })}</div>
        {state.members.length === 0 ? (
          <div className="team-note s12 muted">{t('team.noMembers')}</div>
        ) : (
          <>
            <div className="team-roster">
              {state.members.map((m) => (
                <MemberRow key={m.name} member={m} view={view} />
              ))}
            </div>
            <MemberHint />
          </>
        )}
      </section>

      <section className="team-section">
        <div className="sec">{t('team.section.board', { n: state.board.length })}</div>
        {state.board.length === 0 ? (
          <div className="team-note s12 muted">{t('team.boardEmpty')}</div>
        ) : (
          <div className="team-board">
            {BOARD_COLUMNS.map((column) => (
              <div key={column} className="team-column col" aria-label={t(BOARD_COLUMN_KEYS[column])}>
                <div className="team-column-head s12 muted">
                  {t(BOARD_COLUMN_KEYS[column])} · <span className="mono">{columns[column].length}</span>
                </div>
                {columns[column].map((task) => (
                  <BoardCard key={task.id} task={task} />
                ))}
              </div>
            ))}
          </div>
        )}
      </section>

      <section className="team-section">
        <div className="sec">{t('team.section.messages', { n: view.messages.length })}</div>
        {view.messages.length === 0 ? (
          <div className="team-note s12 muted">{t('team.noMessages')}</div>
        ) : (
          <ol className="team-timeline">
            {view.messages.slice(0, TIMELINE_SHOWN).map((m) => (
              <MessageRow key={m.seq} message={m} />
            ))}
          </ol>
        )}
      </section>
    </div>
  );
}

/**
 * The health alert chip of a team header: shown while any alert that is not
 * dismissed concerns this team. A click opens the health popover.
 */
function TeamAlertChip({ teamId, teamName }: { teamId: string; teamName: string }) {
  const { t } = useI18n();
  const snapshot = useAppStore((s) => s.health.snapshot);
  const setOpen = useAppStore((s) => s.setHealthPopover);
  const alerts = teamAlerts(snapshot, teamId, teamName);
  if (alerts.length === 0) return null;
  const severity = alerts.some((a) => a.severity === 'action') ? 'action' : 'attention';
  return (
    <button
      type="button"
      className="chip team-alert-chip"
      data-severity={severity}
      data-testid="team-alert-chip"
      title={t('health.indicator.title')}
      onClick={() => setOpen(true)}
    >
      {t(severity === 'action' ? 'health.indicator.action' : 'health.indicator.attention', { n: alerts.length })}
    </button>
  );
}

/** A member's live process figure and leftover marker, from the health snapshot. */
export function MemberHealth({ teamId, teamName, member }: { teamId: string; teamName: string; member: string }) {
  const { t, locale } = useI18n();
  const health = useAppStore((s) => healthMemberOf(s.health.snapshot, teamId, teamName, member));
  if (!health || health.pids.length === 0) return null;
  const pids = t('health.pids', { pids: health.pids.join(' ') });
  return (
    <>
      {health.state === 'leftover' ? (
        <span className="s11 team-leftover" data-testid="member-leftover" title={pids}>
          {t('health.member.leftover')}
        </span>
      ) : null}
      <span className="s11 team-mem" data-testid="member-memory" title={pids}>
        {formatGb(health.rssBytes, locale)}
      </span>
    </>
  );
}

/** The line under the roster: what selecting a member does. */
function MemberHint() {
  const { t } = useI18n();
  const selected = useAppStore((s) => s.team.selectedMember !== null);
  if (selected) return null;
  return <div className="team-note s12 muted">{t('team.selectMember')}</div>;
}

/** A member's reason as one sentence (with `until` when the engine gave one). */
export function reasonSentence(
  reason: NonNullable<CcoMemberState['reason']>,
  t: I18n['t'],
  locale: Locale,
): { text: string; title: string } {
  const label = reasonLabel(reason.code);
  const until = reason.until !== undefined ? ` ${t('team.until', { time: formatTimeOfDay(reason.until, locale) })}` : '';
  const detail = noticeDisplay(reason.text, t, CAP.reason);
  const title = [detail, label.known ? '' : t('team.code', { code: label.code })].filter(Boolean).join(' · ');
  return { text: `${t(label.key)}${until}`, title };
}

/**
 * One member, at most two lines (F7): who and in what state, then why (when
 * the engine gave a reason), else what it last did, else its account and
 * model. Clicking selects it — its details open in the inspector. Nothing else.
 */
function MemberRow({ member, view }: { member: CcoMemberState; view: CcoTeamView }) {
  const { t, locale } = useI18n();
  const selected = useAppStore((s) => s.team.selectedMember === member.name);
  const selectTeamMember = useAppStore((s) => s.selectTeamMember);
  const pill = memberPill(member, view.believed);
  const model = member.model ? flat(member.model, CAP.label) : t('team.modelDefault');
  const reason = member.reason ? reasonSentence(member.reason, t, locale) : null;
  const activity = member.lastActivity?.text ? flat(member.lastActivity.text, CAP.activity) : '';

  return (
    <button
      type="button"
      className="team-member"
      data-selected={selected ? 'true' : undefined}
      aria-pressed={selected}
      onClick={() => selectTeamMember(selected ? null : member.name)}
    >
      <span className="row gap-6 team-member-line">
        <StatusDot status={pill.status} />
        <span className="strong">{flat(member.name, CAP.name)}</span>
        <span className="muted ell">{flat(member.role, CAP.role)}</span>
        <span className="grow" />
        <span className="s12 team-tone" data-tone={statusTone(pill.status)}>
          {pill.key ? t(pill.key) : pill.raw}
        </span>
        <MemberHealth teamId={view.teamId} teamName={view.state.name} member={member.name} />
        <Badge>{providerBadge(member.provider)}</Badge>
      </span>
      {reason ? (
        <span
          className="s12 team-reason team-tone ell"
          data-tone={statusTone(pill.status)}
          title={reason.title || undefined}
        >
          {reason.text}
        </span>
      ) : activity ? (
        <span className="s12 muted ell team-member-activity">{activity}</span>
      ) : (
        <span className="s12 muted ell team-member-meta">
          {flat(member.accountLabel || member.worker, CAP.label)} · {model}
        </span>
      )}
    </button>
  );
}

function BoardCard({ task }: { task: CcoBoardTask }) {
  const { t } = useI18n();
  return (
    <div className="team-task col gap-2">
      <div className="row gap-6">
        <span className="mono s11 muted">{flat(task.id, 8)}</span>
        <span className="ell">{flat(task.title, CAP.title)}</span>
      </div>
      <div className="row gap-6 s11 muted">
        {task.assignee ? <span>{flat(task.assignee, CAP.name)}</span> : null}
        {task.state === 'cancelled' ? <span>{t('team.board.cancelled')}</span> : null}
        {task.paths.length > 0 ? <span>{t('team.board.paths', { n: task.paths.length })}</span> : null}
      </div>
      {task.note ? <div className="s11 muted ell">{flat(task.note, CAP.note)}</div> : null}
    </div>
  );
}

function MessageRow({ message }: { message: CcoTeamMessage }) {
  const { t, locale } = useI18n();
  const kindKey = messageKindKey(message.kind);
  return (
    <li className="team-message col gap-2" title={kindKey ? undefined : t('team.code', { code: flat(message.kind, 16) })}>
      <div className="row gap-6 s12">
        <span className="mono muted">{formatTimeOfDay(message.ts, locale)}</span>
        <span className="strong" data-sender={message.from === ENGINE_SENDER ? 'engine' : undefined}>
          {senderDisplay(message.from, t)}
        </span>
        <span className="muted">→</span>
        <span>{flat(message.to, CAP.name)}</span>
        {kindKey ? <Badge>{t(kindKey)}</Badge> : null}
        {message.urgent ? <Badge tone="warn">{t('team.urgent')}</Badge> : null}
        {message.task ? <span className="mono muted">{flat(message.task, 8)}</span> : null}
      </div>
      <div
        className="s12 team-message-body"
        title={isEngineMessage(message) && localizeNotice(message.body) ? flat(message.body, CAP.tip) : undefined}
      >
        {isEngineMessage(message) ? noticeDisplay(message.body, t) : flat(message.body, CAP.message)}
        {message.cut ? <span className="muted"> ({t('team.messageCut')})</span> : null}
      </div>
    </li>
  );
}
