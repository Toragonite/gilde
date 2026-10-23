/**
 * The Korean table: every area's `ko` merged. Korean is the default locale
 * (ux-ia §9.1) and the source of the key set (`MessageKey = keyof typeof ko`).
 */

import * as common from './areas/common';
import * as states from './areas/states';
import * as errors from './areas/errors';
import * as notices from './areas/notices';
import * as shell from './areas/shell';
import * as home from './areas/home';
import * as palette from './areas/palette';
import * as session from './areas/session';
import * as team from './areas/team';
import * as floor from './areas/floor';
import * as history from './areas/history';
import * as accounts from './areas/accounts';
import * as newsession from './areas/newsession';
import * as settings from './areas/settings';
import * as quit from './areas/quit';

/** The area modules, in table order (`i18n.test.ts` checks that no key is in two). */
export const AREAS = { common, states, errors, notices, shell, home, palette, session, team, floor, history, accounts, newsession, settings, quit } as const;

export const ko = {
  ...common.ko,
  ...states.ko,
  ...errors.ko,
  ...notices.ko,
  ...shell.ko,
  ...home.ko,
  ...palette.ko,
  ...session.ko,
  ...team.ko,
  ...floor.ko,
  ...history.ko,
  ...accounts.ko,
  ...newsession.ko,
  ...settings.ko,
  ...quit.ko,
};
