/**
 * What the Team screen does — mode switches and via-lead actions.
 *
 * Via-lead actions (contract §7.1.3, §7.2.3; ux-ia §3.10) put the localized
 * text of `team.prefill.*` into the lead's composer and move the cursor there;
 * **nothing is sent** until the user presses `⏎` in that composer. They are
 * offered only while the lead runs in Gilde (`leadTarget`). A draft the user
 * already had in the lead's composer is kept: the prefill goes on a new line.
 *
 * Team modes (Panes · Board · Floor, `⌥⌘1/2/3`) are sub-views of the Team
 * screen like the Session screen's sub-views (ux-ia K7, K8): `setTeamMode`
 * (uiSlice, A30.4) replaces the main view instead of pushing history and
 * saves the mode in the team's layout (`teamLayout:<TeamKey>`).
 */

import { translate } from '../i18n';
import type { MessageKey, MessageParams } from '../i18n';
import type { TeamKey } from '../lib/gen/TeamKey';
import type { TeamViewMode } from '../lib/gen/TeamViewMode';
import { requestComposerFocus } from '../split/focus';
import { appStore } from '../store/useAppStore';
import { leadMember, leadTarget, panesAvailable } from './model';

/** Show `mode` for the team (replacing the view, like a session sub-view) and remember it (A30.4). */
export function setTeamMode(teamKey: TeamKey, mode: TeamViewMode): void {
  appStore.getState().setTeamMode(teamKey, mode);
}

/** The localized prefill line in the window's language. */
export function prefillText(key: MessageKey, params?: MessageParams): string {
  return translate(appStore.getState().ui.locale, key, params);
}

/**
 * Put `text` into the lead's composer and the cursor after it. Returns false
 * (and changes nothing) when the lead is not running in Gilde.
 */
export function prefillLead(teamKey: TeamKey, text: string): boolean {
  const s = appStore.getState();
  const team = s.world.teams.get(teamKey);
  if (!team) return false;
  const lead = leadTarget(s.world, team);
  const member = leadMember(s.world, team);
  if (!lead || !member) return false;
  const draft = s.ui.drafts[lead.uid] ?? '';
  const next = draft.trim() === '' ? text : `${draft.replace(/\s+$/, '')}\n${text}`;
  s.setDraft(lead.uid, next);
  const view = s.ui.view;
  if (view.screen === 'team' && view.teamKey === teamKey && panesAvailable(team)) {
    s.setTeamLayout(teamKey, { focused: member.key, zoomed: null });
    setTeamMode(teamKey, 'panes');
  } else {
    s.openSession(lead.uid);
  }
  requestComposerFocus(lead.uid);
  return true;
}

/** "Tell {member}: …" — the typed text follows the sentence; empty leaves the cursor after it. */
export function tellLine(member: string, text = ''): string {
  const head = prefillText('team.prefill.tell', { member });
  return text.trim() === '' ? `${head} ` : `${head} ${text.trim()}`;
}
