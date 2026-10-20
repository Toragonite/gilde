/**
 * The Team view's scope and grouping — pure rules, kept out of the components
 * so the tests can pin them.
 *
 * Repository identity is WORKTREE-AWARE and comes from Rust (`repo_identity`,
 * held per path in `team.identities`): two paths are one repository when their
 * keys are equal — the normalized `origin` URL when there is one (separate
 * clones collapse), else git's common dir (the main checkout, its subfolders,
 * a lead running in a linked worktree and the members' worktrees under the
 * Orchestrator's data root all collapse). git never runs from here.
 *
 * Until a path's identity arrives it is grouped by itself (`path:<path>`), so
 * the list renders at once and regroups when the answers come in. "This
 * workspace" uses a path-prefix test against the selected folder for exactly
 * that window, then the keys.
 */

import type { MessageKey } from '../i18n';
import type { I18n } from '../i18n/useI18n';
import type { CcoMemberState, CcoTeamView, Project, RepoIdentity } from '../lib/ipc';
import type { TeamGroupBy, TeamScope } from '../store/uiSlice';
import { CAP, flat } from './model';

export type IdentityMap = ReadonlyMap<string, RepoIdentity>;

// ---------------------------------------------------------------------------
// paths
// ---------------------------------------------------------------------------

/** The last segment of a POSIX path (`/a/b/` → `b`). */
export function baseName(path: string): string {
  const parts = path.split('/').filter((p) => p !== '');
  return parts[parts.length - 1] ?? path;
}

/** The parent folder's own name (`/a/b/c` → `b`), or '' at the top. */
export function parentName(path: string): string {
  const parts = path.split('/').filter((p) => p !== '');
  return parts.length >= 2 ? parts[parts.length - 2] : '';
}

/** `path` is `base` or inside it (whole segments only). */
export function isWithin(path: string, base: string): boolean {
  if (base === '' || path === '') return false;
  const b = base.endsWith('/') ? base.slice(0, -1) : base;
  return path === b || path.startsWith(`${b}/`);
}

// ---------------------------------------------------------------------------
// identity
// ---------------------------------------------------------------------------

/** The identity of a path, when Rust gave a definite one (not `unknown`). */
export function knownIdentity(path: string, ids: IdentityMap): RepoIdentity | undefined {
  const id = ids.get(path);
  return id && id.kind !== 'unknown' ? id : undefined;
}

/** The repository key of a path: its identity's key, else the path alone. */
export function repoKeyOf(path: string, ids: IdentityMap): string {
  const id = knownIdentity(path, ids);
  return id && id.kind === 'repo' ? id.key : `path:${path}`;
}

/** Every path whose identity the Team view needs: workspaces, member worktrees, folders. */
export function identityPaths(views: Iterable<CcoTeamView>, projects: readonly Project[]): string[] {
  const out = new Set<string>();
  for (const view of views) {
    if (view.state.workspace) out.add(view.state.workspace);
    for (const member of view.state.members) {
      if (member.worktree?.path) out.add(member.worktree.path);
    }
  }
  for (const project of projects) {
    if (project.path) out.add(project.path);
  }
  return [...out].sort();
}

// ---------------------------------------------------------------------------
// scope
// ---------------------------------------------------------------------------

/** The folder "This workspace" means: the chosen one if it is still listed, else the first. */
export function scopeFolder(projects: readonly Project[], chosenId: string | null): Project | null {
  // The user's choice stands even when its folder is gone (they see it marked);
  // the automatic default prefers a folder that is still there.
  return (
    projects.find((p) => p.id === chosenId) ??
    projects.find((p) => p.pathExists !== false) ??
    projects[0] ??
    null
  );
}

/** Whether a project's folder is gone (an older backend never says so). */
export function folderMissing(project: Project): boolean {
  return project.pathExists === false;
}

/** The scope in force: "This workspace" needs a folder; without one it is all workspaces. */
export function effectiveScope(scope: TeamScope, folder: Project | null): TeamScope {
  return scope === 'workspace' && folder !== null ? 'workspace' : 'all';
}

/**
 * Is a team in the selected folder's repository? By identity key once both
 * answers are known; until then, by path (the workspace inside the folder or
 * its repository root).
 */
export function teamInFolderRepo(view: CcoTeamView, folder: Project, ids: IdentityMap): boolean {
  const workspace = view.state.workspace;
  const teamId = knownIdentity(workspace, ids);
  const folderId = knownIdentity(folder.path, ids);
  if (teamId && folderId) {
    return teamId.kind === 'repo' && folderId.kind === 'repo'
      ? teamId.key === folderId.key
      : isWithin(workspace, folder.path);
  }
  return isWithin(workspace, folder.path) || isWithin(workspace, folder.canonicalPath);
}

/** The teams the scope shows, in the given order. */
export function teamsInScope(
  views: readonly CcoTeamView[],
  scope: TeamScope,
  folder: Project | null,
  ids: IdentityMap,
): CcoTeamView[] {
  if (effectiveScope(scope, folder) === 'all' || folder === null) return [...views];
  return views.filter((view) => teamInFolderRepo(view, folder, ids));
}

// ---------------------------------------------------------------------------
// grouping
// ---------------------------------------------------------------------------

/** The status groups, in display order. `other` = read-only or a status this build does not know. */
export const STATUS_GROUPS = ['active', 'suspended', 'ended', 'damaged', 'other'] as const;
export type StatusGroup = (typeof STATUS_GROUPS)[number];

export const STATUS_GROUP_KEYS: Record<StatusGroup, MessageKey> = {
  active: 'team.group.status.active',
  suspended: 'team.group.status.suspended',
  ended: 'team.group.status.ended',
  damaged: 'team.group.status.damaged',
  other: 'team.group.status.other',
};

/** A team's status group: an `active` team whose host is gone (L7) is suspended. */
export function statusGroupOf(view: CcoTeamView): StatusGroup {
  switch (view.state.status) {
    case 'active':
      return view.believed ? 'active' : 'suspended';
    case 'suspended':
    case 'ended':
    case 'damaged':
      return view.state.status;
    default:
      return 'other';
  }
}

/**
 * What a group header says, as data (the component translates it):
 *
 *   repo      `<name>`                           (a repository)
 *   folder    `<name>`                           (pending identity, or a subfolder)
 *   notGit    `Not a git folder: <name>`         (not a repository, or gone)
 *   worktree  `<name> · worktree <branch>`       (Workspace folder mode, a linked worktree)
 *   status    the status group's words
 *
 * `parent` is set only to tell two same-named groups apart.
 */
export interface GroupLabel {
  kind: 'repo' | 'folder' | 'notGit' | 'worktree' | 'status';
  name: string;
  parent?: string;
  branch?: string | null;
  status?: StatusGroup;
}

export interface TeamGroup {
  key: string;
  label: GroupLabel;
  /** Full paths for the tooltip (repository roots, or the workspace). */
  paths: string[];
  teams: CcoTeamView[];
}

interface Draft {
  key: string;
  label: GroupLabel;
  paths: Set<string>;
  /** The path whose parent folder disambiguates the label. */
  anchor: string;
  teams: CcoTeamView[];
}

/** The label (and tooltip path) of a path's repository group. */
function repositoryLabel(path: string, ids: IdentityMap): { label: GroupLabel; path: string } {
  const id = knownIdentity(path, ids);
  if (id === undefined) return { label: { kind: 'folder', name: baseName(path) }, path };
  if (id.kind !== 'repo') return { label: { kind: 'notGit', name: baseName(path) }, path };
  const root = id.root ?? id.toplevel ?? path;
  return { label: { kind: 'repo', name: baseName(root) }, path: root };
}

/** The label of a path's own (Workspace folder) group. */
function folderLabel(path: string, ids: IdentityMap): GroupLabel {
  const id = knownIdentity(path, ids);
  if (id === undefined) return { kind: 'folder', name: baseName(path) };
  if (id.kind !== 'repo') return { kind: 'notGit', name: baseName(path) };
  const repo = baseName(id.root ?? id.toplevel ?? path);
  if (id.linkedWorktree) return { kind: 'worktree', name: repo, branch: id.branch };
  const top = id.toplevel ?? id.root ?? path;
  if (path === top || !isWithin(path, top)) return { kind: 'repo', name: repo };
  return { kind: 'folder', name: `${repo}/${path.slice(top.length + 1)}` };
}

/** Give same-named groups their parent folder's name. */
function disambiguate(drafts: Draft[]): void {
  const byName = new Map<string, Draft[]>();
  for (const d of drafts) {
    if (d.label.kind === 'status') continue;
    const name = `${d.label.kind === 'worktree' ? 'wt' : 'x'}:${d.label.name}:${d.label.branch ?? ''}`;
    byName.set(name, [...(byName.get(name) ?? []), d]);
  }
  for (const same of byName.values()) {
    if (same.length < 2) continue;
    for (const d of same) {
      const parent = parentName(d.anchor);
      if (parent !== '') d.label = { ...d.label, parent };
    }
  }
}

/**
 * The teams in groups, in the order given (groups by their first team). Every
 * team lands in exactly one group.
 */
export function groupTeams(views: readonly CcoTeamView[], by: TeamGroupBy, ids: IdentityMap): TeamGroup[] {
  if (by === 'status') {
    const groups = new Map<StatusGroup, CcoTeamView[]>();
    for (const view of views) {
      const group = statusGroupOf(view);
      groups.set(group, [...(groups.get(group) ?? []), view]);
    }
    return STATUS_GROUPS.filter((g) => groups.has(g)).map((g) => ({
      key: `status:${g}`,
      label: { kind: 'status', name: g, status: g },
      paths: [],
      teams: groups.get(g) ?? [],
    }));
  }

  const drafts = new Map<string, Draft>();
  for (const view of views) {
    const workspace = view.state.workspace;
    let key: string;
    let label: GroupLabel;
    let tipPath: string;
    if (by === 'repository') {
      key = repoKeyOf(workspace, ids);
      ({ label, path: tipPath } = repositoryLabel(workspace, ids));
    } else {
      key = `folder:${workspace}`;
      label = folderLabel(workspace, ids);
      tipPath = workspace;
    }
    const held = drafts.get(key);
    if (held) {
      held.teams.push(view);
      held.paths.add(tipPath);
      // The group's label is its alphabetically first root (a stable choice
      // when clones of one origin collapse).
      if (by === 'repository' && label.kind === 'repo' && tipPath < held.anchor) {
        held.label = label;
        held.anchor = tipPath;
      }
    } else {
      drafts.set(key, { key, label, paths: new Set([tipPath]), anchor: tipPath, teams: [view] });
    }
  }
  const out = [...drafts.values()];
  disambiguate(out);
  return out.map((d) => ({ key: d.key, label: d.label, paths: [...d.paths].sort(), teams: d.teams }));
}

// ---------------------------------------------------------------------------
// worktrees
// ---------------------------------------------------------------------------

/** `cco/<team>-<suffix>/<member>` → `<member>`; any other branch as it is. */
export function memberBranchShort(branch: string): string {
  const match = /^cco\/[^/]+\/(.+)$/.exec(branch);
  return match ? match[1] : branch;
}

/** The lead runs in a linked worktree: its branch (null = detached); else null. */
export function leadWorktree(view: CcoTeamView, ids: IdentityMap): { branch: string | null } | null {
  const id = knownIdentity(view.state.workspace, ids);
  return id && id.kind === 'repo' && id.linkedWorktree ? { branch: id.branch } : null;
}

/**
 * Does a member's worktree follow the team's repository?
 *
 *   same     it is a worktree of the lead's repository
 *   missing  the folder is gone, or git no longer sees a repository there
 *   other    it belongs to a different repository
 *   pending  not known yet (or the member has no worktree yet)
 */
export type WorktreeFollow = 'same' | 'missing' | 'other' | 'pending';

export function memberWorktreeFollow(view: CcoTeamView, member: CcoMemberState, ids: IdentityMap): WorktreeFollow {
  const path = member.worktree?.path;
  if (!path) return 'pending';
  const id = knownIdentity(path, ids);
  if (id === undefined) return 'pending';
  if (id.kind !== 'repo') return 'missing';
  const lead = knownIdentity(view.state.workspace, ids);
  if (lead === undefined || lead.kind !== 'repo') return 'pending';
  return lead.key === id.key ? 'same' : 'other';
}

// ---------------------------------------------------------------------------
// project folders
// ---------------------------------------------------------------------------

export interface FolderEntry {
  project: Project;
  /** The folder is a linked worktree: its branch (null = detached). */
  worktree: { branch: string | null } | null;
}

export interface FolderGroup {
  key: string;
  label: GroupLabel;
  paths: string[];
  folders: FolderEntry[];
}

/** The project folders, grouped by the same repository identity (in the roster's order). */
export function groupFolders(projects: readonly Project[], ids: IdentityMap): FolderGroup[] {
  const drafts = new Map<string, Draft & { folders: FolderEntry[] }>();
  for (const project of projects) {
    const key = repoKeyOf(project.path, ids);
    const { label, path } = repositoryLabel(project.path, ids);
    const id = knownIdentity(project.path, ids);
    const entry: FolderEntry = {
      project,
      worktree: id && id.kind === 'repo' && id.linkedWorktree ? { branch: id.branch } : null,
    };
    const held = drafts.get(key);
    if (held) {
      held.folders.push(entry);
      held.paths.add(path);
      if (label.kind === 'repo' && path < held.anchor) {
        held.label = label;
        held.anchor = path;
      }
    } else {
      drafts.set(key, { key, label, paths: new Set([path]), anchor: path, teams: [], folders: [entry] });
    }
  }
  const out = [...drafts.values()];
  disambiguate(out);
  return out.map((d) => ({ key: d.key, label: d.label, paths: [...d.paths].sort(), folders: d.folders }));
}

// ---------------------------------------------------------------------------
// words
// ---------------------------------------------------------------------------

/** Live teams first, then the newest — the order the Team pane's chips use. */
export function orderedTeams(views: Map<string, CcoTeamView>): CcoTeamView[] {
  return [...views.values()].sort(
    (a, b) => Number(b.live) - Number(a.live) || b.state.createdAt - a.state.createdAt,
  );
}

export const GROUP_BY_KEYS = {
  repository: 'team.groupBy.repository',
  folder: 'team.groupBy.folder',
  status: 'team.groupBy.status',
} as const;

/** A group header's words (see `GroupLabel`). Every part is flattened and capped. */
export function groupLabelText(label: GroupLabel, t: I18n['t']): string {
  let text: string;
  switch (label.kind) {
    case 'status':
      return t(STATUS_GROUP_KEYS[label.status ?? 'other']);
    case 'notGit':
      text = t('team.group.notGit', { name: flat(label.name, CAP.name) });
      break;
    case 'worktree':
      text = t('team.group.worktree', {
        repo: flat(label.name, CAP.name),
        branch: label.branch ? flat(label.branch, CAP.label) : t('team.group.detached'),
      });
      break;
    default:
      text = flat(label.name, CAP.label);
  }
  return label.parent ? t('team.group.parent', { name: text, parent: flat(label.parent, CAP.name) }) : text;
}

/** "lead in worktree <branch>", or null when the lead is not in a linked worktree. */
export function leadWorktreeText(view: CcoTeamView, ids: IdentityMap, t: I18n['t']): string | null {
  const lead = leadWorktree(view, ids);
  if (lead === null) return null;
  return t('team.leadInWorktree', {
    branch: lead.branch ? flat(lead.branch, CAP.label) : t('team.group.detached'),
  });
}

