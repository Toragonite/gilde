/**
 * Screen slots — how the shell finds the screens other wave-2 tasks own,
 * without importing a module that may not exist yet and without anyone
 * editing the shell later.
 *
 * Each area exposes named components from its `index.ts(x)`; the shell reads
 * them through Vite's `import.meta.glob` (eager, so they are in the bundle the
 * moment the file exists). A slot whose module is not there yet is `null`, and
 * the shell shows one neutral line in its place. Only `index.{ts,tsx}` is
 * globbed, so old files that share an area directory are never pulled in.
 *
 *   src/session/index    SessionScreen({ uid, sub }) · StopSessionSheet         T15
 *   src/changes/index    CommitSheet · MergeSheet · DiscardWorktreeSheet        T15
 *   src/split/index      SplitScreen()                                         T16
 *   src/team/index       TeamScreen({ teamKey, mode }) · ClosePaneSheet        T16
 *   src/floor/index      FloorScreen()                                 T17 (A23.2)
 *   src/history/index    HistoryScreen()                                       T18
 *   src/accounts/index   AccountsScreen({ section }) · CleanUpSheet            T18
 *   src/settings/index   SettingsScreen()                                      T18
 *   src/newsession/index NewSessionSheet()                                     T18
 *   src/quit/index       QuitSheet()                                           T18
 *   src/dialogs/index    Dialogs()  (non-sheet dialogs, e.g. Recovered)         T18
 *
 * Sheets take no props: they read their context from the store and close with
 * `closeSheet()` (uiSlice).
 */

import type { ComponentType } from 'react';

import type { SessionUid } from '../lib/gen/SessionUid';
import type { TeamKey } from '../lib/gen/TeamKey';
import type { TeamViewMode } from '../lib/gen/TeamViewMode';
import type { AccountsSection, SessionSub } from '../store/uiSlice';

type Module = Record<string, unknown>;

const MODULES = {
  session: import.meta.glob<Module>('../session/index.{ts,tsx}', { eager: true }),
  changes: import.meta.glob<Module>('../changes/index.{ts,tsx}', { eager: true }),
  split: import.meta.glob<Module>('../split/index.{ts,tsx}', { eager: true }),
  team: import.meta.glob<Module>('../team/index.{ts,tsx}', { eager: true }),
  floor: import.meta.glob<Module>('../floor/index.{ts,tsx}', { eager: true }),
  history: import.meta.glob<Module>('../history/index.{ts,tsx}', { eager: true }),
  accounts: import.meta.glob<Module>('../accounts/index.{ts,tsx}', { eager: true }),
  settings: import.meta.glob<Module>('../settings/index.{ts,tsx}', { eager: true }),
  newsession: import.meta.glob<Module>('../newsession/index.{ts,tsx}', { eager: true }),
  quit: import.meta.glob<Module>('../quit/index.{ts,tsx}', { eager: true }),
  dialogs: import.meta.glob<Module>('../dialogs/index.{ts,tsx}', { eager: true }),
} as const;

type Area = keyof typeof MODULES;

/** A React component value: a function, or a memo/forwardRef object. */
function isComponent(value: unknown): boolean {
  return typeof value === 'function' || (typeof value === 'object' && value !== null && '$$typeof' in value);
}

function pick<P>(area: Area, name: string): ComponentType<P> | null {
  for (const mod of Object.values(MODULES[area])) {
    const value = mod[name];
    if (isComponent(value)) return value as ComponentType<P>;
  }
  return null;
}

export interface SessionScreenProps {
  uid: SessionUid;
  sub: SessionSub;
}

export interface TeamScreenProps {
  teamKey: TeamKey;
  mode: TeamViewMode;
}

export interface AccountsScreenProps {
  section: AccountsSection;
}

export interface Slots {
  SessionScreen: ComponentType<SessionScreenProps> | null;
  SplitScreen: ComponentType | null;
  TeamScreen: ComponentType<TeamScreenProps> | null;
  FloorScreen: ComponentType | null;
  HistoryScreen: ComponentType | null;
  AccountsScreen: ComponentType<AccountsScreenProps> | null;
  SettingsScreen: ComponentType | null;
  NewSessionSheet: ComponentType | null;
  StopSessionSheet: ComponentType | null;
  CleanUpSheet: ComponentType | null;
  CommitSheet: ComponentType | null;
  MergeSheet: ComponentType | null;
  DiscardWorktreeSheet: ComponentType | null;
  ClosePaneSheet: ComponentType | null;
  QuitSheet: ComponentType | null;
  Dialogs: ComponentType | null;
}

function discover(): Slots {
  return {
    SessionScreen: pick<SessionScreenProps>('session', 'SessionScreen'),
    SplitScreen: pick('split', 'SplitScreen'),
    TeamScreen: pick<TeamScreenProps>('team', 'TeamScreen'),
    FloorScreen: pick('floor', 'FloorScreen'),
    HistoryScreen: pick('history', 'HistoryScreen'),
    AccountsScreen: pick<AccountsScreenProps>('accounts', 'AccountsScreen'),
    SettingsScreen: pick('settings', 'SettingsScreen'),
    NewSessionSheet: pick('newsession', 'NewSessionSheet'),
    StopSessionSheet: pick('session', 'StopSessionSheet'),
    CleanUpSheet: pick('accounts', 'CleanUpSheet'),
    CommitSheet: pick('changes', 'CommitSheet'),
    MergeSheet: pick('changes', 'MergeSheet'),
    DiscardWorktreeSheet: pick('changes', 'DiscardWorktreeSheet'),
    ClosePaneSheet: pick('team', 'ClosePaneSheet'),
    QuitSheet: pick('quit', 'QuitSheet'),
    Dialogs: pick('dialogs', 'Dialogs'),
  };
}

let current: Slots = discover();

/** The screens available in this build. */
export function slots(): Slots {
  return current;
}

/** Tests only: replace some slots; `null` restores what the build discovered. */
export function setSlotsForTests(partial: Partial<Slots> | null): void {
  current = partial === null ? discover() : { ...current, ...partial };
}
