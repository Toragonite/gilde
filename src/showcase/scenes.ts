/**
 * The five showcase scenes — the landing page's screenshot slots
 * (`site/README.md` § Screenshots) — and how the URL picks one:
 *
 *   ?scene=team|workers|health|health-snoozed|grouping   (default team)
 *   &lang=en|ko                           (default en; the app's own language setting)
 *   &theme=system|light|dark              (default system; the app's own theme setting)
 *   &member=<name>|none                   (Team scene: whose details are open; default api)
 *
 * A value this build does not know falls back to the default, so a typo still
 * shows a scene rather than nothing.
 */

import type { Locale, ThemePreference } from '../lib/ipc';
import type { TeamGroupBy, TeamScope, ViewKey } from '../store/uiSlice';
import { MAIN_MEMBER } from './world';
import type { WorldOptions } from './world';

export type SceneName = 'team' | 'workers' | 'health' | 'health-snoozed' | 'grouping';

export const SCENE_NAMES: readonly SceneName[] = ['team', 'workers', 'health', 'health-snoozed', 'grouping'];

export interface SceneSpec {
  /** The view in front. */
  view: ViewKey;
  world: WorldOptions;
  groupBy: TeamGroupBy;
  scope: TeamScope;
  /** The health popover is open over the status bar. */
  popover: boolean;
  /**
   * The user closed the action alerts' toasts (which dismisses them, as in the
   * app): the alerts are snoozed for the cooldown.
   */
  snoozeAlerts: boolean;
  /** The member whose details are open when the URL names none. */
  member: string | null;
}

export const SCENES: Record<SceneName, SceneSpec> = {
  // The Team view of one live team, the selected member's details open.
  team: {
    view: 'team',
    world: { allTeams: false, leftover: false },
    groupBy: 'repository',
    scope: 'all',
    popover: false,
    snoozeAlerts: false,
    member: MAIN_MEMBER,
  },
  // The Worker accounts view: five accounts across the agent CLIs.
  workers: {
    view: 'workers',
    world: { allTeams: false, leftover: false },
    groupBy: 'repository',
    scope: 'all',
    popover: false,
    snoozeAlerts: false,
    member: null,
  },
  // The Team view with the health popover open: one leftover agent of an ended team.
  // Its sticky toast is hidden while the popover shows the same alert.
  health: {
    view: 'team',
    world: { allTeams: false, leftover: true },
    groupBy: 'repository',
    scope: 'all',
    popover: true,
    snoozeAlerts: false,
    member: null,
  },
  // The health scene after the user closed the alert's toast: the alert is
  // snoozed, the popover says so (never "all well") and still offers the clean-up.
  'health-snoozed': {
    view: 'team',
    world: { allTeams: false, leftover: true },
    groupBy: 'repository',
    scope: 'all',
    popover: true,
    snoozeAlerts: true,
    member: null,
  },
  // The Team sidebar grouped by repository: three repositories, four teams.
  grouping: {
    view: 'team',
    world: { allTeams: true, leftover: false },
    groupBy: 'repository',
    scope: 'all',
    popover: false,
    snoozeAlerts: false,
    member: null,
  },
};

export interface ShowcaseOptions {
  scene: SceneName;
  locale: Locale;
  theme: ThemePreference;
  /** The selected member; null = none. */
  member: string | null;
}

/** The scene, language, theme and member the query string asks for. Never throws. */
export function parseShowcaseQuery(search: string): ShowcaseOptions {
  let params: URLSearchParams;
  try {
    params = new URLSearchParams(search);
  } catch {
    params = new URLSearchParams();
  }
  const sceneParam = params.get('scene') ?? '';
  const scene: SceneName = (SCENE_NAMES as readonly string[]).includes(sceneParam) ? (sceneParam as SceneName) : 'team';
  const langParam = params.get('lang');
  const locale: Locale = langParam === 'ko' ? 'ko' : 'en';
  const themeParam = params.get('theme');
  const theme: ThemePreference = themeParam === 'light' || themeParam === 'dark' ? themeParam : 'system';
  const memberParam = params.get('member');
  const member =
    memberParam === null ? SCENES[scene].member : memberParam === '' || memberParam === 'none' ? null : memberParam;
  return { scene, locale, theme, member };
}
