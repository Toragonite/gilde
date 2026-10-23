/**
 * The showcase scenes and how the URL picks one.
 *
 *   ?scene=<name>                 one of SCENE_NAMES (default home)
 *   &lang=en|ko                   the app language (default en)
 *   &theme=system|light|dark      (default system)
 *   &t=<seconds>                  start the patch script at this moment (default 0)
 *   &live=0|1                     deliver later script steps on timers (default 1);
 *                                 gates pass live=0 and drive `__gildeShowcase.advanceTo`
 *   &agents=<n>&teams=<list>      override the world size and teams
 *                                 (teams: native,nativeInferred,orchestrator or none)
 *
 * The scenes cover the screens the visual-snapshot and accessibility gates
 * name (contract §14.3: Home (Focus) — also with its notice and warning line
 * for the density gate (A26.6) —, Session with a card, Watching, Split 4,
 * Team Panes/Board/Floor, New session, History, Accounts & health, Quit sheet)
 * and the Floor geometry sizes (1, 8, 20, 31 agents).
 * A value this build does not know falls back to the default, so a typo still
 * shows a scene.
 */

import type { JsonValue } from '../lib/gen/serde_json/JsonValue';
import type { Locale } from '../lib/gen/Locale';
import type { TeamViewMode } from '../lib/gen/TeamViewMode';
import type { Theme } from '../lib/gen/Theme';
import type { SheetId } from '../store/uiSlice';
import { sessionUid } from './ids';
import type { TeamKind, WorldOptions, WorldRoles } from './world';

export const SCENE_NAMES = [
  'home', 'home-notice', 'empty', 'session', 'changes', 'terminal', 'watching', 'split',
  'team', 'team-panes', 'board', 'team-floor', 'floor', 'floor-1', 'floor-8', 'floor-20', 'floor-31',
  'newsession', 'history', 'accounts', 'health', 'settings', 'quit',
  'history-read', 'recovered',
] as const;

export type SceneName = (typeof SCENE_NAMES)[number];

/** Where the scene opens, given the world's roles: the persisted `lastView` (uiSlice `parseView`). */
export type SceneView = (roles: WorldRoles) => JsonValue;

export interface SceneSpec {
  world: WorldOptions;
  view: SceneView;
  /** Put the world's Split set in `splitSet` (the Split scene). */
  split: number;
  /** The layout of the scene's team (`teamLayout:<key>`). */
  teamMode: TeamViewMode | null;
  /** A sheet the scene opens once the app has hydrated. */
  sheet: SheetId | null;
  /** The quit sheet, through the quit-requested path. */
  quit: boolean;
  /** Launch after a quit that stopped sessions: Home shows its one notice (A26.6). */
  notice: boolean;
}

const BASE: WorldOptions = { agents: 12, teams: ['native'], pending: true, leftovers: 2, history: 40 };
const FLOOR_TEAMS: Readonly<Record<number, readonly TeamKind[]>> = {
  1: [],
  8: ['native'],
  20: ['nativeInferred', 'orchestrator'],
  31: ['native', 'nativeInferred', 'orchestrator'],
};

const HOME: JsonValue = { screen: 'home' };
const home: SceneView = () => HOME;
const sessionView = (role: keyof Pick<WorldRoles, 'approval' | 'watching' | 'terminal'>, sub: string): SceneView => (roles) => {
  const uid = roles[role];
  return uid ? { screen: 'session', uid, sub } : HOME;
};
const teamView = (mode: TeamViewMode): SceneView => (roles) => (roles.team ? { screen: 'team', teamKey: roles.team, mode } : HOME);

function scene(partial: Partial<SceneSpec> & Pick<SceneSpec, 'view'>): SceneSpec {
  return { world: BASE, split: 0, teamMode: null, sheet: null, quit: false, notice: false, ...partial };
}

function floorScene(agents: number): SceneSpec {
  return scene({ world: { ...BASE, agents, teams: FLOOR_TEAMS[agents] ?? ['native'], leftovers: 0 }, view: () => ({ screen: 'floor' }) });
}

export const SCENES: Readonly<Record<SceneName, SceneSpec>> = {
  home: scene({ view: home }),
  'home-notice': scene({ world: { ...BASE, quotaHot: true }, view: home, notice: true }),
  empty: scene({ world: { agents: 0, teams: [], pending: false, leftovers: 0, history: 0 }, view: home }),
  session: scene({ view: sessionView('approval', 'conversation') }),
  changes: scene({ view: sessionView('approval', 'changes') }),
  terminal: scene({ view: sessionView('terminal', 'terminal') }),
  watching: scene({ view: sessionView('watching', 'conversation') }),
  split: scene({ view: () => ({ screen: 'split' }), split: 4 }),
  team: scene({ view: teamView('panes'), teamMode: 'panes' }),
  // A30.1: a small hosted team whose panes include a starting, a failed and a running unknown pane.
  'team-panes': scene({ world: { ...BASE, agents: 4, teams: ['native'], pending: false, paneTiles: true }, view: teamView('panes'), teamMode: 'panes' }),
  board: scene({ world: { ...BASE, teams: ['native', 'nativeInferred'] }, view: teamView('board'), teamMode: 'board' }),
  'team-floor': scene({ view: teamView('floor'), teamMode: 'floor' }),
  floor: floorScene(20),
  'floor-1': floorScene(1),
  'floor-8': floorScene(8),
  'floor-20': floorScene(20),
  'floor-31': floorScene(31),
  newsession: scene({ view: home, sheet: 'newSession' }),
  history: scene({ view: () => ({ screen: 'history' }) }),
  accounts: scene({ view: () => ({ screen: 'accounts', section: 'accounts' }) }),
  health: scene({ view: () => ({ screen: 'accounts', section: 'health' }) }),
  settings: scene({ view: () => ({ screen: 'settings' }) }),
  quit: scene({ view: home, quit: true }),
  // T18b (A29.2): History's read-only view of a session World does not hold.
  'history-read': scene({ view: () => ({ screen: 'session', uid: sessionUid(5000), sub: 'conversation' }) }),
  // T18b (A29.3): the boot sweep's undo window — the Recovered dialog over Home.
  recovered: scene({ world: { ...BASE, recovery: true }, view: home }),
};

export interface ShowcaseOptions {
  scene: SceneName;
  locale: Locale;
  theme: Theme;
  /** Seconds into the patch script. */
  t: number;
  live: boolean;
  /** The scene's world, with the URL's overrides applied. */
  world: WorldOptions;
}

const TEAM_KINDS: readonly TeamKind[] = ['native', 'nativeInferred', 'orchestrator'];

/** The scene, language, theme, time and world the query string asks for. Never throws. */
export function parseShowcaseQuery(search: string): ShowcaseOptions {
  let params: URLSearchParams;
  try {
    params = new URLSearchParams(search);
  } catch {
    params = new URLSearchParams();
  }
  const sceneParam = params.get('scene') ?? '';
  const scene: SceneName = (SCENE_NAMES as readonly string[]).includes(sceneParam) ? (sceneParam as SceneName) : 'home';
  const locale: Locale = params.get('lang') === 'ko' ? 'ko' : 'en';
  const themeParam = params.get('theme');
  const theme: Theme = themeParam === 'light' || themeParam === 'dark' ? themeParam : 'system';
  const tRaw = Number(params.get('t') ?? '0');
  const t = Number.isFinite(tRaw) && tRaw >= 0 && tRaw <= 3600 ? tRaw : 0;
  const live = params.get('live') !== '0';
  const world: WorldOptions = { ...SCENES[scene].world };
  const agents = Number(params.get('agents'));
  if (params.has('agents') && Number.isInteger(agents) && agents >= 0 && agents <= 200) world.agents = agents;
  const teams = params.get('teams');
  if (teams !== null) {
    world.teams = teams === 'none' ? [] : teams.split(',').filter((k): k is TeamKind => (TEAM_KINDS as readonly string[]).includes(k));
  }
  return { scene, locale, theme, t, live, world };
}
