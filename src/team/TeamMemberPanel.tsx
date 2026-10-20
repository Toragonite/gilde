/**
 * TeamMemberPanel — one member, looked at closely (team contract v2 §8.2): its
 * activity stream (tool ticker and text, as the engine's feed recorded it), its
 * last turn's outcome (in words) and head, and the messages to and from it.
 * Mounted in the Team view's inspector (`TeamMemberInspector`).
 *
 * Read-only like the rest of the preview: selecting a member is a local choice
 * and sends nothing anywhere.
 */

import { useI18n } from '../i18n/useI18n';
import { formatTimeOfDay } from '../lib/format';
import type { CcoTeamView } from '../lib/ipc';
import { useAppStore } from '../store/useAppStore';
import { CAP, activityOf, flat, messagesOf, outcomeLabel } from './model';
import { isEngineMessage, noticeDisplay, senderDisplay } from './notices';

/** Activity lines shown (the sidecar keeps 30 per member). */
const ACTIVITY_SHOWN = 30;
/** Messages of this member shown. */
const MEMBER_MESSAGES_SHOWN = 10;

export function TeamMemberPanel({ view }: { view: CcoTeamView }) {
  const { t, locale } = useI18n();
  const selected = useAppStore((s) => s.team.selectedMember);
  const member = selected === null ? undefined : view.state.members.find((m) => m.name === selected);

  if (member === undefined) {
    return <div className="team-note s12 muted">{t('team.selectMember')}</div>;
  }

  const lines = activityOf(view, member.name).slice(-ACTIVITY_SHOWN);
  const outcome = member.lastTurn ? outcomeLabel(member.lastTurn.outcome) : null;
  const messages = messagesOf(view, member.name).slice(0, MEMBER_MESSAGES_SHOWN);

  return (
    <div className="team-member-panel col gap-4" aria-label={t('team.section.activity', { name: flat(member.name, CAP.name) })}>
      <div className="sec">{t('team.section.activity', { name: flat(member.name, CAP.name) })}</div>
      {member.lastTurn && outcome ? (
        <div className="s12 team-last-turn">
          <span className="muted" title={outcome.known ? undefined : t('team.code', { code: outcome.code })}>
            {t('team.lastTurn', { n: member.lastTurn.n, outcome: t(outcome.key) })}
          </span>
          {member.lastTurn.head ? <span> · {flat(member.lastTurn.head, CAP.activity)}</span> : null}
        </div>
      ) : null}
      {lines.length === 0 ? (
        <div className="team-note s12 muted">{t('team.noActivity')}</div>
      ) : (
        <ol className="team-activity mono s11">
          {lines.map((line, i) => (
            <li key={`${line.ts}-${i}`} className="row gap-6" data-kind={flat(line.k, 12)}>
              <span className="muted">{formatTimeOfDay(line.ts, locale)}</span>
              <span className="ell">
                {line.k === 'notice' ? noticeDisplay(line.text, t, CAP.activity) : flat(line.text, CAP.activity)}
              </span>
            </li>
          ))}
        </ol>
      )}
      {messages.length > 0 ? (
        <ol className="team-timeline">
          {messages.map((m) => (
            <li key={m.seq} className="team-message col gap-2">
              <div className="row gap-6 s12">
                <span className="mono muted">{formatTimeOfDay(m.ts, locale)}</span>
                <span className="strong">{senderDisplay(m.from, t)}</span>
                <span className="muted">→</span>
                <span>{flat(m.to, CAP.name)}</span>
              </div>
              <div className="s12 team-message-body">
                {isEngineMessage(m) ? noticeDisplay(m.body, t) : flat(m.body, CAP.message)}
                {m.cut ? <span className="muted"> ({t('team.messageCut')})</span> : null}
              </div>
            </li>
          ))}
        </ol>
      ) : null}
    </div>
  );
}
