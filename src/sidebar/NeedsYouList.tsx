/**
 * The sidebar's Needs-you section (ux-ia §2.4, §2.5): every visible queue
 * item once, in the core's rank order, with the header "NEEDS YOU n · +m"
 * (n = bands 1–2, m = bands 3–6, straight from `queue.needYou`/`also`, never
 * recounted). Hidden on Home, which shows the same queue at full width.
 *
 * While the pointer or focus is on the list its order holds and a row handled
 * elsewhere stays as a "handled" ghost for 1.5 s (T13's `useHeldOrder`), so a
 * click never lands on a row that slid in. Keys: ↑↓ move, ⏎ opens, `L` Later,
 * `⌫` marks a New reply seen.
 */

import { useRef } from 'react';
import type { KeyboardEvent as ReactKeyboardEvent } from 'react';

import { StateGlyph } from '../components/StateGlyph';
import { useEngagement, useHeldOrder } from '../hooks/useHold';
import { isImeComposing } from '../hooks/useHotkeys';
import { useRovingTabIndex } from '../hooks/useRovingTabIndex';
import { useI18n } from '../i18n/useI18n';
import { clock } from '../lib/clock';
import { formatBytes } from '../lib/format';
import type { AttentionItem } from '../lib/gen/AttentionItem';
import { useAppStore } from '../store/useAppStore';
import { sessionOf, visibleQueueItems } from '../store/worldSlice';
import { dismissNewReply, laterItem, openItem } from '../shell/actions';
import { stepQueue } from '../shell/useShellKeys';
import { itemAge, useAgeTick } from '../shell/useAges';
import { itemGlyph, itemName, itemStateWord, ownerWord, waitAge } from '../shell/words';
import { sessionDragProps, useSplitAddKey } from './splitAdd';

export function NeedsYouList() {
  const { t, locale } = useI18n();
  useAgeTick();
  const world = useAppStore((s) => s.world);
  const openUid = useAppStore((s) => (s.ui.view.screen === 'session' ? s.ui.view.uid : null));
  const items = visibleQueueItems(world, clock.wall());
  const engagement = useEngagement();
  const name = (item: AttentionItem) => itemName(t, world, item, locale);
  const held = useHeldOrder(items, (i) => i.key, name, engagement.engaged);
  const focusable = held.rows.filter((r) => r.kind === 'item');
  const roving = useRovingTabIndex(focusable.length);
  const { needYou, also } = world.queue;
  const listRef = useRef<HTMLUListElement>(null);
  useSplitAddKey(listRef);

  const onKey = (event: ReactKeyboardEvent<HTMLElement>, item: AttentionItem) => {
    if (isImeComposing(event.nativeEvent) || event.metaKey || event.ctrlKey || event.altKey || event.repeat) return;
    if (event.code === 'KeyL' && !event.shiftKey) {
      event.preventDefault();
      held.markOwnAction(item.key);
      laterItem(item);
    } else if (event.code === 'Backspace' && item.reason.kind === 'newReply' && item.subject.kind === 'session') {
      event.preventDefault();
      held.markOwnAction(item.key);
      dismissNewReply(item.subject.uid);
    }
  };

  let index = -1;
  let separated = false;
  return (
    <section className="sb-needs" aria-labelledby="sb-needs-h">
      <div className="sb-sec">
        <h2 id="sb-needs-h" className="sb-sec-title" aria-label={t('sidebar.needsYouAria', { n: needYou, m: also })}>
          {t('nav.needsYou')}
          <span className="sb-needs-n mono" aria-hidden="true">
            {needYou}
            {also > 0 ? <span className="muted"> · +{also}</span> : null}
          </span>
        </h2>
        <button type="button" className="btn ghost sb-next" aria-keyshortcuts="Meta+J" onClick={() => stepQueue(1)}>
          {t('queue.next')}
        </button>
      </div>
      {items.length === 0 && held.rows.length === 0 ? (
        <p className="sb-empty">{t('empty.needsYou')}</p>
      ) : (
        <ul ref={listRef} className="sb-qlist" {...engagement.props}>
          {held.rows.map((row) => {
            if (row.kind === 'ghost') {
              return (
                <li key={row.key} className="sb-q sb-q-ghost">
                  <span className="ell grow">{row.name}</span>
                  <span className="muted">{t('home.handled')}</span>
                </li>
              );
            }
            const item = row.item;
            index += 1;
            const i = index;
            const session = item.subject.kind === 'session' ? sessionOf(world, item.subject.uid) : null;
            const owner = session ? ownerWord(t, session.control) : null;
            const leftovers = item.subject.kind === 'leftovers' ? item.subject : null;
            const age = leftovers ? null : waitAge(t, itemAge(item, world.queue.rev), locale);
            const label = leftovers
              ? t('home.leftoverLine', { n: leftovers.count, size: formatBytes(leftovers.bytes, locale) })
              : name(item);
            const word = itemStateWord(t, world, item);
            const sep = item.band > 2 && !separated && i > 0;
            if (item.band > 2) separated = true;
            return (
              <li key={row.key} className="sb-q-item" data-sep={sep ? 'true' : undefined}>
                <button
                  type="button"
                  className="sb-q"
                  {...(session ? sessionDragProps(session.uid) : {})}
                  {...roving.getItemProps(i)}
                  aria-current={session && session.uid === openUid ? 'true' : undefined}
                  aria-keyshortcuts={item.subject.kind === 'session' ? 'L' : undefined}
                  onClick={() => {
                    held.markOwnAction(item.key);
                    openItem(item);
                  }}
                  onKeyDown={(event) => {
                    roving.getItemProps(i).onKeyDown(event);
                    onKey(event, item);
                  }}
                >
                  <StateGlyph state={itemGlyph(world, item)} />
                  <span className="ell grow">{label}</span>
                  {owner ? <span className="sb-meta ell">{owner}</span> : null}
                  {age ? <span className="sb-age mono">{age}</span> : null}
                  <span className="g-sr-only">, {word}</span>
                </button>
              </li>
            );
          })}
        </ul>
      )}
    </section>
  );
}
