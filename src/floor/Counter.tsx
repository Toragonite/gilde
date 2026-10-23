/**
 * The counter (pixel-office §5.2, I6): the human's station, the "나를 기다림"
 * queue drawn as a place, in exactly the queue's order. Its counts are the
 * queue's own `needYou` / `also` (one selector with the sidebar, F-8); the
 * Floor never re-ranks. A ticket opens its session with the card focused
 * (`⌘`-click adds it to the Split); approvals happen only on the card [M7].
 */

import { forwardRef } from 'react';

import { StateGlyph } from '../components/StateGlyph';
import type { GlyphKind } from '../components/StateGlyph';
import { useI18n } from '../i18n/useI18n';
import type { AttentionItem } from '../lib/gen/AttentionItem';
import type { FlVm } from './model';
import { cardAtCounter, counterBottleneck } from './model';
import { floorDuration, ownerWord, stateWord } from './words';

export interface CounterProps {
  vm: FlVm;
  onOpen: (item: AttentionItem, split: boolean) => void;
  onHover: (seatId: string | null) => void;
  onSelect: (seatId: string | null) => void;
}

export const Counter = forwardRef<HTMLDivElement, CounterProps>(function Counter({ vm, onOpen, onHover, onSelect }, ref) {
  const { t, locale } = useI18n();
  const cb = counterBottleneck(vm);
  const n = vm.needYou;
  const m = vm.also;
  return (
    <div className="fl-counter" role="region" aria-label={t('floor.counter')} ref={ref}>
      <span className="fl-cnt-l">
        <StateGlyph state={n ? 'needsApproval' : 'ready'} />
        <span>{t('floor.counterLine', { waiting: n })}</span>
        {m > 0 && <span className="fl-muted">{t('floor.counterMore', { more: m })}</span>}
        {cb && <span className="fl-warn">{t('floor.counterBottleneck')}</span>}
      </span>
      <div className="fl-tickets">
        {vm.queue.length === 0 && <span className="fl-muted">{t('floor.counterEmpty')}</span>}
        {vm.queue.map((item) => {
          if (item.subject.kind === 'leftovers') {
            const label = t('floor.ticket.leftovers', { procs: item.subject.count });
            return (
              <button key={item.key} type="button" className="fl-ticket" data-fl-ticket="" onClick={(e) => onOpen(item, e.metaKey)}>
                <StateGlyph state="leftover" />
                <span className="fl-ell">{label}</span>
              </button>
            );
          }
          const uid = item.subject.uid;
          const seatId = vm.seatOfUid.get(uid) ?? null;
          const seat = seatId ? vm.seats.get(seatId) : undefined;
          const name = seat?.name ?? t('floor.untitled', { vendor: '' }).trim();
          const kind: GlyphKind = seat ? seat.state : reasonGlyph(item);
          const word = seat ? stateWord(locale, seat) : '';
          const age = floorDuration(locale, Math.max(0, vm.now - item.since));
          const card = cardAtCounter(vm, uid);
          const owner = seat && item.watched ? ownerWord(locale, seat) : null;
          const watched = owner && kind === 'needsApproval' ? t('floor.ticket.watchingApproval', { owner }) : null;
          const colour = card?.owner ? vm.seats.get(card.owner)?.colour : seat?.colour;
          // the name is the button's own text (visible text first in the accessible name); the state word and
          // the "{owner}에서 승인 대기" line are read but not shown (the glyph shows the state)
          const hidden = [word, watched].filter(Boolean).join(', ');
          return (
            <button
              key={item.key}
              type="button"
              className={'fl-ticket' + (item.band <= 2 ? ' fl-hot' : '')}
              data-fl-ticket=""
              data-flseat={seatId ?? undefined}
              title={watched ?? undefined}
              onClick={(e) => onOpen(item, e.metaKey)}
              onMouseEnter={() => onHover(seatId)}
              onMouseLeave={() => onHover(null)}
              onFocus={() => {
                onHover(seatId);
                onSelect(seatId);
              }}
              onBlur={() => onHover(null)}
            >
              <StateGlyph state={kind} likely={seat?.likely} />
              {hidden && (
                <span className="fl-sr-only">
                  <span>{hidden + ', '}</span>
                </span>
              )}
              {seat?.colour && <i className="fl-sw" data-c={seat.colour} aria-hidden="true" />}
              <span className="fl-ell">{name}</span>
              {card && (
                <span className="fl-tk-card">
                  <i className="fl-stripe" data-c={colour ?? undefined} aria-hidden="true" />
                  <span className="fl-ell">{card.title}</span>
                </span>
              )}
              <span className="fl-age">{age}</span>
            </button>
          );
        })}
      </div>
    </div>
  );
});

function reasonGlyph(item: AttentionItem): GlyphKind {
  switch (item.reason.kind) {
    case 'needsApproval':
      return 'needsApproval';
    case 'needsAnswer':
      return 'needsAnswer';
    case 'error':
      return 'error';
    case 'crashed':
      return 'crashed';
    case 'noProgress':
      return 'noProgress';
    case 'newReply':
      return 'newReply';
    default:
      return 'leftover';
  }
}
