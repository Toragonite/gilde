/**
 * The name card (pixel-office §6.4): hover (150 ms) or `Space` on the
 * selected character. DOM, selectable text, reachable by screen readers (the
 * selected twin item points at it with `aria-describedby` while pinned).
 * Numbers use tabular figures. No content beyond the store's facts: no
 * message text, no tool output.
 */

import { useI18n } from '../i18n/useI18n';
import type { MemberKey } from '../lib/gen/MemberKey';
import type { FlVm } from './model';
import { bottleneckOf, currentItem, itemsOf, mailTo, teamOf } from './model';
import { elapsedWord } from './twinText';
import { floorDuration, ownershipWord, stateWord } from './words';

export const NAME_CARD_ID = 'fl-name-card';

export function NameCard({ vm, seatId, rect, pinned }: { vm: FlVm; seatId: string; rect: { x: number; y: number; w: number; h: number }; pinned: boolean }) {
  const { t, locale } = useI18n();
  const seat = vm.seats.get(seatId);
  if (!seat) return null;
  const team = teamOf(vm, seat.teamKey);
  const project = vm.projects.find((p) => p.id === seat.projectId)?.name ?? t('floor.noProject');
  const role = seat.role ? t(seat.role === 'lead' ? 'floor.role.lead' : 'floor.role.member') : null;
  const cur = team ? currentItem(team, seat.id as MemberKey) : null;
  const queued = team ? itemsOf(team, seat.id as MemberKey, 'assigned').length : 0;
  const flight = team ? mailTo(team, seat.id as MemberKey) : 0;
  const bn = team ? bottleneckOf(team, seat.id as MemberKey) : null;
  const inState = floorDuration(locale, Math.max(0, vm.now - seat.since));
  const blocks = cur && team ? team.items.filter((i) => i.blockedBy === cur.key && i.station !== 'done').length : 0;
  let why: string | null = null;
  if (bn) {
    why = bn.why === 'blocks' ? t('floor.bnBlocks', { who: seat.name, size: bn.n }) : bn.why === 'tray' ? t('floor.bnTray', { who: seat.name, size: bn.n }) : t('floor.bnSlow', { who: seat.name });
  }
  const style = { left: Math.round(rect.x + rect.w + 4), top: Math.max(0, Math.round(rect.y)) };
  return (
    <div id={NAME_CARD_ID} className={'fl-card' + (pinned ? ' fl-pinned' : '')} style={style} data-fl-card="">
      <div className="fl-card-r">
        <b className="fl-ell">{seat.name}</b>
        <span className="fl-badge">{seat.vendor}</span>
      </div>
      <div className="fl-card-r">
        <span>{t('floor.card.since', { state: stateWord(locale, seat), time: inState })}</span>
        <span className="fl-muted">{ownershipWord(locale, seat)}</span>
      </div>
      <div className="fl-muted">{[project, team?.name, role].filter(Boolean).join(' · ')}</div>
      {cur && <div>{t('floor.card.task', { task: cur.title, time: elapsedWord(locale, vm, cur) ?? '—' })}</div>}
      {cur?.inferred && <div className="fl-muted">{t('floor.card.inferred')}</div>}
      {cur?.noReport && <div className="fl-warn">{t('floor.noReport')}</div>}
      {blocks > 0 && <div>{t('floor.waitsOnThis', { size: blocks })}</div>}
      {team && (queued > 0 || flight > 0) && <div className="fl-num">{t('floor.card.queue', { queued, flight })}</div>}
      {why && <div className="fl-warn">{why}</div>}
      <div className="fl-muted">{t('floor.activityNotShown')}</div>
      <div className="fl-muted fl-num">{seat.quotaLeft !== null ? t('floor.card.quota', { left: Math.round(seat.quotaLeft) }) : t('floor.card.noQuota')}</div>
    </div>
  );
}
