/**
 * One-line queue rows on Home (the Next list and an opened chip) [M15].
 *
 *  - glyph · name · project (· owner) · age; the state word shows on hover or
 *    focus only (the M15 exception) and is always in the accessible name; the
 *    glyph shapes stay distinct per state;
 *  - one Tab stop, ↑↓ move, ⏎ opens, `L` Later; the row's actions (the core's
 *    primary action, Open) appear on hover or focus;
 *  - while hovered or focused the order holds and a row handled elsewhere
 *    stays as a "처리되었습니다" ghost (T13's `useHeldOrder`);
 *  - for 700 ms after the rows change — including a stacked request taking a
 *    row's place under the same queue key (A44.1) — action clicks are
 *    ignored, visibly.
 */

import type { KeyboardEvent as ReactKeyboardEvent } from 'react';

import { StateGlyph } from '../components/StateGlyph';
import { useAnswerGuard } from '../hooks/useAnswerGuard';
import { useEngagement, useHeldOrder } from '../hooks/useHold';
import { isImeComposing } from '../hooks/useHotkeys';
import { useRovingTabIndex } from '../hooks/useRovingTabIndex';
import { useI18n } from '../i18n/useI18n';
import type { AttentionItem } from '../lib/gen/AttentionItem';
import type { WorldState } from '../store/worldSlice';
import { sessionOf } from '../store/worldSlice';
import { cardOf, laterItem, openItem, runPrimary } from '../shell/actions';
import { itemAge } from '../shell/useAges';
import { itemGlyph, itemName, itemStateWord, ownerWord, waitAge } from '../shell/words';
import { primaryLabel, whatLine } from './FocusCard';

/**
 * The 700 ms guard key of a list of queue rows (A44.1): each row's queue key
 * with the request its top card shows, so a stacked request that takes a
 * row's place under the same queue key (`q_<uid>`) restarts the guard.
 */
export function rowsGuardKey(world: WorldState, items: readonly AttentionItem[]): string {
  return items.map((i) => `${i.key}:${cardOf(world, i)?.interaction.requestId ?? ''}`).join('|');
}

export interface QueueRowsProps {
  world: WorldState;
  items: readonly AttentionItem[];
  /** Accessible name of the list. */
  label: string;
  /** Show the one-line "what" text (opened chips do; Next rows stay one short line). */
  showWhat?: boolean;
}

export function QueueRows({ world, items, label, showWhat = false }: QueueRowsProps) {
  const i18n = useI18n();
  const { t, locale } = i18n;
  const engagement = useEngagement();
  const name = (item: AttentionItem) => itemName(t, world, item, locale);
  const held = useHeldOrder(items, (i) => i.key, name, engagement.engaged);
  const live = held.rows.filter((r) => r.kind === 'item');
  const roving = useRovingTabIndex(live.length);
  const guard = useAnswerGuard(
    held.rows.length > 0 ? held.rows.map((r) => (r.kind === 'item' ? rowsGuardKey(world, [r.item]) : r.key)).join('|') : null,
  );

  const onKey = (event: ReactKeyboardEvent<HTMLElement>, index: number, item: AttentionItem) => {
    if (isImeComposing(event.nativeEvent) || event.metaKey || event.ctrlKey || event.altKey) return;
    if (event.target !== event.currentTarget) return;
    if (event.code === 'Enter') {
      event.preventDefault();
      if (!event.repeat) openItem(item);
      return;
    }
    if (event.code === 'KeyL' && !event.shiftKey) {
      event.preventDefault();
      if (event.repeat || guard.isGuarded() || item.subject.kind !== 'session') return;
      held.markOwnAction(item.key);
      laterItem(item);
      return;
    }
    roving.getItemProps(index).onKeyDown(event);
  };

  let index = -1;
  return (
    <ul className="hr-list" aria-label={label} {...engagement.props}>
      {held.rows.map((row) => {
        if (row.kind === 'ghost') {
          return (
            <li key={row.key} className="hr hr-ghost">
              <span className="hr-name ell">{row.name}</span>
              <span className="hr-handled">{t('home.handled')}</span>
            </li>
          );
        }
        const item = row.item;
        index += 1;
        const i = index;
        const props = roving.getItemProps(i);
        const session = item.subject.kind === 'session' ? sessionOf(world, item.subject.uid) : null;
        const project = session?.projectId ? (world.projects.get(session.projectId)?.name ?? null) : null;
        const owner = session ? ownerWord(t, session.control) : null;
        const word = itemStateWord(t, world, item);
        const age = waitAge(t, itemAge(item, world.queue.rev), locale);
        const what = showWhat ? whatLine(i18n, world, item) : null;
        const primary = item.primary === 'open' || item.primary === 'message' ? null : primaryLabel(t, world, item, session);
        const where = [project, owner].filter(Boolean).join(' · ');
        return (
          <li key={row.key} className="hr" data-item={item.key}>
            <div
              className="hr-row"
              role="group"
              tabIndex={props.tabIndex}
              ref={props.ref}
              onFocus={props.onFocus}
              data-roving-index={props['data-roving-index']}
              aria-label={[name(item), word, age, where, what].filter(Boolean).join(', ')}
              aria-keyshortcuts={item.subject.kind === 'session' ? 'Enter L' : 'Enter'}
              onKeyDown={(event) => onKey(event, i, item)}
              onClick={(event) => {
                if ((event.target as HTMLElement).closest('button')) return;
                openItem(item);
              }}
            >
              <StateGlyph state={itemGlyph(world, item)} />
              <span className="hr-name ell">{name(item)}</span>
              {where ? <span className="hr-where ell">{where}</span> : null}
              <span className="hr-word" aria-hidden="true">
                {word}
              </span>
              {what ? <span className="hr-what ell">{what}</span> : null}
              <span className="grow" />
              <span className="hr-age mono">{age}</span>
              <span className="hr-actions">
                {primary ? (
                  <button
                    type="button"
                    className="btn"
                    tabIndex={-1}
                    {...guard.props}
                    onClick={guard.guard(() => {
                      held.markOwnAction(item.key);
                      runPrimary(item);
                    })}
                  >
                    {primary}
                  </button>
                ) : null}
                <button type="button" className="btn ghost" tabIndex={-1} onClick={() => openItem(item)}>
                  {t('home.open')}
                </button>
              </span>
            </div>
          </li>
        );
      })}
    </ul>
  );
}
