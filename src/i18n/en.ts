/** The English table: every area's `en` merged, with exactly the keys of `ko`. */

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
import type { ko } from './ko';

export const en: Record<keyof typeof ko, string> = {
  ...common.en,
  ...states.en,
  ...errors.en,
  ...notices.en,
  ...shell.en,
  ...home.en,
  ...palette.en,
  ...session.en,
  ...team.en,
  ...floor.en,
  ...history.en,
  ...accounts.en,
  ...newsession.en,
  ...settings.en,
  ...quit.en,
};
