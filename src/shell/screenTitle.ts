/**
 * The name of the current main view — the top bar's title and the `main`
 * landmark's accessible name ("Session api-refactor", "Team auth-split",
 * "Floor", ux-ia §10 Landmarks).
 */

import type { Locale } from '../lib/gen/Locale';
import type { MainView, ReadTitle } from '../store/uiSlice';
import type { WorldState } from '../store/worldSlice';
import { sessionOf } from '../store/worldSlice';
import type { T } from './words';
import { displayName } from './words';

/**
 * `readTitle` is a History read's title (A29.2): used only while the uid is
 * absent from the store — the store's entity wins as soon as it appears.
 */
export function screenTitle(t: T, view: MainView, world: WorldState, locale: Locale, readTitle: ReadTitle | null = null): string {
  switch (view.screen) {
    case 'home':
      return t('nav.home');
    case 'session': {
      const session = sessionOf(world, view.uid);
      const read = !session && readTitle && readTitle.uid === view.uid ? readTitle.title : null;
      return t('shell.screen.session', { name: session ? displayName(t, world, session, locale) : (read ?? t('common.unknown')) });
    }
    case 'split':
      return t('shell.screen.split');
    case 'team': {
      const team = world.teams.get(view.teamKey);
      return t('shell.screen.team', { name: team?.name ?? t('common.unknown') });
    }
    case 'floor':
      return t('shell.screen.floor');
    case 'history':
      return t('nav.history');
    case 'accounts':
      return t('nav.accountsHealth');
    case 'settings':
      return t('shell.screen.settings');
  }
}
