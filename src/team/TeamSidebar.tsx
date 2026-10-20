/**
 * TeamSidebar — the left column on the Team view (F4).
 *
 *   Scope            All workspaces (default) | This workspace — the teams whose
 *                    REPOSITORY is the selected project folder's. Disabled, with a
 *                    one-line hint, while no project folder is added.
 *   Group by         Repository (default) | Workspace folder | Status. Repository
 *                    identity is worktree-aware (`grouping.ts`, from Rust's
 *                    `repo_identity`): the main checkout, a lead in a linked
 *                    worktree, the members' worktrees and a second clone of the
 *                    same origin are one group.
 *   Teams            each with its members; a member row shows its name first,
 *                    a worktree icon (branch and path in the tooltip) that
 *                    turns into a warning when the worktree is gone or belongs
 *                    to another repository, then its branch (short form, only
 *                    when it differs from the name), memory and state.
 *                    Clicking a team selects it in the Team pane; clicking a
 *                    member also selects that member.
 *   Project folders  grouped by the same repository identity; clicking one makes
 *                    it the folder "This workspace" means. Add (the project_add
 *                    folder picker) and Remove (forget the folder — nothing on
 *                    disk is touched).
 *
 * Read-only towards the orchestrator: selecting, scoping and grouping are local
 * (scope and grouping persist with the layout); the backend calls are the
 * project roster (`loadRoster`, `addProject`, `removeProject`) and the
 * read-only repository identities.
 */

import { useEffect, useMemo } from 'react';

import { StatusDot } from '../components/StatusDot';
import { AlertIcon, GitBranchIcon, TrashIcon } from '../components/icons';
import { useI18n } from '../i18n/useI18n';
import type { CcoMemberState, CcoTeamView } from '../lib/ipc';
import { useAppStore, useShallow } from '../store/useAppStore';
import type { TeamGroupBy, TeamScope } from '../store/uiSlice';
import { TEAM_GROUP_BYS } from '../store/uiSlice';
import { AddProjectFolderButton, MemberHealth, ProjectFolderError } from './TeamPane';
import {
  GROUP_BY_KEYS,
  folderMissing,
  groupFolders,
  groupLabelText,
  groupTeams,
  leadWorktreeText,
  memberBranchShort,
  memberWorktreeFollow,
  orderedTeams,
  teamsInScope,
} from './grouping';
import type { IdentityMap } from './grouping';
import { CAP, flat, memberPill, teamPill } from './model';
import { useRepoIdentities, useTeamScope } from './useTeamScope';
import './team.css';

export { orderedTeams };

export function TeamSidebar() {
  const { t } = useI18n();
  const { views, selectedTeamId, selectedMember } = useAppStore(
    useShallow((s) => ({
      views: s.team.views,
      selectedTeamId: s.team.selectedTeamId,
      selectedMember: s.team.selectedMember,
    })),
  );
  const loadRoster = useAppStore((s) => s.loadRoster);
  const removeProject = useAppStore((s) => s.removeProject);
  const refreshTeams = useAppStore((s) => s.refreshTeams);
  const selectTeam = useAppStore((s) => s.selectTeam);
  const selectTeamMember = useAppStore((s) => s.selectTeamMember);
  const setTeamScope = useAppStore((s) => s.setTeamScope);
  const setTeamGroupBy = useAppStore((s) => s.setTeamGroupBy);
  const setTeamFolder = useAppStore((s) => s.setTeamFolder);

  useEffect(() => {
    void loadRoster();
  }, [loadRoster]);

  const ids = useRepoIdentities();
  const { effective, groupBy, folder, projects } = useTeamScope();

  const teams = useMemo(() => orderedTeams(views), [views]);
  const scoped = useMemo(() => teamsInScope(teams, effective, folder, ids), [teams, effective, folder, ids]);
  const groups = useMemo(() => groupTeams(scoped, groupBy, ids), [scoped, groupBy, ids]);
  const folderGroups = useMemo(() => groupFolders(projects, ids), [projects, ids]);

  const pickMember = (teamId: string, name: string) => {
    if (selectedTeamId !== teamId) selectTeam(teamId);
    selectTeamMember(selectedTeamId === teamId && selectedMember === name ? null : name);
  };

  // Narrowing the scope moves the selection onto a team that is still shown.
  const chooseScope = (next: TeamScope) => {
    setTeamScope(next);
    const shown = teamsInScope(teams, next, folder, ids);
    if (shown.length > 0 && !shown.some((v) => v.teamId === selectedTeamId)) selectTeam(shown[0].teamId);
  };

  return (
    <div className="sb team-sb">
      <div className="hdr">
        <span className="strong s12">{t('team.list.label')}</span>
        <span className="muted s12">{scoped.length}</span>
        <div className="grow" />
        <select
          className="input team-sb-groupby s11"
          aria-label={t('team.groupBy.label')}
          title={t('team.groupBy.label')}
          value={groupBy}
          onChange={(e) => setTeamGroupBy(e.target.value as TeamGroupBy)}
        >
          {TEAM_GROUP_BYS.map((by) => (
            <option key={by} value={by}>
              {t(GROUP_BY_KEYS[by])}
            </option>
          ))}
        </select>
      </div>

      <div className="team-sb-scope col gap-4">
        <div className="seg" role="group" aria-label={t('team.scope.label')}>
          <button
            type="button"
            className="team-scope-all"
            aria-pressed={effective === 'all'}
            onClick={() => chooseScope('all')}
          >
            {t('team.scope.all')}
          </button>
          <button
            type="button"
            className="team-scope-workspace"
            aria-pressed={effective === 'workspace'}
            disabled={folder === null}
            title={folder === null ? t('team.scope.needsFolder') : flat(folder.path, CAP.tip)}
            onClick={() => chooseScope('workspace')}
          >
            {t('team.scope.workspace')}
          </button>
        </div>
        {folder === null ? (
          <div className="s11 muted team-scope-hint">{t('team.scope.needsFolder')}</div>
        ) : effective === 'workspace' ? (
          <div className="s11 muted ell team-scope-hint" title={flat(folder.path, CAP.tip)}>
            {t('team.scope.of', { name: flat(folder.name, CAP.name) })}
          </div>
        ) : null}
      </div>

      {teams.length === 0 ? (
        <div className="team-note s12 muted">{t('team.sidebar.none')}</div>
      ) : scoped.length === 0 ? (
        <div className="team-note s12 muted">{t('team.scope.empty')}</div>
      ) : (
        <div className="sidebar-tree team-sb-teams">
          {groups.map((group) => (
            <div key={group.key} className="col team-sb-group" data-group={group.key}>
              <div className="team-sb-group-hdr row gap-6 s11 muted" title={group.paths.map((p) => flat(p, CAP.tip)).join('\n') || undefined}>
                <span className="ell grow strong">{groupLabelText(group.label, t)}</span>
                <span className="mono">{group.teams.length}</span>
              </div>
              {group.teams.map((view) => {
                const pill = teamPill(view.state, view.believed);
                const teamSelected = view.teamId === selectedTeamId;
                const lead = leadWorktreeText(view, ids, t);
                const status = pill.key ? t(pill.key) : pill.raw;
                return (
                  <div key={view.teamId} className="col">
                    <button
                      type="button"
                      className="tree-row team-sb-team"
                      aria-pressed={teamSelected}
                      onClick={() => selectTeam(view.teamId)}
                      title={[status, lead, flat(view.state.workspace, CAP.tip)].filter(Boolean).join('\n')}
                    >
                      <StatusDot status={pill.status} pulse={false} />
                      <span className="ell grow s12 strong">{flat(view.state.name, CAP.name)}</span>
                      <span className="s11 muted">{status}</span>
                    </button>
                    {view.state.members.map((member) => (
                      <MemberRow
                        key={member.name}
                        view={view}
                        member={member}
                        ids={ids}
                        selected={teamSelected && selectedMember === member.name}
                        onPick={() => pickMember(view.teamId, member.name)}
                      />
                    ))}
                  </div>
                );
              })}
            </div>
          ))}
        </div>
      )}

      <div className="hdr team-sb-folders-hdr">
        <span className="strong s12">{t('team.folders')}</span>
        <span className="muted s12" data-testid="team-folders-count">{folderRowCount(folderGroups)}</span>
        <div className="grow" />
        <AddProjectFolderButton variant="icon" />
      </div>
      <ProjectFolderError />
      {folderRowCount(folderGroups) === 0 ? (
        <div className="team-note s12 muted">{t('team.folders.none')}</div>
      ) : (
        <div className="sidebar-tree">
          {folderGroups.map((group) => {
            const single = group.folders.length === 1 && group.folders[0].worktree === null;
            return (
              <div key={group.key} className="col team-sb-folder-group" data-group={group.key}>
                {single ? null : (
                  <div className="team-sb-group-hdr row s11 muted" title={group.paths.map((p) => flat(p, CAP.tip)).join('\n') || undefined}>
                    <span className="ell grow strong">{groupLabelText(group.label, t)}</span>
                  </div>
                )}
                {group.folders.map(({ project, worktree }) => {
                  const name = worktree
                    ? t('team.folders.worktree', {
                        branch: worktree.branch ? flat(worktree.branch, CAP.label) : t('team.group.detached'),
                      })
                    : flat(project.name, CAP.name);
                  const missing = folderMissing(project);
                  return (
                    <div
                      key={project.id}
                      className="tree-row team-sb-folder"
                      data-nested={single ? undefined : 'true'}
                      data-missing={missing ? 'true' : undefined}
                    >
                      <button
                        type="button"
                        className="team-sb-folder-pick grow ell s12"
                        aria-pressed={folder?.id === project.id}
                        title={`${t('team.scope.useFolder', { name: flat(project.name, CAP.name) })}\n${flat(project.path, CAP.tip)}${missing ? `\n${t('team.folders.missingHint')}` : ''}`}
                        onClick={() => setTeamFolder(project.id)}
                      >
                        {name}
                      </button>
                      {missing ? (
                        <span className="s11 team-folder-missing" title={t('team.folders.missingHint')}>
                          {t('team.folders.missing')}
                        </span>
                      ) : null}
                      <button
                        type="button"
                        className="btn icon ghost"
                        title={t('team.folders.removeHint')}
                        aria-label={t('team.folders.remove', { name: project.name })}
                        onClick={() => {
                          void removeProject(project.id).then(() => void refreshTeams());
                        }}
                      >
                        <TrashIcon size="sm" />
                      </button>
                    </div>
                  );
                })}
              </div>
            );
          })}
        </div>
      )}
    </div>
  );
}

/**
 * How many folder rows the list shows — the header's count is this number, so
 * the count and the rows can never disagree (live check G-b).
 */
export function folderRowCount(groups: readonly { folders: readonly unknown[] }[]): number {
  return groups.reduce((n, group) => n + group.folders.length, 0);
}

/**
 * One member under its team, readable at the default sidebar width:
 *
 *   identity   status dot, the member name (never truncated before anything
 *              else: it is the only item of its line that may shrink, and
 *              only once it alone is wider than the row), and the worktree
 *              marker as a compact icon whose tooltip carries the branch and
 *              the path (a warning icon when the worktree is gone or belongs to
 *              another repository).
 *   details    the short branch — omitted when it equals the member name, as
 *              `cco/<team>/<member>` usually does — the leftover marker, the
 *              memory figure and the state. They sit to the right while they
 *              fit and wrap, as one group, onto a second line when they do not.
 */
function MemberRow({
  view,
  member,
  ids,
  selected,
  onPick,
}: {
  view: CcoTeamView;
  member: CcoMemberState;
  ids: IdentityMap;
  selected: boolean;
  onPick: () => void;
}) {
  const { t } = useI18n();
  const mp = memberPill(member, view.believed);
  const name = flat(member.name, CAP.name);
  const branch = member.worktree?.branch ?? '';
  const path = member.worktree?.path ?? '';
  const short = branch ? flat(memberBranchShort(branch), CAP.name) : '';
  const showBranch = short !== '' && short !== name;
  const branchTitle = branch ? t('team.worktree.branchTitle', { branch: flat(branch, CAP.title) }) : '';
  const follow = memberWorktreeFollow(view, member, ids);
  const warn = follow === 'missing' || follow === 'other';
  const marker = follow === 'missing' ? t('team.worktree.missing') : follow === 'other' ? t('team.worktree.other') : t('team.worktree.marker');
  return (
    <button type="button" className="tree-row team-sb-member" aria-pressed={selected} onClick={onPick}>
      <span className="team-sb-member-id">
        <StatusDot status={mp.status} pulse={false} />
        <span className="s12 team-sb-member-name" data-testid="sb-member-name" title={name}>
          {name}
        </span>
        {path ? (
          <span
            className={`team-wt${warn ? ' team-warn' : ' muted'}`}
            data-follow={follow}
            role="img"
            aria-label={marker}
            title={[marker, branchTitle, flat(path, CAP.tip)].filter(Boolean).join('\n')}
          >
            {warn ? <AlertIcon size="sm" /> : <GitBranchIcon size="sm" />}
          </span>
        ) : null}
      </span>
      <span className="s11 team-sb-member-meta">
        {showBranch ? (
          <span className="mono muted ell team-sb-branch" title={[branchTitle, flat(path, CAP.tip)].filter(Boolean).join('\n')}>
            {short}
          </span>
        ) : null}
        <MemberHealth teamId={view.teamId} teamName={view.state.name} member={member.name} />
        <span className="muted team-sb-member-state">{mp.key ? t(mp.key) : mp.raw}</span>
      </span>
    </button>
  );
}
