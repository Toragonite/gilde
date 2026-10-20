/**
 * The Team view's scope, grouping preference and repository identities, shared
 * by the sidebar and the pane. Identities are asked of Rust (`repo_identity`)
 * for every path in sight — team workspaces, member worktrees, project folders —
 * when that set changes and every IDENTITY_POLL_MS after (the slice skips paths
 * asked recently). Nothing here blocks rendering: until an answer arrives each
 * path is its own provisional group.
 */

import { useEffect, useMemo } from 'react';

import type { Project } from '../lib/ipc';
import { useAppStore, useShallow } from '../store/useAppStore';
import type { TeamGroupBy, TeamScope } from '../store/uiSlice';
import { effectiveScope, identityPaths, scopeFolder } from './grouping';
import type { IdentityMap } from './grouping';

/** How often the paths in sight are offered for (re)identification. */
export const IDENTITY_POLL_MS = 10_000;

/** The identities of every path in sight, kept fresh while the caller is mounted. */
export function useRepoIdentities(): IdentityMap {
  const views = useAppStore((s) => s.team.views);
  const roster = useAppStore((s) => s.worktree.roster);
  const identities = useAppStore((s) => s.team.identities);
  const resolve = useAppStore((s) => s.resolveRepoIdentities);

  const pathsKey = useMemo(
    () => JSON.stringify(identityPaths(views.values(), roster.map((entry) => entry.project))),
    [views, roster],
  );

  useEffect(() => {
    const paths = JSON.parse(pathsKey) as string[];
    if (paths.length === 0) return undefined;
    void resolve(paths);
    const timer = setInterval(() => void resolve(paths), IDENTITY_POLL_MS);
    return () => clearInterval(timer);
  }, [pathsKey, resolve]);

  return identities;
}

export interface TeamScopeState {
  /** What the user chose. */
  scope: TeamScope;
  /** What is in force: `workspace` only when there is a folder to mean it. */
  effective: TeamScope;
  groupBy: TeamGroupBy;
  /** The folder "This workspace" means (null when no folder is added). */
  folder: Project | null;
  projects: Project[];
}

export function useTeamScope(): TeamScopeState {
  const { scope, groupBy, folderId } = useAppStore(
    useShallow((s) => ({ scope: s.ui.teamScope, groupBy: s.ui.teamGroupBy, folderId: s.ui.teamFolderId })),
  );
  const roster = useAppStore((s) => s.worktree.roster);
  const projects = useMemo(() => roster.map((entry) => entry.project), [roster]);
  const folder = scopeFolder(projects, folderId);
  return { scope, effective: effectiveScope(scope, folder), groupBy, folder, projects };
}
