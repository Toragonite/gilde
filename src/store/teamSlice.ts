/**
 * Team slice — the READ-ONLY preview of Claude Code Orchestrator teams (team
 * contract v2 §8.2, S1-2).
 *
 * A team is run by an engine in another product (a Claude Code session's dispatch
 * host). The sidecar observes it and Rust relays one whole VIEW per team; this
 * slice only keeps the newest view of each and asks for the list. Nothing here
 * sends anything to a team: there is no composer, no member action, no take-over.
 *
 * Three rules:
 *
 * 1. **Latest wins, by `(generation, rev)`.** A view arrives two ways — the
 *    `gilde://team/snapshot` event and `cco_team_get` — in either order. A view
 *    (or a withdrawal, `view: null`) is applied only when it is newer than what
 *    was last applied for that team; the sidecar's revs restart with it, which
 *    is what the generation is for.
 * 2. **The list repairs what the events lost.** Snapshot notifications are lossy
 *    under backpressure. Every list summary carries its team's newest rev, so a
 *    summary ahead of the held view triggers one `cco_team_get`, and a held view
 *    whose team the list no longer has is dropped.
 * 3. **Watching is lazy.** Nothing is asked of the sidecar until the Team view is
 *    opened (`startTeamWatch`); while it is shown the list is re-read every
 *    TEAM_LIST_POLL_MS, and once it was opened a sidecar that comes back is
 *    asked again.
 */

import type { StateCreator } from 'zustand';

import { onGildeEvent } from '../hooks/useEventBridge';
import { asGildeError, cmd, isUnavailable } from '../lib/commands';
import { SIDECAR_STATE, TEAM_SNAPSHOT } from '../lib/ipc';
import type {
  CcoTeamListResult,
  CcoTeamView,
  GildeErrorPayload,
  RepoIdentity,
  TeamSnapshotEvent,
} from '../lib/ipc';

/** How often the list is re-read while the Team view is shown. */
export const TEAM_LIST_POLL_MS = 5_000;
/** A path's repository identity is asked again after this long. */
export const IDENTITY_STALE_MS = 30_000;
/** …and sooner when git could not answer in time (`unknown`). */
export const IDENTITY_RETRY_MS = 5_000;
/** Paths per `repo_identity` call (Rust refuses more). */
export const IDENTITY_CHUNK = 256;

/** The `(generation, rev)` of what was last applied for a team. */
export interface TeamRevision {
  generation: number;
  rev: number;
}

export interface TeamPreviewState {
  /** The last `cco_team_list` answer; null before the first one. */
  list: CcoTeamListResult | null;
  views: Map<string, CcoTeamView>;
  /** What was last applied per team — a view or a withdrawal. */
  revisions: Map<string, TeamRevision>;
  selectedTeamId: string | null;
  selectedMember: string | null;
  /** The Team view was opened at least once: the sidecar watches on our behalf. */
  watching: boolean;
  loading: boolean;
  error: GildeErrorPayload | null;
  /**
   * Repository identity per path (team workspaces, member worktrees, project
   * folders) — what the sidebar groups and scopes by. A path not in here yet is
   * grouped by itself (provisionally) until its answer arrives.
   */
  identities: Map<string, RepoIdentity>;
}

export interface TeamSlice {
  team: TeamPreviewState;
  /** Re-read the list (and any view it shows to be stale). Never throws. */
  refreshTeams: () => Promise<void>;
  /** Fold one `gilde://team/snapshot` (or a `cco_team_get` answer) in. */
  applyTeamSnapshot: (event: TeamSnapshotEvent) => void;
  selectTeam: (teamId: string | null) => void;
  selectTeamMember: (name: string | null) => void;
  /** The Team view is shown: read now, then poll. Returns the stop function. */
  startTeamWatch: () => () => void;
  /**
   * Ask Rust for the repository identity of the paths not known yet (or known
   * for longer than IDENTITY_STALE_MS). Never throws; a backend without the
   * command leaves the provisional grouping in place.
   */
  resolveRepoIdentities: (paths: readonly string[]) => Promise<void>;
}

export function initialTeamState(): TeamPreviewState {
  return {
    list: null,
    views: new Map(),
    revisions: new Map(),
    selectedTeamId: null,
    selectedMember: null,
    watching: false,
    loading: false,
    error: null,
    identities: new Map(),
  };
}

/** Two identities that would render the same. */
export function sameIdentity(a: RepoIdentity | undefined, b: RepoIdentity): boolean {
  return a !== undefined && JSON.stringify(a) === JSON.stringify(b);
}

/** Fold `repo_identity` answers in; the same map when nothing changed. Pure. */
export function mergeIdentities(
  held: Map<string, RepoIdentity>,
  answers: readonly RepoIdentity[],
): Map<string, RepoIdentity> {
  let next: Map<string, RepoIdentity> | null = null;
  for (const answer of answers) {
    if (!answer || typeof answer.path !== 'string') continue;
    if (sameIdentity((next ?? held).get(answer.path), answer)) continue;
    next ??= new Map(held);
    next.set(answer.path, answer);
  }
  return next ?? held;
}

/** Is `next` newer than `held`? A missing `held` is always older. */
export function isNewerRevision(next: TeamRevision, held: TeamRevision | undefined): boolean {
  if (held === undefined) return true;
  if (next.generation !== held.generation) return next.generation > held.generation;
  return next.rev > held.rev;
}

/**
 * The team to show when none is chosen (or the chosen one left): the team of a
 * Gilde pane first, then a live one, then the newest.
 */
export function defaultTeamId(views: Map<string, CcoTeamView>): string | null {
  const all = [...views.values()];
  if (all.length === 0) return null;
  all.sort(
    (a, b) =>
      Number(b.paneSessionPid !== undefined) - Number(a.paneSessionPid !== undefined) ||
      Number(b.live) - Number(a.live) ||
      b.state.createdAt - a.state.createdAt,
  );
  return all[0]?.teamId ?? null;
}

/** Apply one snapshot to a state, by rule 1. Pure, so the tests can drive it. */
export function reduceTeamSnapshot(
  state: TeamPreviewState,
  event: TeamSnapshotEvent,
): TeamPreviewState {
  const next: TeamRevision = { generation: event.generation, rev: event.rev };
  if (!isNewerRevision(next, state.revisions.get(event.teamId))) return state;
  if (event.view !== null && event.view.teamId !== event.teamId) return state;
  const views = new Map(state.views);
  const revisions = new Map(state.revisions);
  revisions.set(event.teamId, next);
  if (event.view === null) views.delete(event.teamId);
  else views.set(event.teamId, event.view);
  let { selectedTeamId, selectedMember } = state;
  if (selectedTeamId === null || !views.has(selectedTeamId)) {
    selectedTeamId = defaultTeamId(views);
    selectedMember = null;
  }
  if (
    selectedMember !== null &&
    !(views.get(selectedTeamId ?? '')?.state.members ?? []).some((m) => m.name === selectedMember)
  ) {
    selectedMember = null;
  }
  return { ...state, views, revisions, selectedTeamId, selectedMember };
}

/** Drop every held view (and its revision) whose team is not in `listed`. Pure. */
export function pruneUnlisted(state: TeamPreviewState, listed: Set<string>): TeamPreviewState {
  const gone = [...state.views.keys()].filter((teamId) => !listed.has(teamId));
  if (gone.length === 0) return state;
  const views = new Map(state.views);
  const revisions = new Map(state.revisions);
  for (const teamId of gone) {
    views.delete(teamId);
    revisions.delete(teamId);
  }
  const keep = state.selectedTeamId !== null && views.has(state.selectedTeamId);
  return {
    ...state,
    views,
    revisions,
    selectedTeamId: keep ? state.selectedTeamId : defaultTeamId(views),
    selectedMember: keep ? state.selectedMember : null,
  };
}

export const createTeamSlice: StateCreator<
  TeamSlice,
  [['zustand/subscribeWithSelector', never]],
  [],
  TeamSlice
> = (set, get) => {
  const patch = (update: Partial<TeamPreviewState>) => set({ team: { ...get().team, ...update } });

  let inFlight: Promise<void> | null = null;
  /** When each path's identity was last asked (answered or not), and what is being asked now. */
  const identityAskedAt = new Map<string, number>();
  const identityAsking = new Set<string>();

  const refresh = async (): Promise<void> => {
    patch({ watching: true, loading: get().team.list === null });
    let list: CcoTeamListResult;
    try {
      list = await cmd.ccoTeamList();
    } catch (e) {
      // A backend without the command (an older build) reads as "no sidecar".
      if (isUnavailable(e)) {
        patch({ loading: false, error: null, list: emptyList() });
      } else {
        patch({ loading: false, error: asGildeError(e) });
      }
      return;
    }
    patch({ list, loading: false, error: null });
    if (!list.available) return;

    // Rule 2: a summary ahead of the held view is re-read; a held view whose team
    // left the list is dropped — with its revision, so the team's next view (whatever
    // its rev) is taken when it comes back.
    const pruned = pruneUnlisted(get().team, new Set(list.teams.map((t) => t.teamId)));
    if (pruned !== get().team) set({ team: pruned });
    for (const summary of list.teams) {
      const held = get().team.revisions.get(summary.teamId);
      const stale =
        !get().team.views.has(summary.teamId) ||
        isNewerRevision({ generation: list.generation, rev: summary.rev }, held);
      if (!stale) continue;
      try {
        const view = await cmd.ccoTeamGet(summary.teamId);
        if (view === null) continue;
        get().applyTeamSnapshot({
          generation: view.generation,
          rev: view.rev,
          teamId: view.teamId,
          view,
        });
      } catch (e) {
        if (!isUnavailable(e)) patch({ error: asGildeError(e) });
      }
    }
  };

  // ── event fan-out (one Tauri listener, in `useEventBridge`) ───────────────
  // Deferred by a microtask: this slice runs while the store is being created,
  // and `useEventBridge` imports the store back (same as dispatchSlice).
  queueMicrotask(() => {
    onGildeEvent(TEAM_SNAPSHOT, (event) => get().applyTeamSnapshot(event));
    onGildeEvent(SIDECAR_STATE, (event) => {
      // A sidecar that came back has lost what it watched: say it again.
      if (event.state === 'ready' && get().team.watching) void get().refreshTeams();
    });
  });

  return {
    team: initialTeamState(),

    refreshTeams: () => {
      if (inFlight) return inFlight;
      inFlight = refresh().finally(() => {
        inFlight = null;
      });
      return inFlight;
    },

    applyTeamSnapshot: (event) => {
      const current = get().team;
      const next = reduceTeamSnapshot(current, event);
      if (next !== current) set({ team: next });
    },

    selectTeam: (teamId) => {
      const { views } = get().team;
      patch({
        selectedTeamId: teamId !== null && views.has(teamId) ? teamId : defaultTeamId(views),
        selectedMember: null,
      });
    },

    selectTeamMember: (name) => {
      const team = get().team;
      const view = team.selectedTeamId === null ? undefined : team.views.get(team.selectedTeamId);
      const known = name !== null && (view?.state.members ?? []).some((m) => m.name === name);
      patch({ selectedMember: known ? name : null });
    },

    resolveRepoIdentities: async (paths) => {
      const now = Date.now();
      const want: string[] = [];
      for (const path of new Set(paths)) {
        if (typeof path !== 'string' || path === '' || identityAsking.has(path)) continue;
        const known = get().team.identities.get(path);
        const wait = known?.kind === 'unknown' ? IDENTITY_RETRY_MS : IDENTITY_STALE_MS;
        const at = identityAskedAt.get(path);
        if (at !== undefined && now - at < wait) continue;
        want.push(path);
      }
      for (let i = 0; i < want.length; i += IDENTITY_CHUNK) {
        const chunk = want.slice(i, i + IDENTITY_CHUNK);
        for (const path of chunk) {
          identityAsking.add(path);
          identityAskedAt.set(path, now);
        }
        try {
          const answers = await cmd.repoIdentity(chunk);
          const asked = new Set(chunk);
          const merged = mergeIdentities(
            get().team.identities,
            (Array.isArray(answers) ? answers : []).filter((a) => asked.has(a?.path)),
          );
          if (merged !== get().team.identities) patch({ identities: merged });
        } catch (e) {
          // An older backend (or the test bundle) has no such command: the
          // provisional per-path grouping stays, and nothing is shown as an error.
          if (!isUnavailable(e)) console.warn('[gilde] repo identity failed', e);
        } finally {
          for (const path of chunk) identityAsking.delete(path);
        }
      }
    },

    startTeamWatch: () => {
      void get().refreshTeams();
      const timer = setInterval(() => void get().refreshTeams(), TEAM_LIST_POLL_MS);
      return () => clearInterval(timer);
    },
  };
};

function emptyList(): CcoTeamListResult {
  return {
    available: false,
    supported: true,
    dataRoot: '',
    teamMode: 'unknown',
    hosts: [],
    teams: [],
    generation: 0,
  };
}
