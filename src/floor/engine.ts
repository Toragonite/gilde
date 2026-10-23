/**
 * The Floor engine: one per window (only one Floor is mounted at a time —
 * global or one team's). It owns the canvas, the frame scheduler, the
 * animations, the selection, follow, zoom, Pause and replay, and keeps the
 * places stable across mounts (G4).
 *
 * Scheduler (pixel-office §8, contract §11.2 rule 6): there is no
 * unconditional animation loop. A store change requests exactly one frame.
 * Frames repeat only while something is due: walks, card trips, planes and
 * replay play at 15 fps; typing and monitor lines at 4 fps; error smoke at
 * 2 fps; 1 fps for those two while the window is not frontmost; elapsed
 * labels one frame a minute. Nothing runs while the view is hidden, the
 * document is hidden, the window is occluded or asleep, or the canvas is
 * scrolled out of view; animations then settle to their end states.
 */

import { clock } from '../lib/clock';
import type { MemberKey } from '../lib/gen/MemberKey';
import type { SessionUid } from '../lib/gen/SessionUid';
import type { TeamKey } from '../lib/gen/TeamKey';
import { appStore } from '../store/useAppStore';
import { CELL, FIT_SCALES, MAX_CANVAS_PIXELS, MAX_SCALE, MIN_CSS_W, MIN_SCALE, PLANES_PER_BAY, STRIP_WIN_S, WALL, metrics } from './constants';
import type { Metrics } from './constants';
import {

  drawCardTrips,
  drawFloorText,
  drawFollowDim,
  drawFrameDashed,
  drawOverlays,
  drawPlanes,
  drawReplayBanner,
  drawRooms,
  drawSeat,
  drawSelection,
  drawShelves,
  drawStrip,
  drawTraffic,
  drawWalker,
  pathLen,
  stripData,
  walkPath,
} from './draw';
import type { Anim, Box, DrawEnv, Pt, StripData, StripGeom, Words } from './draw';
import type { Overlap } from './geometry';
import { checkGeometry } from './geometry';
import type { Area, Cell, FloorScope, Room, StripLane } from './layout';
import { SeatBook, buildRooms, cellsOf, itemPos, packRooms, stripHeight, stripLanes, trackY, lineOy } from './layout';
import type { FlSeat, FlTeam, FlVm, Structure } from './model';
import { bottleneckOf, buildVm } from './model';
import { paletteSignature, readPalette } from './palette';
import { floorRing } from './ring';
import type { ReplayRing } from './ring';
import type { Ctx2D, Identity } from './sprites';
import { SpriteCache, identityOf } from './sprites';

// ---------------------------------------------------------------------------
// A context for environments without a canvas (tests): records nothing,
// measures text at a fixed advance so the geometry still lays out.
// ---------------------------------------------------------------------------

export class NullCtx implements Ctx2D {
  fillStyle: string | CanvasGradient | CanvasPattern = '';
  globalAlpha = 1;
  font = '10px sans-serif';
  textAlign: CanvasTextAlign = 'left';
  textBaseline: CanvasTextBaseline = 'top';
  imageSmoothingEnabled = false;
  fills = 0;
  fillRect(): void {
    this.fills++;
  }
  fillText(): void {}
  measureText(text: string): { width: number } {
    const m = /(\d+(?:\.\d+)?)px/.exec(this.font);
    const size = m ? Number(m[1]) : 10;
    let w = 0;
    for (const ch of text) {
      const c = ch.codePointAt(0) ?? 0;
      // East Asian wide characters take a full em, the rest about 0.6 em
      w += c >= 0x1100 && (c <= 0x115f || (c >= 0x2e80 && c <= 0xa4cf) || (c >= 0xac00 && c <= 0xd7a3) || (c >= 0xf900 && c <= 0xfaff) || (c >= 0xff00 && c <= 0xff60)) ? size : size * 0.6;
    }
    return { width: w };
  }
  setTransform(): void {}
  drawImage(): void {}
}

export function prefersReducedMotion(): boolean {
  try {
    return typeof matchMedia === 'function' && matchMedia('(prefers-reduced-motion: reduce)').matches;
  } catch {
    return false;
  }
}

export interface Hit {
  x: number;
  y: number;
  w: number;
  h: number;
  id: string;
}

export interface Frame {
  vm: FlVm;
  rooms: Room[];
  cells: Map<string, Cell>;
  scale: number;
  M: Metrics;
  world: { w: number; h: number };
  hits: Hit[];
  strip: StripGeom | null;
  /** Seats in reading order. */
  order: string[];
}

type Listener = () => void;

export interface EngineSnapshot {
  sel: string | null;
  follow: TeamKey | null;
  paused: boolean;
  replayT: number | null;
  playing: boolean;
  zoom: number;
  traffic: boolean;
  frozen: boolean;
  /** Bumped on every drawn frame that changed what the DOM parts show. */
  rev: number;
}

export class FloorEngine {
  canvas: HTMLCanvasElement | null = null;
  private ctx: Ctx2D | null = null;
  private root: HTMLElement | null = null;
  private wrap: HTMLElement | null = null;
  private scroll: HTMLElement | null = null;
  private counter: HTMLElement | null = null;
  scope: FloorScope = { kind: 'global' };
  words: Words | null = null;

  readonly book = new SeatBook();
  readonly sprites = new SpriteCache();
  readonly anim: Anim = { walks: new Map(), cards: [], planes: [] };
  private seenSeats = new Set<string>();
  private seenItems = new Map<string, { ar: Area | null; pos: Pt | null }>();
  private planeSeen: Set<string> | null = null;
  private hairShift = new Map<string, number>();

  sel: string | null = null;
  hl: string | null = null;
  follow: TeamKey | null = null;
  private followScroll = false;
  zoom = 0;
  traffic = true;
  paused = false;
  replayT: number | null = null;
  private replayPlay: { from: number; t0: number } | null = null;
  /** The frozen wall clock of `__flSetTime` (ms), or null for real time. */
  frozenAt: number | null = null;

  private lastDraw = 0;
  private timer: ReturnType<typeof setTimeout> | null = null;
  private minuteTimer: ReturnType<typeof setTimeout> | null = null;
  private rafPending = false;
  private intersecting = true;
  private observer: IntersectionObserver | null = null;
  private unsubscribe: (() => void) | null = null;
  private hiddenSince: number | null = null;
  private flags = { typing: false, error: false, elapsed: false };
  private stripMemo: { key: string; data: StripData } | null = null;
  private listeners = new Set<Listener>();
  private snap: EngineSnapshot;
  private failed = false;

  last: Frame | null = null;
  /** Boxes the last geometry check looked at (diagnostics). */
  lastBoxes = 0;
  /** Frames drawn since load (the dev frame counter). */
  frames = 0;

  constructor(
    private readonly store: typeof appStore = appStore,
    private readonly ring: ReplayRing = floorRing,
  ) {
    this.snap = this.makeSnapshot(0);
  }

  // -------------------------------------------------------------------------
  // React side: mount, unmount, subscribe
  // -------------------------------------------------------------------------

  mount(els: { canvas: HTMLCanvasElement; root: HTMLElement; wrap: HTMLElement; scroll: HTMLElement; counter: HTMLElement | null }, scope: FloorScope, words: Words): void {
    this.unmountCanvas();
    this.canvas = els.canvas;
    this.root = els.root;
    this.wrap = els.wrap;
    this.scroll = els.scroll;
    this.counter = els.counter;
    this.words = words;
    this.setScope(scope);
    let ctx: Ctx2D | null = null;
    try {
      ctx = els.canvas.getContext('2d') as Ctx2D | null;
    } catch {
      ctx = null;
    }
    this.ctx = ctx;
    // Hidden ≥ 60 s: compaction (I4); the floor then lays out from scratch, without walk-ins.
    const now = clock.now();
    if (this.hiddenSince !== null && now - this.hiddenSince >= 60_000) this.tidy(false);
    this.hiddenSince = null;
    this.lastDraw = 0;
    if (typeof IntersectionObserver === 'function') {
      this.observer = new IntersectionObserver((entries) => {
        const e = entries[entries.length - 1];
        const was = this.intersecting;
        this.intersecting = !e || e.isIntersecting;
        if (this.intersecting && !was) this.request();
        if (!this.intersecting) this.settle();
      });
      this.observer.observe(els.canvas);
    }
    this.unsubscribe = this.store.subscribe((s, prev) => {
      if (s.world !== prev.world || s.ui.locale !== prev.ui.locale || s.ui.theme !== prev.ui.theme || s.ui.visibility !== prev.ui.visibility) {
        if (s.ui.visibility !== prev.ui.visibility && !this.visible()) this.settle();
        this.request();
      }
    });
    if (typeof document !== 'undefined') document.addEventListener('visibilitychange', this.onVisibility);
    this.request();
  }

  unmountCanvas(): void {
    if (!this.canvas) return;
    this.observer?.disconnect();
    this.observer = null;
    this.unsubscribe?.();
    this.unsubscribe = null;
    if (typeof document !== 'undefined') document.removeEventListener('visibilitychange', this.onVisibility);
    this.clearTimers();
    this.canvas = null;
    this.ctx = null;
    this.root = null;
    this.wrap = null;
    this.scroll = null;
    this.counter = null;
    this.last = null;
    this.hiddenSince = clock.now();
    this.settle();
  }

  setScope(scope: FloorScope): void {
    const same = scope.kind === this.scope.kind && (scope.kind === 'global' || (this.scope.kind === 'team' && this.scope.teamKey === scope.teamKey));
    this.scope = scope;
    if (!same) {
      this.lastDraw = 0;
      this.anim.walks.clear();
      this.anim.cards = [];
      this.anim.planes = [];
    }
  }

  setWords(words: Words): void {
    this.words = words;
    this.request();
  }

  /** Stable across renders (useSyncExternalStore resubscribes when it changes). */
  subscribe = (l: Listener): (() => void) => {
    this.listeners.add(l);
    return () => {
      this.listeners.delete(l);
    };
  };

  getSnapshot = (): EngineSnapshot => this.snap;

  private makeSnapshot(rev: number): EngineSnapshot {
    return { sel: this.sel, follow: this.follow, paused: this.paused, replayT: this.replayT, playing: !!this.replayPlay, zoom: this.zoom, traffic: this.traffic, frozen: this.frozenAt !== null, rev };
  }

  private emit(): void {
    this.snap = this.makeSnapshot(this.snap.rev + 1);
    for (const l of this.listeners) l();
  }

  // -------------------------------------------------------------------------
  // Clocks and conditions
  // -------------------------------------------------------------------------

  nowMs(): number {
    return this.frozenAt ?? clock.wall();
  }

  private rt(): number {
    return clock.now() / 1000;
  }

  /** Nothing moves: reduced motion, Pause, replay or a frozen clock. */
  still(): boolean {
    return prefersReducedMotion() || this.paused || this.replayT !== null || this.frozenAt !== null;
  }

  visible(): boolean {
    if (!this.canvas) return false;
    if (typeof document !== 'undefined' && document.hidden) return false;
    const v = this.store.getState().ui.visibility;
    if (v.occluded || v.asleep) return false;
    return this.intersecting;
  }

  private onVisibility = (): void => {
    if (this.visible()) this.request();
    else this.settle();
  };

  /** Stop every timer and let animations end (hidden, occluded, unmounted, Pause). */
  private settle(): void {
    this.clearTimers();
    this.anim.walks.clear();
    this.anim.cards = [];
    this.anim.planes = [];
    if (this.replayPlay) {
      this.replayT = this.replayPlay.from;
      this.replayPlay = null;
    }
  }

  private clearTimers(): void {
    if (this.timer) clearTimeout(this.timer);
    if (this.minuteTimer) clearTimeout(this.minuteTimer);
    this.timer = null;
    this.minuteTimer = null;
  }

  /** Request exactly one frame (coalesced). */
  request(): void {
    if (!this.canvas || this.rafPending) return;
    if (!this.visible()) return;
    this.rafPending = true;
    const run = () => {
      this.rafPending = false;
      this.frame();
    };
    if (typeof requestAnimationFrame === 'function') requestAnimationFrame(run);
    else setTimeout(run, 0);
  }

  /** The current animation rate in frames per second (0 = no frame is scheduled). */
  rate(): number {
    if (!this.canvas || !this.visible()) return 0;
    if (this.replayPlay) return 15;
    if (this.anim.planes.length || this.anim.cards.length || this.anim.walks.size) return 15;
    if (this.still()) return 0;
    const front = this.store.getState().ui.visibility.frontmost;
    if (this.flags.typing) return front ? 4 : 1;
    if (this.flags.error) return front ? 2 : 1;
    return 0;
  }

  private schedule(): void {
    this.clearTimers();
    if (!this.canvas || !this.visible()) return;
    const r = this.rate();
    if (r > 0) this.timer = setTimeout(() => this.frame(), 1000 / r);
    else if (this.flags.elapsed && this.frozenAt === null && this.replayT === null) {
      // elapsed labels change once a minute: one frame at the next minute boundary
      const now = this.nowMs();
      this.minuteTimer = setTimeout(() => this.frame(), 60_000 - (now % 60_000) + 50);
    }
  }

  /** Draw now and schedule what is due. */
  frame(): void {
    if (!this.canvas) return;
    try {
      this.draw(false);
    } catch (e) {
      if (!this.failed) {
        this.failed = true;
        console.error('[gilde] floor frame failed', e);
      }
      return;
    }
    this.schedule();
  }

  // -------------------------------------------------------------------------
  // The frame
  // -------------------------------------------------------------------------

  private identityFor(P: ReturnType<typeof readPalette>, frameIds: Map<string, Identity>, seat: FlSeat): Identity {
    const have = frameIds.get(seat.id);
    if (have) return have;
    const id = identityOf(P, seat.uid ?? seat.id, seat.vendor, seat.colour ? P.id[seat.colour] || null : null, this.hairShift.get(seat.id) ?? 0);
    frameIds.set(seat.id, id);
    return id;
  }

  /** Distinguishability (§4.1): within a bay, two characters differ in at least two of accessory, shirt, hair, skin. */
  private resolveIdentities(P: ReturnType<typeof readPalette>, vm: FlVm, rooms: Room[], cells: Map<string, Cell>): Map<string, Identity> {
    const ids = new Map<string, Identity>();
    for (const r of rooms) {
      const seen: Identity[] = [];
      for (const [id, c] of cells) {
        if (c.room !== r) continue;
        const seat = vm.seats.get(id);
        if (!seat) continue;
        let shift = this.hairShift.get(id) ?? 0;
        let ident = identityOf(P, seat.uid ?? seat.id, seat.vendor, seat.colour ? P.id[seat.colour] || null : null, shift);
        const differs = (a: Identity, b: Identity) => (a.vendor !== b.vendor ? 1 : 0) + (a.shirt !== b.shirt ? 1 : 0) + (a.hair !== b.hair ? 1 : 0) + (a.skin !== b.skin ? 1 : 0);
        for (let tries = 0; tries < 6 && seen.some((o) => differs(o, ident) < 2); tries++) {
          shift++;
          ident = identityOf(P, seat.uid ?? seat.id, seat.vendor, seat.colour ? P.id[seat.colour] || null : null, shift);
        }
        if (shift !== (this.hairShift.get(id) ?? 0)) this.hairShift.set(id, shift); // the later one changes; the earlier never does
        ids.set(id, ident);
        seen.push(ident);
      }
    }
    return ids;
  }

  private buildFrameModel(nowMs: number, structure: Structure | null): FlVm {
    const words = this.words;
    const world = this.store.getState().world;
    return buildVm(world, nowMs, {
      untitled: (vendor) => (words ? words.t('floor.untitled', { vendor }) : vendor),
      request: (k) => (words ? words.t('floor.request', { k }) : String(k)),
      structure,
    });
  }

  /** Lay out at the largest integer scale that fits (text bands re-sized per scale); zoom steps from there. */
  private layout(vm: FlVm, lanes: StripLane[], cssW: number, availH: number, dpr: number): { scale: number; M: Metrics; rooms: Room[]; world: { w: number; h: number }; dpr: number } {
    const noProject = this.words ? this.words.t('floor.noProject') : '';
    const opts = { scope: this.scope, noProjectLabel: noProject };
    const at = (sv: number) => {
      const M = metrics(sv);
      const rooms = buildRooms(vm, M, this.book, opts);
      const world = packRooms(rooms, Math.floor(cssW / sv), M);
      world.w = Math.max(world.w, 160);
      return { M, rooms, world };
    };
    let fitted = MIN_SCALE;
    let res = at(MIN_SCALE);
    for (const sv of FIT_SCALES) {
      const r = at(sv);
      const sh = stripHeight(r.M, lanes.length);
      if (sv === MIN_SCALE || (r.world.w * sv <= cssW && (r.world.h + sh) * sv <= availH * 1.6)) {
        fitted = sv;
        res = r;
        break;
      }
    }
    let scale = fitted;
    if (this.zoom !== 0) {
      scale = Math.max(MIN_SCALE, Math.min(MAX_SCALE, fitted + this.zoom));
      if (scale !== fitted) res = at(scale);
    }
    // memory guard: the backing store stays under MAX_CANVAS_PIXELS
    let d = dpr;
    const area = (s: number, dd: number) => res.world.w * s * dd * ((res.world.h + stripHeight(res.M, lanes.length)) * s * dd);
    while (area(scale, d) > MAX_CANVAS_PIXELS && (d > 1 || scale > MIN_SCALE)) {
      if (d > 1) d = 1;
      else {
        scale--;
        res = at(scale);
      }
    }
    return { scale, M: res.M, rooms: res.rooms, world: res.world, dpr: d };
  }

  /** Draw one frame; with `record`, return the boxes of everything that must not collide. */
  draw(record: boolean): Box[] | null {
    const c = this.canvas;
    if (!c) return null;
    const ctx: Ctx2D = this.ctx ?? new NullCtx();
    const words = this.words;
    if (!words) return null;
    const P = readPalette(this.root ?? c);
    this.sprites.sync(paletteSignature(P));
    const rt = this.rt();
    const liveNow = this.nowMs();
    if (this.replayPlay) {
      const T = this.replayPlay.from + (rt - this.replayPlay.t0) * 6 * 1000;
      if (T >= liveNow) {
        this.replayT = null;
        this.replayPlay = null;
        this.emit();
      } else this.replayT = T;
    }
    const replay = this.replayT !== null;
    const structure = replay ? this.ring.at(this.replayT as number) : null;
    const nowMs = replay ? (this.replayT as number) : liveNow;
    const vm = this.buildFrameModel(nowMs, structure);
    const liveVm = replay ? this.buildFrameModel(liveNow, null) : vm;
    const lanes = stripLanes(liveVm, this.scope, this.follow, words.t('floor.you'));
    const wrapW = this.wrap?.clientWidth ?? 0;
    const cssW = Math.max(MIN_CSS_W, wrapW > 0 ? wrapW : 1200);
    const availH = Math.max(260, (this.scroll?.clientHeight || 600) - 120);
    const dpr0 = Math.min(2, Math.max(1, Math.round((typeof window !== 'undefined' && window.devicePixelRatio) || 1)));
    const L = this.layout(vm, lanes, cssW, availH, dpr0);
    const { scale, M, rooms, world } = L;
    const dpr = L.dpr;
    const stripH = stripHeight(M, lanes.length);
    const cw = world.w * scale;
    const ch = (world.h + stripH) * scale;
    if (c.width !== Math.round(cw * dpr) || c.height !== Math.round(ch * dpr)) {
      c.width = Math.round(cw * dpr);
      c.height = Math.round(ch * dpr);
    }
    c.style.width = cw + 'px';
    c.style.height = ch + 'px';
    const cells = cellsOf(rooms, M);
    if (this.sel && !cells.has(this.sel)) this.sel = null;

    // animations (live only): walk-ins, card trips, planes
    const still = this.still();
    const fresh = !replay && this.frozenAt === null && this.lastDraw > 0 && rt - this.lastDraw < 2.5 && !still;
    if (still) {
      this.anim.planes = [];
      this.anim.cards = [];
      this.anim.walks.clear();
    }
    if (!replay) {
      for (const [id, cell] of cells) {
        if (this.seenSeats.has(id)) continue;
        this.seenSeats.add(id);
        if (fresh) {
          const path = walkPath(cell.room, cell);
          this.anim.walks.set(id, { path, t0: rt, dur: pathLen(path) / 40 });
        }
      }
    }
    for (const [id, w] of this.anim.walks) if (replay || !cells.has(id) || rt - w.t0 >= w.dur) this.anim.walks.delete(id);
    this.anim.cards = this.anim.cards.filter((a) => rt - a.t0 < a.dur);
    this.anim.planes = this.anim.planes.filter((p) => rt - p.t0 < p.dur);
    const seated = (id: string) => !this.anim.walks.has(id);
    if (!replay) {
      for (const r of rooms) {
        for (const l of r.lines) {
          for (const it of l.team.items) {
            const key = l.team.key + ':' + it.key;
            const pos = itemPos(l, it, cells, seated);
            const prev = this.seenItems.get(key);
            if (prev && pos && prev.pos && prev.ar !== pos.ar && fresh) {
              const ty = trackY(l, M) - 3;
              const from = prev.pos;
              const path = [from, { x: from.x, y: ty }, { x: pos.x, y: ty }, { x: pos.x, y: pos.y }];
              this.anim.cards = this.anim.cards.filter((a) => a.key !== key);
              this.anim.cards.push({ key, it, team: l.team, path, t0: rt, dur: Math.max(0.5, pathLen(path) / 70), band: [lineOy(l) + 31, lineOy(l) + M.lb + 2] });
            }
            this.seenItems.set(key, { ar: pos ? pos.ar : null, pos: pos ? { x: pos.x, y: pos.y } : null });
          }
        }
      }
    }
    const frameIds = this.resolveIdentities(P, vm, rooms, cells);
    const env: DrawEnv = {
      c: ctx,
      P,
      M,
      scale,
      dpr,
      vm,
      rooms,
      cells,
      nowMs,
      rt,
      still,
      replay,
      anim: this.anim,
      sprites: this.sprites,
      identity: (seat) => this.identityFor(P, frameIds, seat),
      dbg: record ? [] : null,
      words,
      sel: this.sel,
      hl: this.hl,
      traffic: this.traffic,
      global: this.scope.kind === 'global',
      follow: this.follow,
      cps: new Map(),
      flags: { typing: false, error: false, elapsed: false },
    };

    ctx.setTransform(1, 0, 0, 1, 0, 0);
    ctx.imageSmoothingEnabled = false;
    ctx.fillStyle = P.bg;
    ctx.fillRect(0, 0, c.width, c.height);
    ctx.setTransform(scale * dpr, 0, 0, scale * dpr, 0, 0);
    ctx.imageSmoothingEnabled = false;
    drawRooms(env);
    for (const r of rooms) for (const l of r.lines) drawShelves(env, l);

    const order: { seat: FlSeat; cell: Cell; team: FlTeam | null }[] = [];
    for (const r of rooms) {
      for (const l of r.lines) for (const id of l.team.line) {
        const seat = vm.seats.get(id);
        const cell = cells.get(id);
        if (seat && cell) order.push({ seat, cell, team: l.team });
      }
      for (const s of r.seats.slice().sort((a, b) => a.idx - b.idx)) {
        const seat = vm.seats.get(s.id);
        const cell = cells.get(s.id);
        if (seat && cell) order.push({ seat, cell, team: null });
      }
    }
    const hits: Hit[] = [];
    for (const o of order) {
      env.cps.set(o.seat.id, drawSeat(env, o.seat, o.cell, o.team));
      hits.push({ x: o.cell.x, y: o.cell.y - 13, w: CELL, h: M.lb + 15, id: o.seat.id });
    }
    for (const [id, w] of this.anim.walks) {
      const seat = vm.seats.get(id);
      const cell = cells.get(id);
      if (seat && cell) env.cps.set(id, drawWalker(env, seat, cell, w));
    }
    for (const r of rooms) for (const l of r.lines) for (const id of l.team.line) if (bottleneckOf(l.team, id as MemberKey)) drawFrameDashed(env, cells.get(id) as Cell, P.warn, id);
    if (!replay) {
      // new hand-offs fly as paper planes in the sender's colour (at most 6 per bay; the rest go to the counts)
      const firstSeen = this.planeSeen === null;
      const seen = this.planeSeen ?? new Set<string>();
      this.planeSeen = seen;
      for (const team of vm.teams) {
        for (const m of team.msgs) {
          if (seen.has(m.key)) continue;
          seen.add(m.key);
          const room = cells.get(m.from)?.room;
          const inBay = this.anim.planes.filter((p) => cells.get(p.from)?.room === room).length;
          if (!firstSeen && fresh && env.cps.has(m.from) && env.cps.has(m.to) && inBay < PLANES_PER_BAY && liveNow - m.at < 2000) {
            this.anim.planes.push({ from: m.from, to: m.to, t0: rt, dur: 0.8 });
          }
        }
      }
    }
    drawTraffic(env);
    drawCardTrips(env);
    for (const o of order) {
      if (this.anim.walks.has(o.seat.id) || o.seat.ghost) continue;
      drawOverlays(env, o.seat, env.cps.get(o.seat.id) as Pt, o.cell, o.team);
    }
    if (!replay) drawPlanes(env);
    if (this.sel && cells.has(this.sel)) drawSelection(env, this.sel, false);
    if (this.hl && this.hl !== this.sel && cells.has(this.hl)) drawSelection(env, this.hl, true);
    drawFloorText(env);
    drawFollowDim(env);
    if (this.followScroll) {
      this.followScroll = false;
      const fr = rooms.find((r) => r.lines.some((l) => l.team.key === this.follow));
      if (fr) {
        this.wrap?.scrollTo?.({ left: Math.max(0, fr.x * scale - 16) });
        this.scroll?.scrollTo?.({ top: Math.max(0, fr.y * scale - 16) });
      }
    }
    const T1 = liveNow;
    const T0 = T1 - STRIP_WIN_S * 1000;
    const memoKey = [this.ring.version, Math.floor(T1 / 1000), lanes.map((l) => l.label + ':' + l.members.join(',')).join('|')].join(';');
    if (!this.stripMemo || this.stripMemo.key !== memoKey) this.stripMemo = { key: memoKey, data: stripData(liveVm, lanes, this.ring, T0, T1) };
    const strip = drawStrip(env, world, lanes, this.stripMemo.data, stripH, this.replayT, T1);
    if (replay) drawReplayBanner(env, words.t('floor.replayBanner', { ago: words.dur(Math.max(0, liveNow - (this.replayT as number))) }));

    this.flags = env.flags;
    this.lastDraw = replay ? 0 : rt;
    this.frames++;
    const readingOrder = order.map((o) => o.seat.id);
    const prevSig = this.last ? this.last.order.join(',') + '|' + this.last.vm.queue.length : '';
    this.last = { vm, rooms, cells, scale, M, world, hits, strip, order: readingOrder };
    if (prevSig !== readingOrder.join(',') + '|' + vm.queue.length) this.emit();
    return env.dbg;
  }

  // -------------------------------------------------------------------------
  // The geometry gate (A9.2) and its clock
  // -------------------------------------------------------------------------

  /** `__flOverlaps()`: draw one frame with the boxes recorded and check them. */
  overlaps(): Overlap[] {
    if (!this.canvas) return [{ a: 'floor', b: 'canvas', kind: 'missing' }];
    const boxes = this.draw(true);
    const f = this.last;
    if (!boxes || !f) return [{ a: 'floor', b: 'frame', kind: 'missing' }];
    this.lastBoxes = boxes.length;
    const out = checkGeometry(boxes, f.cells, f.rooms);
    if (this.counter && this.scroll) {
      const cr = this.counter.getBoundingClientRect();
      const sr = this.scroll.getBoundingClientRect();
      if (cr.height > 0 && sr.height > 0 && sr.bottom > cr.top + 0.5 && sr.top < cr.bottom - 0.5) out.push({ a: 'floor view', b: 'counter', kind: 'counter' });
    }
    this.schedule();
    return out;
  }

  /** Diagnostics for screenshots and checks (showcase hook `__flInfo`). */
  info(): Record<string, unknown> {
    const f = this.last;
    const b = this.canvas?.getBoundingClientRect();
    return {
      mounted: !!this.canvas,
      sel: this.sel,
      scale: f?.scale ?? null,
      seats: f?.cells.size ?? 0,
      rooms: f ? f.rooms.map((r) => ({ key: r.key, x: r.x, y: r.y, w: r.w, h: r.h, lines: r.lines.map((l) => l.team.name), seats: r.seats.length })) : [],
      canvas: b ? { x: Math.round(b.left), y: Math.round(b.top), w: Math.round(b.width), h: Math.round(b.height) } : null,
      frames: this.frames,
      rate: this.rate(),
      frozenAt: this.frozenAt,
      boxes: this.lastBoxes,
      ring: this.ring.size,
    };
  }

  /** Stop the real-time clock and render at `atMs` (the test hook's frozen time). */
  freeze(atMs: number): void {
    this.frozenAt = atMs;
    this.replayT = null;
    this.replayPlay = null;
    this.settle();
    this.emit();
    if (this.canvas) this.frame();
  }

  // -------------------------------------------------------------------------
  // Pointer and keyboard
  // -------------------------------------------------------------------------

  /** The seat under a canvas-relative CSS point. */
  hitAt(x: number, y: number): string | null {
    const f = this.last;
    if (!f) return null;
    const wx = x / f.scale;
    const wy = y / f.scale;
    const h = f.hits.find((b) => wx >= b.x && wx < b.x + b.w && wy >= b.y && wy < b.y + b.h);
    return h ? h.id : null;
  }

  /** The team whose header (wall band or sign row) is under a canvas-relative CSS point. */
  headerAt(x: number, y: number): TeamKey | null {
    const f = this.last;
    if (!f) return null;
    const wx = x / f.scale;
    const wy = y / f.scale;
    for (const r of f.rooms) {
      if (wx < r.x || wx >= r.x + r.w) continue;
      for (const l of r.lines) {
        const top = l.idx === 0 ? r.y : l.y - f.M.signH;
        const bottom = l.idx === 0 ? r.y + WALL : l.y;
        if (wy >= top && wy < bottom) return l.team.key;
      }
    }
    return null;
  }

  /** The replay time under a canvas-relative CSS point on the strip, or null. */
  stripTimeAt(x: number, y: number): number | null {
    const st = this.last?.strip;
    if (!st || y < st.y0 || y >= st.y1 || x < st.x0) return null;
    return st.T0 + ((Math.min(x, st.x1) - st.x0) / (st.x1 - st.x0)) * (st.T1 - st.T0);
  }

  /** A seat's box in canvas CSS px (for the name card). */
  seatRect(id: string): { x: number; y: number; w: number; h: number } | null {
    const f = this.last;
    const cl = f?.cells.get(id);
    if (!f || !cl) return null;
    return { x: cl.x * f.scale, y: (cl.y - 2) * f.scale, w: CELL * f.scale, h: (f.M.lb + 4) * f.scale };
  }

  seat(id: string | null): FlSeat | null {
    return id ? (this.last?.vm.seats.get(id) ?? null) : null;
  }

  select(id: string | null): void {
    if (id === this.sel) return;
    this.sel = id;
    this.emit();
    this.request();
  }

  highlight(id: string | null): void {
    if (id === this.hl) return;
    this.hl = id;
    this.request();
  }

  /** The nearest seat in a direction (±60° cone; score = primary + 2 × cross; ties in reading order). */
  nearest(dir: 'left' | 'right' | 'up' | 'down'): string | null {
    const f = this.last;
    if (!f || f.order.length === 0) return null;
    const from = this.sel ? f.cells.get(this.sel) : undefined;
    if (!from) return f.order[0] ?? null;
    const cx = from.x + CELL / 2;
    const cy = from.y;
    let best: string | null = null;
    let bestScore = Infinity;
    for (const id of f.order) {
      if (id === this.sel) continue;
      const c = f.cells.get(id) as Cell;
      const dx = c.x + CELL / 2 - cx;
      const dy = c.y - cy;
      const primary = dir === 'left' ? -dx : dir === 'right' ? dx : dir === 'up' ? -dy : dy;
      const cross = dir === 'left' || dir === 'right' ? Math.abs(dy) : Math.abs(dx);
      if (primary <= 0 || cross > primary * Math.tan(Math.PI / 3)) continue;
      const score = primary + 2 * cross;
      if (score < bestScore) {
        bestScore = score;
        best = id;
      }
    }
    return best;
  }

  /** The next / previous seat in reading order, or null at the ends. */
  step(delta: 1 | -1): string | null {
    const f = this.last;
    if (!f || f.order.length === 0) return null;
    const i = this.sel ? f.order.indexOf(this.sel) : -1;
    if (i < 0) return delta > 0 ? (f.order[0] ?? null) : (f.order[f.order.length - 1] ?? null);
    return f.order[i + delta] ?? null;
  }

  uidOf(id: string | null): SessionUid | null {
    return this.seat(id)?.uid ?? null;
  }

  setPaused(on: boolean): void {
    this.paused = on;
    if (on) this.settle();
    this.emit();
    this.request();
  }

  setTraffic(on: boolean): void {
    this.traffic = on;
    this.emit();
    this.request();
  }

  zoomBy(d: number): void {
    const next = d === 0 ? 0 : Math.max(-4, Math.min(4, this.zoom + d));
    if (next === this.zoom) return;
    this.zoom = next;
    this.emit();
    this.request();
  }

  /** Follow a team (the selected seat's team, else the first live one); again stops. */
  toggleFollow(): void {
    if (this.follow) {
      this.follow = null;
    } else {
      const vm = this.last?.vm;
      const sel = this.seat(this.sel);
      const live = vm ? vm.teams.filter((t) => t.alive) : [];
      this.follow = sel?.teamKey ?? live[0]?.key ?? null;
      this.followScroll = !!this.follow;
    }
    this.emit();
    this.request();
  }

  /** Tidy (I4): forget the seats so the floor packs from scratch; one redraw, no walk-ins. */
  tidy(redraw = true): void {
    this.book.clear();
    this.hairShift.clear();
    this.lastDraw = 0;
    this.anim.walks.clear();
    this.seenItems.clear();
    if (redraw) this.request();
  }

  // replay (pixel-office §2.7)

  private replayLo(): number {
    const start = this.ring.start ?? this.nowMs();
    return Math.max(start, this.nowMs() - STRIP_WIN_S * 1000);
  }

  replayAt(T: number): void {
    if (T >= this.nowMs()) {
      this.goLive();
      return;
    }
    this.replayT = Math.max(this.replayLo(), T);
    this.replayPlay = null;
    this.emit();
    this.request();
  }

  /** Play the last 10 minutes at 6× (steps instead of playing under reduced motion or Pause). */
  replayPlayFromStart(): void {
    const lo = this.replayLo();
    if (prefersReducedMotion() || this.paused || this.frozenAt !== null) {
      this.replayAt(lo);
      return;
    }
    this.replayT = lo;
    this.replayPlay = { from: lo, t0: this.rt() };
    this.emit();
    this.request();
  }

  stepReplay(deltaMs: number): void {
    this.replayAt((this.replayT ?? this.nowMs()) + deltaMs);
  }

  goLive(): void {
    if (this.replayT === null && !this.replayPlay) return;
    this.replayT = null;
    this.replayPlay = null;
    this.lastDraw = 0;
    this.emit();
    this.request();
  }

  /** Esc: leave replay → stop follow → (the card and selection are React's) → false when nothing was left. */
  stepOut(): boolean {
    if (this.replayT !== null || this.replayPlay) {
      this.goLive();
      return true;
    }
    if (this.follow) {
      this.follow = null;
      this.emit();
      this.request();
      return true;
    }
    return false;
  }

  /** Tests: forget everything (a fresh window). */
  resetForTests(): void {
    this.unmountCanvas();
    this.tidy(false);
    this.seenSeats.clear();
    this.planeSeen = null;
    this.sel = null;
    this.hl = null;
    this.follow = null;
    this.zoom = 0;
    this.traffic = true;
    this.paused = false;
    this.replayT = null;
    this.replayPlay = null;
    this.frozenAt = null;
    this.hiddenSince = null;
    this.frames = 0;
    this.failed = false;
    this.stripMemo = null;
    this.intersecting = true;
  }
}

/** The window's Floor engine. */
export const floorEngine = new FloorEngine();
