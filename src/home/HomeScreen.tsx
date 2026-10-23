/**
 * Focus Home [M15] — the owner's answer to "the old Home showed too much at
 * once": one question, "what do I do next", and calm otherwise.
 *
 *   Home                                   4 need you · 18 running
 *   [one notice, at most]
 *   project › name                     claude · …        Open ⏎  Later L
 *   ┌ the focus card: queue #1 with the action that resolves it ┐
 *   Next 3            ▲ db-agent · auth-split   checkout-service      1m
 *   Also  [× 1] [~ 1] [● 2] [⊗ 3]   (bands 3–6 as count chips)
 *   Teams  ▲ auth-split  checkout-service  ▬▬▬ 4/8  Bottleneck · db-agent
 *   18 running · 4 projects ›
 *   [one warning line, only when something is off]
 *
 * With nothing waiting: "Nothing needs you." and the team lines. With nothing
 * running at all: "Nothing is running. [New session]". Counts are the core's
 * (`queue.needYou`), never recounted; Later-ed items are left out as the
 * sidebar leaves them out.
 */

import { useEffect, useRef, useState } from 'react';

import { EmptyState } from '../components/EmptyState';
import { StateGlyph } from '../components/StateGlyph';
import { GaugeIcon } from '../components/icons';
import { useAnswerGuard } from '../hooks/useAnswerGuard';
import { useEngagement, useHeldFocus } from '../hooks/useHold';
import { useHotkeys } from '../hooks/useHotkeys';
import type { HotkeyBinding } from '../hooks/useHotkeys';
import type { MessageKey } from '../i18n';
import { useI18n } from '../i18n/useI18n';
import { clock } from '../lib/clock';
import type { AttentionItem } from '../lib/gen/AttentionItem';
import { appStore, useAppStore } from '../store/useAppStore';
import { visibleQueueItems } from '../store/worldSlice';
import type { WorldState } from '../store/worldSlice';
import { answerByKey, answerableInline, cardOf, laterItem, openItem, runPrimary } from '../shell/actions';
import { useAgeTick } from '../shell/useAges';
import { itemName, liveProjectCount, runningCount, stateKey } from '../shell/words';
import { FocusCard } from './FocusCard';
import { HomeNotice } from './HomeNotice';
import { QueueRows, rowsGuardKey } from './QueueRows';
import type { Chip } from './model';
import { accountName, chipCount, quotaWarnings, splitQueue, teamLines, visibleNext } from './model';
import './home.css';

const CHIP_STATE: Readonly<Record<Chip['band'], 'error' | 'noProgress' | 'newReply' | 'leftover'>> = {
  3: 'error',
  4: 'noProgress',
  5: 'newReply',
  6: 'leftover',
};

/** The item a key on Home acts on: the focused row's, else the focus card's. */
function itemUnderFocus(root: HTMLElement | null, items: readonly AttentionItem[], focusKey: string | null): AttentionItem | null {
  const active = document.activeElement;
  const row = active instanceof HTMLElement ? active.closest<HTMLElement>('[data-item]') : null;
  const key = row && root?.contains(row) ? row.getAttribute('data-item') : focusKey;
  return items.find((i) => i.key === key) ?? null;
}

function Chips({ world, chips }: { world: WorldState; chips: Chip[] }) {
  const { t } = useI18n();
  const [open, setOpen] = useState<Chip['band'] | null>(null);
  const opened = chips.find((c) => c.band === open) ?? null;
  if (chips.length === 0) return null;
  return (
    <section className="fh-sec" aria-label={t('home.alsoWaiting')}>
      <div className="fh-chips">
        <span className="fh-lbl" aria-hidden="true">
          {t('home.also')}
        </span>
        {chips.map((chip) => {
          const state = CHIP_STATE[chip.band];
          const word = t(stateKey({ kind: state }));
          const n = chipCount(chip);
          return (
            <button
              key={chip.band}
              type="button"
              className="fh-chip"
              aria-expanded={open === chip.band}
              onClick={() => setOpen(open === chip.band ? null : chip.band)}
            >
              <StateGlyph state={state} />
              <span>{word}</span>
              <span className="mono fh-chip-n">{n}</span>
            </button>
          );
        })}
      </div>
      {opened ? <QueueRows world={world} items={opened.items} label={t('home.alsoWaiting')} showWhat /> : null}
    </section>
  );
}

function TeamLines({ world }: { world: WorldState }) {
  const { t } = useI18n();
  const navigate = useAppStore((s) => s.navigate);
  const layouts = useAppStore((s) => s.ui.teamLayouts);
  const lines = teamLines(world);
  if (lines.length === 0) return null;
  return (
    <section className="fh-sec" aria-labelledby="fh-teams-h">
      <h2 className="fh-lbl" id="fh-teams-h">
        {t('home.teams')}
      </h2>
      <ul className="fh-teams">
        {lines.map(({ team, bottlenecks }) => {
          const project = team.projectId ? (world.projects.get(team.projectId)?.name ?? null) : null;
          const word = t(stateKey(team.glyphState));
          const progress = team.progress;
          const frac = progress.inferred ? null : `${progress.approx ? '≈' : ''}${progress.done}/${progress.total}`;
          const bottleneck = bottlenecks[0] ? t('home.bottleneck', { who: bottlenecks[0] }) : null;
          return (
            <li key={team.key}>
              <button
                type="button"
                className="fh-team"
                onClick={() => navigate({ screen: 'team', teamKey: team.key, mode: layouts[team.key]?.mode ?? 'panes' })}
              >
                <StateGlyph state={team.glyphState.kind} />
                <span className="g-sr-only">{t('home.teamPrefix')} </span>
                <span className="fh-team-name ell">{team.name}</span>
                {project ? <span className="fh-team-project ell">{project}</span> : null}
                {frac ? (
                  <>
                    <span className="bar fh-team-bar" aria-hidden="true">
                      <span style={{ width: `${progress.total > 0 ? Math.round((progress.done / progress.total) * 100) : 0}%` }} />
                    </span>
                    <span className="mono fh-team-frac">{frac}</span>
                  </>
                ) : (
                  <span className="muted">{t('floor.noTaskList')}</span>
                )}
                {bottleneck ? <span className="fh-bottleneck">{bottleneck}</span> : null}
                <span className="g-sr-only">, {word}</span>
              </button>
            </li>
          );
        })}
      </ul>
    </section>
  );
}

function RunningLine({ world, sidebarShown }: { world: WorldState; sidebarShown: boolean }) {
  const { t } = useI18n();
  const text = t('home.runLine', { run: runningCount(world), projects: liveProjectCount(world) });
  if (sidebarShown) return <p className="fh-run">{text}</p>;
  return (
    <button
      type="button"
      className="btn ghost fh-run"
      onClick={() => appStore.getState().setSidebarVisible(true)}
    >
      {text}
      <span className="g-sr-only">, {t('home.showTree')}</span>
      <span aria-hidden="true"> ›</span>
    </button>
  );
}

function WarningLine({ world }: { world: WorldState }) {
  const { t } = useI18n();
  const navigate = useAppStore((s) => s.navigate);
  const warnings = quotaWarnings(world);
  if (warnings.length === 0) return null;
  const windowWord = (kind: string, label: string | null) =>
    label ?? t(`home.window.${kind === 'fiveHour' || kind === 'week' ? kind : 'other'}` as MessageKey);
  const text = warnings
    .map((w) => t('home.warnAccount', { account: accountName(w.account), window: windowWord(w.window, w.label), pct: w.pct }))
    .join(' · ');
  return (
    <div className="fh-warn" role="note">
      <GaugeIcon size="sm" />
      <span className="fh-warn-text">{text}</span>
      <button type="button" className="btn" onClick={() => navigate({ screen: 'accounts', section: 'accounts' })}>
        {t('home.seeAccounts')}
      </button>
    </div>
  );
}

export interface HomeScreenProps {
  /** Whether the sidebar is on screen (the running line links to it only when it is not). */
  sidebarShown?: boolean;
}

export function HomeScreen({ sidebarShown = true }: HomeScreenProps) {
  const { t, locale } = useI18n();
  useAgeTick();
  const world = useAppStore((s) => s.world);
  const root = useRef<HTMLDivElement>(null);
  const [moreNext, setMoreNext] = useState(false);
  const items = visibleQueueItems(world, clock.wall());
  const engagement = useEngagement();
  const focus = useHeldFocus(
    items,
    (i) => i.key,
    (i) => itemName(t, world, i, locale),
    engagement.engaged,
  );
  const focusKey = focus.ghost ? focus.ghost.key : (focus.item?.key ?? null);
  // A26.1: the focus card's session counts as visible while Home shows it.
  const focusUid = focus.item && !focus.ghost && focus.item.subject.kind === 'session' ? focus.item.subject.uid : null;
  useEffect(() => {
    appStore.getState().setHomeFocusUid(focusUid);
  }, [focusUid]);
  useEffect(() => () => appStore.getState().setHomeFocusUid(null), []);
  const split = splitQueue(items, focusKey);
  const next = visibleNext(split.next, moreNext);
  const running = runningCount(world);
  // The 700 ms guards (§8.6): one for the focus card's item, one for the Next
  // rows. Both are keyed on the request each item's top card shows, not only
  // on the queue key (`q_<uid>`, one per session): a stacked request that
  // replaces the one just answered starts the guard again (A44.1).
  const focusTarget = focus.item && !focus.ghost ? cardOf(world, focus.item) : null;
  const focusGuard = useAnswerGuard(focus.item && !focus.ghost ? `${focus.item.key}:${focusTarget?.interaction.requestId ?? ''}` : null);
  const listGuard = useAnswerGuard(split.next.length > 0 ? rowsGuardKey(world, split.next) : null);

  // Arrival (⌘0, a nav click, launch) puts focus on the focus card; events never
  // do. The first item shown after arrival takes it (T15's card through
  // `autoFocus`, a plain card here); a later item never does.
  const [arrival] = useState(() => {
    const active = document.activeElement;
    return !active || active === document.body || active.closest('[data-region="sidebar"],[data-region="main"]') !== null;
  });
  const arrivalKey = useRef<string | null>(null);
  if (arrival && arrivalKey.current === null && focus.item && !focus.ghost) arrivalKey.current = focus.item.key;
  const autoFocus = arrival && focus.item !== null && !focus.ghost && focus.item.key === arrivalKey.current;
  useEffect(() => {
    if (!arrival) return;
    root.current?.querySelector<HTMLElement>('.fh .fh-card')?.focus({ preventScroll: true });
    // Only on mount; T15's card focuses itself through `autoFocus`.
  }, [arrival]);

  /**
   * What ⌘⏎ / ⌘⌫ do on Home [M7]:
   *  - the focus card's own card answers itself (T15's shortcuts, so this
   *    returns null and the key falls through to it);
   *  - on a focused row the key is the row's: its approval is answered here,
   *    decided by the card stack's `shortcutAction` (A44.2) behind the rows'
   *    guard (A44.1); a watched item goes to its owner on ⌘⏎; anything else
   *    is inert — never passed on to the focus card, which is not what the
   *    user is on.
   */
  const keyAction = (allow: boolean): (() => void) | null => {
    const s = appStore.getState();
    const visible = visibleQueueItems(s.world, clock.wall());
    const item = itemUnderFocus(root.current, visible, focusKey);
    if (!item) return null;
    const onFocusCard = item.key === focusKey;
    const target = cardOf(s.world, item);
    if (onFocusCard && target) return null;
    const guard = onFocusCard ? focusGuard : listGuard;
    return () => {
      if (guard.isGuarded()) return;
      if (item.watched || item.primary === 'goToOwner') {
        if (allow) runPrimary(item);
        return;
      }
      if (!target || !answerableInline(target)) return;
      const result = answerByKey(target, allow ? 'allowOnce' : 'deny');
      if (result === 'sent') focus.markOwnAction(item.key);
      else if (result === 'open') openItem(item);
    };
  };
  const bindings: HotkeyBinding[] = [
    { id: 'answer.primary', combo: 'mod+enter', when: () => keyAction(true) !== null, run: () => keyAction(true)?.() },
    { id: 'answer.deny', combo: 'mod+backspace', when: () => keyAction(false) !== null, run: () => keyAction(false)?.() },
    {
      id: 'home.open',
      combo: 'enter',
      when: (event) => {
        const target = event.target as HTMLElement | null;
        return !!target && (target.classList.contains('fh-card') || target.classList.contains('hc'));
      },
      run: () => {
        if (focus.item && !focus.ghost) openItem(focus.item);
      },
    },
    {
      id: 'home.later',
      combo: 'l',
      when: (event) => {
        const target = event.target as HTMLElement | null;
        return !!target && !!target.closest('.fh') && !target.closest('[data-item].hr');
      },
      run: () => {
        if (focus.item && !focus.ghost) {
          focus.markOwnAction(focus.item.key);
          laterItem(focus.item);
        }
      },
    },
  ];
  useHotkeys(bindings, { scope: 'L', element: root, requireFocus: true });

  const queueEmpty = items.length === 0 && !focus.ghost;
  const nothingRunning = running === 0 && queueEmpty && world.teams.size === 0;

  return (
    <div className="fh-scroll" ref={root}>
      <div className="fh-col">
        <div className="fh-head">
          <h1 className="fh-title">{t('nav.home')}</h1>
          <p className="fh-sum">{t('home.summary', { need: world.queue.needYou, run: running })}</p>
        </div>
        <HomeNotice />
        {nothingRunning ? (
          <EmptyState message={t('empty.home')} actionLabel={t('action.newSession')} onAction={() => appStore.getState().openSheet('newSession')} />
        ) : queueEmpty ? (
          <>
            <p className="fh-calm">{t('empty.needsYou')}</p>
            <TeamLines world={world} />
            <RunningLine world={world} sidebarShown={sidebarShown} />
          </>
        ) : (
          <>
            <FocusCard
              world={world}
              item={focus.item}
              ghost={focus.ghost}
              guard={focusGuard}
              autoFocus={autoFocus}
              engagement={engagement.props}
              markOwnAction={focus.markOwnAction}
            />
            {split.next.length > 0 ? (
              <section className="fh-sec" aria-labelledby="fh-next-h">
                <h2 className="fh-lbl" id="fh-next-h">
                  {t('home.next')}
                  <span className="mono fh-lbl-n">{split.next.length}</span>
                  <span className="fh-lbl-hint kbd" aria-hidden="true">
                    ⌘J
                  </span>
                </h2>
                <QueueRows world={world} items={next.shown} label={t('home.next')} />
                {next.more > 0 || moreNext ? (
                  <button type="button" className="btn ghost fh-more" aria-expanded={moreNext} onClick={() => setMoreNext(!moreNext)}>
                    {moreNext ? t('home.less') : t('home.moreN', { n: next.more })}
                  </button>
                ) : null}
              </section>
            ) : null}
            <Chips world={world} chips={split.chips} />
            <TeamLines world={world} />
            <RunningLine world={world} sidebarShown={sidebarShown} />
            <WarningLine world={world} />
          </>
        )}
        {queueEmpty && !nothingRunning ? <WarningLine world={world} /> : null}
      </div>
    </div>
  );
}
