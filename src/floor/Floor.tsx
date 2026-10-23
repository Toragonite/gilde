/**
 * The Floor / 작업장 (M9, pixel-office v3): the pipeline drawn as a
 * workshop — who works, who waits, who talks to whom — plus its text twin and
 * the counter. A second rendering of the store (G3): it opens sessions and
 * adds them to the Split, and nothing else; approvals happen only on the card
 * [M7] (`⌘⏎` is inert here).
 *
 * `FloorScreen` is the global Floor (screen S5); `TeamFloor` is the body of
 * Team › Floor (`⌥⌘3`).
 */

import { useCallback, useEffect, useMemo, useRef, useState, useSyncExternalStore } from 'react';
import type { MouseEvent as ReactMouseEvent, PointerEvent as ReactPointerEvent } from 'react';

import { useHotkeys } from '../hooks/useHotkeys';
import type { HotkeyBinding } from '../hooks/useHotkeys';
import { useI18n } from '../i18n/useI18n';
import { clock } from '../lib/clock';
import type { AttentionItem } from '../lib/gen/AttentionItem';
import type { TeamKey } from '../lib/gen/TeamKey';
import { announce } from '../lib/notify';
import { appStore, useAppStore } from '../store/useAppStore';
import { Counter } from './Counter';
import { NAME_CARD_ID, NameCard } from './NameCard';
import { TaskTable } from './TaskTable';
import { Twin } from './Twin';
import type { TwinRow } from './Twin';
import { floorEngine, prefersReducedMotion } from './engine';
import type { FloorScope } from './layout';
import type { FlVm } from './model';
import { bottleneckOf, buildVm, counterBottleneck } from './model';
import { useFloorPrefs } from './prefs';
import { startFloorRing } from './ring';
import { installFloorTestHooks } from './testHooks';
import { twinGroups } from './twinText';
import { useFloorAnnouncements } from './useAnnouncements';
import { floorDuration, makeWords } from './words';
import { addToSplitOrAsk } from '../split/replace';
import './floor.css';

const HOVER_MS = 150;

// Loading the Floor starts the replay ring (store structure only, never persisted), so replay covers the time
// before the Floor was first opened; showcase builds also get the geometry gate's hooks (A9.2). The guard is the
// build-time switch itself, so an app build drops the hooks module.
startFloorRing(appStore);
if (import.meta.env.VITE_GILDE_SHOWCASE === '1' && typeof window !== 'undefined') {
  installFloorTestHooks(window as unknown as Record<string, unknown>, floorEngine, appStore);
}

/** A tick at every wall-clock minute while `active` (elapsed labels in the DOM parts). */
function useMinuteTick(active: boolean): number {
  const [tick, setTick] = useState(0);
  useEffect(() => {
    if (!active) return;
    let timer: ReturnType<typeof setTimeout> | null = null;
    const arm = () => {
      timer = setTimeout(() => {
        setTick((n) => n + 1);
        arm();
      }, 60_000 - (clock.wall() % 60_000) + 50);
    };
    arm();
    return () => {
      if (timer) clearTimeout(timer);
    };
  }, [active]);
  return tick;
}

function useReducedMotion(): boolean {
  const [reduced, setReduced] = useState(prefersReducedMotion);
  useEffect(() => {
    let mq: MediaQueryList | null = null;
    try {
      mq = typeof matchMedia === 'function' ? matchMedia('(prefers-reduced-motion: reduce)') : null;
    } catch {
      mq = null;
    }
    if (!mq || typeof mq.addEventListener !== 'function') return;
    const on = () => {
      setReduced(mq?.matches ?? false);
      floorEngine.request();
    };
    mq.addEventListener('change', on);
    return () => mq?.removeEventListener('change', on);
  }, []);
  return reduced;
}

export function FloorScreen() {
  return <Floor scope={GLOBAL} heading />;
}

export function TeamFloor({ teamKey }: { teamKey: TeamKey }) {
  const scope = useMemo<FloorScope>(() => ({ kind: 'team', teamKey }), [teamKey]);
  return <Floor scope={scope} heading={false} />;
}

const GLOBAL: FloorScope = { kind: 'global' };

export function Floor({ scope, heading }: { scope: FloorScope; heading: boolean }) {
  const { t, locale } = useI18n();
  const [prefs, setPrefs] = useFloorPrefs();
  const eng = useSyncExternalStore(floorEngine.subscribe, floorEngine.getSnapshot, floorEngine.getSnapshot);
  const world = useAppStore((s) => s.world);
  const visibility = useAppStore((s) => s.ui.visibility);
  const reduced = useReducedMotion();
  const active = !eng.frozen && visibility.documentVisible && !visibility.occluded && !visibility.asleep;
  const tick = useMinuteTick(active);
  const now = eng.frozen ? floorEngine.nowMs() : Math.floor(clock.wall() / 60_000) * 60_000;
  const words = useMemo(() => makeWords(locale), [locale]);
  const vm: FlVm = useMemo(
    () => buildVm(world, now, { untitled: (vendor) => words.t('floor.untitled', { vendor }), request: (k) => words.t('floor.request', { k }) }),
    // `tick` re-renders at the minute; `now` is derived from it
    [world, now, words, tick],
  );
  useFloorAnnouncements(vm, locale);

  const rootRef = useRef<HTMLDivElement>(null);
  const scrollRef = useRef<HTMLDivElement>(null);
  const wrapRef = useRef<HTMLDivElement>(null);
  const canvasRef = useRef<HTMLCanvasElement>(null);
  const counterRef = useRef<HTMLDivElement>(null);
  const list = prefs.list;
  const global = scope.kind === 'global';
  const groups = useMemo(() => twinGroups(vm, scope), [vm, scope]);
  const empty = groups.length === 0;
  const picture = !list && !empty;

  // the engine: mounted with the canvas, told about scope, words and prefs
  useEffect(() => {
    const canvas = canvasRef.current;
    const root = rootRef.current;
    const wrap = wrapRef.current;
    const scroll = scrollRef.current;
    if (!picture || !canvas || !root || !wrap || !scroll) return;
    floorEngine.mount({ canvas, root, wrap, scroll, counter: counterRef.current }, scope, words);
    let ro: ResizeObserver | null = null;
    if (typeof ResizeObserver === 'function') {
      ro = new ResizeObserver(() => floorEngine.request());
      ro.observe(wrap);
      ro.observe(scroll);
    }
    return () => {
      ro?.disconnect();
      floorEngine.unmountCanvas();
    };
    // mount once per canvas; scope and words follow below
  }, [picture]);
  useEffect(() => {
    floorEngine.setScope(scope);
    floorEngine.request();
  }, [scope]);
  useEffect(() => floorEngine.setWords(words), [words]);
  useEffect(() => {
    if (floorEngine.traffic !== prefs.traffic) floorEngine.setTraffic(prefs.traffic);
  }, [prefs.traffic]);
  useEffect(() => {
    if (global || !floorEngine.follow) return;
    floorEngine.toggleFollow();
  }, [global]);

  // selection, hover and the name card
  const [hover, setHover] = useState<string | null>(null);
  const [pinned, setPinned] = useState(false);
  const hoverTimer = useRef<ReturnType<typeof setTimeout> | null>(null);
  const setHoverSoon = useCallback((id: string | null) => {
    if (hoverTimer.current) clearTimeout(hoverTimer.current);
    hoverTimer.current = null;
    floorEngine.highlight(id);
    if (id === null) {
      setHover(null);
      return;
    }
    hoverTimer.current = setTimeout(() => setHover(id), HOVER_MS);
  }, []);
  useEffect(() => () => {
    if (hoverTimer.current) clearTimeout(hoverTimer.current);
  }, []);
  const cardSeat = pinned && eng.sel ? eng.sel : hover;
  const cardRect = cardSeat && picture ? floorEngine.seatRect(cardSeat) : null;

  const focusSeat = useCallback((id: string | null) => {
    const root = rootRef.current;
    if (!root || !id) return;
    for (const el of root.querySelectorAll<HTMLElement>('[data-flsel]')) {
      if (el.getAttribute('data-flsel') === id) {
        el.focus();
        return;
      }
    }
  }, []);
  const selectSeat = useCallback(
    (id: string | null, focus = true) => {
      floorEngine.select(id);
      if (focus) focusSeat(id);
    },
    [focusSeat],
  );

  const openSeat = useCallback(
    (id: string | null, split: boolean) => {
      const seat = id ? vm.seats.get(id) : undefined;
      if (!seat || !seat.uid) return;
      const s = useAppStore.getState();
      if (split) addToSplitOrAsk(seat.uid);
      else s.openSession(seat.uid, { focusCard: seat.state === 'needsApproval' || seat.state === 'needsAnswer' });
    },
    [vm],
  );
  const openTeamBoard = useCallback((team: TeamKey) => {
    useAppStore.getState().navigate({ screen: 'team', teamKey: team, mode: 'board' });
  }, []);
  const openTicket = useCallback(
    (item: AttentionItem, split: boolean) => {
      const s = useAppStore.getState();
      if (item.subject.kind === 'leftovers') {
        s.navigate({ screen: 'accounts', section: 'health' });
        return;
      }
      if (split) addToSplitOrAsk(item.subject.uid);
      else s.openSession(item.subject.uid, { focusCard: item.band <= 2 });
    },
    [],
  );
  const activateRow = useCallback(
    (row: TwinRow, e: { metaKey: boolean }) => {
      if (row.kind === 'team' && row.team) openTeamBoard(row.team);
      else if (row.opens) openSeat(row.opens, e.metaKey);
    },
    [openSeat, openTeamBoard],
  );
  const onFocusRow = useCallback((row: TwinRow) => {
    if (row.seat) floorEngine.select(row.seat);
  }, []);

  // pointer on the canvas
  const pt = (e: { clientX: number; clientY: number; currentTarget: HTMLCanvasElement }) => {
    const r = e.currentTarget.getBoundingClientRect();
    return { x: e.clientX - r.left, y: e.clientY - r.top };
  };
  const onCanvasClick = (e: ReactMouseEvent<HTMLCanvasElement>) => {
    const p = pt(e);
    const T = floorEngine.stripTimeAt(p.x, p.y);
    if (T !== null) {
      floorEngine.replayAt(T);
      return;
    }
    const id = floorEngine.hitAt(p.x, p.y);
    if (id) {
      selectSeat(id);
      openSeat(id, e.metaKey);
      return;
    }
    const team = floorEngine.headerAt(p.x, p.y);
    if (team) {
      openTeamBoard(team);
      return;
    }
    // empty floor: keyboard focus moves into the Floor (its twin), so the Floor keys work
    const first = rootRef.current?.querySelector<HTMLElement>('[data-fl-twin] [tabindex="0"]');
    first?.focus();
  };
  const onCanvasMove = (e: ReactPointerEvent<HTMLCanvasElement>) => {
    const p = pt(e);
    if (e.buttons & 1) {
      const T = floorEngine.stripTimeAt(p.x, p.y);
      if (T !== null) floorEngine.replayAt(T);
    }
    const id = floorEngine.hitAt(p.x, p.y);
    e.currentTarget.style.cursor = id || floorEngine.headerAt(p.x, p.y) || floorEngine.stripTimeAt(p.x, p.y) !== null ? 'pointer' : 'default';
    if (id !== hover) setHoverSoon(id);
  };

  // keys (scope F: only while focus is inside the Floor; letters by event.code, never during IME composition)
  const inTwin = (e: KeyboardEvent) => e.target instanceof Element && !!e.target.closest('[data-fl-twin]');
  const bindings: HotkeyBinding[] = [
    ...(['left', 'right', 'up', 'down'] as const).map((dir) => ({
      id: 'floor.move',
      combo: 'arrow' + dir,
      allowRepeat: true,
      when: (e: KeyboardEvent) => picture && inTwin(e),
      run: () => {
        const next = floorEngine.nearest(dir);
        if (next) selectSeat(next);
      },
    })),
    {
      id: 'floor.nextCharacter',
      combo: 'tab',
      when: (e) => picture && inTwin(e) && floorEngine.step(1) !== null,
      run: () => selectSeat(floorEngine.step(1)),
    },
    {
      id: 'floor.previousCharacter',
      combo: 'shift+tab',
      when: (e) => picture && inTwin(e) && floorEngine.step(-1) !== null,
      run: () => selectSeat(floorEngine.step(-1)),
    },
    {
      id: 'floor.open',
      combo: 'enter',
      when: (e) => inTwin(e),
      run: (e) => {
        const el = e.target instanceof Element ? e.target.closest('[data-fl-row]') : null;
        const sel = el?.getAttribute('data-flsel');
        if (sel) openSeat(sel, false);
        else (el as HTMLElement | null)?.click();
      },
    },
    { id: 'floor.addToSplit', combo: 'mod+d', run: () => openSeat(floorEngine.sel, true) },
    {
      id: 'floor.nameCard',
      combo: 'space',
      when: (e) => picture && inTwin(e) && !!floorEngine.sel,
      run: () => setPinned((p) => !p),
    },
    {
      id: 'floor.nextTicket',
      combo: 'j',
      run: () => {
        const tickets = [...(rootRef.current?.querySelectorAll<HTMLElement>('[data-fl-ticket]') ?? [])];
        if (!tickets.length) return;
        const i = tickets.findIndex((el) => el === document.activeElement);
        const next = tickets[(i + 1) % tickets.length] as HTMLElement;
        next.focus();
        const seat = next.getAttribute('data-flseat');
        if (seat) floorEngine.select(seat);
      },
    },
    { id: 'floor.followTeam', combo: 'f', when: () => picture && global, run: () => floorEngine.toggleFollow() },
    { id: 'floor.zoomIn', combo: ['equal', 'shift+equal'], when: () => picture, run: () => floorEngine.zoomBy(1) },
    { id: 'floor.zoomOut', combo: 'minus', when: () => picture, run: () => floorEngine.zoomBy(-1) },
    { id: 'floor.zoomReset', combo: '0', when: () => picture, run: () => floorEngine.zoomBy(0) },
    { id: 'floor.trafficLines', combo: 'm', run: () => setPrefs({ traffic: !prefs.traffic }) },
    {
      id: 'floor.tidy',
      combo: 't',
      when: () => picture,
      run: () => {
        floorEngine.tidy();
        announce(t('floor.tidyDone'), { politeness: 'polite' });
      },
    },
    { id: 'floor.pause', combo: 'period', run: () => floorEngine.setPaused(!floorEngine.paused) },
    { id: 'floor.legend', combo: 'l', run: () => setPrefs({ legend: !prefs.legend }) },
    { id: 'floor.replay', combo: 'r', when: () => picture, run: () => (floorEngine.replayT !== null ? floorEngine.goLive() : floorEngine.replayPlayFromStart()) },
    { id: 'floor.stepBack', combo: 'bracketleft', when: () => picture, allowRepeat: true, run: () => floorEngine.stepReplay(-30_000) },
    { id: 'floor.stepForward', combo: 'bracketright', when: () => picture, allowRepeat: true, run: () => floorEngine.stepReplay(30_000) },
    {
      id: 'floor.stepOut',
      combo: 'escape',
      run: () => {
        if (floorEngine.stepOut()) return;
        if (pinned) {
          setPinned(false);
          return;
        }
        if (floorEngine.sel) {
          floorEngine.select(null);
          return;
        }
        rootRef.current?.querySelector<HTMLElement>('[data-fl-twin] [tabindex="0"]')?.focus();
      },
    },
  ];
  useHotkeys(bindings, { scope: 'F', element: rootRef });

  // the bottleneck chips of the toolbar (pixel-office §2.3: the team header names the bottleneck stations)
  const chips: { key: string; label: string; seat: string | null }[] = [];
  for (const g of groups) {
    for (const team of g.teams) {
      for (const id of team.bottlenecks) {
        const b = bottleneckOf(team, id);
        const who = vm.seats.get(id)?.name ?? '';
        if (!b) continue;
        const why = b.why === 'blocks' ? t('floor.bnBlocks', { who, size: b.n }) : b.why === 'tray' ? t('floor.bnTray', { who, size: b.n }) : t('floor.bnSlow', { who });
        chips.push({ key: team.key + id, label: t('floor.bottleneckN', { size: b.n }) + ' · ' + why, seat: id });
      }
    }
  }
  const cb = counterBottleneck(vm);
  if (cb) chips.push({ key: 'you', label: t('floor.bottleneckN', { size: cb.n }) + ' · ' + t('floor.bnYou', { size: cb.n, age: floorDuration(locale, cb.age) }), seat: null });

  const teamsShown = groups.reduce((n, g) => n + g.teams.length, 0);
  const soloShown = groups.reduce((n, g) => n + g.solo.length, 0);
  const replay = eng.replayT !== null;
  const followName = eng.follow ? vm.teams.find((x) => x.key === eng.follow)?.name : undefined;
  const soloIndex = useCallback((bay: string, id: string) => floorEngine.book.seats.get(bay)?.get(id), []);

  return (
    <div className="fl-root" ref={rootRef} data-fl-scope={scope.kind}>
      {heading && (
        <div className="fl-head">
          <h1 className="fl-h1">{t('floor.name')}</h1>
          <span className="fl-muted">{t('floor.summary', { teams: teamsShown, solo: soloShown })}</span>
        </div>
      )}
      <div className="fl-bar" role="toolbar" aria-label={t('floor.view')}>
        {chips.map((c) => (
          <button
            key={c.key}
            type="button"
            className="fl-chip"
            onClick={() => {
              if (c.seat) openSeat(c.seat, false);
              else rootRef.current?.querySelector<HTMLElement>('[data-fl-ticket]')?.focus(); // the counter's bottleneck: its first ticket
            }}
          >
            <span className="fl-warn">{c.label}</span>
          </button>
        ))}
        <span className="fl-sp" />
        {picture && global && (
          <button type="button" className="fl-btn" aria-pressed={!!eng.follow} aria-keyshortcuts="F" onClick={() => floorEngine.toggleFollow()}>
            {followName ? t('floor.followTeam', { team: followName }) : t('floor.follow')}
          </button>
        )}
        {picture && (
          <button type="button" className="fl-btn" aria-pressed={replay} aria-keyshortcuts="R" onClick={() => (replay ? floorEngine.goLive() : floorEngine.replayPlayFromStart())}>
            {replay ? t('floor.live') : t('floor.replay')}
          </button>
        )}
        <button type="button" className="fl-btn" aria-pressed={eng.paused} aria-keyshortcuts="." onClick={() => floorEngine.setPaused(!eng.paused)}>
          {t('floor.pauseAnimation')}
        </button>
        <button type="button" className="fl-btn" aria-pressed={list} onClick={() => setPrefs({ list: !list })}>
          {list ? t('floor.showPicture') : t('action.showAsList')}
        </button>
        {picture && (
          <details className="fl-more">
            <summary className="fl-btn">{t('floor.view')}</summary>
            <div className="fl-menu">
              <button type="button" className="fl-btn" aria-keyshortcuts="-" onClick={() => floorEngine.zoomBy(-1)}>
                {t('floor.zoomOut')}
              </button>
              <button type="button" className="fl-btn" aria-keyshortcuts="=" onClick={() => floorEngine.zoomBy(1)}>
                {t('floor.zoomIn')}
              </button>
              <button type="button" className="fl-btn" aria-keyshortcuts="0" onClick={() => floorEngine.zoomBy(0)}>
                {t('floor.zoomFit')}
              </button>
              <button type="button" className="fl-btn" aria-pressed={prefs.traffic} aria-keyshortcuts="M" onClick={() => setPrefs({ traffic: !prefs.traffic })}>
                {t('floor.traffic')}
              </button>
              <button type="button" className="fl-btn" aria-pressed={prefs.legend} aria-keyshortcuts="L" onClick={() => setPrefs({ legend: !prefs.legend })}>
                {t('floor.legendToggle')}
              </button>
              <button
                type="button"
                className="fl-btn"
                aria-keyshortcuts="T"
                onClick={() => {
                  floorEngine.tidy();
                  announce(t('floor.tidyDone'), { politeness: 'polite' });
                }}
              >
                {t('floor.tidy')}
              </button>
            </div>
          </details>
        )}
      </div>
      {(reduced || eng.paused) && picture && <p className="fl-note">{t('floor.reducedMotion')}</p>}
      {replay && <p className="fl-note fl-replay">{t('floor.replayNow')}</p>}
      <div className="fl-scroll" ref={scrollRef}>
        {empty && (
          <div className="fl-empty">
            <span>{t('floor.empty')}</span>
            <button type="button" className="fl-btn" onClick={() => useAppStore.getState().openSheet('newSession')}>
              {t('action.newSession')}
            </button>
          </div>
        )}
        {picture && (
          <>
            <p className="fl-sr-only">
              <span>{t('floor.listHint')}</span>
            </p>
            <div className="fl-wrap" ref={wrapRef}>
              <canvas
                ref={canvasRef}
                className="fl-canvas"
                aria-hidden="true"
                onClick={onCanvasClick}
                onPointerMove={onCanvasMove}
                onPointerLeave={() => setHoverSoon(null)}
              />
              {cardSeat && cardRect && <NameCard vm={vm} seatId={cardSeat} rect={cardRect} pinned={pinned} />}
            </div>
            {prefs.legend && (
              <p className="fl-legend">
                {t('floor.legend')} {t('floor.keys')}
              </p>
            )}
            {groups.flatMap((g) => g.teams).map((team) => (
              <TaskTable key={team.key} vm={vm} team={team} headingLevel={heading ? 2 : 3} />
            ))}
          </>
        )}
        {!empty && (
          <Twin
            vm={vm}
            scope={scope}
            visible={list}
            selected={eng.sel}
            describedBy={pinned && eng.sel && cardRect ? NAME_CARD_ID : null}
            soloIndex={soloIndex}
            onFocusRow={onFocusRow}
            onActivate={activateRow}
          />
        )}
      </div>
      <Counter ref={counterRef} vm={vm} onOpen={openTicket} onHover={(id) => floorEngine.highlight(id)} onSelect={(id) => id && floorEngine.select(id)} />
    </div>
  );
}
